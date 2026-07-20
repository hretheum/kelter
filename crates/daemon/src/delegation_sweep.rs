//! The delegation-authorization `ask` timeout sweeper (ADR 0023,
//! `[delegation] agent_ask_fallback = "queue"`): a paused run must never
//! hang indefinitely. Mirrors `skill_sweep.rs` field-for-field — a SECOND,
//! small sweeper rather than a generalization of the skill one: the two
//! kernel methods being called (`decide_skill_activation` vs
//! `decide_delegation`) take different pending-request shapes
//! (`PendingSkillActivation` vs `PendingDelegation`) and different decision
//! enums (`SkillActivationDecision` vs `AgentDelegationDecision`), so a
//! shared generic sweep would need a new trait-level abstraction in
//! kernel-core purely to serve this one call site — more invasive than the
//! ~35 lines duplicated here. See ADR 0023 for this documented scope call.

use std::sync::Arc;

use kernel_core::{AgentDelegationDecision, Kernel, RunStatus};
use uuid::Uuid;

/// One sweep: force-deny (principal `"system:timeout"`) and mark for resume
/// every run that has been `AwaitingDelegationDecision` for at least
/// `timeout_secs`, relative to `now`. Returns the ids it acted on — the
/// caller re-invokes `execute_run` for each to actually resume, exactly
/// like `sweep_timed_out_skill_activations`.
pub async fn sweep_timed_out_delegation_decisions(
    kernel: &Arc<Kernel>,
    timeout_secs: u64,
    now: u64,
) -> Vec<Uuid> {
    let mut resumed = Vec::new();
    let Ok(runs) = kernel.store().list().await else {
        return resumed;
    };
    for run in runs {
        if run.status != RunStatus::AwaitingDelegationDecision {
            continue;
        }
        let Some(pending) = &run.pending_delegation else {
            continue;
        };
        if now.saturating_sub(pending.requested_at) < timeout_secs {
            continue;
        }
        tracing::warn!(
            run_id = %run.id,
            agent = %pending.agent,
            "agents: ask-delegation timed out — auto-denying (principal system:timeout)"
        );
        if kernel
            .decide_delegation(
                run.id,
                &pending.agent,
                AgentDelegationDecision::Deny,
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
