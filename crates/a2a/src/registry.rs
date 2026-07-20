//! Runtime agent trust + provenance registry (ADR 0023) — mirrors
//! `skills::SkillRegistry`'s API shape (open/list/set_trust, mtime-cached)
//! at the API level, but as a SINGLE TOML file rather than a directory of
//! per-skill sidecars: an agent has no natural per-agent directory the way
//! a skill's `SKILL.md` does.
//!
//! The default-trust split (ADR 0023, stated here and in the ADR): agents
//! seeded from the daemon's own static config start `trust = "trusted"` —
//! putting an agent in your own config file IS the operator's trust act,
//! exactly parallel to a skill sidecar edit (ADR 0019's framing). Agents
//! registered dynamically over HTTP (`AgentRegistry::register`) always
//! start `trust = "untrusted"` — nothing but an operator or a human's
//! `allow_always` decision can ever change that.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{action_name, A2aClient, A2aError, RemoteAgent};

/// One registered agent's trust + provenance record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentManifest {
    pub name: String,
    pub card_url: String,
    /// From the agent's own Agent Card, captured at registration time.
    /// Empty for a config-seeded agent (seeding is synchronous, at daemon
    /// startup, and deliberately does not add a new network dependency
    /// there — see the ADR 0023 default-trust split note).
    #[serde(default)]
    pub description: String,
    /// Env var NAME to read the bearer token from at dial time — never the
    /// token value itself on disk.
    #[serde(default)]
    pub token_env: Option<String>,
    /// "untrusted" | "trusted" — subsystem-owned, never agent-claimed. An
    /// agent's own Agent Card or response can never assert its own trust.
    pub trust: String,
    /// "config" | "discovery".
    pub source_kind: String,
    pub source: String,
    pub registered_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoredAgents {
    #[serde(default)]
    agents: Vec<AgentManifest>,
}

struct Cache {
    agents: Vec<AgentManifest>,
    stamp: Option<SystemTime>,
}

pub struct AgentRegistry {
    path: PathBuf,
    cache: Mutex<Cache>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl AgentRegistry {
    /// Open (and create) the registry file at `path`, then seed it from the
    /// daemon's static config-declared agents — an IDEMPOTENT merge: an
    /// agent name already present in the file (a previous run's discovery,
    /// or an operator's manual edit) is NEVER overwritten by re-seeding.
    /// Every config-seeded agent starts `trust = "trusted"` (see the module
    /// docs for why).
    pub fn open(path: impl Into<PathBuf>, configured: &[RemoteAgent]) -> Result<Self, A2aError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let reg = Self {
            path,
            cache: Mutex::new(Cache {
                agents: Vec::new(),
                stamp: None,
            }),
        };
        reg.seed_from_config(configured)?;
        Ok(reg)
    }

    fn seed_from_config(&self, configured: &[RemoteAgent]) -> Result<(), A2aError> {
        let mut stored = self.read()?;
        let mut changed = false;
        for agent in configured {
            if !stored.agents.iter().any(|m| m.name == agent.name) {
                stored.agents.push(AgentManifest {
                    name: agent.name.clone(),
                    card_url: agent.card_url.clone(),
                    description: String::new(),
                    token_env: None,
                    trust: "trusted".into(),
                    source_kind: "config".into(),
                    source: agent.card_url.clone(),
                    registered_at: now_secs(),
                });
                changed = true;
            }
        }
        if changed {
            self.write(&stored)?;
        }
        self.touch();
        Ok(())
    }

    fn read(&self) -> Result<StoredAgents, A2aError> {
        match std::fs::read_to_string(&self.path) {
            Ok(raw) => toml::from_str(&raw).map_err(|e| A2aError::Toml(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StoredAgents::default()),
            Err(e) => Err(A2aError::Io(e)),
        }
    }

    fn write(&self, stored: &StoredAgents) -> Result<(), A2aError> {
        let raw = toml::to_string_pretty(stored).map_err(|e| A2aError::Toml(e.to_string()))?;
        std::fs::write(&self.path, raw)?;
        Ok(())
    }

    fn stamp(&self) -> Option<SystemTime> {
        std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok()
    }

    fn with_fresh<R>(&self, f: impl FnOnce(&Vec<AgentManifest>) -> R) -> R {
        let mut cache = self.cache.lock().unwrap();
        let stamp = self.stamp();
        if cache.stamp != stamp || stamp.is_none() {
            cache.agents = self.read().map(|s| s.agents).unwrap_or_default();
            cache.stamp = stamp;
        }
        f(&cache.agents)
    }

    fn touch(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.stamp = None;
        }
    }

    pub fn list(&self) -> Vec<AgentManifest> {
        self.with_fresh(|a| a.clone())
    }

    /// Register a NEW agent, discovered by card URL — dynamic discovery has
    /// no "install a local agent" concept, so there is no operator-typed
    /// name: the agent is registered under whatever name ITS OWN Agent Card
    /// declares. Dials the card URL immediately — via `A2aClient::new` +
    /// [`A2aClient::verify`], the SAME SSRF-pinning origin resolution real
    /// delegation uses — to fail fast on a bad URL, an unreachable host, or
    /// a hostile redirect BEFORE persisting anything: an unreachable or
    /// SSRF-violating card is never recorded as "registered". A card
    /// declaring a name that collides with an already-registered agent
    /// (however it got there) is refused, never silently overwritten.
    /// `initial_trust` is `"untrusted"` for every caller except the
    /// config-seed path above (which never calls this method).
    pub async fn register(
        &self,
        card_url: &str,
        token_env: Option<&str>,
        source_kind: &str,
        initial_trust: &str,
    ) -> Result<AgentManifest, A2aError> {
        let token = token_env.and_then(|var| std::env::var(var).ok());
        let probe = RemoteAgent {
            name: card_url.to_string(),
            card_url: card_url.to_string(),
            token,
        };
        let client = A2aClient::new(&probe)?;
        let (name, description) = client.verify().await?;
        if name.trim().is_empty() {
            return Err(A2aError::Protocol(
                card_url.to_string(),
                "agent card declares no name".into(),
            ));
        }
        if self.list().iter().any(|m| m.name == name) {
            return Err(A2aError::AlreadyRegistered(name));
        }

        let manifest = AgentManifest {
            name,
            card_url: card_url.to_string(),
            description,
            token_env: token_env.map(String::from),
            trust: initial_trust.to_string(),
            source_kind: source_kind.to_string(),
            source: card_url.to_string(),
            registered_at: now_secs(),
        };
        let mut stored = self.read()?;
        stored.agents.push(manifest.clone());
        self.write(&stored)?;
        self.touch();
        Ok(manifest)
    }

    /// Elevate (or otherwise change) an agent's trust label. Mirrors
    /// `SkillRegistry::set_trust` exactly: only an operator action or (via
    /// `DelegationGate::elevate_trust`) a human's bearer-gated `allow_always`
    /// decision reaches this — never model-initiated, never a self-claim
    /// from the agent being trusted.
    pub fn set_trust(&self, name: &str, trust: &str) -> Result<(), A2aError> {
        let mut stored = self.read()?;
        let entry = stored
            .agents
            .iter_mut()
            .find(|m| m.name == name)
            .ok_or_else(|| A2aError::NotFound(name.to_string()))?;
        entry.trust = trust.to_string();
        self.write(&stored)?;
        self.touch();
        Ok(())
    }

    pub fn trust_of(&self, name: &str) -> Option<String> {
        self.with_fresh(|a| a.iter().find(|m| m.name == name).map(|m| m.trust.clone()))
    }

    pub fn remove(&self, name: &str) -> Result<(), A2aError> {
        let mut stored = self.read()?;
        let before = stored.agents.len();
        stored.agents.retain(|m| m.name != name);
        if stored.agents.len() == before {
            return Err(A2aError::NotFound(name.to_string()));
        }
        self.write(&stored)?;
        self.touch();
        Ok(())
    }

    /// The manifest whose `delegate_to_*` action name matches `action` — the
    /// lookup `RegistryDelegationGate` and the API layer use, since
    /// kernel-core only ever sees action names, never agent display names.
    pub fn manifest_for_action(&self, action: &str) -> Option<AgentManifest> {
        self.with_fresh(|a| a.iter().find(|m| action_name(&m.name) == action).cloned())
    }
}
