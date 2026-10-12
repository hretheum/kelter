//! Deterministic agent kernel: a typed state machine owns control flow;
//! LLMs act as constrained policy oracles at explicit decision points.
//!
//! Layering:
//! - [`action`] — action space, proposal, kernel-side validation (the trust
//!   boundary between kernel and any model adapter).
//! - [`oracle`] — the policy oracle trait (`propose(context, schema)`) and
//!   the latitude knob.
//! - [`engine`] — the run state machine: retries, persistence, events,
//!   gated reflection.
//! - [`outcome`] — the success predicate: which steps let a run complete
//!   (ADR 0033).
//! - [`contract`] — the caller's declarative success contract (ADR 0037).
//! - [`action_gate`] — the operator's allow/ask/deny policy on mutating
//!   actions (ADR 0038).
//! - [`budget`] — per-run and per-principal cost ceilings, enforced before
//!   they are crossed (ADR 0039).
//! - [`store`] — run persistence trait + in-memory implementation.

pub mod action;
pub mod action_gate;
pub mod budget;
pub mod contract;
pub mod cost;
pub mod delegation_gate;
pub mod engine;
pub mod journal;
pub mod learned;
pub mod oracle;
pub mod outcome;
pub mod plan;
pub mod redact;
pub mod skill_gate;
pub mod store;
#[cfg(feature = "testkit")]
pub mod testkit;

pub use action::{
    ActionSchema, ActionSpec, ProposedAction, ValidatedAction, ValidationError, A2A_ANCESTRY_ARG,
    A2A_DEPTH_ARG, IDEMPOTENCY_KEY_ARG,
};
pub use action_gate::{ActionApprovalDecision, ActionGateConfig, ActionPolicy};
pub use budget::{
    BudgetCheckpoint, BudgetConfig, BudgetLimitKind, BudgetLimits, BudgetStop, RunBudget,
};
pub use contract::{
    ConditionResult, ContractCondition, ContractReport, SuccessContract, Verification,
};
pub use cost::{ModelPrice, PricingTable};
pub use delegation_gate::{
    AgentDelegationDecision, DelegationDecisionOutcome, DelegationGate, DelegationRequestMeta,
};
pub use engine::{
    crash_point, is_a2a_action, ActionSource, AgentUsage, CircuitCheck, CompositeActionSource,
    DelegationLimits, ExecutionContext, ExecutionError, Kernel, KernelConfig, KernelError,
    PendingActionApproval, PendingDelegation, PendingDelegationInput, PendingSkillActivation,
    PlanResilience, ResilienceConfig, Run, RunConfirmation, RunEvent, RunEventEnvelope, RunMode,
    RunOptions, RunStatus, StepRecord, A2A_CONTINUATION_INPUT_NOTE, ACTIVATE_SKILL_ACTION,
    DEACTIVATE_SKILL_ACTION, FINISH_ACTION, MAX_PROGRESS_NOTES_PER_STEP,
};
pub use journal::{
    ConfirmationDecision, JournalEntry, PendingActivationFold, PendingApprovalFold,
    PendingDelegationFold, Usage,
};
pub use learned::{
    DistillReport, InMemoryLearnedStore, LearnedFact, LearnedPlan, LearnedProvenance, LearnedStore,
    LEARNED_LEVEL,
};
pub use oracle::{CallBound, Latitude, PolicyContext, PolicyError, PolicyOracle, Proposal};
pub use plan::{NodeType, Plan, PlanNode, ValidatedPlan};
pub use skill_gate::{
    ActivationAuthorization, ActivationRequestMeta, SkillActivationDecision, SkillGate,
};
pub use store::{InMemoryRunStore, PruneReport, RetentionPolicy, RunStore, StoreError};
