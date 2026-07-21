//! Governed, durable outbound A2A: the kernel as an A2A client (ADR 0009).
//!
//! A registered `agent` extension is a REMOTE endpoint declared by manifest
//! (Agent Card URL + the `network` permission). Delegation is an ordinary
//! kernel action: the model proposes it, the kernel validates and
//! authorizes, and THIS crate dials the remote agent — never the model,
//! never a subprocess.
//!
//! Governance lives on the kernel's own egress:
//! - **SSRF allowlist:** every request is pinned to the host of the
//!   manifest-declared card URL; redirects are disabled; a card advertising
//!   a service endpoint on another host is refused.
//! - **Version + shape validation:** requests carry `A2A-Version: 1.0`;
//!   responses are parsed strictly and unknown task states are rejected —
//!   a hostile agent cannot feed the run an unvalidated observation.
//! - **Durability:** the created task id is journaled (`ExecutionContext::
//!   record_note`) the moment it exists; a crash-resume reconnects via
//!   `GetTask` instead of re-delegating (exactly-once delegation).
//!
//! Implements A2A protocol v1.0.0 (JSON-RPC binding) per the canonical
//! spec/proto — see docs/BUILD-EVIDENCE.md Phase 4b for the live-source
//! log. Minimal client by design (two methods); the official `a2a-client-lf`
//! crate is noted as a possible future swap.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;

use kernel_core::{
    crash_point, ActionSchema, ActionSource, ActionSpec, CircuitCheck, ExecutionContext,
    ExecutionError, Latitude, ValidatedAction, A2A_ANCESTRY_ARG, A2A_CONTINUATION_INPUT_NOTE,
    A2A_DEPTH_ARG, IDEMPOTENCY_KEY_ARG,
};

pub const A2A_VERSION: &str = "1.0";
pub const WELL_KNOWN_CARD: &str = "/.well-known/agent-card.json";
const NOTE_TASK_ID: &str = "a2a_task_id";
/// Set the moment a continuation message is dispatched to a paused remote task
/// (ADR 0026), so a resume after the marker is journaled RE-POLLS rather than
/// re-sending — the same at-most-reconnect durability the initial send's
/// `NOTE_TASK_ID` provides.
const NOTE_CONTINUATION_SENT: &str = "a2a_continuation_sent";
/// Durability pivot for an inline (Task-less) `SendMessage` completion (audit
/// finding 3-a hardening): `NOTE_TASK_ID` is only journaled when the remote
/// returns a `Task` to reconnect to; a remote that answers inline (a bare
/// `Message`, no task) had NOTHING journaled before `execute` returned
/// `TASK_STATE_COMPLETED` — so a crash after that return but before the
/// engine's `Receipt` lands would resume into a dangling `Intent` with no
/// reconnect marker, and re-dispatch would send a FRESH `SendMessage` (a new
/// `messageId`, since the wire protocol never carries `idempotency_key`),
/// double-executing the remote task. Journaling the completed output under
/// this key the moment it exists closes the gap exactly like `NOTE_TASK_ID`
/// does for the Task-returning path: a resume checks this FIRST and returns
/// the SAME output instead of re-dialing.
const NOTE_INLINE_COMPLETE: &str = "a2a_inline_complete";

/// Cap on any single non-streaming JSON response body the client buffers
/// (audit finding 2-c/#17): `reqwest`'s `.json()` reads the whole body into
/// memory with no bound, so a registered-but-hostile agent could return a
/// giant Agent Card or RPC reply and OOM the daemon. 8 MiB is orders of
/// magnitude above any real card or task response, so it never trips a
/// legitimate peer. Enforced by [`A2aClient::read_json_capped`].
const MAX_RESPONSE_BYTES: usize = 8 << 20; // 8 MiB

/// Cap on the SSE reassembly buffer (ADR 0027 hardening, audit finding 9-a):
/// `stream_to_terminal`'s `buf` grows with every chunk until a complete event
/// (`\n\n`-terminated) is found. A streaming peer that never sends a
/// separator would otherwise grow it without bound — an OOM vector gated only
/// on the peer being registered/trusted enough to dial, not on it being
/// well-behaved. Once the UNDRAINED tail exceeds this, the dial falls back to
/// polling exactly like any other mid-stream failure — never grows further.
const MAX_SSE_BUFFER_BYTES: usize = 1 << 20; // 1 MiB

pub mod gate;
pub mod registry;
pub mod server;

pub use gate::{
    parse_agent_ask_fallback, parse_agent_delegation_policy, AgentAskFallback,
    AgentAskPolicyConfig, AgentDelegationPolicy, RegistryDelegationGate,
};
pub use registry::{AgentManifest, AgentRegistry};

/// A manifest-registered remote agent.
#[derive(Debug, Clone)]
pub struct RemoteAgent {
    pub name: String,
    pub card_url: String,
    /// Optional bearer token for a remote agent whose endpoint requires auth
    /// (e.g. another Kelter's inbound A2A endpoint, ADR 0013). Read from an
    /// env var by the daemon — never stored in the manifest/tree. `None` for
    /// an open endpoint (like the toy target).
    pub token: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum A2aError {
    #[error("invalid card url '{0}': {1}")]
    BadUrl(String, String),
    #[error("agent '{agent}' HTTP error: {source}")]
    Http {
        agent: String,
        #[source]
        source: reqwest::Error,
    },
    #[error(
        "SSRF allowlist violation for agent '{0}': card advertises endpoint '{1}' outside the \
         declared host '{2}' — refusing to dial"
    )]
    SsrfViolation(String, String, String),
    #[error("agent '{0}' card declares no JSONRPC interface")]
    NoJsonRpcInterface(String),
    #[error("agent '{0}' returned a protocol violation: {1}")]
    Protocol(String, String),
    #[error("agent '{0}' returned JSON-RPC error {1}: {2}")]
    Rpc(String, i64, String),
    #[error("agent '{0}' is already registered")]
    AlreadyRegistered(String),
    #[error("agent '{0}' is not registered")]
    NotFound(String),
    #[error("agent registry I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("agent registry file is corrupt: {0}")]
    Toml(String),
}

// ---- wire types (A2A v1.0.0, JSON-RPC binding) ---------------------------

#[derive(Debug, Deserialize)]
struct AgentCard {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(rename = "supportedInterfaces")]
    supported_interfaces: Vec<AgentInterface>,
    /// The remote's declared capabilities. Only `streaming` is read (ADR
    /// 0027): a card that does not advertise it is dialed with plain
    /// `SendMessage` + `GetTask` polling, exactly as before — the client
    /// never sends `SendStreamingMessage` to an endpoint that did not
    /// announce support, so streaming can never accidentally create a second
    /// task on a non-streaming peer.
    #[serde(default)]
    capabilities: AgentCapabilities,
}

#[derive(Debug, Deserialize, Default)]
struct AgentCapabilities {
    #[serde(default)]
    streaming: bool,
}

#[derive(Debug, Deserialize)]
struct AgentInterface {
    url: String,
    #[serde(rename = "protocolBinding")]
    protocol_binding: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Task {
    pub id: String,
    pub status: TaskStatus,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskStatus {
    pub state: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Artifact {
    #[serde(default)]
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Part {
    #[serde(default)]
    pub text: Option<String>,
}

const KNOWN_STATES: [&str; 9] = [
    "TASK_STATE_UNSPECIFIED",
    "TASK_STATE_SUBMITTED",
    "TASK_STATE_WORKING",
    "TASK_STATE_COMPLETED",
    "TASK_STATE_FAILED",
    "TASK_STATE_CANCELED",
    "TASK_STATE_INPUT_REQUIRED",
    "TASK_STATE_REJECTED",
    "TASK_STATE_AUTH_REQUIRED",
];

fn is_terminal(state: &str) -> bool {
    matches!(
        state,
        "TASK_STATE_COMPLETED"
            | "TASK_STATE_FAILED"
            | "TASK_STATE_CANCELED"
            | "TASK_STATE_REJECTED"
    )
}

fn artifact_text(task: &Task) -> String {
    task.artifacts
        .iter()
        .flat_map(|a| a.parts.iter())
        .filter_map(|p| p.text.as_deref())
        .collect::<Vec<_>>()
        .join("\n")
}

// ---- the client ----------------------------------------------------------

/// One remote agent, dialed with a pinned origin.
pub struct A2aClient {
    http: reqwest::Client,
    agent: String,
    card_url: String,
    /// scheme://host:port the manifest declared — the ONLY origin dialed.
    allowed_origin: String,
    /// Optional bearer token sent to an endpoint that requires auth (another
    /// Kelter's inbound endpoint declares this in its Agent Card).
    token: Option<String>,
    endpoint: tokio::sync::OnceCell<String>,
    /// (card name, card description), captured the moment the card is first
    /// resolved (same `endpoint()` dial) — used by [`Self::verify`] so
    /// dynamic discovery (ADR 0023) never needs a second request.
    card_info: tokio::sync::OnceCell<(String, String)>,
    /// Whether the resolved card advertised `capabilities.streaming` (ADR
    /// 0027), captured on the SAME `endpoint()` dial. Gates the streaming
    /// path so the client never sends `SendStreamingMessage` to a peer that
    /// did not announce support.
    card_streaming: tokio::sync::OnceCell<bool>,
}

/// Identifies these calls as originating from a Kelter A2A client, so an
/// inbound peer can attribute the caller in its journal (ADR 0013).
const CALLER_HEADER: &str = "A2A-Caller";
const CALLER_ID: &str = "kelter-a2a-client";

fn origin_of(url: &str) -> Result<String, String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| e.to_string())?;
    let host = parsed.host_str().ok_or("no host")?;
    let scheme = parsed.scheme();
    match parsed.port() {
        Some(p) => Ok(format!("{scheme}://{host}:{p}")),
        None => Ok(format!("{scheme}://{host}")),
    }
}

impl A2aClient {
    pub fn new(agent: &RemoteAgent) -> Result<Self, A2aError> {
        let mut card_url = agent.card_url.clone();
        // A bare origin is allowed in the manifest; normalize to the
        // well-known discovery path.
        let parsed = reqwest::Url::parse(&card_url)
            .map_err(|e| A2aError::BadUrl(card_url.clone(), e.to_string()))?;
        if parsed.path() == "/" || parsed.path().is_empty() {
            card_url = format!("{}{}", card_url.trim_end_matches('/'), WELL_KNOWN_CARD);
        }
        let allowed_origin =
            origin_of(&card_url).map_err(|e| A2aError::BadUrl(card_url.clone(), e))?;
        Ok(Self {
            // Redirects disabled: a redirect is an egress escape hatch.
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(15))
                .build()
                .expect("client builds"),
            agent: agent.name.clone(),
            card_url,
            allowed_origin,
            token: agent.token.clone(),
            endpoint: tokio::sync::OnceCell::new(),
            card_info: tokio::sync::OnceCell::new(),
            card_streaming: tokio::sync::OnceCell::new(),
        })
    }

    /// Attach the common A2A headers (version, caller id) and — when the
    /// remote requires it — the bearer token.
    fn with_headers(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let req = req
            .header("A2A-Version", A2A_VERSION)
            .header(CALLER_HEADER, CALLER_ID);
        match &self.token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    pub fn agent_name(&self) -> &str {
        &self.agent
    }

    /// The pinned origin (`scheme://host:port`) this client dials — the
    /// SSRF allowlist pin, reused as the ancestry token for A2A delegation
    /// lineage (ADR 0022): origins, not agent display names, are the right
    /// identity for cross-deployment cycle detection (a display name is
    /// locally chosen per-deployment and not globally unique).
    pub fn allowed_origin(&self) -> &str {
        &self.allowed_origin
    }

    /// Read a response body as JSON with a hard size cap (audit 2-c/#17):
    /// reject early on an advertised `Content-Length` over the cap, then
    /// stream the body accumulating into a buffer that is never allowed to
    /// grow past [`MAX_RESPONSE_BYTES`] — so a hostile peer cannot OOM the
    /// daemon by returning an unbounded body, whether or not it advertises a
    /// length. Replaces `reqwest`'s unbounded `.json()`.
    async fn read_json_capped<T: serde::de::DeserializeOwned>(
        &self,
        resp: reqwest::Response,
    ) -> Result<T, A2aError> {
        if let Some(len) = resp.content_length() {
            if len as usize > MAX_RESPONSE_BYTES {
                return Err(A2aError::Protocol(
                    self.agent.clone(),
                    format!(
                        "response body advertises {len} bytes, over the {MAX_RESPONSE_BYTES}-byte cap"
                    ),
                ));
            }
        }
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| A2aError::Http {
                agent: self.agent.clone(),
                source: e,
            })?;
            if buf.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(A2aError::Protocol(
                    self.agent.clone(),
                    format!(
                        "response body exceeded the {MAX_RESPONSE_BYTES}-byte cap — refusing to buffer more"
                    ),
                ));
            }
            buf.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&buf)
            .map_err(|e| A2aError::Protocol(self.agent.clone(), format!("malformed JSON: {e}")))
    }

    /// Resolve the Agent Card and pick the JSONRPC interface — refusing any
    /// endpoint outside the declared origin.
    async fn endpoint(&self) -> Result<&str, A2aError> {
        self.endpoint
            .get_or_try_init(|| async {
                let resp = self
                    .with_headers(self.http.get(&self.card_url))
                    .send()
                    .await
                    .and_then(|r| r.error_for_status())
                    .map_err(|e| A2aError::Http {
                        agent: self.agent.clone(),
                        source: e,
                    })?;
                let card: AgentCard = self.read_json_capped(resp).await?;

                let interface = card
                    .supported_interfaces
                    .iter()
                    .find(|i| i.protocol_binding == "JSONRPC")
                    .ok_or_else(|| A2aError::NoJsonRpcInterface(self.agent.clone()))?;

                let endpoint_origin = origin_of(&interface.url)
                    .map_err(|e| A2aError::BadUrl(interface.url.clone(), e))?;
                if endpoint_origin != self.allowed_origin {
                    return Err(A2aError::SsrfViolation(
                        self.agent.clone(),
                        interface.url.clone(),
                        self.allowed_origin.clone(),
                    ));
                }
                let _ = self
                    .card_info
                    .set((card.name.clone(), card.description.clone()));
                let _ = self.card_streaming.set(card.capabilities.streaming);
                tracing::info!(
                    agent = %self.agent,
                    card_name = %card.name,
                    endpoint = %interface.url,
                    streaming = card.capabilities.streaming,
                    "a2a: card resolved (origin-pinned)"
                );
                Ok(interface.url.clone())
            })
            .await
            .map(String::as_str)
    }

    /// Dial the card and resolve its JSONRPC endpoint (SSRF-pinning
    /// enforced), without sending any task. Used by `AgentRegistry::register`
    /// (ADR 0023) to fail fast at registration time — a bad URL, an
    /// unreachable host, or a hostile redirect is refused HERE, before the
    /// agent is ever persisted, rather than only being discovered on first
    /// real delegation. Returns the card's own declared `(name,
    /// description)` — dynamic discovery registers agents under the name
    /// THEY declare, not an operator-typed one (there is no "install a
    /// local agent" concept for a network protocol).
    pub async fn verify(&self) -> Result<(String, String), A2aError> {
        self.endpoint().await?;
        Ok(self.card_info.get().cloned().unwrap_or_default())
    }

    async fn rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, A2aError> {
        let endpoint = self.endpoint().await?.to_string();
        // Defense in depth: re-check the origin on every dial.
        let origin = origin_of(&endpoint).map_err(|e| A2aError::BadUrl(endpoint.clone(), e))?;
        if origin != self.allowed_origin {
            return Err(A2aError::SsrfViolation(
                self.agent.clone(),
                endpoint,
                self.allowed_origin.clone(),
            ));
        }

        let body = json!({
            "jsonrpc": "2.0",
            "id": uuid::Uuid::new_v4().to_string(),
            "method": method,
            "params": params,
        });
        let resp = self
            .with_headers(self.http.post(&endpoint))
            .json(&body)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| A2aError::Http {
                agent: self.agent.clone(),
                source: e,
            })?;
        let response: serde_json::Value = self.read_json_capped(resp).await?;

        if let Some(err) = response.get("error") {
            return Err(A2aError::Rpc(
                self.agent.clone(),
                err.get("code").and_then(|c| c.as_i64()).unwrap_or(0),
                err.get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("?")
                    .to_string(),
            ));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| A2aError::Protocol(self.agent.clone(), "no result in response".into()))
    }

    fn parse_task(&self, value: serde_json::Value) -> Result<Task, A2aError> {
        let task: Task = serde_json::from_value(value)
            .map_err(|e| A2aError::Protocol(self.agent.clone(), format!("malformed task: {e}")))?;
        // Kernel-side gate: an unknown state is a protocol violation, not
        // something to guess about.
        if !KNOWN_STATES.contains(&task.status.state.as_str()) {
            return Err(A2aError::Protocol(
                self.agent.clone(),
                format!("unknown task state '{}'", task.status.state),
            ));
        }
        Ok(task)
    }

    /// `SendMessage`: returns either a completed-inline Message (text) or a
    /// Task to poll. `lineage`, when present (ADR 0022), embeds the A2A
    /// delegation chain depth + ancestry this run is extending as a
    /// `metadata` object on the outbound message — additive to the A2A wire
    /// shape (an unknown JSON field is expected to be ignored by a
    /// non-Kelter peer, never a protocol break), read back on the inbound
    /// side by `server::message_lineage`.
    pub async fn send_message(
        &self,
        text: &str,
        lineage: Option<(u32, &[String])>,
    ) -> Result<SendOutcome, A2aError> {
        let mut message = json!({
            "messageId": uuid::Uuid::new_v4().to_string(),
            "role": "ROLE_USER",
            "parts": [{"text": text}],
        });
        if let Some((depth, ancestry)) = lineage {
            message["metadata"] = json!({
                "kelter_a2a_depth": depth,
                "kelter_a2a_ancestry": ancestry,
            });
        }
        let result = self
            .rpc("SendMessage", json!({ "message": message }))
            .await?;

        if let Some(task) = result.get("task") {
            return Ok(SendOutcome::Task(self.parse_task(task.clone())?));
        }
        if let Some(message) = result.get("message") {
            let text = message
                .get("parts")
                .and_then(|p| p.as_array())
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            return Ok(SendOutcome::Message(text));
        }
        Err(A2aError::Protocol(
            self.agent.clone(),
            "SendMessage result carries neither task nor message".into(),
        ))
    }

    pub async fn get_task(&self, id: &str) -> Result<Task, A2aError> {
        let result = self.rpc("GetTask", json!({ "id": id })).await?;
        self.parse_task(result)
    }

    /// Continue an existing remote task that entered `input-required` (ADR
    /// 0026): a `SendMessage` carrying `taskId` so the peer routes it to the
    /// paused task rather than creating a new one. The reply is polled via
    /// `get_task` afterwards, so the return here only reports the send.
    pub async fn send_continuation(&self, task_id: &str, text: &str) -> Result<(), A2aError> {
        let message = json!({
            "messageId": uuid::Uuid::new_v4().to_string(),
            "role": "ROLE_USER",
            "parts": [{"text": text}],
            "taskId": task_id,
        });
        self.rpc("SendMessage", json!({ "message": message }))
            .await?;
        Ok(())
    }

    /// Whether the resolved Agent Card advertised `capabilities.streaming`
    /// (ADR 0027). Resolves the card first if it has not been (the same
    /// origin-pinned dial `endpoint()` performs), so a caller can gate the
    /// streaming path before ever sending a message.
    pub async fn supports_streaming(&self) -> Result<bool, A2aError> {
        self.endpoint().await?;
        Ok(self.card_streaming.get().copied().unwrap_or(false))
    }

    /// Open a `SendStreamingMessage` request (ADR 0027) and return the raw
    /// `text/event-stream` response for the caller to consume frame by frame.
    /// Same message shape (and `lineage` metadata) as [`Self::send_message`],
    /// with origin re-pinned exactly as `rpc` does. A generous per-request
    /// timeout overrides the client's short default so a legitimately
    /// long-running stream is not cut mid-flight — the caller still bounds the
    /// whole consume with its own deadline.
    pub async fn open_message_stream(
        &self,
        text: &str,
        lineage: Option<(u32, &[String])>,
    ) -> Result<reqwest::Response, A2aError> {
        let endpoint = self.endpoint().await?.to_string();
        let origin = origin_of(&endpoint).map_err(|e| A2aError::BadUrl(endpoint.clone(), e))?;
        if origin != self.allowed_origin {
            return Err(A2aError::SsrfViolation(
                self.agent.clone(),
                endpoint,
                self.allowed_origin.clone(),
            ));
        }
        let mut message = json!({
            "messageId": uuid::Uuid::new_v4().to_string(),
            "role": "ROLE_USER",
            "parts": [{"text": text}],
        });
        if let Some((depth, ancestry)) = lineage {
            message["metadata"] = json!({
                "kelter_a2a_depth": depth,
                "kelter_a2a_ancestry": ancestry,
            });
        }
        let body = json!({
            "jsonrpc": "2.0",
            "id": uuid::Uuid::new_v4().to_string(),
            "method": "SendStreamingMessage",
            "params": { "message": message },
        });
        let resp = self
            .with_headers(self.http.post(&endpoint))
            .header("Accept", "text/event-stream")
            .timeout(Duration::from_secs(600))
            .json(&body)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| A2aError::Http {
                agent: self.agent.clone(),
                source: e,
            })?;
        Ok(resp)
    }
}

pub enum SendOutcome {
    Task(Task),
    Message(String),
}

// ---- the action source ---------------------------------------------------

/// The `delegate_to_<sanitized-name>` action name a registered agent's
/// display name maps to — the SAME sanitization `A2aActionSource` applies
/// internally, exposed so callers (the daemon's config wiring, the
/// `AgentRegistry`/`RegistryDelegationGate`) never need to reimplement it.
pub fn action_name(agent: &str) -> String {
    let sanitized: String = agent
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("delegate_to_{sanitized}")
}

/// Circuit-breaker state for one agent origin (ADR 0023): daemon-level,
/// cross-run — a circuit is about the AGENT's observed operational health,
/// not one run's budget, so it is keyed by origin and lives here rather
/// than on `Run`.
#[derive(Debug, Clone, Copy, Default)]
struct CircuitState {
    consecutive_failures: u32,
    opened_at: Option<Instant>,
}

/// Remote agents as kernel actions: `delegate_to_<name>(task)`. Runtime-
/// mutable (ADR 0023, mirrors `RegistrySkillGate`'s policy `RwLock`): a
/// newly-registered agent's `delegate_to_X` action appears in
/// `action_schema()` the moment [`Self::register_runtime`] returns, no
/// daemon restart needed. Each client is held behind an `Arc` (not `Clone`
/// itself) so `execute`/`circuit_state` can clone the handle out of the
/// lock and use it across an `.await` without holding the lock.
pub struct A2aActionSource {
    agents: RwLock<Vec<(String, Arc<A2aClient>)>>,
    poll_interval: Duration,
    poll_timeout: Duration,
    circuits: Mutex<HashMap<String, CircuitState>>,
    circuit_failure_threshold: u32,
    circuit_cooldown: Duration,
    /// Whether to consume streamed progress (ADR 0027) for a fresh delegation
    /// dial when the remote's card advertises `capabilities.streaming`. `true`
    /// by default: it is safe (a non-streaming card silently falls back to the
    /// unchanged `SendMessage` + `GetTask` poll path), and a mid-stream
    /// transport failure also falls back to polling the already-created task.
    streaming: bool,
}

impl A2aActionSource {
    pub fn new(agents: Vec<RemoteAgent>) -> Result<Self, A2aError> {
        let mut out = Vec::new();
        for agent in &agents {
            out.push((action_name(&agent.name), Arc::new(A2aClient::new(agent)?)));
        }
        Ok(Self {
            agents: RwLock::new(out),
            poll_interval: Duration::from_secs(1),
            poll_timeout: Duration::from_secs(120),
            circuits: Mutex::new(HashMap::new()),
            circuit_failure_threshold: 3,
            circuit_cooldown: Duration::from_secs(60),
            streaming: true,
        })
    }

    /// Toggle streamed-progress consumption (ADR 0027). Off restores the
    /// pure `SendMessage` + `GetTask` polling behaviour for every dial,
    /// regardless of what a card advertises.
    pub fn with_streaming(mut self, streaming: bool) -> Self {
        self.streaming = streaming;
        self
    }

    /// Set the circuit-breaker's trip threshold + cooldown (ADR 0023
    /// `ResilienceConfig`; defaults 3 consecutive failures / 60s if never
    /// called).
    pub fn with_circuit_breaker(mut self, failure_threshold: u32, cooldown: Duration) -> Self {
        self.circuit_failure_threshold = failure_threshold.max(1);
        self.circuit_cooldown = cooldown;
        self
    }

    pub fn is_empty(&self) -> bool {
        self.agents.read().unwrap().is_empty()
    }

    /// Register a new remote agent at runtime (ADR 0023, Part 3 dynamic
    /// discovery): dials nothing itself — the caller (`AgentRegistry::
    /// register`) already did, via the SAME SSRF-pinning `A2aClient`
    /// construction this uses. Idempotent by action name: re-registering an
    /// existing agent replaces its client (e.g. after a token rotation)
    /// rather than duplicating the action.
    pub fn register_runtime(&self, agent: &RemoteAgent) -> Result<(), A2aError> {
        let client = Arc::new(A2aClient::new(agent)?);
        let name = action_name(&agent.name);
        let mut guard = self.agents.write().unwrap();
        if let Some(entry) = guard.iter_mut().find(|(n, _)| *n == name) {
            entry.1 = client;
        } else {
            guard.push((name, client));
        }
        Ok(())
    }

    /// Drop a runtime-registered agent's action (ADR 0023) — the daemon
    /// calls this after `AgentRegistry::remove` succeeds, so a removed
    /// agent's `delegate_to_X` action disappears from `action_schema()`
    /// immediately.
    pub fn deregister_runtime(&self, agent_name: &str) {
        let name = action_name(agent_name);
        self.agents.write().unwrap().retain(|(n, _)| *n != name);
    }

    /// Record one delegation outcome against `origin` (ADR 0023): success
    /// resets the failure streak and closes the circuit; failure increments
    /// it and, at the configured threshold, opens the circuit for
    /// `circuit_cooldown`.
    fn record_circuit_outcome(&self, origin: &str, success: bool) {
        let mut circuits = self.circuits.lock().unwrap();
        let state = circuits.entry(origin.to_string()).or_default();
        if success {
            state.consecutive_failures = 0;
            state.opened_at = None;
        } else {
            state.consecutive_failures += 1;
            if state.consecutive_failures >= self.circuit_failure_threshold {
                state.opened_at = Some(Instant::now());
            }
        }
    }

    async fn poll_to_terminal(&self, client: &A2aClient, task_id: &str) -> Result<Task, A2aError> {
        let deadline = tokio::time::Instant::now() + self.poll_timeout;
        loop {
            let task = client.get_task(task_id).await?;
            if is_terminal(&task.status.state) {
                return Ok(task);
            }
            // `input-required` is a STOP state, not terminal: the remote is
            // waiting on more input (ADR 0026). Return the task and let
            // `execute` decide — a fresh input-required becomes a durable
            // pause; `auth-required` stays an error (out of scope).
            if task.status.state == "TASK_STATE_INPUT_REQUIRED" {
                return Ok(task);
            }
            if task.status.state == "TASK_STATE_AUTH_REQUIRED" {
                return Err(A2aError::Protocol(
                    client.agent.clone(),
                    format!(
                        "remote task entered '{}' — auth-required continuation is not supported",
                        task.status.state
                    ),
                ));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(A2aError::Protocol(
                    client.agent.clone(),
                    format!(
                        "remote task {task_id} did not reach a terminal state within {:?}",
                        self.poll_timeout
                    ),
                ));
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }

    /// A fresh streaming dial (ADR 0027): `SendStreamingMessage`, then consume
    /// the `text/event-stream` reply. Each non-final update is recorded as
    /// advisory progress (`ctx.record_progress`) — journaled AND surfaced live
    /// on the timeline, but never a validated action. Returns the final `Task`
    /// (terminal or `input-required`), exactly what `poll_to_terminal` returns,
    /// so `execute`'s downstream handling is identical whether the dial
    /// streamed or polled.
    ///
    /// Durability parity with the poll path: the moment the remote task id
    /// first appears in the stream it is journaled (`NOTE_TASK_ID`) — the same
    /// durability pivot the polling dial has — so a crash mid-stream, or a
    /// mid-stream transport failure, resumes/falls back by POLLING the existing
    /// task, never by re-dialing. Streaming is therefore strictly a
    /// lower-latency-progress enhancement, never a durability regression.
    async fn stream_to_terminal(
        &self,
        client: &A2aClient,
        ctx: &ExecutionContext,
        task_text: &str,
        lineage: Option<(u32, &[String])>,
        action_name: &str,
    ) -> Result<Task, A2aError> {
        let resp = client.open_message_stream(task_text, lineage).await?;
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut task_id: Option<String> = None;
        let mut last_state = String::from("TASK_STATE_SUBMITTED");
        let deadline = tokio::time::Instant::now() + self.poll_timeout;

        loop {
            let chunk = match tokio::time::timeout_at(deadline, stream.next()).await {
                Ok(Some(Ok(bytes))) => bytes,
                // Transport error mid-stream, or the stream closed before a
                // final frame, or our deadline elapsed: fall back to polling
                // the already-created task (resilience). If we never even
                // learned the task id, there is nothing to poll — surface it.
                Ok(Some(Err(e))) => {
                    return self
                        .stream_fallback(
                            client,
                            &task_id,
                            A2aError::Http {
                                agent: client.agent.clone(),
                                source: e,
                            },
                        )
                        .await;
                }
                Ok(None) => {
                    return self
                        .stream_fallback(
                            client,
                            &task_id,
                            A2aError::Protocol(
                                client.agent.clone(),
                                "stream ended before a terminal frame".into(),
                            ),
                        )
                        .await;
                }
                Err(_) => {
                    return self
                        .stream_fallback(
                            client,
                            &task_id,
                            A2aError::Protocol(
                                client.agent.clone(),
                                format!(
                                    "stream did not reach terminal within {:?}",
                                    self.poll_timeout
                                ),
                            ),
                        )
                        .await;
                }
            };
            buf.extend_from_slice(&chunk);

            while let Some(event) = split_sse_event(&mut buf) {
                let Some(json) = parse_sse_data(&event) else {
                    continue;
                };
                // Unwrap the JSON-RPC envelope if present; a bare result is
                // also accepted (permissive on shape, strict on state below).
                let result = json.get("result").cloned().unwrap_or(json);
                let frame = extract_frame(&result);

                // Governance parity with the poll path: an unknown task state
                // is a protocol violation, never guessed about.
                if let Some(state) = &frame.state {
                    if !KNOWN_STATES.contains(&state.as_str()) {
                        return Err(A2aError::Protocol(
                            client.agent.clone(),
                            format!("unknown task state '{state}' in stream"),
                        ));
                    }
                    last_state = state.clone();
                }

                // Durability pivot: journal the task id the FIRST time it
                // appears, before doing anything else with it.
                if let Some(id) = &frame.task_id {
                    if task_id.is_none() {
                        task_id = Some(id.clone());
                        ctx.record_note(NOTE_TASK_ID, json!(id))
                            .await
                            .map_err(|e| {
                                A2aError::Protocol(
                                    client.agent.clone(),
                                    format!("cannot journal streamed task id: {e}"),
                                )
                            })?;
                        tracing::info!(
                            agent = %client.agent_name(),
                            task_id = %id,
                            "a2a: streamed remote task created and journaled"
                        );
                        crash_point("after_a2a_create", action_name);
                    }
                }

                if frame.is_final {
                    let id = task_id
                        .clone()
                        .or_else(|| frame.task_id.clone())
                        .unwrap_or_default();
                    let state = frame.state.clone().unwrap_or_else(|| last_state.clone());
                    return Ok(Task {
                        id,
                        status: TaskStatus { state },
                        artifacts: vec![Artifact {
                            parts: vec![Part {
                                text: Some(frame.text),
                            }],
                        }],
                    });
                }

                // Intermediate progress: advisory only. Needs a known task id
                // (the durability anchor); a progress frame that arrives before
                // any task id is simply skipped rather than mis-attributed.
                if let Some(id) = &task_id {
                    let note = if frame.text.is_empty() {
                        format!("remote task is {last_state}")
                    } else {
                        frame.text.clone()
                    };
                    ctx.record_progress(client.agent_name(), id, &last_state, &note)
                        .await;
                }
            }

            // Reassembly-buffer cap (audit 9-a): the drain loop above already
            // extracted every COMPLETE event this chunk contributed to; what
            // remains in `buf` is only an undrained tail waiting on a future
            // `\n\n`. If that tail alone exceeds the cap, the peer is either
            // hostile or broken — fall back to polling rather than growing
            // `buf` further. Checked strictly AFTER draining, so a burst of
            // many small complete events never trips this spuriously.
            if buf.len() > MAX_SSE_BUFFER_BYTES {
                return self
                    .stream_fallback(
                        client,
                        &task_id,
                        A2aError::Protocol(
                            client.agent.clone(),
                            format!(
                                "SSE reassembly buffer exceeded {MAX_SSE_BUFFER_BYTES} bytes \
                                 without a complete event — refusing to grow further"
                            ),
                        ),
                    )
                    .await;
            }
        }
    }

    /// After a streamed dial is interrupted (ADR 0027), either poll the
    /// already-created task to a terminal/stop state (resilience — the task
    /// exists remotely regardless of our broken stream) or, if the stream
    /// broke before we ever learned a task id, surface the original error.
    async fn stream_fallback(
        &self,
        client: &A2aClient,
        task_id: &Option<String>,
        err: A2aError,
    ) -> Result<Task, A2aError> {
        match task_id {
            Some(id) => {
                tracing::warn!(
                    agent = %client.agent_name(),
                    task_id = %id,
                    "a2a: stream interrupted ({err}) — falling back to GetTask polling"
                );
                self.poll_to_terminal(client, id).await
            }
            None => Err(err),
        }
    }
}

/// One decoded SSE update frame (ADR 0027), reduced to the fields the kernel
/// acts on. Permissive across the A2A streaming shapes (a full task
/// snapshot, a `TaskStatusUpdateEvent`, or a `TaskArtifactUpdateEvent`);
/// state validation happens at the call site.
struct StreamFrame {
    task_id: Option<String>,
    state: Option<String>,
    text: String,
    is_final: bool,
}

fn collect_parts(parts: &serde_json::Value, out: &mut Vec<String>) {
    if let Some(arr) = parts.as_array() {
        for p in arr {
            if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                out.push(t.to_string());
            }
        }
    }
}

fn extract_frame(result: &serde_json::Value) -> StreamFrame {
    let task_id = result
        .get("id")
        .or_else(|| result.get("taskId"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let state = result
        .get("status")
        .and_then(|s| s.get("state"))
        .or_else(|| result.get("state"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let mut texts = Vec::new();
    if let Some(parts) = result.get("artifact").and_then(|a| a.get("parts")) {
        collect_parts(parts, &mut texts);
    }
    if let Some(arr) = result.get("artifacts").and_then(|a| a.as_array()) {
        for a in arr {
            if let Some(parts) = a.get("parts") {
                collect_parts(parts, &mut texts);
            }
        }
    }
    let is_final = result
        .get("final")
        .and_then(|f| f.as_bool())
        .unwrap_or(false)
        || state
            .as_deref()
            .map(|s| is_terminal(s) || s == "TASK_STATE_INPUT_REQUIRED")
            .unwrap_or(false);
    StreamFrame {
        task_id,
        state,
        text: texts.join("\n"),
        is_final,
    }
}

/// Split one complete SSE event (bytes up to the next `\n\n`) out of `buf`,
/// consuming it and its separator. Returns `None` if no complete event is
/// buffered yet.
fn split_sse_event(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    let pos = buf.windows(2).position(|w| w == b"\n\n")?;
    let event: Vec<u8> = buf.drain(..pos).collect();
    buf.drain(..2); // drop the "\n\n" separator
    Some(event)
}

/// Concatenate the `data:` payload lines of one SSE event and JSON-parse
/// them. `None` for a comment/keep-alive event or unparseable data.
fn parse_sse_data(event: &[u8]) -> Option<serde_json::Value> {
    let text = String::from_utf8_lossy(event);
    let mut data = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest.trim_start());
        }
    }
    if data.is_empty() {
        return None;
    }
    serde_json::from_str(&data).ok()
}

#[async_trait]
impl ActionSource for A2aActionSource {
    async fn action_schema(&self, _latitude: Latitude) -> Result<ActionSchema, ExecutionError> {
        Ok(ActionSchema {
            actions: self
                .agents
                .read()
                .unwrap()
                .iter()
                .map(|(name, client)| ActionSpec {
                    name: name.clone(),
                    description: format!(
                        "Delegate a sub-task to the remote agent '{}' (A2A). Provide the full \
                         sub-task as text; the agent's answer comes back as the result.",
                        client.agent_name()
                    ),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "task": {
                                "type": "string",
                                "description": "The sub-task to delegate, self-contained."
                            },
                            IDEMPOTENCY_KEY_ARG: {"type": "string"}
                        },
                        "required": ["task"],
                        "additionalProperties": false
                    }),
                })
                .collect(),
        })
    }

    async fn execute(
        &self,
        action: &ValidatedAction,
        ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, ExecutionError> {
        let client = self
            .agents
            .read()
            .unwrap()
            .iter()
            .find(|(name, _)| name == action.action())
            .map(|(_, c)| c.clone())
            .ok_or_else(|| {
                ExecutionError::Tool(format!("unknown delegation action '{}'", action.action()))
            })?;
        let client = client.as_ref();
        let task_text = action.arguments()["task"].as_str().unwrap_or_default();
        let started = std::time::Instant::now();

        // A2A delegation lineage (ADR 0022): present only when the kernel
        // injected it (i.e. only for an actual `delegate_to_*` action,
        // which this function only ever handles anyway). Only read here —
        // never used on the reconnect path below, since a reconnect polls
        // an existing remote task instead of sending a fresh message.
        let lineage_depth = action
            .arguments()
            .get(A2A_DEPTH_ARG)
            .and_then(|v| v.as_u64())
            .map(|d| d as u32);
        let lineage_ancestry: Option<Vec<String>> = action
            .arguments()
            .get(A2A_ANCESTRY_ARG)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            });
        let lineage: Option<(u32, &[String])> = match (lineage_depth, &lineage_ancestry) {
            (Some(depth), Some(ancestry)) => Some((depth, ancestry.as_slice())),
            _ => None,
        };

        let outcome: Result<(String, String, String), A2aError> = async {
            // Exactly-once reconnect for an inline (Task-less) completion
            // (audit finding 3-a): if this step already completed inline
            // before a crash, return the SAME journaled output — never
            // re-dial. Checked FIRST, before the NOTE_TASK_ID reconnect
            // below, since an inline completion never creates a remote task
            // id to reconnect to in the first place.
            if let Some(output) = ctx.note(NOTE_INLINE_COMPLETE).and_then(|v| v.as_str()) {
                tracing::info!(
                    agent = %client.agent_name(),
                    "a2a: resuming from a journaled inline completion — NOT re-dialing"
                );
                return Ok((
                    "(inline)".to_string(),
                    "TASK_STATE_COMPLETED".to_string(),
                    output.to_string(),
                ));
            }
            // Durable reconnect: if this step already created a remote task
            // before a crash, reattach to it — never delegate twice.
            let task = if let Some(existing) = ctx.note(NOTE_TASK_ID).and_then(|v| v.as_str()) {
                // Interactive continuation (ADR 0026): a human's continuation
                // input for a paused input-required task. Send it ONCE — the
                // NOTE_CONTINUATION_SENT marker (journaled right after the
                // send) makes a later resume re-poll instead of re-sending,
                // the same at-most-reconnect durability NOTE_TASK_ID gives the
                // initial dial.
                let continuation = ctx
                    .note(A2A_CONTINUATION_INPUT_NOTE)
                    .and_then(|v| v.as_str().map(String::from));
                let already_sent = ctx.note(NOTE_CONTINUATION_SENT).is_some();
                if let (Some(input), false) = (&continuation, already_sent) {
                    tracing::info!(
                        agent = %client.agent_name(),
                        task_id = %existing,
                        "a2a: sending human continuation to paused remote task (ADR 0026)"
                    );
                    client.send_continuation(existing, input).await?;
                    ctx.record_note(NOTE_CONTINUATION_SENT, json!(true))
                        .await
                        .map_err(|e| {
                            A2aError::Protocol(
                                client.agent.clone(),
                                format!("cannot journal continuation-sent marker: {e}"),
                            )
                        })?;
                    crash_point("after_a2a_continuation", action.action());
                } else {
                    tracing::info!(
                        agent = %client.agent_name(),
                        task_id = %existing,
                        "a2a: reconnecting to existing remote task from the journal — NOT re-delegating"
                    );
                }
                self.poll_to_terminal(client, existing).await?
            } else if self.streaming && client.supports_streaming().await.unwrap_or(false) {
                // Streaming first dial (ADR 0027): `stream_to_terminal` journals
                // NOTE_TASK_ID itself (durability pivot) and records advisory
                // progress as it goes; it returns the final Task exactly like
                // the poll path, so the outcome mapping below is unchanged.
                tracing::info!(
                    agent = %client.agent_name(),
                    "a2a: dialing with SendStreamingMessage (card advertises streaming)"
                );
                self.stream_to_terminal(client, ctx, task_text, lineage, action.action())
                    .await?
            } else {
                match client.send_message(task_text, lineage).await? {
                    SendOutcome::Message(text) => {
                        // Durability pivot for the inline path (audit 3-a):
                        // journal the completed output BEFORE returning, so a
                        // crash after this point resumes into the
                        // NOTE_INLINE_COMPLETE check above instead of
                        // re-dialing a fresh SendMessage.
                        ctx.record_note(NOTE_INLINE_COMPLETE, json!(text))
                            .await
                            .map_err(|e| {
                                A2aError::Protocol(
                                    client.agent.clone(),
                                    format!("cannot journal inline completion: {e}"),
                                )
                            })?;
                        tracing::info!(
                            agent = %client.agent_name(),
                            "a2a: inline completion journaled (exactly-once reconnect anchor)"
                        );
                        crash_point("after_a2a_inline_complete", action.action());
                        return Ok(("(inline)".to_string(), "TASK_STATE_COMPLETED".into(), text));
                    }
                    SendOutcome::Task(task) => {
                        // Journal the task id THE MOMENT it exists: this is
                        // the durability pivot for delegation.
                        ctx.record_note(NOTE_TASK_ID, json!(task.id))
                            .await
                            .map_err(|e| {
                                A2aError::Protocol(
                                    client.agent.clone(),
                                    format!("cannot journal task id: {e}"),
                                )
                            })?;
                        tracing::info!(
                            agent = %client.agent_name(),
                            task_id = %task.id,
                            "a2a: remote task created and journaled"
                        );
                        crash_point("after_a2a_create", action.action());
                        if is_terminal(&task.status.state) {
                            task
                        } else {
                            self.poll_to_terminal(client, &task.id).await?
                        }
                    }
                }
            };
            Ok((
                task.id.clone(),
                task.status.state.clone(),
                artifact_text(&task),
            ))
        }
        .await;

        let elapsed_ms = started.elapsed().as_millis() as u64;
        ctx.attribute_agent(client.agent_name(), elapsed_ms);

        // Circuit-breaker bookkeeping (ADR 0023): every dial outcome against
        // this origin updates its consecutive-failure streak, regardless of
        // whether the caller was a fresh proposal, a kernel-initiated retry,
        // or a fallback attempt — the circuit tracks the AGENT, not who
        // asked. `input-required` is a HEALTHY outcome (ADR 0026): the agent
        // responded and is waiting on us — it must NOT count as a failure and
        // trip the circuit.
        let succeeded = matches!(
            &outcome,
            Ok((_, state, _))
                if state == "TASK_STATE_COMPLETED" || state == "TASK_STATE_INPUT_REQUIRED"
        );
        self.record_circuit_outcome(client.allowed_origin(), succeeded);

        match outcome {
            Ok((task_id, state, output)) if state == "TASK_STATE_COMPLETED" => Ok(json!({
                "agent": client.agent_name(),
                "a2a_task_id": task_id,
                "state": state,
                "output": output,
                "remote_ms": elapsed_ms,
            })),
            // Interactive delegation (ADR 0026): the remote needs input. On a
            // FRESH dial this is a durable pause (the engine catches
            // DelegationInputRequired). If we ALREADY sent a continuation this
            // step (the human answered once) and the remote asks AGAIN, that
            // is multi-round — not supported in this slice, so fail cleanly
            // instead of pausing forever.
            Ok((task_id, state, prompt)) if state == "TASK_STATE_INPUT_REQUIRED" => {
                if ctx.note(A2A_CONTINUATION_INPUT_NOTE).is_some() {
                    Err(ExecutionError::Tool(format!(
                        "remote agent '{}' task {task_id} asked for input again after a \
                         continuation — multi-round interactive delegation is not supported \
                         in this slice",
                        client.agent_name()
                    )))
                } else {
                    Err(ExecutionError::DelegationInputRequired {
                        agent: action.action().to_string(),
                        task_id,
                        prompt: if prompt.is_empty() {
                            format!("remote agent '{}' needs more input", client.agent_name())
                        } else {
                            prompt
                        },
                    })
                }
            }
            Ok((task_id, state, _)) => Err(ExecutionError::Tool(format!(
                "remote agent '{}' task {task_id} ended in {state}",
                client.agent_name()
            ))),
            Err(e) => Err(ExecutionError::Tool(e.to_string())),
        }
    }

    fn a2a_origin(&self, action: &str) -> Option<String> {
        self.agents
            .read()
            .unwrap()
            .iter()
            .find(|(name, _)| name == action)
            .map(|(_, client)| client.allowed_origin().to_string())
    }

    fn circuit_state(&self, action: &str) -> CircuitCheck {
        let Some(origin) = self.a2a_origin(action) else {
            return CircuitCheck::Closed;
        };
        let mut circuits = self.circuits.lock().unwrap();
        let Some(state) = circuits.get_mut(&origin) else {
            return CircuitCheck::Closed;
        };
        match state.opened_at {
            None => CircuitCheck::Closed,
            Some(opened) => {
                let elapsed = opened.elapsed();
                if elapsed >= self.circuit_cooldown {
                    // Cooldown elapsed: half-open. Report Closed so the
                    // engine lets exactly the next dial attempt through; a
                    // failure on that attempt re-opens the circuit with a
                    // fresh cooldown window (via `record_circuit_outcome`),
                    // a success closes it (`consecutive_failures` resets to
                    // 0, `opened_at` clears).
                    CircuitCheck::Closed
                } else {
                    CircuitCheck::Open {
                        retry_after: self.circuit_cooldown - elapsed,
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_names_are_sanitized() {
        assert_eq!(action_name("Toy Agent-1"), "delegate_to_toy_agent_1");
    }

    #[test]
    fn origin_pinning_math() {
        assert_eq!(
            origin_of("http://127.0.0.1:7581/.well-known/agent-card.json").unwrap(),
            "http://127.0.0.1:7581"
        );
        assert_ne!(
            origin_of("http://127.0.0.1:7581/x").unwrap(),
            origin_of("http://evil.example/x").unwrap()
        );
    }

    #[test]
    fn terminal_states() {
        assert!(is_terminal("TASK_STATE_COMPLETED"));
        assert!(is_terminal("TASK_STATE_FAILED"));
        assert!(!is_terminal("TASK_STATE_WORKING"));
        assert!(!is_terminal("TASK_STATE_SUBMITTED"));
    }
}
