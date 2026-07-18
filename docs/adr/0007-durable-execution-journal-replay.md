---
provenance: ai_authored
provenance_metadata:
  agent: claude-fable-5
  session_date: 2026-07-18
  human_validated: false
  session_context: "Phase 3: durable execution design (journal/replay + idempotent tool boundaries)"
---

# ADR 0007: Durable execution — journal/replay with idempotent tool boundaries

Status: accepted · Date: 2026-07-18

## Context

Until Phase 3 the kernel was deterministic but ephemeral: a crash lost every
in-flight run. A runtime you trust must resume interrupted work — and resume
it *correctly*, which is where most durable-execution attempts fail in two
specific ways: they re-query the nondeterministic model during replay
(making replay non-deterministic), or they re-fire side effects (making
resume worse than the crash). The patterns here — deterministic journal +
replay, idempotency keys at effect boundaries — are well-established in
durable-execution systems; Kelter implements its own dependency-light
version rather than adopting a heavyweight engine (single-binary ethos).

## Decision

**Journal, then act.** Every run appends an ordered, durable journal
(SQLite by default):

- `Decision {step, action, arguments, usage}` — written the moment a model
  proposal passes kernel validation, *before* anything acts on it.
- `Intent {step, action, arguments, idempotency_key}` — written before a
  tool executes.
- `Receipt {step, ok, result}` — written after it executed.
- `Terminal {status, output, error}` — written before the run record is
  finalized.

**The replay law: model calls are journaled at first execution and never
re-queried.** On resume, a journaled `Decision` is replayed (re-validated
against the current schema, then used); the model is consulted only for
steps the journal has never seen. This is what makes replay deterministic
despite a nondeterministic LLM — memoize-once, replay-forever.

**Side effects live between Intent and Receipt.** On resume:

- `Receipt` present → the step is done; record it, never re-execute.
- No `Intent` → the effect never started; execute normally.
- `Intent` without `Receipt` — the ambiguous crash window — resolves by
  tool contract: tools that declare an `idempotency_key` parameter in their
  schema are re-dispatched with the **journaled** key and must dedupe
  (kernel-generated key `run_id:step`; the bundled ledger tool makes the
  appended line itself carry the key, so effect and dedupe marker are one
  atomic write). Tools without the contract are **not** re-executed: the
  run fails with an explicit "manual check required" error. Exactly-once or
  honest-failure — never silent duplication.

**Resume is automatic.** On startup the daemon scans for non-terminal runs
and resumes each from its journal.

A `KELTER_CRASH_POINT` test hook aborts the process at either boundary
(`after_intent:<tool>` / `after_execute:<tool>`) so the crash windows are
exercised by real kill-and-resume smokes, not just unit tests.

## Consequences

- Kill the daemon mid-run and restart it: the run completes, completed
  steps don't repeat, the model isn't re-billed for made decisions, and a
  keyed side effect lands exactly once (proven by
  `scripts/smoke-durability.sh`).
- The journal doubles as the audit trail and the per-run token/cost
  accounting record (`Decision.usage`, aggregated on the run).
- Tool authors that mutate state must accept `idempotency_key` to be
  resume-safe across the ambiguous window; read-only or deterministic
  overwrite tools declare it and may ignore the value.
- Replay validates journaled decisions against the *current* action schema;
  changing tool configuration between crash and resume fails the run
  explicitly instead of executing against a different contract.
