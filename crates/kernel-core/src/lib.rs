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
pub mod engine;
pub mod oracle;
pub mod store;

pub use action::{ActionSchema, ActionSpec, ProposedAction, ValidatedAction, ValidationError};
pub use engine::{
    ActionSource, ExecutionError, Kernel, KernelConfig, KernelError, Run, RunEvent,
    RunEventEnvelope, RunStatus, StepRecord, FINISH_ACTION,
};
pub use oracle::{Latitude, PolicyContext, PolicyError, PolicyOracle};
pub use store::{InMemoryRunStore, RunStore, StoreError};
