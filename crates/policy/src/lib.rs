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

use std::sync::Arc;

use kernel_core::{Latitude, PolicyOracle};
use serde::Deserialize;

pub use local::LocalAdapter;
pub use openrouter::{OpenRouterAdapter, DEFAULT_BASE_URL};

pub const LOCAL_DEFAULT_BASE_URL: &str = "http://127.0.0.1:8080";

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
            let api_key =
                std::env::var("OPENROUTER_API_KEY").map_err(|_| BuildError::MissingApiKey)?;
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
        })
        .is_err());
    }
}
