//! Agent-delegation trust/policy/ask gate (ADR 0023) — mirrors
//! `skills::gate`'s `RegistrySkillGate`/`ModelActivationPolicy` almost
//! exactly at the API level, implementing kernel-core's `DelegationGate` (a
//! DISTINCT trait from `SkillGate` — see kernel-core's module docs for why
//! the two are never conflated).

use std::sync::{Arc, RwLock};

use kernel_core::{DelegationDecisionOutcome, DelegationGate, DelegationRequestMeta};

use crate::registry::AgentRegistry;

/// Which registered agents the model may delegate to (mirrors
/// `skills::ModelActivationPolicy` exactly, applied to agent trust instead
/// of skill trust).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentDelegationPolicy {
    /// Only agents an operator (or a human's `allow_always` decision) has
    /// trusted. Default — see the ADR 0023 default-trust split: a
    /// config-declared agent starts trusted, so this is non-breaking for an
    /// existing static-config deployment; a discovery-registered agent
    /// starts untrusted, so this policy makes IT inert until trusted.
    TrustedOnly,
    /// Any registered agent, regardless of trust label.
    All,
    /// An untrusted agent's delegation attempt is neither auto-allowed nor
    /// auto-denied: the gate returns `AskHuman`, pausing the run — subject
    /// to `ask_fallback` when there is no one to ask (e.g. a headless A2A
    /// worker). An already-trusted agent is still allowed silently.
    Ask,
}

impl AgentDelegationPolicy {
    /// Whether an agent with this trust label may be delegated to SILENTLY
    /// under this policy. `Ask` only silently permits an already-trusted
    /// agent.
    pub fn permits(&self, trust: &str) -> bool {
        match self {
            AgentDelegationPolicy::All => true,
            AgentDelegationPolicy::TrustedOnly | AgentDelegationPolicy::Ask => trust != "untrusted",
        }
    }

    /// `"trusted"`/`"all"`/`"ask"` — the inverse of
    /// [`parse_agent_delegation_policy`].
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentDelegationPolicy::TrustedOnly => "trusted",
            AgentDelegationPolicy::All => "all",
            AgentDelegationPolicy::Ask => "ask",
        }
    }
}

/// Parse `[delegation] agent_policy` (config file) or an API policy update.
/// Unrecognized → `TrustedOnly` with a warning (the safe default).
pub fn parse_agent_delegation_policy(s: &str) -> AgentDelegationPolicy {
    match s {
        "trusted" => AgentDelegationPolicy::TrustedOnly,
        "all" => AgentDelegationPolicy::All,
        "ask" => AgentDelegationPolicy::Ask,
        other => {
            tracing::warn!(
                "a2a: unrecognized agent_policy = '{other}' — falling back to 'trusted' (the \
                 safe default)"
            );
            AgentDelegationPolicy::TrustedOnly
        }
    }
}

/// Only consulted under `AgentDelegationPolicy::Ask`: what happens to an
/// ask-worthy (untrusted) delegation attempt when there is nowhere durable
/// to send the question, or as the deployment's default posture. Mirrors
/// `skills::AskFallback` exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAskFallback {
    /// Deny outright, same as `TrustedOnly` — never pauses.
    Deny,
    /// Actually pause and wait, bounded by a daemon-side timeout sweeper.
    Queue,
}

impl AgentAskFallback {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentAskFallback::Deny => "deny",
            AgentAskFallback::Queue => "queue",
        }
    }
}

/// Unrecognized → `Deny` with a warning (never hangs).
pub fn parse_agent_ask_fallback(s: &str) -> AgentAskFallback {
    match s {
        "deny" => AgentAskFallback::Deny,
        "queue" => AgentAskFallback::Queue,
        other => {
            tracing::warn!(
                "a2a: unrecognized agent ask_fallback = '{other}' — falling back to 'deny' (the \
                 safe default — never hangs)"
            );
            AgentAskFallback::Deny
        }
    }
}

/// The live (runtime-mutable) knobs `RegistryDelegationGate` gates on,
/// bundled so a policy update is one atomic swap.
#[derive(Debug, Clone, Copy)]
pub struct AgentAskPolicyConfig {
    pub policy: AgentDelegationPolicy,
    pub ask_fallback: AgentAskFallback,
    pub ask_timeout_secs: u64,
}

/// The registry-backed `DelegationGate`: gates delegation on the agent's
/// registered trust, the operator's policy, and (under `Ask`) whether to
/// pause for a human or fall back. The policy itself is runtime-mutable via
/// [`Self::set_policy`], mirroring `RegistrySkillGate`.
pub struct RegistryDelegationGate {
    registry: Arc<AgentRegistry>,
    config: RwLock<AgentAskPolicyConfig>,
}

impl RegistryDelegationGate {
    pub fn new(registry: Arc<AgentRegistry>, policy: AgentDelegationPolicy) -> Self {
        Self {
            registry,
            config: RwLock::new(AgentAskPolicyConfig {
                policy,
                ask_fallback: AgentAskFallback::Deny,
                ask_timeout_secs: 300,
            }),
        }
    }

    pub fn with_ask_fallback(self, ask_fallback: AgentAskFallback) -> Self {
        self.config.write().unwrap().ask_fallback = ask_fallback;
        self
    }

    pub fn with_ask_timeout_secs(self, secs: u64) -> Self {
        self.config.write().unwrap().ask_timeout_secs = secs;
        self
    }

    pub fn get_policy(&self) -> AgentAskPolicyConfig {
        *self.config.read().unwrap()
    }

    pub fn set_policy(
        &self,
        policy: AgentDelegationPolicy,
        ask_fallback: AgentAskFallback,
        ask_timeout_secs: u64,
    ) {
        *self.config.write().unwrap() = AgentAskPolicyConfig {
            policy,
            ask_fallback,
            ask_timeout_secs,
        };
    }

    fn policy(&self) -> AgentDelegationPolicy {
        self.config.read().unwrap().policy
    }

    fn ask_fallback(&self) -> AgentAskFallback {
        self.config.read().unwrap().ask_fallback
    }
}

impl DelegationGate for RegistryDelegationGate {
    fn authorize_delegation_to(
        &self,
        action: &str,
        target_origin: &str,
    ) -> DelegationDecisionOutcome {
        let Some(manifest) = self.registry.manifest_for_action(action) else {
            return DelegationDecisionOutcome::Deny(format!(
                "no registered agent maps to action '{action}' (target origin \
                 '{target_origin}') — refusing rather than authorizing an unrecorded delegation"
            ));
        };

        let policy = self.policy();
        match policy {
            AgentDelegationPolicy::All => DelegationDecisionOutcome::Allow(manifest.trust),
            AgentDelegationPolicy::TrustedOnly => {
                if policy.permits(&manifest.trust) {
                    DelegationDecisionOutcome::Allow(manifest.trust)
                } else {
                    DelegationDecisionOutcome::Deny(format!(
                        "agent '{}' has trust '{}', which the delegation policy (TrustedOnly) \
                         does not permit — an operator must trust it first",
                        manifest.name, manifest.trust
                    ))
                }
            }
            AgentDelegationPolicy::Ask => {
                if policy.permits(&manifest.trust) {
                    DelegationDecisionOutcome::Allow(manifest.trust)
                } else {
                    match self.ask_fallback() {
                        AgentAskFallback::Deny => DelegationDecisionOutcome::Deny(format!(
                            "agent '{}' has trust '{}' and the delegation ask_fallback is \
                             \"deny\" — an operator must trust it first (no one is configured \
                             to ask)",
                            manifest.name, manifest.trust
                        )),
                        AgentAskFallback::Queue => {
                            DelegationDecisionOutcome::AskHuman(DelegationRequestMeta {
                                trust: manifest.trust,
                                description: format!(
                                    "Authorize sending work to remote agent '{}' via A2A ({}).",
                                    manifest.name, manifest.card_url
                                ),
                                provenance_source_kind: manifest.source_kind,
                                provenance_source: manifest.source,
                            })
                        }
                    }
                }
            }
        }
    }

    fn elevate_trust(&self, action: &str) -> Result<(), String> {
        let manifest = self
            .registry
            .manifest_for_action(action)
            .ok_or_else(|| format!("no registered agent maps to action '{action}'"))?;
        self.registry
            .set_trust(&manifest.name, "trusted")
            .map_err(|e| e.to_string())
    }
}
