//! The agent-delegation authorization seam (ADR 0023) — mirrors
//! [`crate::skill_gate`]'s shapes and control-flow trick almost exactly, but
//! is a DELIBERATELY DISTINCT trait, not a reuse of `SkillGate`.
//!
//! The distinction matters at the type level, not just in prose: a skill
//! activation gate withholds ADVICE (knowledge the model reads); this gate
//! withholds AUTHORIZATION TO ACT (`delegate_to_X` dials a real remote
//! endpoint and sends it real work). Conflating the two types risks the
//! "advisory, never authorizing" framing that `SkillGate`'s docs lean on
//! leaking into what is actually a capability gate. So: same shapes (a
//! three-way `Allow`/`Deny`/`AskHuman` outcome, a durable `AskHuman` meta
//! payload, a three-way human decision with a trust-elevating `AllowAlways`),
//! same seam discipline (kernel-core stays A2A-agnostic; the `a2a` crate
//! implements this trait exactly like `skills` implements `SkillGate`), but
//! a separate trait and separate journal/event vocabulary throughout.
//!
//! Governance ordering (ADR 0022 + ADR 0023): this gate NEVER runs instead of
//! `Kernel::authorize_delegation` (P2's budget/depth/cycle check) — only
//! AFTER it already passed. P2 is cheap and target-agnostic; P3 (this gate)
//! is the more expensive, potentially-interactive check, meaningful only once
//! P2 has already said the delegation is structurally allowed.

use serde::{Deserialize, Serialize};

/// The outcome of gating one proposed (or retried/fallback) delegation
/// attempt to a specific agent. Three-way, never a boolean/Result — a `Deny`
/// and an `AskHuman` are both legitimate, distinct non-actions.
#[derive(Debug, Clone)]
pub enum DelegationDecisionOutcome {
    /// Authorize this dial now — the returned trust label is journaled for
    /// audit.
    Allow(String),
    /// Refuse this dial now, with feedback for the model to retry
    /// differently. No state change.
    Deny(String),
    /// Pause the run and ask a human before authorizing this dial.
    /// Capability-wise this is identical to `Deny` right now (nothing is
    /// dialed) — it only changes WHO decides and WHEN. `meta` carries
    /// everything a human-facing prompt needs so the engine (a2a-agnostic)
    /// never has to ask the gate again to render it.
    AskHuman(DelegationRequestMeta),
}

/// Self-contained: durably journaled verbatim so a restart can re-render the
/// prompt without re-touching the a2a subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationRequestMeta {
    pub trust: String,
    pub description: String,
    pub provenance_source_kind: String,
    pub provenance_source: String,
}

/// A human's answer to a pending `AskHuman` delegation request. Journaled and
/// engine-applied through the SAME mechanism the model's own decisions go
/// through (journal → replay → apply), not a side channel. Field-for-field
/// mirror of `SkillActivationDecision` — the concepts overlap exactly here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentDelegationDecision {
    /// The operator declined — the model gets feedback and must propose
    /// something else. No trust change.
    Deny,
    /// Authorize this run's delegation only — ephemeral. No trust change.
    AllowOnce,
    /// Authorize this delegation AND elevate the agent's persistent trust
    /// label, via `DelegationGate::elevate_trust`. Never model-initiated:
    /// reachable only through a human's bearer-gated decision on a specific
    /// pending request. An agent's own Agent Card or response can never
    /// assert its own trust — only this path can.
    AllowAlways,
}

/// Agent-delegation authorization gate (ADR 0023). See the module docs for
/// the invariant this preserves and why it is a distinct type from
/// `SkillGate`.
pub trait DelegationGate: Send + Sync {
    /// Gate ONE delegation attempt (fresh, retry, or sanctioned fallback) —
    /// `action` is the `delegate_to_*` action name being attempted,
    /// `target_origin` the SSRF-pinned origin it resolves to (informational
    /// for audit; the gate matches on `action`). Only ever called AFTER
    /// `Kernel::authorize_delegation` (P2: budget/depth/cycle) has already
    /// allowed this attempt structurally.
    fn authorize_delegation_to(
        &self,
        action: &str,
        target_origin: &str,
    ) -> DelegationDecisionOutcome;

    /// Called ONLY when applying a human `allow_always` decision (never from
    /// a model-visible path). Best-effort; the engine logs a failure but
    /// still applies the delegation for THIS run (never blocks on it) —
    /// losing a persistence write must not un-pause a run a human already
    /// resolved.
    fn elevate_trust(&self, action: &str) -> Result<(), String>;
}
