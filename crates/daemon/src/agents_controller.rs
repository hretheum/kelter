//! Agent trust/policy management (ADR 0023 Part 3): implements
//! `api::AgentsController` over `a2a::AgentRegistry` +
//! `a2a::RegistryDelegationGate`, and persists a runtime policy change the
//! same way `skills_controller` does — a small TOML overlay beside the
//! config file, applied at startup, so a UI-chosen policy survives a
//! restart. Mirrors `skills_controller.rs`'s shape almost exactly.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use a2a::{
    parse_agent_ask_fallback, parse_agent_delegation_policy, A2aActionSource, AgentRegistry,
    RegistryDelegationGate, RemoteAgent,
};
use api::{AgentPolicyUpdate, AgentPolicyView, AgentRegisterRequest, AgentView, AgentsController};
use async_trait::async_trait;

const OVERRIDE_FILE: &str = "agents-policy-override.toml";

/// The overlay persisted to disk. Every field optional so a partial update
/// (only `policy` sent) does not clobber the other two with a default on
/// the next restart.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct PolicyOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    policy: Option<String>,
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

/// Apply a persisted policy overlay (if any) over the base `[delegation]`
/// agent-policy fields — mirrors `skills_controller::apply_overlay` exactly.
/// Must be called BEFORE the agents wiring block reads `cfg.delegation.
/// agent_*` to build the gate.
pub fn apply_overlay(config_path: &str, delegation: &mut crate::config::DelegationConfig) {
    let Ok(raw) = std::fs::read_to_string(override_path(config_path)) else {
        return;
    };
    let Ok(ov) = toml::from_str::<PolicyOverride>(&raw) else {
        return;
    };
    if let Some(p) = ov.policy {
        tracing::info!("config: applying persisted agents-policy overlay (policy={p})");
        delegation.agent_policy = p;
    }
    if let Some(f) = ov.ask_fallback {
        delegation.agent_ask_fallback = f;
    }
    if let Some(t) = ov.ask_timeout_secs {
        delegation.agent_ask_timeout_secs = t;
    }
}

/// Implements `api::AgentsController` over the registry + the CONCRETE
/// `RegistryDelegationGate` handle (not the `Arc<dyn DelegationGate>`
/// upcast handed to `Kernel::with_delegation_gate` — both point at the same
/// object via `Arc::clone` before the upcast, so a policy change here is
/// visible to the kernel's very next `authorize_delegation_to` call) + the
/// concrete `A2aActionSource` handle, so a newly-registered agent's
/// `delegate_to_X` action appears in the live action space immediately
/// (ADR 0023 Part 3), no daemon restart needed.
pub struct DaemonAgentsController {
    registry: Arc<AgentRegistry>,
    gate: Arc<RegistryDelegationGate>,
    action_source: Arc<A2aActionSource>,
    dir: PathBuf,
}

impl DaemonAgentsController {
    pub fn new(
        registry: Arc<AgentRegistry>,
        gate: Arc<RegistryDelegationGate>,
        action_source: Arc<A2aActionSource>,
        config_path: &str,
    ) -> Self {
        Self {
            registry,
            gate,
            action_source,
            dir: config_dir(config_path),
        }
    }

    fn persist(&self, view: &AgentPolicyView) -> Result<(), String> {
        let ov = PolicyOverride {
            policy: Some(view.policy.clone()),
            ask_fallback: Some(view.ask_fallback.clone()),
            ask_timeout_secs: Some(view.ask_timeout_secs),
        };
        let raw = toml::to_string_pretty(&ov).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        std::fs::write(self.dir.join(OVERRIDE_FILE), raw).map_err(|e| e.to_string())
    }
}

fn view_of(m: a2a::AgentManifest) -> AgentView {
    AgentView {
        name: m.name,
        description: m.description,
        trust: m.trust,
        source_kind: m.source_kind,
        source: m.source,
        registered_at: m.registered_at,
    }
}

#[async_trait]
impl AgentsController for DaemonAgentsController {
    async fn list(&self) -> Vec<AgentView> {
        self.registry.list().into_iter().map(view_of).collect()
    }

    async fn register(&self, req: AgentRegisterRequest) -> Result<AgentView, String> {
        let manifest = self
            .registry
            .register(
                &req.card_url,
                req.token_env.as_deref(),
                "discovery",
                "untrusted",
            )
            .await
            .map_err(|e| e.to_string())?;

        // Wire the new agent into the LIVE action space immediately (ADR
        // 0023 Part 3) — best-effort: a failure here is logged, not fatal,
        // since the agent is already durably registered and will be picked
        // up on the next daemon restart regardless.
        let token = manifest
            .token_env
            .as_ref()
            .and_then(|var| std::env::var(var).ok());
        let remote = RemoteAgent {
            name: manifest.name.clone(),
            card_url: manifest.card_url.clone(),
            token,
        };
        if let Err(e) = self.action_source.register_runtime(&remote) {
            tracing::warn!(
                "agents: '{}' registered but could not be wired into the live action space \
                 (will still appear after a restart): {e}",
                manifest.name
            );
        }
        Ok(view_of(manifest))
    }

    async fn remove(&self, name: &str) -> Result<(), String> {
        self.registry.remove(name).map_err(|e| e.to_string())?;
        self.action_source.deregister_runtime(name);
        Ok(())
    }

    async fn get_policy(&self) -> AgentPolicyView {
        let cfg = self.gate.get_policy();
        AgentPolicyView {
            policy: cfg.policy.as_str().to_string(),
            ask_fallback: cfg.ask_fallback.as_str().to_string(),
            ask_timeout_secs: cfg.ask_timeout_secs,
        }
    }

    async fn set_policy(&self, update: AgentPolicyUpdate) -> Result<AgentPolicyView, String> {
        let policy = parse_agent_delegation_policy(&update.policy);
        let current = self.gate.get_policy();
        let ask_fallback = update
            .ask_fallback
            .as_deref()
            .map(parse_agent_ask_fallback)
            .unwrap_or(current.ask_fallback);
        let ask_timeout_secs = update.ask_timeout_secs.unwrap_or(current.ask_timeout_secs);

        // Apply live first (never lost even if persistence fails), then
        // persist so it survives a restart — mirrors
        // DaemonSkillsController::set_policy's order exactly.
        self.gate.set_policy(policy, ask_fallback, ask_timeout_secs);
        let view = AgentPolicyView {
            policy: policy.as_str().to_string(),
            ask_fallback: ask_fallback.as_str().to_string(),
            ask_timeout_secs,
        };
        self.persist(&view)?;
        tracing::info!(
            "agents: policy updated (policy={}, ask_fallback={}, persisted)",
            view.policy,
            view.ask_fallback
        );
        Ok(view)
    }
}
