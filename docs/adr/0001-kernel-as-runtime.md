# ADR 0001: Deterministic kernel as the runtime, LLM as a constrained policy oracle

Status: accepted · Date: 2026-07-18

## Context

The prevailing pattern in agent orchestration makes the LLM the runtime: the
model's tool calls *are* the control flow. Orchestration reliability then
equals the model's tool-calling reliability, so a weak engine breaks the whole
system, and every engine swap risks rewriting the orchestration.

## Decision

Invert the relationship. A typed, deterministic state machine
(`kernel-core::engine`) owns control flow, retries, persistence and error
handling. The model is consulted only at explicit decision points through one
interface:

```
propose(context, action_schema) -> proposed_action
```

The model *proposes* an action drawn from a defined action space; the kernel
*validates and executes*. `ValidatedAction` has no public constructor other
than `ActionSchema::validate`, so unvalidated model output cannot reach the
executor by construction.

## Consequences

- Reliability degrades in decision *quality*, not in flow integrity, when the
  engine is weak (graceful degradation).
- The kernel is fully testable with a scripted mock oracle
  (`crates/kernel-core/tests/engine_test.rs`) — no model needed.
- Every model interaction costs one explicit decision point; open-ended
  "let the model run" autonomy is deliberately out of scope (see latitude,
  ADR 0003).
