//! Model self-activation of advisory skills (ADR 0020), extended with an
//! interactive `Ask` policy (ADR 0021).
//!
//! Implements `kernel_core::SkillGate` — the seam kernel-core owns so the
//! engine loop can treat `activate_skill`/`deactivate_skill` as kernel-owned
//! actions (exactly like `finish`) without kernel-core knowing anything
//! skills-specific. Authorization here decides WHICH skills the model may
//! switch on: existence, the operator's trust policy, and a shared advisory
//! context budget. It never grants capability — the kernel still re-validates
//! every proposal against the same `ActionSchema` regardless of which skills
//! are active (ADR 0019's invariant, unchanged).

use std::sync::{Arc, RwLock};

use kernel_core::{ActivationAuthorization, ActivationRequestMeta, SkillGate};

use crate::{SkillRegistry, MAX_TOTAL_ACTIVE_BYTES};

/// Which installed skills the model is permitted to self-activate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelActivationPolicy {
    /// Only skills an operator has explicitly trusted (sidecar `trust` !=
    /// `"untrusted"`). Since every skill is `untrusted` at install, this
    /// policy makes self-activation INERT until an operator trusts a skill —
    /// the safe opt-in posture (default).
    TrustedOnly,
    /// Any installed skill, regardless of trust label. Capability-wise this
    /// is still safe (activation is knowledge only, never capability); the
    /// difference from `TrustedOnly` is *steering* risk — an untrusted
    /// SKILL.md nudging the model toward an allowed-but-unwanted action.
    All,
    /// An untrusted skill is neither auto-allowed nor auto-denied: the gate
    /// returns `AskHuman` (ADR 0021), pausing the run for a human decision —
    /// subject to `ask_fallback` when there is no one to ask (e.g. a
    /// headless A2A worker). An already-trusted skill is still allowed
    /// silently (no point asking about something an operator already
    /// trusts).
    Ask,
}

impl ModelActivationPolicy {
    /// Whether a skill with this trust label may be self-activated by the
    /// model SILENTLY under this policy (i.e. without asking). `Ask` only
    /// silently permits an already-trusted skill.
    pub fn permits(&self, trust: &str) -> bool {
        match self {
            ModelActivationPolicy::All => true,
            ModelActivationPolicy::TrustedOnly | ModelActivationPolicy::Ask => trust != "untrusted",
        }
    }

    /// `"trusted"`/`"all"`/`"ask"` — the inverse of [`parse_model_activation_policy`].
    pub fn as_str(&self) -> &'static str {
        match self {
            ModelActivationPolicy::TrustedOnly => "trusted",
            ModelActivationPolicy::All => "all",
            ModelActivationPolicy::Ask => "ask",
        }
    }
}

/// Parse `[skills] model_activation` (config file) or a
/// `SkillsPolicyUpdate.model_activation` (API). Unrecognized → `TrustedOnly`
/// with a warning (the safe default), never a hard failure — shared by both
/// the daemon's startup config parsing and the runtime policy-update
/// endpoint so the two never drift apart.
pub fn parse_model_activation_policy(s: &str) -> ModelActivationPolicy {
    match s {
        "trusted" => ModelActivationPolicy::TrustedOnly,
        "all" => ModelActivationPolicy::All,
        "ask" => ModelActivationPolicy::Ask,
        other => {
            tracing::warn!(
                "skills: unrecognized model_activation = '{other}' — falling back to 'trusted' \
                 (the safe default)"
            );
            ModelActivationPolicy::TrustedOnly
        }
    }
}

/// Only meaningful under `ModelActivationPolicy::Ask`: what happens to an
/// ask-worthy (untrusted) self-activation when there is nowhere durable to
/// send the question, or as the deployment's default posture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskFallback {
    /// Deny outright, same as `TrustedOnly` — the safe default for a
    /// deployment with no one to ask (e.g. a headless A2A worker). A run
    /// under `ask_fallback = Deny` NEVER pauses.
    Deny,
    /// Actually pause and wait, bounded by the daemon's timeout sweeper.
    Queue,
}

impl AskFallback {
    /// `"deny"`/`"queue"` — the inverse of [`parse_ask_fallback`].
    pub fn as_str(&self) -> &'static str {
        match self {
            AskFallback::Deny => "deny",
            AskFallback::Queue => "queue",
        }
    }
}

/// Parse `[skills] ask_fallback` / `SkillsPolicyUpdate.ask_fallback`.
/// Unrecognized → `Deny` with a warning (never hangs).
pub fn parse_ask_fallback(s: &str) -> AskFallback {
    match s {
        "deny" => AskFallback::Deny,
        "queue" => AskFallback::Queue,
        other => {
            tracing::warn!(
                "skills: unrecognized ask_fallback = '{other}' — falling back to 'deny' (the \
                 safe default — never hangs)"
            );
            AskFallback::Deny
        }
    }
}

/// The live (runtime-mutable) knobs `RegistrySkillGate` gates on, bundled so
/// a policy update (ADR 0021 addendum: `POST /api/v1/skills/policy`) is one
/// atomic swap rather than three separately-racy field writes.
#[derive(Debug, Clone, Copy)]
pub struct AskPolicyConfig {
    pub policy: ModelActivationPolicy,
    pub ask_fallback: AskFallback,
    /// Reported back via `SkillsPolicyView` for round-trip honesty; the
    /// gate itself never consults it (the daemon's timeout sweeper does,
    /// from its own startup-config snapshot — see the ADR's noted
    /// limitation on live sweeper-interval changes).
    pub ask_timeout_secs: u64,
}

/// The registry-backed `SkillGate`: gates model self-activation on existence,
/// trust policy, the shared advisory-context budget, and (under `Ask`)
/// whether to pause for a human or fall back. The policy itself is
/// runtime-mutable (ADR 0021 addendum) via [`Self::set_policy`], mirroring
/// how `SwappableOracle` already makes the model choice runtime-mutable.
pub struct RegistrySkillGate {
    registry: Arc<SkillRegistry>,
    config: RwLock<AskPolicyConfig>,
}

impl RegistrySkillGate {
    pub fn new(registry: Arc<SkillRegistry>, policy: ModelActivationPolicy) -> Self {
        Self {
            registry,
            config: RwLock::new(AskPolicyConfig {
                policy,
                ask_fallback: AskFallback::Deny,
                ask_timeout_secs: 300,
            }),
        }
    }

    /// Set the `Ask`-fallback posture. No-op under any other policy.
    pub fn with_ask_fallback(self, ask_fallback: AskFallback) -> Self {
        self.config.write().unwrap().ask_fallback = ask_fallback;
        self
    }

    /// Set the ask-timeout, reported via `get_policy` (the daemon's sweeper
    /// reads its own startup-config copy, not this one — see the struct doc).
    pub fn with_ask_timeout_secs(self, secs: u64) -> Self {
        self.config.write().unwrap().ask_timeout_secs = secs;
        self
    }

    /// The current live policy snapshot.
    pub fn get_policy(&self) -> AskPolicyConfig {
        *self.config.read().unwrap()
    }

    /// Swap the live policy atomically. Takes effect on the NEXT
    /// `authorize_activation` call — there is no separate cache to
    /// invalidate, unlike the registry's own mtime-based cache.
    pub fn set_policy(
        &self,
        policy: ModelActivationPolicy,
        ask_fallback: AskFallback,
        ask_timeout_secs: u64,
    ) {
        *self.config.write().unwrap() = AskPolicyConfig {
            policy,
            ask_fallback,
            ask_timeout_secs,
        };
    }

    fn policy(&self) -> ModelActivationPolicy {
        self.config.read().unwrap().policy
    }

    fn ask_fallback(&self) -> AskFallback {
        self.config.read().unwrap().ask_fallback
    }
}

impl SkillGate for RegistrySkillGate {
    fn any_activatable(&self) -> bool {
        let policy = self.policy();
        match policy {
            // Ask can always be asked about, even an untrusted skill.
            ModelActivationPolicy::All | ModelActivationPolicy::Ask => {
                !self.registry.list().is_empty()
            }
            ModelActivationPolicy::TrustedOnly => self
                .registry
                .list()
                .iter()
                .any(|s| policy.permits(&s.provenance.trust)),
        }
    }

    fn authorize_activation(&self, name: &str, run_active: &[String]) -> ActivationAuthorization {
        let Some((trust, _bytes)) = self.registry.activation_probe(name) else {
            return ActivationAuthorization::Deny(format!("no installed skill named '{name}'"));
        };

        // Budget: sum instruction bytes over the union of the operator-active
        // set, this run's already-active set, and the candidate name. A hard
        // floor, independent of policy — refuse rather than ask about (or
        // silently overflow) something that cannot fit regardless.
        let mut union: Vec<String> = self.registry.active_names();
        for n in run_active {
            if !union.iter().any(|u| u == n) {
                union.push(n.clone());
            }
        }
        if !union.iter().any(|u| u == name) {
            union.push(name.to_string());
        }
        let total: usize = union
            .iter()
            .filter_map(|n| self.registry.instruction_bytes(n))
            .sum();
        if total > MAX_TOTAL_ACTIVE_BYTES {
            return ActivationAuthorization::Deny(format!(
                "activating '{name}' would push the total advisory-instruction budget to \
                 {total} bytes, over the {MAX_TOTAL_ACTIVE_BYTES} byte cap — deactivate \
                 something first"
            ));
        }

        let policy = self.policy();
        match policy {
            ModelActivationPolicy::All => ActivationAuthorization::Allow(trust),
            ModelActivationPolicy::TrustedOnly => {
                if policy.permits(&trust) {
                    ActivationAuthorization::Allow(trust)
                } else {
                    ActivationAuthorization::Deny(format!(
                        "skill '{name}' has trust '{trust}', which the model-activation policy \
                         (TrustedOnly) does not permit — an operator must trust it first"
                    ))
                }
            }
            ModelActivationPolicy::Ask => {
                if policy.permits(&trust) {
                    // Already trusted — no need to ask.
                    ActivationAuthorization::Allow(trust)
                } else {
                    match self.ask_fallback() {
                        AskFallback::Deny => ActivationAuthorization::Deny(format!(
                            "skill '{name}' has trust '{trust}' and [skills] ask_fallback = \
                             \"deny\" — an operator must trust it first (no one is configured \
                             to ask)"
                        )),
                        AskFallback::Queue => {
                            let manifest =
                                self.registry.list().into_iter().find(|s| s.name == name);
                            let description = manifest
                                .as_ref()
                                .map(|s| s.description.clone())
                                .unwrap_or_default();
                            let (source_kind, source) = manifest
                                .map(|s| (s.provenance.source_kind, s.provenance.source))
                                .unwrap_or_else(|| ("unknown".into(), "unknown".into()));
                            ActivationAuthorization::AskHuman(ActivationRequestMeta {
                                trust,
                                description,
                                provenance_source_kind: source_kind,
                                provenance_source: source,
                            })
                        }
                    }
                }
            }
        }
    }

    fn elevate_trust(&self, name: &str) -> Result<(), String> {
        self.registry
            .set_trust(name, "trusted")
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_registry() -> Arc<SkillRegistry> {
        let dir = std::env::temp_dir().join(format!("kelter-gate-test-{}", uuid_ish()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = SkillRegistry::open(dir.join("store")).unwrap();
        let src = dir.join("s");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("SKILL.md"),
            "---\nname: some-skill\ndescription: d\n---\n\nbody\n",
        )
        .unwrap();
        reg.install_from_dir(&src).unwrap();
        Arc::new(reg)
    }

    fn uuid_ish() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        format!(
            "{}-{:x}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// ADR 0021 addendum: `set_policy` is not just a getter/setter round-trip
    /// — it must actually change what the NEXT `authorize_activation` call
    /// does, proving the `RwLock` really threads through to gating behavior.
    #[test]
    fn set_policy_takes_effect_on_next_authorize_activation_call() {
        let reg = tmp_registry();
        let gate = RegistrySkillGate::new(reg, ModelActivationPolicy::TrustedOnly);

        // Starts TrustedOnly: an untrusted skill is denied outright.
        assert!(matches!(
            gate.authorize_activation("some-skill", &[]),
            ActivationAuthorization::Deny(_)
        ));

        // Flip to Ask+Queue at runtime.
        gate.set_policy(ModelActivationPolicy::Ask, AskFallback::Queue, 60);
        let snapshot = gate.get_policy();
        assert_eq!(snapshot.policy, ModelActivationPolicy::Ask);
        assert_eq!(snapshot.ask_fallback, AskFallback::Queue);
        assert_eq!(snapshot.ask_timeout_secs, 60);

        // The VERY NEXT authorize_activation call reflects the new policy —
        // not just the getter echoing the setter.
        assert!(matches!(
            gate.authorize_activation("some-skill", &[]),
            ActivationAuthorization::AskHuman(_)
        ));

        // Flip again, to All: now allowed silently.
        gate.set_policy(ModelActivationPolicy::All, AskFallback::Deny, 60);
        assert!(matches!(
            gate.authorize_activation("some-skill", &[]),
            ActivationAuthorization::Allow(_)
        ));
    }
}
