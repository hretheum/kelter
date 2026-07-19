//! Runtime-mutable model configuration (Phase 8.5).
//!
//! The daemon builds the policy oracle once at startup; this makes the model
//! choice changeable at runtime from the UI, without hand-editing
//! `config.toml`:
//!
//! - **swap** — the kernel holds a `policy::SwappableOracle`; a config change
//!   rebuilds the adapter and swaps the inner oracle in place, so new runs use
//!   the new model with no process restart.
//! - **persist** — the change is written to a `model-override.toml` beside the
//!   config file; on startup that overlay wins over the base `[policy]`, so the
//!   choice survives a restart.
//! - **key handling** — the OpenRouter key is read from `OPENROUTER_API_KEY`
//!   or a restricted-perm (0600), gitignored secret file next to the config;
//!   it is never written to `config.toml`, never returned by the API, and
//!   never logged.
//!
//! Note (documented, honest): the *latitude* is derived from the adapter kind
//! at build time and carried on `KernelConfig`, which is fixed for the process.
//! An engine/model/base_url change applies live; a change that also crosses the
//! local↔frontier boundary re-derives latitude only on the next restart (the
//! persisted overlay makes that correct). This is a deliberate small scope.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use api::{
    ModelConfigController, ModelConfigUpdate, ModelConfigView, ModelTestRequest, ModelTestResult,
};
use async_trait::async_trait;
use policy::{PolicyConfig, SwappableOracle};
use tokio::sync::Mutex;

const OVERRIDE_FILE: &str = "model-override.toml";
const SECRET_FILE: &str = "kelter-secret-openrouter.key";

/// The overlay persisted to disk. Only the model choice — never a key.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct ModelOverride {
    policy: OverridePolicy,
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct OverridePolicy {
    engine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
}

/// Path of the persisted model overlay, beside the config file.
pub fn override_path(config_path: &str) -> PathBuf {
    config_dir(config_path).join(OVERRIDE_FILE)
}

fn config_dir(config_path: &str) -> PathBuf {
    Path::new(config_path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn secret_path(dir: &Path) -> PathBuf {
    dir.join(SECRET_FILE)
}

/// Resolve the OpenRouter key: env first, then the restricted-perm secret file.
/// Returns `None` if neither is present. The value is never logged.
pub fn resolve_key(dir: &Path) -> Option<String> {
    if let Ok(k) = std::env::var("OPENROUTER_API_KEY") {
        if !k.is_empty() {
            return Some(k);
        }
    }
    std::fs::read_to_string(secret_path(dir))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Apply a persisted overlay (if any) over the base `[policy]` at startup, so a
/// UI-chosen model survives a restart. Also injects the resolved key.
pub fn apply_overlay(config_path: &str, policy: &mut PolicyConfig) {
    let dir = config_dir(config_path);
    if let Ok(raw) = std::fs::read_to_string(override_path(config_path)) {
        if let Ok(ov) = toml::from_str::<ModelOverride>(&raw) {
            if !ov.policy.engine.is_empty() {
                tracing::info!(
                    "config: applying persisted model overlay ({})",
                    ov.policy.engine
                );
                policy.engine = ov.policy.engine;
                policy.base_url = ov.policy.base_url;
            }
        }
    }
    policy.api_key = resolve_key(&dir);
}

/// Split `"provider:model"` into its parts.
fn split_engine(engine: &str) -> (String, String) {
    match engine.split_once(':') {
        Some((p, m)) => (p.to_string(), m.to_string()),
        None => (engine.to_string(), String::new()),
    }
}

pub struct DaemonModelConfig {
    swappable: Arc<SwappableOracle>,
    dir: PathBuf,
    state: Mutex<PolicyConfig>,
    http: reqwest::Client,
}

impl DaemonModelConfig {
    pub fn new(config_path: &str, swappable: Arc<SwappableOracle>, initial: PolicyConfig) -> Self {
        Self {
            swappable,
            dir: config_dir(config_path),
            state: Mutex::new(initial),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(8))
                .build()
                .expect("client builds"),
        }
    }

    fn view(&self, cfg: &PolicyConfig, key_set: bool) -> ModelConfigView {
        let (provider, model) = split_engine(&cfg.engine);
        ModelConfigView {
            engine: cfg.engine.clone(),
            base_url: cfg.base_url.clone(),
            provider,
            model,
            api_key_set: key_set,
        }
    }

    fn persist_overlay(&self, cfg: &PolicyConfig) -> Result<(), String> {
        let ov = ModelOverride {
            policy: OverridePolicy {
                engine: cfg.engine.clone(),
                base_url: cfg.base_url.clone(),
            },
        };
        let raw = toml::to_string_pretty(&ov).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        std::fs::write(self.dir.join(OVERRIDE_FILE), raw).map_err(|e| e.to_string())
    }
}

#[async_trait]
impl ModelConfigController for DaemonModelConfig {
    async fn get(&self) -> ModelConfigView {
        let cfg = self.state.lock().await;
        self.view(&cfg, resolve_key(&self.dir).is_some())
    }

    async fn set(&self, update: ModelConfigUpdate) -> Result<ModelConfigView, String> {
        // Build the new adapter (resolving the key from env/secret file) —
        // this validates the engine string and the key requirement.
        let mut cfg = PolicyConfig {
            engine: update.engine.trim().to_string(),
            base_url: update.base_url.filter(|u| !u.trim().is_empty()),
            latitude: None,
            api_key: resolve_key(&self.dir),
        };
        let (oracle, _latitude) = policy::build_oracle(&cfg).map_err(|e| e.to_string())?;
        // Apply live, then persist so it survives a restart.
        self.swappable.swap(oracle);
        self.persist_overlay(&cfg)?;
        tracing::info!("config: model swapped to '{}' (persisted)", cfg.engine);
        let key_set = cfg.api_key.is_some();
        cfg.api_key = None; // never keep the key in the exposed state
        *self.state.lock().await = cfg.clone();
        Ok(self.view(&cfg, key_set))
    }

    async fn test(&self, req: ModelTestRequest) -> ModelTestResult {
        let base = req.base_url.trim().trim_end_matches('/');
        // Real per-provider health probe (never a faked "OK").
        let url = match req.provider.as_str() {
            "local" => format!("{base}/health"), // llama.cpp
            "ollama" => {
                // Ollama's OpenAI-compat base ends in /v1; its health path is
                // at the root.
                let root = base.strip_suffix("/v1").unwrap_or(base);
                format!("{root}/api/version")
            }
            "openrouter" => format!("{base}/models"),
            other => {
                return ModelTestResult {
                    ok: false,
                    detail: format!("unknown provider '{other}'"),
                }
            }
        };
        match self.http.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => ModelTestResult {
                ok: true,
                detail: format!("{} {}", resp.status().as_u16(), url),
            },
            Ok(resp) => ModelTestResult {
                ok: false,
                detail: format!("{} from {url}", resp.status().as_u16()),
            },
            Err(e) => ModelTestResult {
                ok: false,
                detail: format!("cannot reach {url}: {e}"),
            },
        }
    }

    async fn set_key(&self, key: String) -> Result<(), String> {
        let key = key.trim();
        if key.is_empty() {
            return Err("empty api key".into());
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let path = secret_path(&self.dir);
        write_secret_0600(&path, key).map_err(|e| e.to_string())?;
        // NEVER log the key value — only that one was stored, and where.
        tracing::info!("config: OpenRouter key stored (0600) at {}", path.display());
        Ok(())
    }
}

/// Write `contents` to `path` with owner-only (0600) permissions on unix.
fn write_secret_0600(path: &Path, contents: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(contents.as_bytes())?;
        // Re-assert perms in case the file pre-existed with looser mode.
        std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}
