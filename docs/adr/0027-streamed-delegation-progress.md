---
provenance: ai_authored
provenance_metadata:
  agent: claude-opus-4-8
  session_date: 2026-07-21
  human_validated: false
  session_context: "P5 M4 — streamed remote-delegation progress: an authorized delegation whose remote advertises capabilities.streaming is dialed with SendStreamingMessage and its SSE reply consumed frame by frame; intermediate updates surface as advisory, journaled, attributed progress on the timeline, never as validated actions; a mid-stream failure falls back to polling the already-created task"
---

# ADR 0027: Streamed remote-delegation progress

Status: accepted · Date: 2026-07-21 · Builds on: ADR 0009 (outbound A2A),
ADR 0016 (governed self-learning invariant), ADR 0022 (plan fan-out
branches), ADR 0026 (interactive delegation) · Part of: P5 (M4)

## Context

Outbound A2A delegation (ADR 0009, 0026) dialed a remote agent and then
`poll_to_terminal` looped `GetTask` every second until the remote task
reached a terminal (or `input-required`) state. Between the dial and the
result the timeline showed nothing but an indeterminate "waiting on remote
agent…" spinner — correct, but blind. A long-running remote could be three
steps into useful work and the operator had no way to see it.

The A2A protocol already answers this: `SendStreamingMessage` returns a
`text/event-stream` (SSE) of intermediate `TaskStatusUpdateEvent` /
`TaskArtifactUpdateEvent` frames before the final task. The question was how
to surface that stream **without weakening any of the kernel's invariants** —
the governed self-learning rule (ADR 0016) is explicit that an external
source may provide *retrieved context* but must NEVER authorize or influence
what the kernel executes. A hostile remote streaming garbage must be unable
to do anything worse than write advisory notes.

## Decision

**Streamed progress is advisory observation, off the ordered event stream,
gated on card capability, and a strict enhancement over polling.**

1. **Capability-gated, opt-in, non-breaking.** The client sends
   `SendStreamingMessage` only when (a) `A2aActionSource` streaming is on
   (`with_streaming`, default `true`; daemon `[delegation] stream_progress`)
   AND (b) the remote's Agent Card advertises `capabilities.streaming`. A
   card that does not advertise it takes the unchanged `SendMessage` +
   `GetTask` poll path — so the client never sends a streaming request to a
   peer that did not announce support, and can never accidentally create a
   second task there. Every existing (non-streaming) delegation behaves
   exactly as before.

2. **Advisory by construction — never a validated action.** Each non-final
   frame becomes an `ExecutionContext::record_progress(agent, task_id, state,
   note)` call. That does two things and only two: it journals a durable
   side-note (`a2a.progress.<ordinal>`) and, when a progress sink is attached,
   publishes an `AgentDelegationProgress` event. It never constructs a
   `ValidatedAction`, never touches the run's control flow, never feeds the
   model. The delegation's REAL result still flows through the identical
   terminal-state handling (`TASK_STATE_COMPLETED` → validated output,
   `input-required` → ADR 0026 pause, anything else → tool error) and the
   validation floor is untouched. Concretely: intermediate frames carry
   arbitrary remote text, and the completion test asserts that text lands
   ONLY as advisory notes + timeline events while the run's actual result
   comes from the terminal frame — the intermediate stream never becomes a
   validated action, never alters control flow, and (crucially) is never
   added to the model's `PolicyContext`, so an untrusted remote's mid-task
   chatter cannot re-enter the decision loop.

3. **Off the ordered stream (seq 0).** The run's live event `seq` is a
   monotonic per-execution counter the engine threads through its own emits;
   progress is emitted concurrently from inside `execute` (and, in plan mode,
   from several spawned nodes at once). Rather than force a shared atomic
   counter through the whole engine — a broad change to enable an advisory
   signal — progress rides envelope `seq` 0 and carries its own per-`(run,
   step)` `ordinal`. The client applies progress events immediately, bypassing
   the `seq <= lastSeq` gate and never advancing `lastSeq`; ordering and
   dedupe are by `ordinal`. Ordered events are wholly unaffected.

4. **Strict durability enhancement, never a regression.** The moment the
   remote task id first appears in the stream it is journaled (`NOTE_TASK_ID`)
   — the SAME durability pivot the poll path has — so a crash mid-stream, or a
   mid-stream transport failure/timeout/early-close, **falls back to POLLING
   the already-created task**, never to re-dialing. Exactly-once delegation is
   preserved. Streaming can only ever add lower-latency progress; if anything
   about it breaks, the dial degrades to the pre-ADR polling behaviour.

5. **Attributed, and branch-aware.** Progress carries the remote agent name
   and, in plan mode, the plan node's `branch` (ADR 0022) — so a fan-out's
   streamed lines land in the right swimlane. Step mode has no branch
   (`None`), unchanged.

## Consequences

- **New wire event.** `RunEvent::AgentDelegationProgress { step, agent,
  task_id, state, note, ordinal, branch }`. Additive; no existing event
  changed.
- **New `ExecutionContext` capability.** An optional `ProgressSink` (events
  sender + run_id + step + branch + ordinal counter), attached by the engine
  only at the three delegation dispatch sites (step-mode first dial,
  kernel-initiated retries/fallbacks, plan-mode nodes). A context without a
  sink still journals progress durably — only the live event is skipped — so
  tests and the WASM host are unaffected.
- **Frontend.** The timeline renders a per-step progress trail (StepCard,
  reused verbatim by the fan-out swimlanes) — an honest, indeterminate trace
  (`↳ note · state`), never a fabricated percentage or step count; the newest
  line pulses only while the step is still running. On a reconnect the journal
  replay does not reconstruct progress (it is stored as notes the replay path
  does not render), so a reconnected view may show fewer lines than a live
  one — a disclosed best-effort-live limitation consistent with the fan-out
  badge's (design/08 honesty convention).
- **Scope held to one round.** Progress is display + audit only; the model is
  not shown intermediate progress (it would blur the ADR 0016 line), and the
  streaming transport is used only for the initial dial — a resume always
  polls. Both are deliberate, disclosed boundaries, not oversights.

## Alternatives rejected

- **Feed progress to the model as context.** Rejected: it would put an
  untrusted remote's mid-task chatter into the decision loop, exactly the
  coupling ADR 0016 forbids.
- **Share the engine's `seq` counter (make it a shared atomic).** Rejected as
  disproportionate: a broad, invasive change to the whole engine's emit path
  to carry an advisory, best-effort signal. Seq 0 + `ordinal` keeps the change
  local and the ordered stream pristine.
- **Always attempt streaming, fall back on error.** Rejected: dialing
  `SendStreamingMessage` at a non-streaming peer risks creating a duplicate
  task before the error surfaces — a durability hazard. Card-capability gating
  removes the hazard entirely.
