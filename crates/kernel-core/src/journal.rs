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
}

impl JournalEntry {
    pub fn step(&self) -> Option<u32> {
        match self {
            JournalEntry::Decision { step, .. }
            | JournalEntry::Intent { step, .. }
            | JournalEntry::Note { step, .. }
            | JournalEntry::Receipt { step, .. }
            | JournalEntry::SkillActivationRequested { step, .. }
            | JournalEntry::SkillActivationDecided { step, .. } => Some(*step),
            JournalEntry::TrustPosture { .. }
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
            _ => {}
        }
    }
    folded
}
