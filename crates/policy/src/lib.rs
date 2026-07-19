//! Policy plane: model adapters behind the kernel's `PolicyOracle` trait.
//!
//! Two working adapters, swappable with **one config line**
//! (`[policy] engine = "<adapter>:<model>"`):
//!
//! - `local:<model>` — llama.cpp server + GBNF constrained decoding
//!   (the "weak but reliable" path).
//! - `openrouter:<provider/model>` — OpenRouter unified API, native
//!   tool-calling, one adapter → many frontier models.

pub mod gbnf;
mod local;
mod openrouter;
pub mod prompt;

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use kernel_core::{ActionSchema, Latitude, PolicyContext, PolicyError, PolicyOracle, Proposal};
use serde::Deserialize;

pub use local::LocalAdapter;
pub use openrouter::{OpenRouterAdapter, DEFAULT_BASE_URL};

pub const LOCAL_DEFAULT_BASE_URL: &str = "http://127.0.0.1:8080";

/// A `PolicyOracle` whose inner adapter can be **swapped at runtime**
/// (Phase 8.5). The kernel holds this as an ordinary `Arc<dyn PolicyOracle>`
/// and is never rebuilt; the daemon keeps a handle and calls `swap` when the
/// model config changes via the API. Each call reads the current adapter under
/// a short read-lock and releases it before the (slow) network call, so a swap
/// never blocks in-flight proposals and vice-versa.
pub struct SwappableOracle {
    inner: RwLock<Arc<dyn PolicyOracle>>,
}

impl SwappableOracle {
    pub fn new(initial: Arc<dyn PolicyOracle>) -> Self {
        Self {
            inner: RwLock::new(initial),
        }
    }

    pub fn swap(&self, next: Arc<dyn PolicyOracle>) {
        *self.inner.write().expect("oracle lock not poisoned") = next;
    }

    fn current(&self) -> Arc<dyn PolicyOracle> {
        self.inner.read().expect("oracle lock not poisoned").clone()
    }
}

#[async_trait]
impl PolicyOracle for SwappableOracle {
    async fn propose(
        &self,
        context: &PolicyContext,
        schema: &ActionSchema,
    ) -> Result<Proposal, PolicyError> {
        // Clone the Arc out and drop the guard before awaiting the network.
        let oracle = self.current();
        oracle.propose(context, schema).await
    }

    fn describe(&self) -> String {
        self.current().describe()
    }
}

/// The `[policy]` config section. `engine` is the one-line swap:
///
/// ```toml
/// [policy]
/// engine = "local:qwen2.5-0.5b-instruct"
/// # engine = "openrouter:anthropic/claude-sonnet-5"
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct PolicyConfig {
    /// `"<adapter>:<model>"` — adapter is `local` or `openrouter`.
    pub engine: String,
    /// Override the backend URL (llama.cpp server / OpenRouter base).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Override the latitude derived from the adapter kind.
    #[serde(default)]
    pub latitude: Option<Latitude>,
    /// The OpenRouter API key, injected by the daemon from the environment or a
    /// restricted-perm secret file (Phase 8.5). **Never** deserialized from the
    /// config file (`serde(skip)`), so a key can never live in `config.toml`.
    #[serde(skip)]
    pub api_key: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("invalid engine '{0}': expected \"local:<model>\" or \"openrouter:<provider/model>\"")]
    InvalidEngine(String),
    #[error("OPENROUTER_API_KEY is not set — the openrouter adapter reads the key only from the environment")]
    MissingApiKey,
}

/// Build the configured adapter. Capability → latitude: a local small model
/// defaults to `constrained`, a frontier model to `wide`; the config can
/// override either way.
pub fn build_oracle(cfg: &PolicyConfig) -> Result<(Arc<dyn PolicyOracle>, Latitude), BuildError> {
    let (kind, model) = cfg
        .engine
        .split_once(':')
        .ok_or_else(|| BuildError::InvalidEngine(cfg.engine.clone()))?;
    match kind {
        "local" => {
            let base = cfg
                .base_url
                .clone()
                .unwrap_or_else(|| LOCAL_DEFAULT_BASE_URL.to_string());
            let latitude = cfg.latitude.unwrap_or(Latitude::Constrained);
            Ok((Arc::new(LocalAdapter::new(base, model)), latitude))
        }
        "openrouter" => {
            // Key precedence: the daemon-injected value (from env OR a
            // restricted-perm secret file) first, then the env directly. Never
            // from the config file.
            let api_key = cfg
                .api_key
                .clone()
                .filter(|k| !k.is_empty())
                .or_else(|| {
                    std::env::var("OPENROUTER_API_KEY")
                        .ok()
                        .filter(|k| !k.is_empty())
                })
                .ok_or(BuildError::MissingApiKey)?;
            let base = cfg
                .base_url
                .clone()
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
            let latitude = cfg.latitude.unwrap_or(Latitude::Wide);
            Ok((
                Arc::new(OpenRouterAdapter::new(base, model, api_key)),
                latitude,
            ))
        }
        _ => Err(BuildError::InvalidEngine(cfg.engine.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_string_parses_local() {
        let (oracle, latitude) = build_oracle(&PolicyConfig {
            engine: "local:qwen2.5-0.5b-instruct".into(),
            base_url: None,
            latitude: None,
            api_key: None,
        })
        .unwrap();
        assert_eq!(oracle.describe(), "local:qwen2.5-0.5b-instruct");
        assert_eq!(latitude, Latitude::Constrained);
    }

    #[test]
    fn engine_string_rejects_garbage() {
        assert!(build_oracle(&PolicyConfig {
            engine: "banana".into(),
            base_url: None,
            latitude: None,
            api_key: None,
        })
        .is_err());
    }
}
