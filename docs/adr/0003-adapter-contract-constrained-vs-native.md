# ADR 0003: Adapter contract — constrained decoding locally, native tool-calling remotely, kernel validation on top

Status: accepted · Date: 2026-07-18

## Context

Different engines guarantee action-shape correctness in different ways. A
small local model cannot be trusted to emit well-formed tool calls; frontier
APIs offer native tool-calling/structured outputs. The kernel needs one
contract that both can satisfy.

## Decision

The adapter contract is: **return a proposal drawn from the offered action
schema, or fail loudly** (`PolicyOracle::propose`). Each adapter satisfies it
with the strongest mechanism its engine offers:

- `local` (llama.cpp): the action schema is compiled to a **GBNF grammar**
  (`policy::gbnf`) and sent with the request; the *decoder* is constrained so
  a malformed envelope is unrepresentable. This is the
  "weak-but-reliable" path.
- `openrouter`: the action schema is presented as native **tools** in the
  chat-completions request; the proposal is read from the returned tool
  call. One adapter reaches many frontier models via the model string.

Regardless of adapter, **the kernel re-validates every proposal against the
JSON Schema** (`ActionSchema::validate`) before execution. The kernel trusts
no adapter — constrained decoding and native tool-calling are optimizations
for proposal *quality*, not the trust boundary.

A **latitude** knob (`constrained | standard | wide`) widens or narrows the
action space treatment per engine capability: tighter guidance and more
validation retries for weak engines, more freedom for strong ones. Same
kernel, different latitude.

## Consequences

- Engine swap = config change; no orchestration rewrite (ADR 0004).
- Validation failures become retry feedback to the model, not crashes.
- Grammar generation covers the schema subset the reference tools use;
  exotic JSON Schema features fall back to a generic JSON value rule (the
  kernel-side validation still enforces them).
