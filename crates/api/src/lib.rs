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

/// The web UI, embedded into the binary so the daemon is a single artifact.
#[derive(RustEmbed)]
#[folder = "../../apps/web-ui"]
struct WebUi;

#[derive(Clone)]
pub struct ApiState {
    pub kernel: Arc<Kernel>,
    pub token: Arc<String>,
    /// In-flight A2A-inbound runs this process spawned, so `tasks/cancel` can
    /// stop execution before writing the durable canceled state (ADR 0013).
    /// Best-effort: a run resumed after a crash is not tracked here (A2A
    /// permits a cancel to be refused).
    a2a_handles: Arc<Mutex<HashMap<Uuid, AbortHandle>>>,
    /// Failed-auth rate limiter (Phase 8, item F).
    limiter: Arc<AuthLimiter>,
    /// Runtime model-config controller (Phase 8.5); `None` in bare setups.
    config: Option<Arc<dyn ModelConfigController>>,
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
    let state = ApiState {
        kernel,
        token: Arc::new(token),
        a2a_handles: Arc::new(Mutex::new(HashMap::new())),
        limiter: Arc::new(AuthLimiter::default()),
        config,
    };
    Router::new()
        .route("/healthz", get(health))
        .route("/api/v1/runs", get(list_runs).post(create_run))
        .route("/api/v1/runs/{id}", get(get_run))
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
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let app = router_with(kernel, token, config);
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

fn authorized(state: &ApiState, headers: &HeaderMap) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| constant_time_eq(t.as_bytes(), state.token.as_bytes()))
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
    failures: std::sync::Mutex<VecDeque<Instant>>,
}

impl AuthLimiter {
    fn prune(&self, dq: &mut VecDeque<Instant>) {
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
        let mut dq = self.failures.lock().unwrap();
        self.prune(&mut dq);
        dq.len() >= AUTH_MAX_FAILURES
    }
    fn record_failure(&self) {
        let mut dq = self.failures.lock().unwrap();
        self.prune(&mut dq);
        dq.push_back(Instant::now());
    }
    fn record_success(&self) {
        self.failures.lock().unwrap().clear();
    }
}

/// Central auth gate for the REST + A2A endpoints: enforces the rate limiter,
/// then the bearer check. Returns `Some(response)` when the request must be
/// rejected (`429` locked out, or `401` bad/missing token), `None` when it may
/// proceed.
fn auth_gate(state: &ApiState, headers: &HeaderMap) -> Option<Response> {
    if state.limiter.is_locked() {
        return Some(
            (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, "30")],
                "too many failed auth attempts; try again shortly",
            )
                .into_response(),
        );
    }
    if authorized(state, headers) {
        state.limiter.record_success();
        None
    } else {
        state.limiter.record_failure();
        Some(unauthorized())
    }
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
    if let Some(rejection) = auth_gate(&state, &headers) {
        return rejection;
    }
    if req.task.trim().is_empty() {
        return (StatusCode::UNPROCESSABLE_ENTITY, "task must not be empty").into_response();
    }
    match state.kernel.create_run(req.task).await {
        Ok(run_id) => {
            let kernel = state.kernel.clone();
            tokio::spawn(async move {
                if let Err(e) = kernel.execute_run(run_id).await {
                    tracing::error!(%run_id, "run execution error: {e}");
                }
            });
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
    if let Some(rejection) = auth_gate(&state, &headers) {
        return rejection;
    }
    match state.kernel.store().list().await {
        Ok(runs) => Json(runs).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// The action space as currently offered to the policy — which tools (and
/// extensions) are live. Useful for demos and for verifying an extension
/// registered without recompiling anything.
async fn list_tools(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    if let Some(rejection) = auth_gate(&state, &headers) {
        return rejection;
    }
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
    if let Some(rejection) = auth_gate(&state, &headers) {
        return rejection;
    }
    match state.kernel.store().get(id).await {
        Ok(run) => Json(run).into_response(),
        Err(kernel_core::StoreError::NotFound(_)) => {
            (StatusCode::NOT_FOUND, "run not found").into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
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
    if let Some(rejection) = auth_gate(&state, &headers) {
        return rejection;
    }
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
    if let Some(rejection) = auth_gate(&state, &headers) {
        return rejection;
    }
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
    if let Some(rejection) = auth_gate(&state, &headers) {
        return rejection;
    }
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
    if let Some(rejection) = auth_gate(&state, &headers) {
        return rejection;
    }
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
    if let Some(rejection) = auth_gate(&state, &headers) {
        return rejection;
    }
    let limit = body.map(|b| b.limit).unwrap_or_else(default_distill_limit);
    match state.kernel.distill(limit).await {
        Ok(report) => Json(report).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn list_learned(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    if let Some(rejection) = auth_gate(&state, &headers) {
        return rejection;
    }
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
    if let Some(rejection) = auth_gate(&state, &headers) {
        return rejection;
    }
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
    if let Some(rejection) = auth_gate(&state, &headers) {
        return rejection;
    }
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
    if let Some(rejection) = auth_gate(&state, &headers) {
        return rejection;
    }
    let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(serde_json::json!({}));

    match a2a::server::normalize_method(method) {
        Some(a2a::server::Method::Send) => a2a_send(&state, &headers, id, &params).await,
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
    id: serde_json::Value,
    params: &serde_json::Value,
) -> Response {
    let text = a2a::server::message_text(params);
    if text.is_empty() {
        return rpc_error(id, -32602, "invalid params: message has no text");
    }
    let run_id = match state.kernel.create_run(text).await {
        Ok(r) => r,
        Err(e) => return rpc_error(id, -32000, &format!("cannot create run: {e}")),
    };

    // Attribute the caller in the run's journal (ADR 0013): who delegated,
    // over which authenticated surface. The bearer token authorised the call;
    // the optional A2A-Caller header names the peer.
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
                    "caller": caller,
                    "auth": "bearer",
                    "via": "a2a/message-send",
                }),
            },
        )
        .await;
    tracing::info!(%run_id, %caller, "a2a inbound: task accepted as a durable run");

    // Execute durably in the background; track the handle so tasks/cancel can
    // stop it.
    let kernel = state.kernel.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) = kernel.execute_run(run_id).await {
            tracing::error!(%run_id, "a2a inbound run error: {e}");
        }
    });
    state
        .a2a_handles
        .lock()
        .await
        .insert(run_id, handle.abort_handle());

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
    if let Some(handle) = state.a2a_handles.lock().await.remove(&task_id) {
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
    let ok = authorized(&state, &headers)
        || auth
            .token
            .as_deref()
            .is_some_and(|t| constant_time_eq(t.as_bytes(), state.token.as_bytes()));
    if !ok {
        return unauthorized();
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
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
