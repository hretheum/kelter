//! The one stable kernel contract: HTTP for commands, WebSocket for event
//! streaming, bearer-token auth. Every client — web UI, desktop shell,
//! remote CLI — talks to this same seam, on localhost or across a tailnet.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rust_embed::RustEmbed;
use serde::Deserialize;
use uuid::Uuid;

use kernel_core::Kernel;

/// The web UI, embedded into the binary so the daemon is a single artifact.
#[derive(RustEmbed)]
#[folder = "../../apps/web-ui"]
struct WebUi;

#[derive(Clone)]
pub struct ApiState {
    pub kernel: Arc<Kernel>,
    pub token: Arc<String>,
}

pub fn router(kernel: Arc<Kernel>, token: String) -> Router {
    let state = ApiState {
        kernel,
        token: Arc::new(token),
    };
    Router::new()
        .route("/healthz", get(health))
        .route("/api/v1/runs", get(list_runs).post(create_run))
        .route("/api/v1/runs/{id}", get(get_run))
        .route("/api/v1/events", get(events_ws))
        .fallback(get(static_asset))
        .with_state(state)
}

/// Serve the API + UI on `addr` until the process ends.
pub async fn serve(kernel: Arc<Kernel>, token: String, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_listener(kernel, token, listener).await
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

fn authorized(state: &ApiState, headers: &HeaderMap) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| t == state.token.as_str())
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "invalid or missing bearer token").into_response()
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
    if !authorized(&state, &headers) {
        return unauthorized();
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
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    match state.kernel.store().list().await {
        Ok(runs) => Json(runs).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn get_run(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    match state.kernel.store().get(id).await {
        Ok(run) => Json(run).into_response(),
        Err(kernel_core::StoreError::NotFound(_)) => {
            (StatusCode::NOT_FOUND, "run not found").into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
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
    let ok = authorized(&state, &headers) || auth.token.as_deref() == Some(state.token.as_str());
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
