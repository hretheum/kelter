//! Skills management (ADR 0021 addendum): implements `api::SkillsController`
//! over the existing `SkillRegistry` + `RegistrySkillGate`, and persists a
//! runtime policy change the same way `model-config` persists a runtime
//! model change — a small TOML overlay beside the config file, applied at
//! startup, so a UI-chosen policy survives a restart.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use api::{SkillInstallReport, SkillView, SkillsController, SkillsPolicyUpdate, SkillsPolicyView};
use async_trait::async_trait;
use skills::{parse_ask_fallback, parse_model_activation_policy, RegistrySkillGate, SkillRegistry};

use crate::config::SkillsConfig;

const OVERRIDE_FILE: &str = "skills-policy-override.toml";

/// The overlay persisted to disk. Every field optional so a partial update
/// (only `model_activation` sent) does not clobber the other two with a
/// default on the next restart.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct PolicyOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_activation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ask_fallback: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ask_timeout_secs: Option<u64>,
}

fn config_dir(config_path: &str) -> PathBuf {
    Path::new(config_path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn override_path(config_path: &str) -> PathBuf {
    config_dir(config_path).join(OVERRIDE_FILE)
}

/// Apply a persisted policy overlay (if any) over the base `[skills]`
/// config — mirrors `model_config::apply_overlay` exactly. Must be called
/// BEFORE the skills wiring block reads `cfg.skills.*` to build the gate.
pub fn apply_overlay(config_path: &str, skills: &mut SkillsConfig) {
    let Ok(raw) = std::fs::read_to_string(override_path(config_path)) else {
        return;
    };
    let Ok(ov) = toml::from_str::<PolicyOverride>(&raw) else {
        return;
    };
    if let Some(m) = ov.model_activation {
        tracing::info!("config: applying persisted skills-policy overlay (model_activation={m})");
        skills.model_activation = m;
    }
    if let Some(f) = ov.ask_fallback {
        skills.ask_fallback = f;
    }
    if let Some(t) = ov.ask_timeout_secs {
        skills.ask_timeout_secs = t;
    }
}

/// Implements `api::SkillsController` over the registry + the CONCRETE
/// `RegistrySkillGate` handle (not the `Arc<dyn SkillGate>` upcast handed to
/// `Kernel::with_skills` — both point at the same object via `Arc::clone`
/// before the upcast, so a policy change here is visible to the kernel's very
/// next `authorize_activation` call).
pub struct DaemonSkillsController {
    registry: Arc<SkillRegistry>,
    gate: Arc<RegistrySkillGate>,
    dir: PathBuf,
}

impl DaemonSkillsController {
    pub fn new(
        registry: Arc<SkillRegistry>,
        gate: Arc<RegistrySkillGate>,
        config_path: &str,
    ) -> Self {
        Self {
            registry,
            gate,
            dir: config_dir(config_path),
        }
    }

    fn persist(&self, view: &SkillsPolicyView) -> Result<(), String> {
        let ov = PolicyOverride {
            model_activation: Some(view.model_activation.clone()),
            ask_fallback: Some(view.ask_fallback.clone()),
            ask_timeout_secs: Some(view.ask_timeout_secs),
        };
        let raw = toml::to_string_pretty(&ov).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        std::fs::write(self.dir.join(OVERRIDE_FILE), raw).map_err(|e| e.to_string())
    }
}

#[async_trait]
impl SkillsController for DaemonSkillsController {
    async fn list(&self) -> Vec<SkillView> {
        let active = self.registry.active_names();
        self.registry
            .list()
            .into_iter()
            .map(|s| SkillView {
                active: active.contains(&s.name),
                name: s.name,
                description: s.description,
                trust: s.provenance.trust,
                provenance_source_kind: s.provenance.source_kind,
                provenance_source: s.provenance.source,
                has_scripts: s.has_scripts,
                has_extension_manifest: s.has_extension_manifest,
            })
            .collect()
    }

    async fn install_from_url(&self, url: String) -> Result<SkillInstallReport, String> {
        self.registry
            .install_from_url(&url)
            .await
            .map(|r| SkillInstallReport {
                installed: r.installed,
                skipped: r.skipped,
            })
            .map_err(|e| e.to_string())
    }

    async fn remove(&self, name: &str) -> Result<(), String> {
        self.registry.remove(name).map_err(|e| e.to_string())
    }

    async fn activate(&self, name: &str) -> Result<(), String> {
        self.registry.activate(name).map_err(|e| e.to_string())
    }

    async fn deactivate(&self, name: &str) -> Result<(), String> {
        self.registry.deactivate(name).map_err(|e| e.to_string())
    }

    async fn get_policy(&self) -> SkillsPolicyView {
        let cfg = self.gate.get_policy();
        SkillsPolicyView {
            model_activation: cfg.policy.as_str().to_string(),
            ask_fallback: cfg.ask_fallback.as_str().to_string(),
            ask_timeout_secs: cfg.ask_timeout_secs,
        }
    }

    async fn set_policy(&self, update: SkillsPolicyUpdate) -> Result<SkillsPolicyView, String> {
        let policy = parse_model_activation_policy(&update.model_activation);
        let current = self.gate.get_policy();
        let ask_fallback = update
            .ask_fallback
            .as_deref()
            .map(parse_ask_fallback)
            .unwrap_or(current.ask_fallback);
        let ask_timeout_secs = update.ask_timeout_secs.unwrap_or(current.ask_timeout_secs);

        // Apply live first (so it's never lost even if persistence fails),
        // then persist so it survives a restart — mirrors
        // DaemonModelConfig::set()'s swap-then-persist order exactly.
        self.gate.set_policy(policy, ask_fallback, ask_timeout_secs);
        let view = SkillsPolicyView {
            model_activation: policy.as_str().to_string(),
            ask_fallback: ask_fallback.as_str().to_string(),
            ask_timeout_secs,
        };
        self.persist(&view)?;
        tracing::info!(
            "skills: policy updated (model_activation={}, ask_fallback={}, persisted)",
            view.model_activation,
            view.ask_fallback
        );
        Ok(view)
    }
}
