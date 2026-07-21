//! The one stable kernel contract: HTTP for commands, WebSocket for event
//! streaming, bearer-token auth. Every client — web UI, desktop shell,
//! remote CLI — talks to this same seam, on localhost or across a tailnet.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::task::AbortHandle;
use uuid::Uuid;

use kernel_core::Kernel;

// ---- model config control (Phase 8.5) ------------------------------------

/// The current model config, safe to return to a client — **never** the API
/// key (only whether one is set).
#[derive(Debug, Clone, Serialize)]
pub struct ModelConfigView {
    pub engine: String,
    pub base_url: Option<String>,
    pub provider: String,
    pub model: String,
    pub api_key_set: bool,
}

/// A requested model-config change from the UI.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfigUpdate {
    pub engine: String,
    #[serde(default)]
    pub base_url: Option<String>,
}

/// A "Test connection" request: a provider + its base URL to health-check.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelTestRequest {
    pub provider: String,
    pub base_url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelTestResult {
    pub ok: bool,
    pub detail: String,
}

/// Runtime model configuration, implemented by the daemon. The API exposes it
/// behind bearer auth; the daemon owns persistence, the oracle swap, the live
/// health-check, and secure key storage. Absent (`None`) → the config
/// endpoints report "not supported" (e.g. in tests with no controller).
#[async_trait]
pub trait ModelConfigController: Send + Sync {
    async fn get(&self) -> ModelConfigView;
    async fn set(&self, update: ModelConfigUpdate) -> Result<ModelConfigView, String>;
    async fn test(&self, req: ModelTestRequest) -> ModelTestResult;
    /// Store the API key securely (never logged, never in the tree).
    async fn set_key(&self, key: String) -> Result<(), String>;
}

// ---- skills management (ADR 0021 addendum) --------------------------------

/// One installed skill, safe to return to a client. `active` is the
/// OPERATOR-persistent active set (`SkillRegistry::active_names()`) — never
/// a run's own ephemeral `active_skills`, which lives on the `Run` record
/// instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillView {
    pub name: String,
    pub description: String,
    pub trust: String,
    pub provenance_source_kind: String,
    pub provenance_source: String,
    pub active: bool,
    pub has_scripts: bool,
    pub has_extension_manifest: bool,
}

/// URL-install only over HTTP (a deliberate scope line, not an oversight): a
/// browser cannot meaningfully pick a server-side folder path or upload a
/// `.tar.gz` bundle without a new upload mechanism this pass doesn't need.
/// Folder/bundle install stays CLI-only (`kelter skill add <dir>`).
#[derive(Debug, Clone, Deserialize)]
pub struct SkillInstallRequest {
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInstallReport {
    pub installed: Vec<String>,
    pub skipped: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsPolicyView {
    /// "trusted" | "all" | "ask".
    pub model_activation: String,
    /// "deny" | "queue". Only meaningful under "ask".
    pub ask_fallback: String,
    pub ask_timeout_secs: u64,
}

/// A partial update: `model_activation` is required; the other two keep
/// their current value when omitted.
#[derive(Debug, Clone, Deserialize)]
pub struct SkillsPolicyUpdate {
    pub model_activation: String,
    #[serde(default)]
    pub ask_fallback: Option<String>,
    #[serde(default)]
    pub ask_timeout_secs: Option<u64>,
}

/// Skills management, implemented by the daemon (mirrors
/// [`ModelConfigController`]'s shape exactly) — kept decoupled from the
/// `skills` crate's own types so `api` does not need a hard dependency on it.
#[async_trait]
pub trait SkillsController: Send + Sync {
    async fn list(&self) -> Vec<SkillView>;
    async fn install_from_url(&self, url: String) -> Result<SkillInstallReport, String>;
    async fn remove(&self, name: &str) -> Result<(), String>;
    async fn activate(&self, name: &str) -> Result<(), String>;
    async fn deactivate(&self, name: &str) -> Result<(), String>;
    async fn get_policy(&self) -> SkillsPolicyView;
    async fn set_policy(&self, update: SkillsPolicyUpdate) -> Result<SkillsPolicyView, String>;
}

// ---- agent trust/policy management (ADR 0023 Part 3) ---------------------

/// One registered agent, safe to return to a client. Mirrors `SkillView`'s
/// shape/convention exactly. No `card_url` field — it is redundant with
/// `source` for a discovery-registered agent (`source_kind == "discovery"`
/// records the card URL there), and a config-seeded agent's card URL is an
/// operator config detail, not something the API needs to round-trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentView {
    pub name: String,
    pub description: String,
    pub trust: String,
    pub source_kind: String,
    pub source: String,
    pub registered_at: u64,
}

/// Dynamic discovery (ADR 0023 Part 3): register a remote agent by its
/// Agent Card URL alone — mirrors skills' URL-only-install scope decision
/// (no "install a local agent" concept makes sense for a network protocol).
/// There is no `name` field: the agent is registered under whatever name
/// ITS OWN Agent Card declares, never an operator-typed one.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentRegisterRequest {
    pub card_url: String,
    #[serde(default)]
    pub token_env: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPolicyView {
    /// "trusted" | "all" | "ask" — mirrors `SkillsPolicyView::
    /// model_activation`'s wire values.
    pub policy: String,
    /// "deny" | "queue". Only meaningful under "ask".
    pub ask_fallback: String,
    pub ask_timeout_secs: u64,
}

/// A partial update: `policy` is required; the other two keep their current
/// value when omitted.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentPolicyUpdate {
    pub policy: String,
    #[serde(default)]
    pub ask_fallback: Option<String>,
    #[serde(default)]
    pub ask_timeout_secs: Option<u64>,
}

/// Agent trust/policy management, implemented by the daemon (mirrors
/// [`SkillsController`]'s shape exactly) — kept decoupled from the `a2a`
/// crate's own types so `api` does not need a hard dependency on it.
#[async_trait]
pub trait AgentsController: Send + Sync {
    async fn list(&self) -> Vec<AgentView>;
    async fn register(&self, req: AgentRegisterRequest) -> Result<AgentView, String>;
    async fn remove(&self, name: &str) -> Result<(), String>;
    async fn get_policy(&self) -> AgentPolicyView;
    async fn set_policy(&self, update: AgentPolicyUpdate) -> Result<AgentPolicyView, String>;
}

/// The web UI, embedded into the binary so the daemon is a single artifact.
/// This is the **Vite build output** (design/06 §5): build the frontend first
/// (`pnpm --dir apps/web-ui install && pnpm --dir apps/web-ui build`), then
/// `cargo build`. Debug builds read the folder lazily from disk (rust-embed's
/// default), so the dev loop needs no recompile; release builds embed.
#[derive(RustEmbed)]
#[folder = "../../apps/web-ui/dist"]
struct WebUi;

/// The default principal name for the single-token flow — nothing changes for a
/// deployment that never configures named principals.
pub const DEFAULT_PRINCIPAL: &str = "default";

/// Named bearer tokens → principal identities (Phase 11, ADR 0018). A presented
/// token resolves to the principal it belongs to, attributed per run. With one
/// token everything is principal "default"; additive, no behaviour change.
pub struct Principals {
    /// (principal name, token value). Token values come from env/secret files
    /// (never the config tree); this struct only holds them in memory.
    entries: Vec<(String, String)>,
}

impl Principals {
    /// The single-token flow: one token, principal "default".
    pub fn single(token: String) -> Self {
        Self {
            entries: vec![(DEFAULT_PRINCIPAL.to_string(), token)],
        }
    }

    /// The default token plus named principals.
    pub fn with_named(default_token: String, named: Vec<(String, String)>) -> Self {
        let mut entries = vec![(DEFAULT_PRINCIPAL.to_string(), default_token)];
        entries.extend(named);
        Self { entries }
    }

    /// The principal a presented token belongs to, or `None`. Compares against
    /// every token in constant time (no early-out timing leak).
    pub fn resolve(&self, presented: &str) -> Option<&str> {
        let mut matched: Option<&str> = None;
        for (name, token) in &self.entries {
            if constant_time_eq(presented.as_bytes(), token.as_bytes()) {
                matched = Some(name);
            }
        }
        matched
    }
}

#[derive(Clone)]
pub struct ApiState {
    pub kernel: Arc<Kernel>,
    principals: Arc<Principals>,
    /// In-flight runs this process spawned — via A2A `message/send` (ADR 0013)
    /// or `POST /runs` — so a cancel (A2A `tasks/cancel` or the web UI's
    /// `POST /runs/{id}/cancel`, D-2) can stop execution before writing the
    /// durable canceled state. Best-effort: a run resumed after a crash is not
    /// tracked here (a cancel may then only write the terminal state).
    run_handles: Arc<Mutex<HashMap<Uuid, AbortHandle>>>,
    /// Failed-auth rate limiter (Phase 8, item F).
    limiter: Arc<AuthLimiter>,
    /// Runtime model-config controller (Phase 8.5); `None` in bare setups.
    config: Option<Arc<dyn ModelConfigController>>,
    /// Skills management controller (ADR 0021 addendum); `None` when skills
    /// are disabled or the caller has no skills wiring (e.g. bare test
    /// setups) — every `/api/v1/skills*` route then reports 404, which is
    /// also the frontend's existing capability-flag signal (no special-casing
    /// needed on either side).
    skills: Option<Arc<dyn SkillsController>>,
    /// Agent trust/policy management controller (ADR 0023 Part 3); `None`
    /// when absent — every `/api/v1/agents*` route then reports 404, the
    /// SAME capability-flag convention as `skills`.
    agents: Option<Arc<dyn AgentsController>>,
}

pub fn router(kernel: Arc<Kernel>, token: String) -> Router {
    router_with(kernel, token, None)
}

/// Router with an optional runtime model-config controller (Phase 8.5).
pub fn router_with(
    kernel: Arc<Kernel>,
    token: String,
    config: Option<Arc<dyn ModelConfigController>>,
) -> Router {
    router_with_principals(kernel, Principals::single(token), config)
}

/// Router with named principals (Phase 11) + an optional config controller.
/// No skills controller — every `/api/v1/skills*` route reports 404. Use
/// [`router_with_principals_and_skills`] to wire one in.
pub fn router_with_principals(
    kernel: Arc<Kernel>,
    principals: Principals,
    config: Option<Arc<dyn ModelConfigController>>,
) -> Router {
    router_with_principals_and_skills(kernel, principals, config, None)
}

/// Router with named principals + an optional config controller + an
/// optional skills controller (ADR 0021 addendum) — kept as a separate
/// function (rather than widening [`router_with_principals`]'s signature)
/// so the existing, few call sites (tests, the desktop shell) need no
/// changes. No agents controller — every `/api/v1/agents*` route reports
/// 404. Use [`router_with_principals_and_skills_and_agents`] to wire one in.
pub fn router_with_principals_and_skills(
    kernel: Arc<Kernel>,
    principals: Principals,
    config: Option<Arc<dyn ModelConfigController>>,
    skills: Option<Arc<dyn SkillsController>>,
) -> Router {
    router_with_principals_and_skills_and_agents(kernel, principals, config, skills, None)
}

/// [`router_with_principals_and_skills`] plus an optional agents controller
/// (ADR 0023 Part 3) — the daemon's real-wiring path.
pub fn router_with_principals_and_skills_and_agents(
    kernel: Arc<Kernel>,
    principals: Principals,
    config: Option<Arc<dyn ModelConfigController>>,
    skills: Option<Arc<dyn SkillsController>>,
    agents: Option<Arc<dyn AgentsController>>,
) -> Router {
    let state = ApiState {
        kernel,
        principals: Arc::new(principals),
        run_handles: Arc::new(Mutex::new(HashMap::new())),
        limiter: Arc::new(AuthLimiter::default()),
        config,
        skills,
        agents,
    };
    Router::new()
        .route("/healthz", get(health))
        .route("/api/v1/runs", get(list_runs).post(create_run))
        .route("/api/v1/runs/{id}", get(get_run))
        // Web-UI redesign additive endpoints (design/07 D-1 + D-2, bearer):
        // journal replay for the historical execution axis, and cancel.
        .route("/api/v1/runs/{id}/events", get(get_run_events))
        .route("/api/v1/runs/{id}/cancel", post(cancel_run))
        // Interactive skill activation (ADR 0021, design/07 D-9): a human's
        // deny/allow_once/allow_always decision on a paused run's pending
        // `activate_skill` ask-request. Bearer.
        .route(
            "/api/v1/runs/{id}/skill-activation",
            post(decide_skill_activation),
        )
        // Interactive delegation authorization (ADR 0023, mirrors the
        // skill-activation route immediately above field-for-field): a
        // human's deny/allow_once/allow_always decision on a paused run's
        // pending `delegate_to_*` ask-request. Bearer.
        .route("/api/v1/runs/{id}/delegation", post(decide_delegation))
        .route(
            "/api/v1/runs/{id}/delegation-input",
            post(provide_delegation_input),
        )
        .route("/api/v1/tools", get(list_tools))
        .route("/api/v1/events", get(events_ws))
        // Model configuration (Phase 8.5): read/write the model config, test a
        // provider connection, and store the API key — all bearer-gated.
        .route(
            "/api/v1/config/model",
            get(get_model_config).post(set_model_config),
        )
        .route("/api/v1/config/model/test", post(test_model_connection))
        .route("/api/v1/config/model/key", post(set_model_key))
        // Skills management (ADR 0021 addendum, design/07 D-10): list/
        // install-from-url/remove/activate/deactivate + the model-activation
        // policy, all bearer-gated. 404 when no skills controller is wired.
        .route("/api/v1/skills", get(list_skills).post(install_skill))
        .route("/api/v1/skills/{name}", delete(remove_skill))
        .route("/api/v1/skills/{name}/activate", post(activate_skill_route))
        .route(
            "/api/v1/skills/{name}/deactivate",
            post(deactivate_skill_route),
        )
        .route(
            "/api/v1/skills/policy",
            get(get_skills_policy).post(set_skills_policy),
        )
        // Agent trust/policy management (ADR 0023 Part 3, design/07-style
        // addendum): list/register-by-card-url/remove + the delegation
        // policy, all bearer-gated. 404 when no agents controller is wired
        // — mirrors the skills-CRUD 404-when-absent convention exactly.
        .route("/api/v1/agents", get(list_agents).post(register_agent))
        .route("/api/v1/agents/{name}", delete(remove_agent))
        .route(
            "/api/v1/agents/policy",
            get(get_agents_policy).post(set_agents_policy),
        )
        // Governed self-learning (Phase 10, ADR 0016): distil facts from past
        // runs, inspect what was learned, and completely forget it. Bearer.
        .route("/api/v1/learn", get(list_learned))
        .route("/api/v1/learn/distill", post(distill_learned))
        .route("/api/v1/learn/{id}", delete(forget_learned))
        .route(
            "/api/v1/learn/batch/{batch_id}",
            delete(forget_learned_batch),
        )
        // A2A inbound (ADR 0013): Kelter as an A2A server. Card discovery is
        // public; the JSON-RPC task methods require the bearer token.
        .route("/.well-known/agent-card.json", get(a2a_agent_card))
        .route("/a2a/rpc", post(a2a_rpc))
        .fallback(get(static_asset))
        .with_state(state)
}

/// Serve the API + UI on `addr` until the process ends.
pub async fn serve(kernel: Arc<Kernel>, token: String, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_listener(kernel, token, listener).await
}

/// Serve with a runtime model-config controller (Phase 8.5) — the daemon path.
pub async fn serve_with_config(
    kernel: Arc<Kernel>,
    token: String,
    addr: SocketAddr,
    config: Option<Arc<dyn ModelConfigController>>,
) -> std::io::Result<()> {
    serve_with_principals(kernel, Principals::single(token), addr, config).await
}

/// Serve with named principals (Phase 11) + a config controller — the daemon
/// path when `[auth] principals` are configured. Returns the built router so
/// the daemon can also serve it over TLS (ADR 0018) instead. No skills
/// controller — see [`build_app_with_skills`].
pub fn build_app(
    kernel: Arc<Kernel>,
    principals: Principals,
    config: Option<Arc<dyn ModelConfigController>>,
) -> Router {
    router_with_principals(kernel, principals, config)
}

/// [`build_app`] plus an optional skills controller (ADR 0021 addendum) — no
/// agents controller; use [`build_app_with_skills_and_agents`] to wire one in.
pub fn build_app_with_skills(
    kernel: Arc<Kernel>,
    principals: Principals,
    config: Option<Arc<dyn ModelConfigController>>,
    skills: Option<Arc<dyn SkillsController>>,
) -> Router {
    router_with_principals_and_skills(kernel, principals, config, skills)
}

/// [`build_app_with_skills`] plus an optional agents controller (ADR 0023
/// Part 3) — the real daemon-wiring path.
pub fn build_app_with_skills_and_agents(
    kernel: Arc<Kernel>,
    principals: Principals,
    config: Option<Arc<dyn ModelConfigController>>,
    skills: Option<Arc<dyn SkillsController>>,
    agents: Option<Arc<dyn AgentsController>>,
) -> Router {
    router_with_principals_and_skills_and_agents(kernel, principals, config, skills, agents)
}

pub async fn serve_with_principals(
    kernel: Arc<Kernel>,
    principals: Principals,
    addr: SocketAddr,
    config: Option<Arc<dyn ModelConfigController>>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let app = build_app(kernel, principals, config);
    if let Ok(addr) = listener.local_addr() {
        tracing::info!("api listening on http://{addr}");
    }
    axum::serve(listener, app).await
}

/// Serve on an already-bound listener (used by the standalone desktop
/// topology, which binds an ephemeral localhost port first).
pub async fn serve_listener(
    kernel: Arc<Kernel>,
    token: String,
    listener: tokio::net::TcpListener,
) -> std::io::Result<()> {
    let app = router(kernel, token);
    if let Ok(addr) = listener.local_addr() {
        tracing::info!("api listening on http://{addr}");
    }
    axum::serve(listener, app).await
}

async fn health() -> &'static str {
    "ok"
}

// ---- auth ----------------------------------------------------------------

/// Constant-time equality: comparison cost must not depend on how many
/// leading bytes of a guessed token match (timing side channel).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The principal a request's bearer token belongs to, or `None`. Accepts the
/// token from the `Authorization: Bearer` header (REST/A2A) or, when provided,
/// a query token (WebSocket — browsers cannot set headers on WS connects).
fn principal_of(
    state: &ApiState,
    headers: &HeaderMap,
    query_token: Option<&str>,
) -> Option<String> {
    let header_token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let presented = header_token.or(query_token)?;
    state.principals.resolve(presented).map(String::from)
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "invalid or missing bearer token").into_response()
}

/// Failed-auth rate limiter (Phase 8, item F): a sliding window of recent
/// auth failures. Past the threshold the endpoint returns `429` for a cooldown
/// — a basic brute-force slowdown on top of the constant-time token compare.
/// Process-global (the daemon serves a single token); a successful auth clears
/// the window.
const AUTH_MAX_FAILURES: usize = 10;
const AUTH_WINDOW: Duration = Duration::from_secs(30);

#[derive(Default)]
struct AuthLimiter {
    // `parking_lot::Mutex` (audit finding 4-a/#16): a `std::sync::Mutex` here
    // POISONS if a thread panics while holding it — and this lock sits on the
    // request hot path, so one panic-under-lock would turn EVERY subsequent
    // auth check into a panic (`.lock().unwrap()`), a self-DoS far worse than
    // the brute-force slowdown this guards against. parking_lot's Mutex does
    // not poison (and `.lock()` returns the guard directly, no unwrap).
    failures: parking_lot::Mutex<VecDeque<Instant>>,
}

impl AuthLimiter {
    fn prune(dq: &mut VecDeque<Instant>) {
        let now = Instant::now();
        while let Some(&front) = dq.front() {
            if now.duration_since(front) > AUTH_WINDOW {
                dq.pop_front();
            } else {
                break;
            }
        }
    }
    fn is_locked(&self) -> bool {
        let mut dq = self.failures.lock();
        Self::prune(&mut dq);
        dq.len() >= AUTH_MAX_FAILURES
    }
    fn record_failure(&self) {
        let mut dq = self.failures.lock();
        Self::prune(&mut dq);
        dq.push_back(Instant::now());
    }
}

/// Central auth gate (REST + A2A + WS): the bearer check, with a brute-force
/// slowdown on FAILED attempts only. `Ok(principal)` on success (the
/// authenticated identity — "default" for the single-token flow),
/// `Err(response)` on rejection (`401` bad/missing token, or `429` once
/// anonymous failures cross the threshold).
///
/// Re-keyed lockout (audit finding 4-b/#10): the credential is validated
/// FIRST, and a request bearing a VALID token is ALWAYS served — never gated
/// by the failure window. So no volume of failed attempts (an attacker's, or
/// a client's own) can ever lock out a legitimate token-holder: the lockout
/// is keyed on "this request failed auth", not on a global counter shared
/// with valid clients. That is exactly "one client cannot self-DoS others."
/// A success never clears the window either (it prunes by time only), so one
/// client's success cannot reset another's slowdown.
///
/// Per-source-IP bucketing was considered and deliberately NOT added: with
/// the valid-token bypass the window only ever throttles UNAUTHENTICATED
/// requests — all of which are, by definition, would-be attackers with no
/// valid credential — so distinguishing one anonymous source from another has
/// no security value here, while threading `ConnectInfo` through every
/// handler and serve site (including the `axum_server` TLS path) would be
/// disproportionate for the single-token local/tailnet deployment. Throttling
/// anonymous failures collectively is the intended behaviour.
#[allow(clippy::result_large_err)]
fn auth_gate(
    state: &ApiState,
    headers: &HeaderMap,
    query_token: Option<&str>,
) -> Result<String, Response> {
    // A valid credential is served unconditionally — the failure window can
    // never touch it.
    if let Some(principal) = principal_of(state, headers, query_token) {
        return Ok(principal);
    }
    // Unauthenticated: record the failure and, past the threshold, answer 429
    // (brute-force slowdown) instead of 401.
    state.limiter.record_failure();
    if state.limiter.is_locked() {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "30")],
            "too many failed auth attempts; try again shortly",
        )
            .into_response());
    }
    Err(unauthorized())
}

// ---- runs ----------------------------------------------------------------

#[derive(Deserialize)]
struct CreateRun {
    task: String,
}

async fn create_run(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(req): Json<CreateRun>,
) -> Response {
    let principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    if req.task.trim().is_empty() {
        return (StatusCode::UNPROCESSABLE_ENTITY, "task must not be empty").into_response();
    }
    match state.kernel.create_run(req.task).await {
        Ok(run_id) => {
            // Attribute the caller (Phase 11, ADR 0018): which principal
            // created this run — durable in the journal, "default" for the
            // single-token flow.
            let _ = state
                .kernel
                .store()
                .append_journal(
                    run_id,
                    kernel_core::JournalEntry::Note {
                        step: 0,
                        key: "principal".into(),
                        value: serde_json::json!({ "principal": principal, "via": "api/runs" }),
                    },
                )
                .await;
            // Track the in-flight task so `POST /runs/{id}/cancel` (D-2) can
            // abort execution, exactly as A2A-inbound runs are tracked. The
            // entry self-cleans when the run finishes; holding the map lock
            // across spawn+insert makes the task's cleanup strictly ordered
            // after the insert (no leaked entry on a fast run).
            let kernel = state.kernel.clone();
            let handles = state.run_handles.clone();
            let mut map = state.run_handles.lock().await;
            let handle = tokio::spawn(async move {
                if let Err(e) = kernel.execute_run(run_id).await {
                    tracing::error!(%run_id, "run execution error: {e}");
                }
                handles.lock().await.remove(&run_id);
            });
            map.insert(run_id, handle.abort_handle());
            drop(map);
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({ "run_id": run_id })),
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn list_runs(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    match state.kernel.store().list().await {
        Ok(runs) => Json(runs).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// The action space as currently offered to the policy — which tools (and
/// extensions) are live. Useful for demos and for verifying an extension
/// registered without recompiling anything.
async fn list_tools(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    match state.kernel.current_action_schema().await {
        Ok(schema) => {
            let tools: Vec<serde_json::Value> = schema
                .actions
                .iter()
                .map(|a| {
                    serde_json::json!({
                        "name": a.name,
                        "description": a.description,
                    })
                })
                .collect();
            Json(serde_json::json!({ "tools": tools })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn get_run(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    match state.kernel.store().get(id).await {
        Ok(run) => Json(run).into_response(),
        Err(kernel_core::StoreError::NotFound(_)) => {
            (StatusCode::NOT_FOUND, "run not found").into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// GET /api/v1/runs/{id}/events (D-1, design/06 §1a): the run's full ordered
/// journal (ADR 0007), replayed for the historical execution axis. Read-only,
/// bearer-gated, no new storage — this returns exactly what the kernel already
/// journaled. Honest scope note: `rejected` proposals and rationale are never
/// journaled (they are live-WS-only), so replay carries decisions
/// (`proposed`), receipts (`executed`), notes and the terminal record.
async fn get_run_events(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    // 404 for an unknown run (an empty journal for a real run is a valid []).
    let run = match state.kernel.store().get(id).await {
        Ok(run) => run,
        Err(kernel_core::StoreError::NotFound(_)) => {
            return (StatusCode::NOT_FOUND, "run not found").into_response()
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    match state.kernel.store().journal(id).await {
        Ok(journal) => Json(serde_json::json!({
            "run_id": id,
            "task": run.task,
            "policy": run.policy,
            "status": run.status,
            "journal": journal,
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// POST /api/v1/runs/{id}/cancel (D-2, design/06 §1a): stop a live run from
/// the UI. Reuses the A2A cancel path exactly — abort the in-flight task (if
/// this process owns it), then write the durable canceled state via the
/// kernel. A terminal run is returned unchanged. Bearer-gated.
async fn cancel_run(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    // Stop execution first so the cancel write is the single terminal record.
    if let Some(handle) = state.run_handles.lock().await.remove(&id) {
        handle.abort();
    }
    match state
        .kernel
        .cancel_run_with_reason(id, "canceled from the UI")
        .await
    {
        Ok(run) => Json(run).into_response(),
        Err(kernel_core::KernelError::Store(kernel_core::StoreError::NotFound(_))) => {
            (StatusCode::NOT_FOUND, "run not found").into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// POST /api/v1/runs/{id}/skill-activation (D-9, ADR 0021): a human's
/// deny/allow_once/allow_always decision on a paused run's pending
/// `activate_skill` ask-request. Mirrors `create_run`'s spawn+track pattern:
/// a successful decision re-spawns `execute_run` so the run actually
/// resumes (the kernel method itself only journals + writes state). Bearer.
#[derive(Deserialize)]
struct SkillActivationDecisionRequest {
    skill: String,
    decision: String,
}

async fn decide_skill_activation(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(req): Json<SkillActivationDecisionRequest>,
) -> Response {
    let principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    let decision = match req.decision.as_str() {
        "deny" => kernel_core::SkillActivationDecision::Deny,
        "allow_once" => kernel_core::SkillActivationDecision::AllowOnce,
        "allow_always" => kernel_core::SkillActivationDecision::AllowAlways,
        other => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("unknown decision '{other}'"),
            )
                .into_response()
        }
    };
    match state
        .kernel
        .decide_skill_activation(id, &req.skill, decision, &principal)
        .await
    {
        Ok(run) => {
            // Re-spawn execution exactly like create_run: track the handle
            // under lock held across spawn+insert, self-cleaning on
            // completion, so `POST /runs/{id}/cancel` also works on a
            // just-resumed run.
            let kernel = state.kernel.clone();
            let handles = state.run_handles.clone();
            let mut map = state.run_handles.lock().await;
            let handle = tokio::spawn(async move {
                if let Err(e) = kernel.execute_run(id).await {
                    tracing::error!(%id, "run execution error: {e}");
                }
                handles.lock().await.remove(&id);
            });
            map.insert(id, handle.abort_handle());
            drop(map);
            (StatusCode::ACCEPTED, Json(run)).into_response()
        }
        Err(kernel_core::KernelError::InvalidState(msg)) => {
            (StatusCode::CONFLICT, msg).into_response()
        }
        Err(kernel_core::KernelError::Store(kernel_core::StoreError::NotFound(_))) => {
            (StatusCode::NOT_FOUND, "run not found").into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// POST /api/v1/runs/{id}/delegation (ADR 0023): a human's deny/allow_once/
/// allow_always decision on a paused run's pending `delegate_to_*`
/// ask-request. Mirrors `decide_skill_activation` immediately above
/// field-for-field, including the re-spawn+track pattern (the kernel method
/// itself only journals + writes state). Bearer.
#[derive(Deserialize)]
struct DelegationDecisionRequest {
    agent: String,
    decision: String,
}

async fn decide_delegation(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(req): Json<DelegationDecisionRequest>,
) -> Response {
    let principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    let decision = match req.decision.as_str() {
        "deny" => kernel_core::AgentDelegationDecision::Deny,
        "allow_once" => kernel_core::AgentDelegationDecision::AllowOnce,
        "allow_always" => kernel_core::AgentDelegationDecision::AllowAlways,
        other => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("unknown decision '{other}'"),
            )
                .into_response()
        }
    };
    match state
        .kernel
        .decide_delegation(id, &req.agent, decision, &principal)
        .await
    {
        Ok(run) => {
            let kernel = state.kernel.clone();
            let handles = state.run_handles.clone();
            let mut map = state.run_handles.lock().await;
            let handle = tokio::spawn(async move {
                if let Err(e) = kernel.execute_run(id).await {
                    tracing::error!(%id, "run execution error: {e}");
                }
                handles.lock().await.remove(&id);
            });
            map.insert(id, handle.abort_handle());
            drop(map);
            (StatusCode::ACCEPTED, Json(run)).into_response()
        }
        Err(kernel_core::KernelError::InvalidState(msg)) => {
            (StatusCode::CONFLICT, msg).into_response()
        }
        Err(kernel_core::KernelError::Store(kernel_core::StoreError::NotFound(_))) => {
            (StatusCode::NOT_FOUND, "run not found").into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// POST /api/v1/runs/{id}/delegation-input (ADR 0026): a human's continuation
/// text for a run paused on an interactive delegation
/// (`AwaitingDelegationInput`). Journals the input and re-spawns the run —
/// the delegation reconnects to the remote task and sends the continuation.
/// Bearer.
#[derive(Deserialize)]
struct DelegationInputRequest {
    input: String,
}

async fn provide_delegation_input(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(req): Json<DelegationInputRequest>,
) -> Response {
    let principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    match state
        .kernel
        .provide_delegation_input(id, &req.input, &principal)
        .await
    {
        Ok(run) => {
            let kernel = state.kernel.clone();
            let handles = state.run_handles.clone();
            let mut map = state.run_handles.lock().await;
            let handle = tokio::spawn(async move {
                if let Err(e) = kernel.execute_run(id).await {
                    tracing::error!(%id, "run execution error: {e}");
                }
                handles.lock().await.remove(&id);
            });
            map.insert(id, handle.abort_handle());
            drop(map);
            (StatusCode::ACCEPTED, Json(run)).into_response()
        }
        Err(kernel_core::KernelError::InvalidState(msg)) => {
            (StatusCode::CONFLICT, msg).into_response()
        }
        Err(kernel_core::KernelError::Store(kernel_core::StoreError::NotFound(_))) => {
            (StatusCode::NOT_FOUND, "run not found").into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ---- skills management (ADR 0021 addendum) --------------------------------

// The Err branch is an axum `Response` (intentionally large) returned
// straight to the client — mirrors `config_controller`'s pattern exactly.
#[allow(clippy::result_large_err)]
fn skills_controller(state: &ApiState) -> Result<&Arc<dyn SkillsController>, Response> {
    state.skills.as_ref().ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            "skills management is not wired in this deployment",
        )
            .into_response()
    })
}

async fn list_skills(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    match skills_controller(&state) {
        Ok(c) => Json(c.list().await).into_response(),
        Err(resp) => resp,
    }
}

async fn install_skill(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(req): Json<SkillInstallRequest>,
) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    let c = match skills_controller(&state) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    match c.install_from_url(req.url).await {
        Ok(report) => Json(report).into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, e).into_response(),
    }
}

async fn remove_skill(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    let c = match skills_controller(&state) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    match c.remove(&name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, e).into_response(),
    }
}

async fn activate_skill_route(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    let c = match skills_controller(&state) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    match c.activate(&name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, e).into_response(),
    }
}

async fn deactivate_skill_route(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    let c = match skills_controller(&state) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    match c.deactivate(&name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, e).into_response(),
    }
}

async fn get_skills_policy(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    match skills_controller(&state) {
        Ok(c) => Json(c.get_policy().await).into_response(),
        Err(resp) => resp,
    }
}

async fn set_skills_policy(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(update): Json<SkillsPolicyUpdate>,
) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    let c = match skills_controller(&state) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    match c.set_policy(update).await {
        Ok(view) => Json(view).into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, e).into_response(),
    }
}

// ---- agent trust/policy management (ADR 0023 Part 3) ----------------------

#[allow(clippy::result_large_err)]
fn agents_controller(state: &ApiState) -> Result<&Arc<dyn AgentsController>, Response> {
    state.agents.as_ref().ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            "agent management is not wired in this deployment",
        )
            .into_response()
    })
}

async fn list_agents(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    match agents_controller(&state) {
        Ok(c) => Json(c.list().await).into_response(),
        Err(resp) => resp,
    }
}

async fn register_agent(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(req): Json<AgentRegisterRequest>,
) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    let c = match agents_controller(&state) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    match c.register(req).await {
        Ok(view) => Json(view).into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, e).into_response(),
    }
}

async fn remove_agent(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    let c = match agents_controller(&state) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    match c.remove(&name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, e).into_response(),
    }
}

async fn get_agents_policy(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    match agents_controller(&state) {
        Ok(c) => Json(c.get_policy().await).into_response(),
        Err(resp) => resp,
    }
}

async fn set_agents_policy(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(update): Json<AgentPolicyUpdate>,
) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    let c = match agents_controller(&state) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    match c.set_policy(update).await {
        Ok(view) => Json(view).into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, e).into_response(),
    }
}

// ---- model config (Phase 8.5) --------------------------------------------

// The Err branch is an axum `Response` (intentionally large) returned straight
// to the client — the idiomatic early-return pattern, not a hot path.
#[allow(clippy::result_large_err)]
fn config_controller(state: &ApiState) -> Result<&Arc<dyn ModelConfigController>, Response> {
    state.config.as_ref().ok_or_else(|| {
        (
            StatusCode::NOT_IMPLEMENTED,
            "model config is not runtime-mutable in this deployment",
        )
            .into_response()
    })
}

async fn get_model_config(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    match config_controller(&state) {
        Ok(cfg) => Json(cfg.get().await).into_response(),
        Err(resp) => resp,
    }
}

async fn set_model_config(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(update): Json<ModelConfigUpdate>,
) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    let cfg = match config_controller(&state) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    match cfg.set(update).await {
        Ok(view) => Json(view).into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, e).into_response(),
    }
}

async fn test_model_connection(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(req): Json<ModelTestRequest>,
) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    match config_controller(&state) {
        Ok(cfg) => Json(cfg.test(req).await).into_response(),
        Err(resp) => resp,
    }
}

#[derive(Deserialize)]
struct KeyBody {
    api_key: String,
}

async fn set_model_key(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(body): Json<KeyBody>,
) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    let cfg = match config_controller(&state) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    // The key value is never logged here or below — only success/failure.
    match cfg.set_key(body.api_key).await {
        Ok(()) => (StatusCode::NO_CONTENT, "").into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, e).into_response(),
    }
}

// ---- governed self-learning (Phase 10, ADR 0016) -------------------------

#[derive(Deserialize)]
struct DistillBody {
    #[serde(default = "default_distill_limit")]
    limit: usize,
}

fn default_distill_limit() -> usize {
    50
}

async fn distill_learned(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: Option<Json<DistillBody>>,
) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    let limit = body.map(|b| b.limit).unwrap_or_else(default_distill_limit);
    match state.kernel.distill(limit).await {
        Ok(report) => Json(report).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn list_learned(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    match state.kernel.list_learned().await {
        Ok(facts) => Json(facts).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn forget_learned(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    match state.kernel.forget_learned(id).await {
        Ok(removed) => Json(serde_json::json!({ "removed": removed })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn forget_learned_batch(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(batch_id): Path<Uuid>,
) -> Response {
    let _principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    match state.kernel.forget_learned_batch(batch_id).await {
        Ok(n) => Json(serde_json::json!({ "removed": n })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ---- A2A inbound: Kelter as an A2A server (ADR 0013) ---------------------

/// The externally reachable origin, derived from the request `Host` header
/// (localhost demo → always http). Used to advertise the RPC interface url in
/// the Agent Card so a client dials back the same origin it discovered.
fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1");
    format!("http://{host}")
}

/// GET /.well-known/agent-card.json — public discovery. Declares the bearer
/// security scheme so a client learns auth is required before calling a task
/// method.
async fn a2a_agent_card(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    let card = a2a::server::agent_card(&base_url(&headers), &state.kernel.policy_description());
    Json(card).into_response()
}

/// A JSON-RPC error response (`{jsonrpc, id, error:{code,message}}`).
fn rpc_error(id: serde_json::Value, code: i64, message: &str) -> Response {
    Json(serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    }))
    .into_response()
}

fn rpc_ok(id: serde_json::Value, result: serde_json::Value) -> Response {
    Json(serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
}

/// POST /a2a/rpc — the A2A JSON-RPC endpoint. **Auth required** (ADR 0013): an
/// anonymous call never reaches a task method. Dispatches `message/send`
/// (create a durable run), `tasks/get` (read the run store), and
/// `tasks/cancel`.
async fn a2a_rpc(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(req): Json<serde_json::Value>,
) -> Response {
    // Auth is the security surface: reject anonymous task calls loudly. Card
    // discovery is the only unauthenticated A2A route.
    let principal = match auth_gate(&state, &headers, None) {
        Ok(p) => p,
        Err(rejection) => return rejection,
    };
    let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(serde_json::json!({}));

    match a2a::server::normalize_method(method) {
        Some(a2a::server::Method::Send) => {
            a2a_send(&state, &headers, &principal, id, &params).await
        }
        Some(a2a::server::Method::Get) => a2a_get(&state, id, &params).await,
        Some(a2a::server::Method::Cancel) => a2a_cancel(&state, id, &params).await,
        None => rpc_error(id, -32601, &format!("method not found: {method}")),
    }
}

/// `message/send`: an inbound task is a **durable Kelter run**. The A2A task
/// id is the run id, so it is journal-backed and crash-durable for free.
async fn a2a_send(
    state: &ApiState,
    headers: &HeaderMap,
    principal: &str,
    id: serde_json::Value,
    params: &serde_json::Value,
) -> Response {
    let text = a2a::server::message_text(params);
    if text.is_empty() {
        return rpc_error(id, -32602, "invalid params: message has no text");
    }
    // A2A delegation lineage (ADR 0022): an upstream Kelter hop may carry
    // its chain depth + ancestry in the message metadata; absent for a
    // non-Kelter caller or the start of a fresh chain — both mean depth 0,
    // empty ancestry, which `message_lineage` already defaults to.
    let (a2a_depth, a2a_ancestry) = a2a::server::message_lineage(params);
    let run_id = match state
        .kernel
        .create_run_with_lineage(text, a2a_depth, a2a_ancestry)
        .await
    {
        Ok(r) => r,
        Err(e) => return rpc_error(id, -32000, &format!("cannot create run: {e}")),
    };

    // Attribute the caller in the run's journal (ADR 0013 + 0018): the
    // authenticated PRINCIPAL, plus the optional A2A-Caller peer label.
    let caller = headers
        .get("a2a-caller")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("anonymous-authenticated");
    let _ = state
        .kernel
        .store()
        .append_journal(
            run_id,
            kernel_core::JournalEntry::Note {
                step: 0,
                key: "a2a_inbound".into(),
                value: serde_json::json!({
                    "principal": principal,
                    "caller": caller,
                    "auth": "bearer",
                    "via": "a2a/message-send",
                }),
            },
        )
        .await;
    tracing::info!(%run_id, %principal, %caller, "a2a inbound: task accepted as a durable run");

    // Execute durably in the background; track the handle so tasks/cancel can
    // stop it. Same spawn+insert-under-lock + self-cleanup pattern as
    // `create_run` (no leaked entry on a fast run).
    let kernel = state.kernel.clone();
    let handles = state.run_handles.clone();
    let mut map = state.run_handles.lock().await;
    let handle = tokio::spawn(async move {
        if let Err(e) = kernel.execute_run(run_id).await {
            tracing::error!(%run_id, "a2a inbound run error: {e}");
        }
        handles.lock().await.remove(&run_id);
    });
    map.insert(run_id, handle.abort_handle());
    drop(map);

    match state.kernel.store().get(run_id).await {
        Ok(run) => rpc_ok(
            id,
            serde_json::json!({ "task": a2a::server::task_json(&run) }),
        ),
        Err(e) => rpc_error(id, -32000, &format!("run vanished after creation: {e}")),
    }
}

/// Parse the `id` param shared by `tasks/get` and `tasks/cancel`.
fn task_id_param(params: &serde_json::Value) -> Option<Uuid> {
    params
        .get("id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
}

/// `tasks/get`: read the durable run store by id — survives a server crash,
/// since the run auto-resumes on restart and this reads its persisted state.
async fn a2a_get(state: &ApiState, id: serde_json::Value, params: &serde_json::Value) -> Response {
    let Some(task_id) = task_id_param(params) else {
        return rpc_error(id, -32602, "invalid params: missing or malformed task id");
    };
    match state.kernel.store().get(task_id).await {
        Ok(run) => rpc_ok(id, a2a::server::task_json(&run)),
        Err(kernel_core::StoreError::NotFound(_)) => rpc_error(id, -32001, "TaskNotFoundError"),
        Err(e) => rpc_error(id, -32000, &e.to_string()),
    }
}

/// `tasks/cancel`: stop the in-flight execution (if this process owns it) and
/// write the durable canceled state. A terminal run is returned unchanged.
async fn a2a_cancel(
    state: &ApiState,
    id: serde_json::Value,
    params: &serde_json::Value,
) -> Response {
    let Some(task_id) = task_id_param(params) else {
        return rpc_error(id, -32602, "invalid params: missing or malformed task id");
    };
    // Stop execution first so the cancel write is the single terminal record.
    if let Some(handle) = state.run_handles.lock().await.remove(&task_id) {
        handle.abort();
    }
    match state.kernel.cancel_run(task_id).await {
        Ok(run) => rpc_ok(id, a2a::server::task_json(&run)),
        Err(kernel_core::KernelError::Store(kernel_core::StoreError::NotFound(_))) => {
            rpc_error(id, -32001, "TaskNotFoundError")
        }
        Err(e) => rpc_error(id, -32000, &e.to_string()),
    }
}

// ---- event stream --------------------------------------------------------

#[derive(Deserialize)]
struct WsAuth {
    /// Browsers cannot set headers on WebSocket connects, so the token may
    /// come via query parameter as well.
    token: Option<String>,
}

async fn events_ws(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(auth): Query<WsAuth>,
    ws: WebSocketUpgrade,
) -> Response {
    // WS auth (Phase 11): same gate as REST — rate limiter + a valid token via
    // header OR the query param (browsers can't set WS headers). No token → the
    // connection is refused before upgrade.
    if let Err(rejection) = auth_gate(&state, &headers, auth.token.as_deref()) {
        return rejection;
    }
    let kernel = state.kernel.clone();
    ws.on_upgrade(move |socket| stream_events(socket, kernel))
}

async fn stream_events(mut socket: WebSocket, kernel: Arc<Kernel>) {
    let mut rx = kernel.subscribe();
    loop {
        tokio::select! {
            event = rx.recv() => match event {
                Ok(envelope) => {
                    let Ok(text) = serde_json::to_string(&envelope) else { continue };
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                // Slow consumer: skip missed events, keep streaming.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            msg = socket.recv() => match msg {
                Some(Ok(_)) => continue, // pings are answered by axum itself
                _ => break,
            },
        }
    }
}

// ---- static UI -----------------------------------------------------------

async fn static_asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match WebUi::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            ([(header::CONTENT_TYPE, mime.as_ref())], file.data).into_response()
        }
        // Stale-dist honesty (design/07 D-7): a binary built without the
        // frontend must say so, not serve a silent 404 shell.
        None if path == "index.html" => (
            StatusCode::NOT_FOUND,
            "web-ui not built into this binary — run `pnpm --dir apps/web-ui install && pnpm --dir apps/web-ui build`, then rebuild",
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

#[cfg(test)]
mod auth_limiter_tests {
    use super::*;
    use std::sync::Arc;

    /// Regression test for audit finding 4-a/#16: the auth limiter's lock must
    /// NOT poison on a panic-under-lock. A `std::sync::Mutex` here would leave
    /// the lock poisoned after any panic while held, so the next
    /// `.lock().unwrap()` on the request hot path would itself panic — a
    /// self-DoS. `parking_lot::Mutex` never poisons: after the panicking
    /// thread unwinds (dropping the guard), the limiter is immediately usable
    /// again.
    #[test]
    fn auth_limiter_survives_a_panic_while_holding_the_lock() {
        let limiter = Arc::new(AuthLimiter::default());
        let l2 = limiter.clone();
        let joined = std::thread::spawn(move || {
            let _held = l2.failures.lock();
            panic!("boom while holding the auth-limiter lock");
        })
        .join();
        assert!(
            joined.is_err(),
            "the thread must have panicked under the lock"
        );

        // With a poisoning std Mutex this next call would panic; parking_lot
        // makes it just work.
        limiter.record_failure();
        assert!(
            !limiter.is_locked(),
            "one failure is below the lock threshold"
        );
    }
}
