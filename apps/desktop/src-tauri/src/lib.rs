//! Desktop shell: the same kernel, two topologies behind a first-run toggle.
//!
//! - **standalone**: links `kernel-core` (and friends) into this process,
//!   starts the daemon in-process on an ephemeral localhost port, and points
//!   the webview at it. Tools run over in-process MCP; the local model
//!   sidecar (llama.cpp) is expected on its usual port.
//! - **remote**: no kernel in this process at all — the webview points at a
//!   remote daemon URL (e.g. over a tailnet).
//!
//! Either way the UI is the same web app the daemon serves (ADR 0004).

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::Mutex;

use kernel_core::{Kernel, KernelConfig, RunStore};
use storage::{Db, SqliteMemoryProvider, SqliteRunStore};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Settings {
    /// "standalone" | "remote" | unset (first run).
    pub mode: Option<String>,
    /// Remote daemon URL (remote mode).
    pub url: Option<String>,
    /// Remote bearer token (remote mode).
    pub token: Option<String>,
    /// Policy engine for standalone mode; same one-line format as the
    /// daemon config.
    pub engine: Option<String>,
}

#[derive(Default)]
struct Standalone {
    /// url + token of the in-process daemon, once started.
    running: Mutex<Option<(String, String)>>,
}

fn settings_path(app: &AppHandle) -> anyhow::Result<PathBuf> {
    let dir = app.path().app_config_dir()?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("settings.json"))
}

fn load_settings(app: &AppHandle) -> Settings {
    settings_path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn store_settings(app: &AppHandle, settings: &Settings) -> Result<(), String> {
    let path = settings_path(app).map_err(|e| e.to_string())?;
    std::fs::write(path, serde_json::to_vec_pretty(settings).unwrap())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_settings(app: AppHandle) -> Settings {
    load_settings(&app)
}

#[tauri::command]
fn save_settings(app: AppHandle, settings: Settings) -> Result<(), String> {
    store_settings(&app, &settings)
}

fn random_token() -> String {
    use rand::Rng;
    let bytes: [u8; 24] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Start the in-process daemon (idempotent per app run: safe to call again —
/// it returns the already-running instance instead of starting a second one).
#[tauri::command]
async fn start_standalone(
    app: AppHandle,
    state: State<'_, Standalone>,
) -> Result<(String, String), String> {
    let mut running = state.running.lock().await;
    if let Some(pair) = running.as_ref() {
        return Ok(pair.clone());
    }

    let settings = load_settings(&app);
    let engine = settings
        .engine
        .unwrap_or_else(|| "local:qwen2.5-0.5b-instruct".to_string());

    let (oracle, latitude) = policy::build_oracle(&policy::PolicyConfig {
        engine,
        base_url: None,
        latitude: None,
        api_key: None,
    })
    .map_err(|e| e.to_string())?;

    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&data_dir).map_err(|e| e.to_string())?;
    let workspace = data_dir.join("work");
    let tools = mcp::McpToolSource::connect_in_process(&workspace.to_string_lossy())
        .await
        .map_err(|e| e.to_string())?;

    // Durable store: the same SQLite backend the daemon uses (ADR 0007), so
    // runs + journal + episodes survive a desktop restart. Two handles to one
    // WAL database file — runs/journal and memory share it, as in the daemon.
    let db_path = data_dir.join("kelter.db");
    let store = Arc::new(SqliteRunStore::new(
        Db::open(&db_path).map_err(|e| e.to_string())?,
    ));
    let memory = Arc::new(SqliteMemoryProvider::new(
        Db::open(&db_path).map_err(|e| e.to_string())?,
    ));

    let kernel = Arc::new(Kernel::new(
        oracle,
        Arc::new(tools),
        memory,
        store.clone(),
        KernelConfig {
            latitude,
            ..KernelConfig::default()
        },
    ));

    // Resume any run interrupted by a previous desktop session before serving.
    if let Ok(ids) = store.unfinished_runs().await {
        for run_id in ids {
            let kernel = kernel.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = kernel.execute_run(run_id).await {
                    tracing::error!(%run_id, "standalone resume failed: {e}");
                }
            });
        }
    }

    let token = random_token();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let url = format!("http://{addr}");

    let serve_token = token.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = api::serve_listener(kernel, serve_token, listener).await {
            tracing::error!("standalone api server stopped: {e}");
        }
    });

    *running = Some((url.clone(), token.clone()));
    Ok((url, token))
}

/// Open the main UI window pointed at a daemon (embedded or remote) and
/// close the launcher.
#[tauri::command]
async fn open_ui(app: AppHandle, url: String, token: String) -> Result<(), String> {
    let target = format!("{}/#token={}", url.trim_end_matches('/'), token);
    let external = target
        .parse()
        .map_err(|e| format!("invalid url '{target}': {e}"))?;

    WebviewWindowBuilder::new(&app, "main", WebviewUrl::External(external))
        .title("Kelter")
        .inner_size(1100.0, 800.0)
        .build()
        .map_err(|e| e.to_string())?;

    if let Some(launcher) = app.get_webview_window("launcher") {
        let _ = launcher.close();
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(Standalone::default())
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            start_standalone,
            open_ui
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
