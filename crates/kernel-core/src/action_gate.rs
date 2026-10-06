//! The human gate on mutating actions (ADR 0038, KR1.2).
//!
//! An operator classifies actions as `allow` / `ask` / `deny` in config. The
//! classification applies ONLY to mutating actions — those whose spec
//! declares `idempotency_key` support, the same criterion ADR 0031 uses for
//! the state epoch and ADR 0007 for exactly-once resume. Read-only actions,
//! the kernel-owned actions (`finish`, skill (de)activation) and `delegate_to_*`
//! (which have their own gate, ADR 0023) are never touched by it.
//!
//! `ask` reuses the pause mechanism ADR 0021/0023/0025 established — a
//! journaled `*Requested` entry, a non-terminal `Awaiting*` status the resume
//! sweep re-surfaces, a journaled `*Decided` entry with the principal, a
//! sweeper that turns silence into `deny`, and the replay branch that
//! dispatches an allowed proposal. Only the vocabulary is new: a tool
//! approval is not a delegation, and reusing the delegation entries would
//! change what `agent_delegation_*` means to every existing reader.
//!
//! Nothing here is reachable by the model: the policy is config, and a
//! decision is an authenticated API call (or the timeout sweeper's deny).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::plan::REVIEW_ACTION;

/// What the operator wants done with one mutating action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionPolicy {
    /// Execute without asking — the pre-ADR-0038 behaviour, and the default.
    #[default]
    Allow,
    /// Pause the run until a human allows or denies this specific proposal.
    Ask,
    /// Refuse every proposal of this action; the model is told why.
    Deny,
}

/// A human's answer to a pending action approval. Two-way on purpose: there
/// is no persistent "allow always" — the standing policy is the config
/// file, edited by the operator, never by a decision on one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionApprovalDecision {
    Allow,
    Deny,
}

/// The configured gate. The default is inert: every action `allow`, so a
/// kernel built without it behaves exactly as before ADR 0038.
#[derive(Debug, Clone, Default)]
pub struct ActionGateConfig {
    /// Policy for a mutating action with no entry in `actions`.
    pub default: ActionPolicy,
    /// Per-action overrides, keyed by action name. The key
    /// [`REVIEW_ACTION`] (`review_verdict`) is special: `ask` makes plan-mode
    /// `review` nodes a human verdict instead of a model call.
    pub actions: HashMap<String, ActionPolicy>,
    /// How long a pause may wait before the sweeper denies it (principal
    /// `system:timeout`). `0` disables the sweeper — the pause then waits
    /// for a human indefinitely.
    pub ask_timeout_secs: u64,
}

impl ActionGateConfig {
    /// The policy for a proposed action. `mutating` is the caller's reading
    /// of the action's spec (`supports_idempotency_key`); a non-mutating or
    /// delegation action is always `Allow` here, whatever the config says.
    pub fn policy_for(&self, action: &str, mutating: bool) -> ActionPolicy {
        if !mutating || crate::engine::is_a2a_action(action) || action == REVIEW_ACTION {
            return ActionPolicy::Allow;
        }
        self.actions.get(action).copied().unwrap_or(self.default)
    }

    /// Whether plan-mode `review` nodes are decided by a human (ADR 0038):
    /// `review_verdict = "ask"`.
    pub fn review_by_human(&self) -> bool {
        self.actions.get(REVIEW_ACTION) == Some(&ActionPolicy::Ask)
    }

    /// True when the gate can ever pause a run — the daemon starts the
    /// timeout sweeper only then.
    pub fn can_ask(&self) -> bool {
        self.default == ActionPolicy::Ask || self.actions.values().any(|p| *p == ActionPolicy::Ask)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate() -> ActionGateConfig {
        ActionGateConfig {
            default: ActionPolicy::Allow,
            actions: HashMap::from([
                ("write_file".to_string(), ActionPolicy::Ask),
                ("delete_file".to_string(), ActionPolicy::Deny),
                ("read_file".to_string(), ActionPolicy::Deny),
                ("delegate_to_x".to_string(), ActionPolicy::Deny),
            ]),
            ask_timeout_secs: 300,
        }
    }

    #[test]
    fn default_gate_allows_everything() {
        let g = ActionGateConfig::default();
        assert_eq!(g.policy_for("write_file", true), ActionPolicy::Allow);
        assert!(!g.can_ask());
        assert!(!g.review_by_human());
    }

    #[test]
    fn only_mutating_actions_are_gated() {
        let g = gate();
        assert_eq!(g.policy_for("write_file", true), ActionPolicy::Ask);
        assert_eq!(g.policy_for("delete_file", true), ActionPolicy::Deny);
        // A read-only action stays allowed even with an entry naming it.
        assert_eq!(g.policy_for("read_file", false), ActionPolicy::Allow);
        // Delegations have their own gate (ADR 0023).
        assert_eq!(g.policy_for("delegate_to_x", true), ActionPolicy::Allow);
        // No entry → the default.
        assert_eq!(g.policy_for("append_ledger", true), ActionPolicy::Allow);
        let strict = ActionGateConfig {
            default: ActionPolicy::Ask,
            ..gate()
        };
        assert_eq!(strict.policy_for("append_ledger", true), ActionPolicy::Ask);
        assert!(strict.can_ask());
    }

    #[test]
    fn review_by_human_needs_an_explicit_ask() {
        let mut g = gate();
        assert!(!g.review_by_human());
        g.actions
            .insert(REVIEW_ACTION.to_string(), ActionPolicy::Ask);
        assert!(g.review_by_human());
        // The review key never gates a tool proposal.
        assert_eq!(g.policy_for(REVIEW_ACTION, true), ActionPolicy::Allow);
    }
}
