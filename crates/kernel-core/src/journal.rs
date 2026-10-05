//! The durable-execution journal.
//!
//! Every run appends an ordered journal of what was *decided* and what was
//! *done*. Replay after a crash follows one law: **anything nondeterministic
//! (a model call) is journaled at first execution and never re-queried; the
//! journal answers instead.** Side effects sit between an `Intent` written
//! before execution and a `Receipt` written after — the crash window in
//! between is closed by tool-side idempotency keys (see
//! [`crate::action::ActionSpec::supports_idempotency_key`]).

use serde::{Deserialize, Serialize};

use crate::contract::{ContractReport, Verification};
use crate::delegation_gate::{AgentDelegationDecision, DelegationRequestMeta};
use crate::engine::RunStatus;
use crate::skill_gate::{ActivationRequestMeta, SkillActivationDecision};

/// Token accounting for one model call (also aggregated per run).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Number of model calls this usage covers (1 for a single proposal).
    pub calls: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.calls += other.calls;
    }
}

/// One journal record. Ordering (per run) is the store's append order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JournalEntry {
    /// A validated action chosen at a decision point. Memoized: on replay
    /// this step's model call is skipped and this entry is used instead.
    Decision {
        step: u32,
        action: String,
        arguments: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    /// Written durably BEFORE a tool executes. A dangling intent (no
    /// matching receipt) marks the ambiguous crash window.
    Intent {
        step: u32,
        action: String,
        arguments: serde_json::Value,
        idempotency_key: String,
    },
    /// A durable side-note recorded by the executing action source at a
    /// critical moment (e.g. a remote A2A task id, journaled at creation so
    /// resume reconnects instead of re-delegating).
    Note {
        step: u32,
        key: String,
        value: serde_json::Value,
    },
    /// Written durably AFTER a tool executed.
    Receipt {
        step: u32,
        ok: bool,
        result: serde_json::Value,
    },
    /// The extension trust posture in effect when the run started (ADR
    /// 0012): `(extension_name, trust_label)`. Journaled once so "what ran
    /// unsandboxed" is durably attributable.
    TrustPosture { entries: Vec<(String, String)> },
    /// The validated plan a `plan`-mode run will execute. Journaled once,
    /// the moment the model proposes it: on resume the plan is replayed from
    /// here and the model is NOT re-queried for it (the plan is a memoized
    /// decision). Node execution reuses `Intent`/`Receipt`/`Note` keyed by
    /// the node's index.
    PlanProposed { plan: crate::plan::Plan },
    /// The run reached a terminal state.
    Terminal {
        status: RunStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// How the success verdict was reached (ADR 0037). Absent for
        /// `failed`/`canceled` and in entries written before ADR 0037.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verification: Option<Verification>,
        /// The contract evaluation behind the verdict (ADR 0037), if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        contract_report: Option<ContractReport>,
    },
    /// A fresh `activate_skill` proposal was gated `AskHuman` (ADR 0021): the
    /// run pauses here, durably, until a matching `SkillActivationDecided`
    /// entry appears. Carries the model's original arguments verbatim so a
    /// later Allow* decision can replay them through the existing decision
    /// mechanism without re-querying the model.
    SkillActivationRequested {
        step: u32,
        skill: String,
        arguments: serde_json::Value,
        trust: String,
        description: String,
        provenance_source_kind: String,
        provenance_source: String,
        requested_at: u64,
    },
    /// A human (or the timeout sweeper, principal `"system:timeout"`)
    /// resolved a pending `SkillActivationRequested`.
    SkillActivationDecided {
        step: u32,
        skill: String,
        decision: SkillActivationDecision,
        principal: String,
    },
    /// A fresh `delegate_to_*` proposal was gated `AskHuman` by the
    /// `DelegationGate` (ADR 0023): the run pauses here, durably, until a
    /// matching `AgentDelegationDecided` entry appears. Carries the model's
    /// original arguments verbatim so a later Allow* decision can replay
    /// them through the existing decision mechanism without re-querying the
    /// model. Field-for-field mirror of `SkillActivationRequested` (`agent`
    /// instead of `skill`) plus `target_origin`, which a delegation request
    /// has and a skill activation does not.
    AgentDelegationRequested {
        step: u32,
        agent: String,
        target_origin: String,
        arguments: serde_json::Value,
        trust: String,
        description: String,
        provenance_source_kind: String,
        provenance_source: String,
        requested_at: u64,
    },
    /// A human (or the timeout sweeper, principal `"system:timeout"`)
    /// resolved a pending `AgentDelegationRequested`.
    AgentDelegationDecided {
        step: u32,
        agent: String,
        decision: AgentDelegationDecision,
        principal: String,
    },
    /// An authorized delegation's remote task entered `input-required` (ADR
    /// 0026): the run pauses here durably until the human's continuation
    /// arrives as a `Note` keyed `A2A_CONTINUATION_INPUT_NOTE` at the same
    /// step. `task_id` is the remote task to reconnect to (never re-created).
    AgentDelegationInputRequested {
        step: u32,
        agent: String,
        task_id: String,
        prompt: String,
        requested_at: u64,
    },
    /// INFORMATIONAL (ADR 0036): the kernel refused something at `step` —
    /// a model proposal, an automated retry/fallback of an accepted
    /// delegation, or a plan node. Mirrors a live `RunEvent::Rejected`
    /// one-for-one. `subject` says what was refused, so a reader never has to
    /// guess by adjacency: `proposal` carries the refused proposal inline
    /// (`action`/`arguments`, bounded — it is journaled nowhere else);
    /// `decision` points at this step's journaled `Decision`; `plan_node`
    /// points at node `branch` of the plan generation `step` belongs to;
    /// `none` means no proposal exists (the model call itself failed).
    ///
    /// Never read by resume: `fold_step`, the replay path and every
    /// exactly-once seed ignore it, and it never reaches the model's context.
    Rejected {
        step: u32,
        attempt: u32,
        source: RejectionSource,
        subject: RejectionSubject,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arguments: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        /// Wall-clock milliseconds since the Unix epoch.
        at_ms: u64,
    },
    /// INFORMATIONAL (ADR 0036): an execution picked this run up from durable
    /// state left by an earlier one. `session` identifies the executing
    /// process (one id per kernel process, [`execution_session_id`]);
    /// `at_step` is where execution continues (step mode: the first step not
    /// yet recorded; plan mode: the active generation's step base);
    /// `replayed_steps` lists the steps whose decision — a journaled
    /// `Decision` (or the original proposal behind a resolved human ask) in
    /// step mode, the journaled `PlanProposed` in plan mode — this execution
    /// takes from the journal instead of querying the model.
    ///
    /// Never read by resume (same contract as `Rejected`).
    Resumed {
        at_step: u32,
        session: String,
        cause: ResumeCause,
        replayed_steps: Vec<u32>,
        /// Wall-clock milliseconds since the Unix epoch.
        at_ms: u64,
    },
    /// INFORMATIONAL for execution (ADR 0037): a human confirmed, or revoked
    /// the confirmation of, an unverified `completed` run — the decision that
    /// admits it to (or removes it from) learning. Written after `Terminal`
    /// by the authenticated API only; the run record's `confirmation` mirrors
    /// the latest one. Never read by resume or replay.
    SuccessConfirmationDecided {
        decision: ConfirmationDecision,
        principal: String,
        /// Wall-clock milliseconds since the Unix epoch.
        at_ms: u64,
    },
}

/// A human's decision on an unverified success (ADR 0037).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmationDecision {
    /// The run did its task: admit it to learning.
    Confirm,
    /// Withdraw an earlier confirmation (and what was learned from it).
    Revoke,
}

/// Which kernel rule refused (ADR 0036). One value per distinct mechanism;
/// the human-readable `reason` carries the specifics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionSource {
    /// The policy call failed (transport, parse) — no proposal exists.
    PolicyError,
    /// The proposal failed action-schema validation.
    Schema,
    /// The anti-stall / duplicate rule (ADR 0031 epoch window).
    Duplicate,
    /// Delegation budget, depth or cycle limit (ADR 0022), or an a2a action
    /// with no resolvable origin.
    DelegationLimit,
    /// The agent-trust `DelegationGate` refused (ADR 0023). In an automated
    /// retry/fallback an `AskHuman` outcome is also a refusal.
    DelegationGate,
    /// The skill self-activation gate denied (ADR 0020).
    SkillGate,
    /// The target agent's circuit is open (ADR 0023): a sanctioned fallback
    /// skipped, or a plan node refused before dialing.
    CircuitOpen,
    /// A step-mode delegation attempt failed and the resilience loop retries
    /// (ADR 0023).
    DelegationRetry,
    /// A plan node attempt failed and is re-admitted (ADR 0030).
    NodeRetry,
    /// A revised plan failed plan validation (ADR 0030).
    PlanValidation,
    /// A recorded operator Deny for a plan-mode delegation ask (ADR 0025),
    /// consumed on resume.
    OperatorDenied,
}

/// What a `Rejected` entry refers to (ADR 0036). See [`JournalEntry::Rejected`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionSubject {
    /// A fresh model proposal, never accepted; carried inline.
    Proposal,
    /// This step's accepted, journaled `Decision`.
    Decision,
    /// Node `branch` of the plan generation that `step` belongs to.
    PlanNode,
    /// Nothing was proposed (the model call failed).
    None,
}

/// Why an execution resumed (ADR 0036), derived from the journal alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeCause {
    /// The last execution-relevant entry resolves a human pause (a skill or
    /// delegation decision, or a delegation continuation input).
    PauseResolved,
    /// The previous execution stopped without a terminal entry and without a
    /// pause — a crash, kill or daemon restart.
    Interrupted,
}

/// Upper bound on a refused proposal's serialized `arguments` (ADR 0036). A
/// weak model can emit arbitrarily large garbage; the journal keeps a
/// bounded preview instead.
pub const MAX_REJECTED_ARGUMENTS_BYTES: usize = 8 * 1024;
/// Characters of the serialized arguments kept when they are truncated.
pub const REJECTED_ARGUMENTS_PREVIEW_CHARS: usize = 1024;
/// Upper bound on a `Rejected` entry's `reason`, in characters.
pub const MAX_REJECTION_REASON_CHARS: usize = 2000;

/// Bound (and scrub) a refused proposal's arguments for the journal.
pub fn bounded_rejected_arguments(arguments: &serde_json::Value) -> serde_json::Value {
    let scrubbed = crate::redact::scrub_json(arguments);
    let raw = scrubbed.to_string();
    if raw.len() <= MAX_REJECTED_ARGUMENTS_BYTES {
        return scrubbed;
    }
    let preview: String = raw.chars().take(REJECTED_ARGUMENTS_PREVIEW_CHARS).collect();
    serde_json::json!({
        "truncated": true,
        "bytes": raw.len(),
        "preview": preview,
    })
}

/// Bound (and scrub) a rejection reason for the journal.
pub fn bounded_rejection_reason(reason: &str) -> String {
    let scrubbed = crate::redact::scrub_secrets(reason);
    if scrubbed.chars().count() <= MAX_REJECTION_REASON_CHARS {
        return scrubbed;
    }
    let cut: String = scrubbed.chars().take(MAX_REJECTION_REASON_CHARS).collect();
    format!("{cut}…")
}

/// This process's execution-session id (ADR 0036): generated once, on first
/// use, and shared by every kernel in the process — so `Resumed` entries from
/// two executions of one run differ exactly when the process did.
pub fn execution_session_id() -> uuid::Uuid {
    static SESSION: std::sync::OnceLock<uuid::Uuid> = std::sync::OnceLock::new();
    *SESSION.get_or_init(uuid::Uuid::new_v4)
}

impl JournalEntry {
    /// True for entries that exist only to inform readers (ADR 0036) and that
    /// no resume, replay or exactly-once logic may depend on.
    pub fn is_informational(&self) -> bool {
        matches!(
            self,
            JournalEntry::Rejected { .. }
                | JournalEntry::Resumed { .. }
                | JournalEntry::SuccessConfirmationDecided { .. }
        )
    }
}

impl JournalEntry {
    pub fn step(&self) -> Option<u32> {
        match self {
            JournalEntry::Decision { step, .. }
            | JournalEntry::Intent { step, .. }
            | JournalEntry::Note { step, .. }
            | JournalEntry::Receipt { step, .. }
            | JournalEntry::SkillActivationRequested { step, .. }
            | JournalEntry::SkillActivationDecided { step, .. }
            | JournalEntry::AgentDelegationRequested { step, .. }
            | JournalEntry::AgentDelegationDecided { step, .. }
            | JournalEntry::AgentDelegationInputRequested { step, .. }
            | JournalEntry::Rejected { step, .. } => Some(*step),
            JournalEntry::TrustPosture { .. }
            | JournalEntry::Resumed { .. }
            | JournalEntry::SuccessConfirmationDecided { .. }
            | JournalEntry::PlanProposed { .. }
            | JournalEntry::Terminal { .. } => None,
        }
    }
}

/// What the journal says about one step, folded for the resume path.
#[derive(Debug, Default, Clone)]
pub struct StepJournal {
    pub decision: Option<(String, serde_json::Value)>,
    pub intent: Option<(String, serde_json::Value, String)>,
    pub notes: std::collections::HashMap<String, serde_json::Value>,
    pub receipt: Option<(bool, serde_json::Value)>,
    /// A pending or decided `activate_skill` ask-request for this step (ADR
    /// 0021). `None` when this step never triggered an `AskHuman` gate
    /// outcome.
    pub skill_activation: Option<PendingActivationFold>,
    /// A pending or decided `delegate_to_*` ask-request for this step (ADR
    /// 0023). `None` when this step never triggered an `AskHuman` gate
    /// outcome from the `DelegationGate`.
    pub delegation: Option<PendingDelegationFold>,
}

/// Folded view of one `SkillActivationRequested` (+ optional matching
/// `SkillActivationDecided`) for a step.
#[derive(Debug, Clone)]
pub struct PendingActivationFold {
    pub skill: String,
    pub arguments: serde_json::Value,
    pub meta: ActivationRequestMeta,
    pub requested_at: u64,
    pub decision: Option<SkillActivationDecision>,
}

/// Folded view of one `AgentDelegationRequested` (+ optional matching
/// `AgentDelegationDecided`) for a step — mirrors `PendingActivationFold`.
#[derive(Debug, Clone)]
pub struct PendingDelegationFold {
    pub agent: String,
    pub target_origin: String,
    pub arguments: serde_json::Value,
    pub meta: DelegationRequestMeta,
    pub requested_at: u64,
    pub decision: Option<AgentDelegationDecision>,
}

/// Fold a run's journal into the per-step view for `step_index`.
pub fn fold_step(journal: &[JournalEntry], step_index: u32) -> StepJournal {
    let mut folded = StepJournal::default();
    for entry in journal {
        match entry {
            JournalEntry::Decision {
                step,
                action,
                arguments,
                ..
            } if *step == step_index => {
                folded.decision = Some((action.clone(), arguments.clone()));
            }
            JournalEntry::Intent {
                step,
                action,
                arguments,
                idempotency_key,
            } if *step == step_index => {
                folded.intent = Some((action.clone(), arguments.clone(), idempotency_key.clone()));
            }
            JournalEntry::Note { step, key, value } if *step == step_index => {
                folded.notes.insert(key.clone(), value.clone());
            }
            JournalEntry::Receipt { step, ok, result } if *step == step_index => {
                folded.receipt = Some((*ok, result.clone()));
            }
            JournalEntry::SkillActivationRequested {
                step,
                skill,
                arguments,
                trust,
                description,
                provenance_source_kind,
                provenance_source,
                requested_at,
            } if *step == step_index => {
                folded.skill_activation = Some(PendingActivationFold {
                    skill: skill.clone(),
                    arguments: arguments.clone(),
                    meta: ActivationRequestMeta {
                        trust: trust.clone(),
                        description: description.clone(),
                        provenance_source_kind: provenance_source_kind.clone(),
                        provenance_source: provenance_source.clone(),
                    },
                    requested_at: *requested_at,
                    decision: None,
                });
            }
            JournalEntry::SkillActivationDecided {
                step,
                skill,
                decision,
                ..
            } if *step == step_index => {
                if let Some(pending) = folded.skill_activation.as_mut() {
                    if &pending.skill == skill {
                        pending.decision = Some(*decision);
                    }
                }
            }
            JournalEntry::AgentDelegationRequested {
                step,
                agent,
                target_origin,
                arguments,
                trust,
                description,
                provenance_source_kind,
                provenance_source,
                requested_at,
            } if *step == step_index => {
                folded.delegation = Some(PendingDelegationFold {
                    agent: agent.clone(),
                    target_origin: target_origin.clone(),
                    arguments: arguments.clone(),
                    meta: DelegationRequestMeta {
                        trust: trust.clone(),
                        description: description.clone(),
                        provenance_source_kind: provenance_source_kind.clone(),
                        provenance_source: provenance_source.clone(),
                    },
                    requested_at: *requested_at,
                    decision: None,
                });
            }
            JournalEntry::AgentDelegationDecided {
                step,
                agent,
                decision,
                ..
            } if *step == step_index => {
                if let Some(pending) = folded.delegation.as_mut() {
                    if &pending.agent == agent {
                        pending.decision = Some(*decision);
                    }
                }
            }
            // Informational entries (ADR 0036) never shape resume — listed
            // explicitly so a future arm above cannot start matching them.
            JournalEntry::Rejected { .. }
            | JournalEntry::Resumed { .. }
            | JournalEntry::SuccessConfirmationDecided { .. } => {}
            _ => {}
        }
    }
    folded
}
