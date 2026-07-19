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
pub mod store;

pub use action::{
    ActionSchema, ActionSpec, ProposedAction, ValidatedAction, ValidationError, IDEMPOTENCY_KEY_ARG,
};
pub use cost::{ModelPrice, PricingTable};
pub use engine::{
    crash_point, is_a2a_action, ActionSource, AgentUsage, CompositeActionSource, ExecutionContext,
    ExecutionError, Kernel, KernelConfig, KernelError, Run, RunEvent, RunEventEnvelope, RunMode,
    RunStatus, StepRecord, FINISH_ACTION,
};
pub use journal::{JournalEntry, Usage};
pub use learned::{
    DistillReport, InMemoryLearnedStore, LearnedFact, LearnedProvenance, LearnedStore,
    LEARNED_LEVEL,
};
pub use oracle::{Latitude, PolicyContext, PolicyError, PolicyOracle, Proposal};
pub use plan::{Plan, PlanNode, ValidatedPlan};
pub use store::{InMemoryRunStore, PruneReport, RetentionPolicy, RunStore, StoreError};
