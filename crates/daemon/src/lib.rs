//! Library surface of the `kelter` daemon binary, split out so pieces of its
//! wiring (currently: the `ask`-activation timeout sweeper) are directly
//! unit-testable rather than only reachable via the running binary.

pub mod agents_controller;
pub mod approval_sweep;
pub mod config;
pub mod delegation_sweep;
pub mod skill_sweep;
pub mod skills_controller;
