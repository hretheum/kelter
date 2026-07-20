//! The `ask`-activation timeout sweeper (ADR 0021, [skills] ask_fallback =
//! "queue"): a paused run must never hang indefinitely. Extracted into a
//! standalone, unit-testable function rather than living only inline in
//! `main()`, so "a paused run cannot hang forever" has a real test, not just
//! inspection.

use std::sync::Arc;

use kernel_core::{Kernel, RunStatus, SkillActivationDecision};
use uuid::Uuid;

/// One sweep: force-deny (principal `"system:timeout"`) and mark for resume
/// every run that has been `AwaitingSkillDecision` for at least
/// `timeout_secs`, relative to `now` (both injected so the sweep is
/// deterministic to test). Returns the ids it acted on. This function only
/// DECIDES (journals + writes state, via `Kernel::decide_skill_activation`)
/// — the caller is responsible for re-invoking `execute_run` for each
/// returned id to actually resume, exactly like the API handler does after a
/// human decision.
pub async fn sweep_timed_out_skill_activations(
    kernel: &Arc<Kernel>,
    timeout_secs: u64,
    now: u64,
) -> Vec<Uuid> {
    let mut resumed = Vec::new();
    let Ok(runs) = kernel.store().list().await else {
        return resumed;
    };
    for run in runs {
        if run.status != RunStatus::AwaitingSkillDecision {
            continue;
        }
        let Some(pending) = &run.pending_skill_activation else {
            continue;
        };
        if now.saturating_sub(pending.requested_at) < timeout_secs {
            continue;
        }
        tracing::warn!(
            run_id = %run.id,
            skill = %pending.skill,
            "skills: ask-activation timed out — auto-denying (principal system:timeout)"
        );
        if kernel
            .decide_skill_activation(
                run.id,
                &pending.skill,
                SkillActivationDecision::Deny,
                "system:timeout",
            )
            .await
            .is_ok()
        {
            resumed.push(run.id);
        }
    }
    resumed
}
