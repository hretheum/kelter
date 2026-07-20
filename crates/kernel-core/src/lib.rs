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
//! - [`store`] — run persistence trait + in-memory implementation.

pub mod action;
pub mod cost;
pub mod engine;
pub mod journal;
pub mod learned;
pub mod oracle;
pub mod plan;
pub mod redact;
pub mod skill_gate;
pub mod store;

pub use action::{
    ActionSchema, ActionSpec, ProposedAction, ValidatedAction, ValidationError, A2A_ANCESTRY_ARG,
    A2A_DEPTH_ARG, IDEMPOTENCY_KEY_ARG,
};
pub use cost::{ModelPrice, PricingTable};
pub use engine::{
    crash_point, is_a2a_action, ActionSource, AgentUsage, CompositeActionSource, DelegationLimits,
    ExecutionContext, ExecutionError, Kernel, KernelConfig, KernelError, PendingSkillActivation,
    Run, RunEvent, RunEventEnvelope, RunMode, RunStatus, StepRecord, ACTIVATE_SKILL_ACTION,
    DEACTIVATE_SKILL_ACTION, FINISH_ACTION,
};
pub use journal::{JournalEntry, PendingActivationFold, Usage};
pub use learned::{
    DistillReport, InMemoryLearnedStore, LearnedFact, LearnedProvenance, LearnedStore,
    LEARNED_LEVEL,
};
pub use oracle::{Latitude, PolicyContext, PolicyError, PolicyOracle, Proposal};
pub use plan::{Plan, PlanNode, ValidatedPlan};
pub use skill_gate::{
    ActivationAuthorization, ActivationRequestMeta, SkillActivationDecision, SkillGate,
};
pub use store::{InMemoryRunStore, PruneReport, RetentionPolicy, RunStore, StoreError};
