---
provenance: ai_authored
provenance_metadata:
  agent: claude-fable-5
  session_date: 2026-07-21
  human_validated: true
  session_context: "P5 enabler — closing the ADR 0023 plan-mode delegation-ask gap: the concurrent admission loop (ADR 0022) drains in-flight siblings then durably pauses on AskHuman, with the per-run delegation budget staying single-writer-atomic and journal-durable across the pause and a restart"
---

# ADR 0025: Plan-mode delegation `AskHuman` — drain-then-pause

Status: accepted · Date: 2026-07-21 · Closes: the ADR 0023 plan-mode gap ·
Builds on: ADR 0022 (concurrent admission loop), ADR 0021 (durable ask
pause), ADR 0023 (agent trust/ask/resilience) · Enables: P5 interactive
multi-turn delegation and remote-progress streaming (deferred — see Scope)

## Context

ADR 0023 shipped the interactive `AskHuman` delegation pause in **step
mode**: when the trust gate wants a human to authorize a `delegate_to_*`
dial, the run journals `AgentDelegationRequested`, suspends as
`AwaitingDelegationDecision`, and resumes from the journal after a human (or
the timeout sweeper) decides. It explicitly did **not** implement this in
**plan mode**, where an `AskHuman` outcome was degraded to a refusal (the
node — and the plan — failed). The disclosed reason: the readiness-driven
concurrent admission loop (ADR 0022) has "no single well-defined *the run is
paused* moment when several nodes may be ask-worthy at once."

The P5 brief framed the enabler as making the plan-mode delegation budget
"cross-task-shareable — a real, non-trivial concurrency change." Reading the
actual code first (as the plan demanded) showed the premise needed
correcting, and the correction *is* this ADR's first decision.

## Decision

### 1. The budget stays single-writer — it is already atomic and durable

The per-run delegation budget (`max_delegations_per_run`, ADR 0022) does
**not** become a shared atomic counter, because it does not need to:

- **Atomic across concurrent branches already.** The plan orchestrator is the
  sole writer of the `a2a_attempts` counter, and it checks-and-increments
  *before* spawning any node's work. A race is structurally impossible — no
  two branches ever observe the same slot. Introducing a shared
  `AtomicU*`/mutex across branches would *replace a provably-safe
  single-writer design with shared mutable concurrency state*, strictly
  riskier for zero benefit.
- **Durable across a pause/restart already.** The counter is not persisted;
  on resume it is **re-seeded from the journal** — every a2a node that has an
  `Intent` or a `Receipt` counts. A restart mid-run reconstructs the exact
  budget from durable facts.
- **Per-run, not cross-run.** There is no global budget shared between
  separate runs in the design, and this ADR does not introduce one (that
  would be a different feature; flagged for the human, not built).

So the "enabler" is not a counter rewrite. The real work is the pause
semantics — decision 2.

### 2. Drain-then-pause

When a **fresh** a2a node's trust gate returns `AskHuman`, the admission loop:

1. records the ask in a single pending-ask holder and **stops admitting new
   nodes** (the same guard fail-fast already uses);
2. lets **already-in-flight siblings drain to their receipts** — the loop
   only exits once `in_flight` is empty;
3. then journals `AgentDelegationRequested`, sets `AwaitingDelegationDecision`
   + `pending_delegation`, and returns — suspending the run durably.

Draining before suspending is what keeps the resume budget seed **exact**:
every sibling that was dialed has a journaled `Intent`/`Receipt` by the time
the run pauses, so the journal-derived seed counts them and never
under-counts (which would let resume over-admit past the budget). A
concurrent sibling *failure* during the drain takes priority — the plan is
doomed anyway, so the ask is abandoned unjournaled and the plan finalizes as
failed.

Only **one** node's ask is surfaced at a time. With the loop frozen to new
admissions, a second ask-worthy node simply waits its turn on the next
resume — the single-decision-at-a-time contract step mode already has, which
is exactly the "no single paused moment" concern ADR 0023 raised, resolved by
*choosing* the first ask as the pause point.

### 3. Three-way gate + resume consumption

The plan-mode trust gate is now three-way, mirroring step mode: `Allow` →
dispatch; `Deny` → node (and plan) fail; `AskHuman` → drain-then-pause. On
resume, a node whose journal already carries a decided
`AgentDelegationRequested` consumes that decision instead of re-consulting
the gate ("replay is law"): `Deny` fails the node/plan; `AllowOnce` and
`AllowAlways` count one budget slot and dispatch normally (the latter also
elevating the agent's trust label, once). Headless deadlock is handled by the
same daemon-side timeout sweeper (deny/queue) ADR 0023 already ships — plan
mode reuses it unchanged.

### 4. Idempotent node-step recording

A clean pause persists `run.steps`, and the resume fast-path re-applies each
journaled receipt — so `record_node_step` became idempotent by node index (a
node already present in `run.steps` is not re-appended). This is exactly-once,
not lossy (node indices are unique), and also hardens ordinary crash-resume.

## Consequences

- Plan-mode delegations that require human authorization now behave like
  step-mode ones: a durable pause, not a silent failure. Governance is
  unchanged — P2 (budget/depth/cycle) still runs first, the trust gate
  second, SSRF-pinning still applies; the pause never bypasses any of them.
- The budget's atomicity and durability are now proven under concurrency
  *with* a pause and *across a restart* (tests below), not just asserted.

## Scope (honestly deferred — the rest of P5)

This ADR is the **enabler + plan-mode ask** slice. Built on the same durable
paused-waiting machinery, but **not** in this slice:

- **Interactive multi-turn delegation** (a delegation that pauses awaiting the
  *remote agent*, exchanging more than one turn). Needs an A2A protocol
  extension (multi-turn `input-required` task states) and a new durable
  waiting-on-remote state — a distinct backend slice.
- **Remote-progress streaming** (consuming A2A SSE so a remote agent's steps
  render in the timeline instead of an opaque lane). Needs streaming
  transport + journaled/attributed intermediate progress.
- **Frontend** (swimlanes showing streamed progress + multi-turn pause states;
  a plan-mode delegation-ask prompt). Depends on the two above.

Each is a substantial, independently-gateable slice; sequencing them behind
this enabler is deliberate, not an oversight.

## Alternatives considered

- **Snapshot-and-pause-immediately** (suspend the instant an `AskHuman` is
  seen, journaling in-flight siblings' intents pre-spawn so resume can
  reconnect them). More responsive UI, but it moves intent-journaling onto the
  orchestrator and complicates resume for mid-flight branches. Rejected for
  this slice in favor of drain-then-pause, which is provably-correct with no
  change to where intents are journaled. Revisitable if pause latency on a
  slow sibling becomes a real UX problem.
- **A shared cross-run/cross-branch atomic budget.** Rejected — see decision 1.
