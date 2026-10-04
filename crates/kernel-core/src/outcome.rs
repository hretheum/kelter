//! The kernel's success predicate (ADR 0033): a run may reach
//! [`RunStatus::Completed`](crate::RunStatus::Completed) only if at least one
//! *effective* step happened — a validated action the kernel routed to an
//! [`ActionSource`](crate::ActionSource) (a tool, a WASM module, a remote
//! agent) that came back `ok`.
//!
//! The model proposes `finish`; the kernel decides whether the run earned
//! success. A run that finishes without a single effective step ends
//! [`RunStatus::Incomplete`](crate::RunStatus::Incomplete): the model's
//! summary is kept as the run's output, but the kernel refuses to vouch for
//! it.
//!
//! **What the predicate does NOT claim.** The kernel cannot tell a read from
//! a write: an `ActionSpec` carries no side-effect classification, and a
//! tool's own "read-only" hint would be untrusted extension input (ADR 0006).
//! So "effective" means *the kernel executed something and it succeeded* —
//! the floor below which success is provably unearned — not *the task's
//! requested effect happened*. A model that lists a directory and then claims
//! to have sent an e-mail still completes. Closing that gap needs a verifier,
//! not a counter.

use crate::engine::{StepRecord, ACTIVATE_SKILL_ACTION, DEACTIVATE_SKILL_ACTION, FINISH_ACTION};
use crate::plan::REVIEW_ACTION;

/// Step action name under which a plan-mode `review` node is recorded. A
/// review is a kernel-owned verdict over upstream outputs — it never
/// dispatches anything, whatever `action` the plan node happens to carry.
pub const REVIEW_STEP_ACTION: &str = "review";

/// Actions the kernel handles itself, never through an `ActionSource`. None
/// of them can make a run effective: they change advisory context or record
/// a verdict, with no effect outside the kernel.
pub const KERNEL_OWNED_ACTIONS: [&str; 5] = [
    FINISH_ACTION,
    ACTIVATE_SKILL_ACTION,
    DEACTIVATE_SKILL_ACTION,
    REVIEW_ACTION,
    REVIEW_STEP_ACTION,
];

/// True if `action` is handled by the kernel itself (see
/// [`KERNEL_OWNED_ACTIONS`]).
pub fn is_kernel_owned(action: &str) -> bool {
    KERNEL_OWNED_ACTIONS.contains(&action)
}

/// True if this step counts toward success: it succeeded AND it was a
/// dispatched action, not a kernel-owned one.
pub fn is_effective(step: &StepRecord) -> bool {
    step.ok && !is_kernel_owned(&step.action)
}

/// Number of effective steps in a run's trace.
pub fn effective_steps(steps: &[StepRecord]) -> usize {
    steps.iter().filter(|s| is_effective(s)).count()
}

/// The `error` an [`Incomplete`](crate::RunStatus::Incomplete) run carries:
/// why the kernel withheld success. Counts are included so a reader can tell
/// "nothing was tried" from "everything that was tried failed".
pub fn no_effect_reason(steps: &[StepRecord]) -> String {
    let attempted = steps.iter().filter(|s| !is_kernel_owned(&s.action)).count();
    if attempted == 0 {
        "finished without executing any action — the kernel cannot verify that the task was \
         done (the model's summary is kept as output, unverified)"
            .to_string()
    } else {
        format!(
            "finished without any successful action ({attempted} attempted, all failed) — the \
             kernel cannot verify that the task was done (the model's summary is kept as \
             output, unverified)"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(action: &str, ok: bool) -> StepRecord {
        StepRecord {
            index: 0,
            action: action.into(),
            arguments: serde_json::json!({}),
            result: serde_json::json!({}),
            ok,
        }
    }

    #[test]
    fn successful_dispatched_action_is_effective() {
        assert!(is_effective(&step("write_file", true)));
        assert!(is_effective(&step("delegate_to_writer", true)));
        assert!(is_effective(&step("zapisz_plik", true)));
    }

    #[test]
    fn failed_action_is_not_effective() {
        assert!(!is_effective(&step("write_file", false)));
    }

    #[test]
    fn kernel_owned_actions_are_never_effective_even_when_ok() {
        for a in KERNEL_OWNED_ACTIONS {
            assert!(!is_effective(&step(a, true)), "{a} must not count");
        }
    }

    #[test]
    fn empty_trace_has_no_effective_steps() {
        assert_eq!(effective_steps(&[]), 0);
    }

    #[test]
    fn counts_only_effective_steps() {
        let steps = vec![
            step(ACTIVATE_SKILL_ACTION, true),
            step("read_file", false),
            step("append_ledger", true),
            step(REVIEW_STEP_ACTION, true),
        ];
        assert_eq!(effective_steps(&steps), 1);
    }

    #[test]
    fn reason_distinguishes_nothing_tried_from_all_failed() {
        let none = no_effect_reason(&[step(ACTIVATE_SKILL_ACTION, true)]);
        assert!(none.contains("without executing any action"), "{none}");

        let failed = no_effect_reason(&[step("send_email", false), step("x", false)]);
        assert!(failed.contains("2 attempted, all failed"), "{failed}");
    }
}
