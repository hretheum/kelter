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

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use kernel_core::{
    crash_point, ActionSchema, ActionSource, ActionSpec, ExecutionContext, ExecutionError,
    Latitude, ValidatedAction, A2A_ANCESTRY_ARG, A2A_DEPTH_ARG, IDEMPOTENCY_KEY_ARG,
};

pub const A2A_VERSION: &str = "1.0";
pub const WELL_KNOWN_CARD: &str = "/.well-known/agent-card.json";
const NOTE_TASK_ID: &str = "a2a_task_id";

pub mod server;

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
}

// ---- wire types (A2A v1.0.0, JSON-RPC binding) ---------------------------

#[derive(Debug, Deserialize)]
struct AgentCard {
    name: String,
    #[serde(rename = "supportedInterfaces")]
    supported_interfaces: Vec<AgentInterface>,
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

    /// Resolve the Agent Card and pick the JSONRPC interface — refusing any
    /// endpoint outside the declared origin.
    async fn endpoint(&self) -> Result<&str, A2aError> {
        self.endpoint
            .get_or_try_init(|| async {
                let card: AgentCard = self
                    .with_headers(self.http.get(&self.card_url))
                    .send()
                    .await
                    .and_then(|r| r.error_for_status())
                    .map_err(|e| A2aError::Http {
                        agent: self.agent.clone(),
                        source: e,
                    })?
                    .json()
                    .await
                    .map_err(|e| A2aError::Http {
                        agent: self.agent.clone(),
                        source: e,
                    })?;

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
                tracing::info!(
                    agent = %self.agent,
                    card_name = %card.name,
                    endpoint = %interface.url,
                    "a2a: card resolved (origin-pinned)"
                );
                Ok(interface.url.clone())
            })
            .await
            .map(String::as_str)
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
        let response: serde_json::Value = self
            .with_headers(self.http.post(&endpoint))
            .json(&body)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| A2aError::Http {
                agent: self.agent.clone(),
                source: e,
            })?
            .json()
            .await
            .map_err(|e| A2aError::Http {
                agent: self.agent.clone(),
                source: e,
            })?;

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
}

pub enum SendOutcome {
    Task(Task),
    Message(String),
}

// ---- the action source ---------------------------------------------------

fn action_name(agent: &str) -> String {
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

/// Remote agents as kernel actions: `delegate_to_<name>(task)`.
pub struct A2aActionSource {
    agents: Vec<(String, A2aClient)>, // (action name, client)
    poll_interval: Duration,
    poll_timeout: Duration,
}

impl A2aActionSource {
    pub fn new(agents: Vec<RemoteAgent>) -> Result<Self, A2aError> {
        let mut out = Vec::new();
        for agent in &agents {
            out.push((action_name(&agent.name), A2aClient::new(agent)?));
        }
        Ok(Self {
            agents: out,
            poll_interval: Duration::from_secs(1),
            poll_timeout: Duration::from_secs(120),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    async fn poll_to_terminal(&self, client: &A2aClient, task_id: &str) -> Result<Task, A2aError> {
        let deadline = tokio::time::Instant::now() + self.poll_timeout;
        loop {
            let task = client.get_task(task_id).await?;
            if is_terminal(&task.status.state) {
                return Ok(task);
            }
            if matches!(
                task.status.state.as_str(),
                "TASK_STATE_INPUT_REQUIRED" | "TASK_STATE_AUTH_REQUIRED"
            ) {
                return Err(A2aError::Protocol(
                    client.agent.clone(),
                    format!(
                        "remote task entered '{}' — interactive continuation is not supported \
                         in outbound delegation v1",
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
}

#[async_trait]
impl ActionSource for A2aActionSource {
    async fn action_schema(&self, _latitude: Latitude) -> Result<ActionSchema, ExecutionError> {
        Ok(ActionSchema {
            actions: self
                .agents
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
        let (_, client) = self
            .agents
            .iter()
            .find(|(name, _)| name == action.action())
            .ok_or_else(|| {
                ExecutionError::Tool(format!("unknown delegation action '{}'", action.action()))
            })?;
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
            // Durable reconnect: if this step already created a remote task
            // before a crash, reattach to it — never delegate twice.
            let task = if let Some(existing) = ctx.note(NOTE_TASK_ID).and_then(|v| v.as_str()) {
                tracing::info!(
                    agent = %client.agent_name(),
                    task_id = %existing,
                    "a2a: reconnecting to existing remote task from the journal — NOT re-delegating"
                );
                self.poll_to_terminal(client, existing).await?
            } else {
                match client.send_message(task_text, lineage).await? {
                    SendOutcome::Message(text) => {
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

        match outcome {
            Ok((task_id, state, output)) if state == "TASK_STATE_COMPLETED" => Ok(json!({
                "agent": client.agent_name(),
                "a2a_task_id": task_id,
                "state": state,
                "output": output,
                "remote_ms": elapsed_ms,
            })),
            Ok((task_id, state, _)) => Err(ExecutionError::Tool(format!(
                "remote agent '{}' task {task_id} ended in {state}",
                client.agent_name()
            ))),
            Err(e) => Err(ExecutionError::Tool(e.to_string())),
        }
    }

    fn a2a_origin(&self, action: &str) -> Option<String> {
        self.agents
            .iter()
            .find(|(name, _)| name == action)
            .map(|(_, client)| client.allowed_origin().to_string())
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
