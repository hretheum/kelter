//! The model self-activation seam (ADR 0020), extended with a third gate
//! outcome for a human-in-the-loop decision (ADR 0021).
//!
//! `activate_skill`/`deactivate_skill` are kernel-owned actions (handled
//! inline in the engine loop exactly like `finish`) — they never touch the
//! tool substrate (`ActionSource`), so they grant zero capability. Their only
//! effect is which advisory skill instructions the model sees on the NEXT
//! step. kernel-core stays skills-agnostic: this trait is the seam the
//! `skills` crate implements (`RegistrySkillGate`) to decide WHICH skills the
//! model may switch on (existence + trust policy + a shared context budget)
//! without kernel-core knowing anything skills-specific.
//!
//! ADR 0021 adds a third outcome, `AskHuman`: instead of allowing or denying
//! a self-activation outright, the gate may ask the engine to pause the run
//! and wait for a human decision. Capability-wise this changes nothing —
//! nothing executes until a human decides, exactly like `Deny` — it only
//! changes WHO decides and WHEN.

use serde::{Deserialize, Serialize};

/// The outcome of gating one model-proposed `activate_skill`. Three-way,
/// never a boolean/Result — a `Deny` and an `AskHuman` are both legitimate,
/// distinct non-executions; conflating them would blur the audit trail.
#[derive(Debug, Clone)]
pub enum ActivationAuthorization {
    /// Proceed now — the returned trust label is journaled for audit.
    Allow(String),
    /// Refuse now, with feedback for the model to retry differently. No
    /// state change, same as today.
    Deny(String),
    /// Pause the run and ask a human. Capability-wise this is identical to
    /// `Deny` right now (nothing executes) — it only changes WHO decides and
    /// WHEN. `meta` carries everything the human-facing prompt needs so the
    /// engine (skills-agnostic) never has to ask the gate again to render it.
    AskHuman(ActivationRequestMeta),
}

/// Self-contained: durably journaled verbatim so a restart can re-render the
/// prompt without re-touching the skills subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivationRequestMeta {
    pub trust: String,
    pub description: String,
    pub provenance_source_kind: String,
    pub provenance_source: String,
}

/// A human's answer to a pending `AskHuman` activation request. Journaled and
/// engine-applied through the SAME mechanism the model's own decisions go
/// through (journal → replay → apply), not a side channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillActivationDecision {
    /// The operator declined — the model gets feedback and must propose
    /// something else. No trust change.
    Deny,
    /// Activate for this run only — ephemeral, exactly like the existing
    /// model self-activation path. No trust change.
    AllowOnce,
    /// Activate for this run AND elevate the skill's persistent trust label,
    /// via `SkillGate::elevate_trust` — a deliberate, explicitly-documented
    /// amendment to ADR 0019 (see ADR 0021). Never model-initiated: reachable
    /// only through a human's bearer-gated decision on a specific pending
    /// request.
    AllowAlways,
}

/// Advisory-skill activation gate. See the module docs for the invariant this
/// preserves.
pub trait SkillGate: Send + Sync {
    /// Any installed skill the model is currently permitted to self-activate?
    /// False → the kernel omits `activate_skill`/`deactivate_skill` from the
    /// offered action space entirely (no noise when there's nothing to load).
    fn any_activatable(&self) -> bool;

    /// Gate ONE model self-activation. `run_active` is the run-scoped set the
    /// model has already switched on this run. See [`ActivationAuthorization`]
    /// for the three possible outcomes.
    fn authorize_activation(&self, name: &str, run_active: &[String]) -> ActivationAuthorization;

    /// Called ONLY when applying a human `allow_always` decision (never from
    /// a model-visible path). Best-effort; the engine logs a failure but
    /// still applies the activation for THIS run (never blocks on it) —
    /// losing a persistence write must not un-pause a run a human already
    /// resolved.
    fn elevate_trust(&self, name: &str) -> Result<(), String>;
}
