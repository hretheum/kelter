//! The action-approval timeout sweeper (ADR 0038, `[approval]
//! ask_timeout_secs`): no decision in time is a `deny`, so a paused run never
//! hangs. A third small sweeper alongside `skill_sweep.rs` and
//! `delegation_sweep.rs`, for the reason `delegation_sweep.rs` gives — each
//! calls a different kernel method with a different pending shape.

use std::sync::Arc;

use kernel_core::{ActionApprovalDecision, Kernel, RunStatus};
use uuid::Uuid;

/// One sweep: deny (principal `"system:timeout"`) every run that has been
/// `AwaitingActionApproval` for at least `timeout_secs`, relative to `now`.
/// Returns the ids it acted on — the caller re-invokes `execute_run` for
/// each, exactly like the other two sweepers.
pub async fn sweep_timed_out_action_approvals(
    kernel: &Arc<Kernel>,
    timeout_secs: u64,
    now: u64,
) -> Vec<Uuid> {
    let mut resumed = Vec::new();
    let Ok(runs) = kernel.store().list().await else {
        return resumed;
    };
    for run in runs {
        if run.status != RunStatus::AwaitingActionApproval {
            continue;
        }
        let Some(pending) = &run.pending_action_approval else {
            continue;
        };
        if now.saturating_sub(pending.requested_at) < timeout_secs {
            continue;
        }
        tracing::warn!(
            run_id = %run.id,
            action = %pending.action,
            "approval: action approval timed out — denying (principal system:timeout)"
        );
        if kernel
            .decide_action_approval(
                run.id,
                &pending.action,
                ActionApprovalDecision::Deny,
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
