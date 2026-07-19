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

/// Fixed preset origins the health-check may probe (Phase 11, ADR 0018). The
/// current configured `base_url` origin and operator-configured extras are
/// added on top; anything else is refused — an authenticated caller must not be
/// able to turn "Test connection" into an SSRF probe of arbitrary hosts (e.g.
/// a cloud metadata IP).
const PRESET_ORIGINS: &[&str] = &[
    "http://127.0.0.1:8080",  // llama.cpp
    "http://127.0.0.1:11434", // ollama
    "https://openrouter.ai",  // OpenRouter
];

/// scheme://host[:port] of a URL, or `None` if unparseable.
pub fn origin_of(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let scheme = parsed.scheme();
    Some(match parsed.port() {
        Some(p) => format!("{scheme}://{host}:{p}"),
        None => format!("{scheme}://{host}"),
    })
}

pub struct DaemonModelConfig {
    swappable: Arc<SwappableOracle>,
    dir: PathBuf,
    state: Mutex<PolicyConfig>,
    http: reqwest::Client,
    /// Extra origins the operator explicitly trusts for the health-check
    /// (e.g. a custom local model server on a non-preset port).
    test_allowlist: Vec<String>,
}

impl DaemonModelConfig {
    pub fn new(
        config_path: &str,
        swappable: Arc<SwappableOracle>,
        initial: PolicyConfig,
        test_allowlist: Vec<String>,
    ) -> Self {
        Self {
            swappable,
            dir: config_dir(config_path),
            state: Mutex::new(initial),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(8))
                // A redirect could bounce an allowlisted origin to an
                // off-allowlist host — disable it (same posture as ADR 0009).
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client builds"),
            test_allowlist,
        }
    }

    /// Is `url`'s origin on the health-check allowlist? (presets + the current
    /// configured base_url + operator extras).
    async fn origin_allowed(&self, url: &str) -> bool {
        let Some(origin) = origin_of(url) else {
            return false;
        };
        if PRESET_ORIGINS.contains(&origin.as_str())
            || self.test_allowlist.iter().any(|o| o == &origin)
        {
            return true;
        }
        // The currently configured model's own base_url is always allowed.
        if let Some(base) = self.state.lock().await.base_url.as_deref() {
            if origin_of(base).is_some_and(|o| o == origin) {
                return true;
            }
        }
        false
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
        // SSRF guard (ADR 0018): the probe target must be on the allowlist —
        // a preset, the configured base_url, or an operator extra. An
        // authenticated caller cannot make the daemon fetch an arbitrary host.
        if !self.origin_allowed(base).await {
            return ModelTestResult {
                ok: false,
                detail: "refused: target origin is not on the health-check allowlist \
                         (presets + configured base_url + [auth] test_allowlist)"
                    .to_string(),
            };
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use api::ModelConfigController;

    fn controller(allowlist: Vec<String>) -> DaemonModelConfig {
        let (oracle, _) = policy::build_oracle(&PolicyConfig {
            engine: "local:test".into(),
            base_url: Some("http://127.0.0.1:8080".into()),
            latitude: None,
            api_key: None,
        })
        .unwrap();
        DaemonModelConfig::new(
            "/tmp/kelter-test/config.toml",
            Arc::new(SwappableOracle::new(oracle)),
            PolicyConfig {
                engine: "local:test".into(),
                base_url: Some("http://127.0.0.1:8080".into()),
                latitude: None,
                api_key: None,
            },
            allowlist,
        )
    }

    #[tokio::test]
    async fn ssrf_off_allowlist_target_is_refused() {
        let c = controller(vec![]);
        // A cloud-metadata IP is the classic SSRF target — must be refused,
        // never fetched.
        let r = c
            .test(api::ModelTestRequest {
                provider: "local".into(),
                base_url: "http://169.254.169.254".into(),
            })
            .await;
        assert!(!r.ok);
        assert!(r.detail.contains("allowlist"), "{}", r.detail);
    }

    #[tokio::test]
    async fn ssrf_preset_and_configured_origins_pass_the_allowlist() {
        let c = controller(vec![]);
        // A preset origin passes the allowlist (the fetch then fails because no
        // server is up — but it is NOT an allowlist refusal).
        let r = c
            .test(api::ModelTestRequest {
                provider: "local".into(),
                base_url: "http://127.0.0.1:8080".into(),
            })
            .await;
        assert!(
            !r.detail.contains("allowlist"),
            "preset refused: {}",
            r.detail
        );
    }

    #[tokio::test]
    async fn ssrf_operator_extra_origin_is_allowed() {
        let c = controller(vec!["http://127.0.0.1:9999".into()]);
        let r = c
            .test(api::ModelTestRequest {
                provider: "local".into(),
                base_url: "http://127.0.0.1:9999".into(),
            })
            .await;
        assert!(
            !r.detail.contains("allowlist"),
            "extra refused: {}",
            r.detail
        );
    }
}
