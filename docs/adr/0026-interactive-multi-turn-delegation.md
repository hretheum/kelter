---
provenance: ai_authored
provenance_metadata:
  agent: claude-fable-5
  session_date: 2026-07-21
  human_validated: false
  session_context: "P5 — interactive multi-turn delegation: an authorized delegation whose remote task enters input-required pauses the run durably (AwaitingDelegationInput) and resumes on a human's continuation, reconnecting to the same remote task; governed and durable across a restart"
---

# ADR 0026: Interactive (multi-turn) delegation

Status: accepted · Date: 2026-07-21 · Builds on: ADR 0009/0013 (A2A
out+in), ADR 0021/0023 (durable ask pause), ADR 0025 (drain-then-pause) ·
Part of: P5

## Context

Outbound A2A delegation (ADR 0009) was single-shot: the kernel sent a
self-contained sub-task and polled the remote task to a terminal state. If the
remote entered `input-required` — it needs more information to continue —
`poll_to_terminal` returned a hard error ("interactive continuation is not
supported in outbound delegation v1"). Real agents routinely ask a clarifying
question mid-task, so a whole class of delegations failed that should have
paused for a human answer.

The pause machinery already existed (ADR 0021 skill ask, ADR 0023 delegation
ask, ADR 0025 plan-mode drain-then-pause): journal a request, suspend the run,
resume from the journal after a human responds. The new piece is carrying an
`input-required` signal from deep inside the a2a `ActionSource::execute`
(where the poll happens) up to the engine, and re-dispatching the delegation
so it *reconnects* to the existing remote task rather than dialing a new one.

## Decision

**A distinct signal across the `ActionSource` boundary.** A new
`ExecutionError::DelegationInputRequired { agent, task_id, prompt }` is NOT a
failure — it is the a2a source telling the engine "the remote paused, needs
input." `poll_to_terminal` now treats `input-required` as a *stop* state
(returns the task) rather than an error; `execute` maps a fresh
`input-required` to this signal, extracting the remote's question (its
artifact text) as the prompt.

**A durable pause, mirroring the ask pauses.** The engine's step-mode execute
catches `DelegationInputRequired`, journals `AgentDelegationInputRequested
{step, agent, task_id, prompt}`, sets `RunStatus::AwaitingDelegationInput` +
`run.pending_delegation_input`, and returns. `unfinished_runs` includes the
new status, so a restart re-surfaces it (`still_awaiting_delegation_input`,
the twin of `still_awaiting_delegation_decision`) and never orphans it. The
authorization already happened (P2+P3 passed, the dial was made) — this is a
*separate* pause from `AwaitingDelegationDecision`, which gates whether to
dial at all.

> **Errata (2026-07-21, audit finding H-1):** the claim above — "a restart
> re-surfaces it... and never orphans it" — held for the `RunStore` trait
> contract and for `InMemoryRunStore`, but the production `SqliteRunStore`'s
> `unfinished_runs` query's `status IN (...)` list had drifted and did NOT
> include `awaiting_delegation_input`, so on the real (SQLite) backend a
> paused interactive delegation WAS silently orphaned across a restart. Found
> by the 2026-07-21 adversarial audit (`docs/audit/AUDIT-2026-07-21.md`,
> finding H-1) and fixed the same day: `crates/storage/src/lib.rs`'s query now
> includes the status, guarded by a new SQLite-backed regression test
> (`unfinished_runs_includes_every_non_terminal_status_sqlite`) that exercises
> every `RunStatus` variant against the real query string, not just the
> in-memory reference store. The claim above is accurate as of that fix.

**Resume reconnects, never re-dials.** A human's continuation arrives via
`POST /api/v1/runs/{id}/delegation-input {input}` →
`Kernel::provide_delegation_input`, which journals the input as a `Note` keyed
`A2A_CONTINUATION_INPUT_NOTE` (the resolving entry
`still_awaiting_delegation_input` looks for), clears the pause, and flips to
`Running`. On resume the delegation's journaled `Decision` replays (the model
is NOT re-queried), `execute` runs again, sees the existing `task_id` note AND
the continuation note, and sends the continuation to the SAME task
(`send_continuation`, a `SendMessage` carrying `taskId`), then polls to
completion.

**Continuation durability = the same at-most-reconnect guarantee as the
initial dial.** A `NOTE_CONTINUATION_SENT` marker is journaled right after the
continuation is dispatched, so a resume after the marker re-POLLS instead of
re-sending — exactly the role `NOTE_TASK_ID` plays for the initial send. The
same narrow crash-window (send done, marker not yet journaled → resume
re-sends) exists here as for the initial dial, and is accepted identically.

**`input-required` is a healthy circuit outcome.** The circuit-breaker
(ADR 0023) must NOT count an agent asking for input as a failure — it responded
and is waiting on us. `record_circuit_outcome` treats `input-required` like
`completed`.

## Governance & invariants (unchanged)

The pause changes WHEN a delegation finishes, never WHETHER it was allowed:
P2 (budget/depth/cycle) and P3 (trust gate) already ran before the dial;
SSRF-pinning still holds (the continuation goes to the same pinned origin);
the remote's continuation output is still a validated observation, never an
action. A malicious remote can stall (ask forever) but cannot escalate — see
the multi-round bound below.

## Scope (this slice) & deferred

- **Single continuation round.** One `input-required` → one human answer →
  terminal. If the remote asks AGAIN after a continuation, that is multi-round
  and is refused with a clean error (never a silent re-pause loop), so a
  hostile "ask forever" remote fails fast rather than deadlocking. Multi-round
  is a natural extension (round-indexed continuation notes) — deferred.
- **Step mode only.** A plan-mode a2a node that hits `input-required` fails the
  node (the generic execute-error path), consistent with ADR 0025's disclosed
  plan-mode-interactive deferral.
- **Retry/fallback dials** that hit `input-required` surface as errors, not
  pauses — only the primary first dial pauses (the resilience loop is for
  failures, not interactive turns).
- **No auto-timeout** on the input pause in this slice (unlike the ask pause's
  sweeper): a headless deployment that cannot answer leaves the run paused
  (cancelable), rather than auto-denying — an input request has no safe default
  answer. A timeout-to-fail sweeper is a possible follow-up.

## Consequences

- Delegations to agents that ask clarifying questions now work, durably.
- The frontend gains a third pause state to render (a delegation-input prompt),
  alongside the skill-activation and delegation-authorization prompts.
- `RunStatus::AwaitingDelegationInput` maps to `TASK_STATE_INPUT_REQUIRED` on
  the inbound A2A server view (Kelter-as-server), so an upstream caller sees a
  Kelter that is itself waiting on input — the states compose.
