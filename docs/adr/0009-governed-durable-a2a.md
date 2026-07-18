---
provenance: ai_authored
provenance_metadata:
  agent: claude-fable-5
  session_date: 2026-07-18
  human_validated: false
  session_context: "Phase 4b: governed, durable outbound A2A — the agent capability as a remote endpoint the kernel dials; network enforcement + SSRF allowlist; durable delegation via journaled task id"
---

# ADR 0009: Governed, durable outbound A2A

Status: accepted · Date: 2026-07-18 · Realizes: ADR 0008 (`agent` capability) · Scope: OUTBOUND only

## Context

ADR 0008 reserved the `agent` capability. Phase 4b implements it for the
outbound direction: Kelter delegating sub-tasks to remote agents over A2A
(Agent-to-Agent protocol v1.0.0). Two things make an `agent` extension
different from tool/memory extensions, and both are the point:

1. It is a **remote endpoint**, not a spawned subprocess — so the kernel is
   the A2A *client*, and the network egress is the kernel's own. That means
   the `network` permission ADR 0008 could only *record* can now be
   *enforced* here (closing that honest-deferral for this seam).
2. Delegation creates remote state (a task) — so "durable execution" (ADR
   0007) must extend across a process boundary: a crash mid-delegation must
   reconnect, not re-delegate.

Explicitly out of scope (separate increments): INBOUND (Kelter as an A2A
server / exposing Agent Cards), transport-level sandboxing of subprocess
extensions, and non-JSON-RPC A2A transports.

## Decision

**An `agent` extension declares a remote endpoint.** Its manifest carries an
`[agent] card_url` (the A2A Agent Card) and MUST request `network` — an
agent manifest without it is refused at startup. The registry routes it to
the A2A client, spawning nothing.

**Delegation is an ordinary kernel action.** Each remote agent appears in
the action space as `delegate_to_<name>(task)`. The model proposes it; the
kernel validates and authorizes (agent registered, `network` granted); only
then does the kernel's A2A client dial. The remote answer returns as a
**validated observation** and is journaled with the agent's identity.

**Egress governance (the `network` enforcement + SSRF allowlist).** Because
the client is the kernel's:

- The client is **pinned to the origin** (scheme+host+port) of the
  manifest-declared `card_url`. The Agent Card's advertised service endpoint
  is checked against that origin and refused if it differs — neither the
  model nor a hostile card can redirect the dial elsewhere (e.g. to a
  cloud metadata IP). HTTP redirects are disabled for the same reason.
- Every request carries `A2A-Version: 1.0`; responses are parsed strictly
  and an **unknown task state is rejected** as a protocol violation — a
  lying agent cannot inject an unvalidated observation into the run (the
  same trust posture as the Phase 4a memory schema gate).

**Durable delegation (the money shot).** A2A's `GetTask(id)` re-fetches a
task by id after a disconnect — the reconnect primitive. The client uses it
via the journal: the created task id is written with
`ExecutionContext::record_note` (a new `Note` journal entry) **the instant
the task exists**, before polling. On resume, if the step's journal already
holds the task id, the client **reconnects with `GetTask` and never calls
`SendMessage` again**. Delegation is exactly-once across a kill-and-resume;
proven by `scripts/smoke-a2a.sh` crashing at `after_a2a_create`.

**Client: minimal, own.** A2A's JSON-RPC binding needs two methods
(`SendMessage`, `GetTask`) plus card resolution — a small, dep-light client
(`reqwest` + `serde`, no A2A SDK). The official Rust SDK (`a2a-client-lf`,
released the day before this work) is a valid future swap, but a hand-rolled
client keeps the egress trivially wrappable in the origin-pin/SSRF gate and
avoids a fast-moving 0.x dependency in the trust path. (The Python **toy
target** likewise avoids the SDK — a ~90-line Starlette server.)

**Per-agent accounting.** `ExecutionContext::attribute_agent` records remote
wall-time; the kernel aggregates delegation count + remote ms per agent onto
`run.agent_usage`, extending Phase 3's per-run accounting and surfaced via
the runs API.

**Controlled degradation.** A remote timeout, failure state, or interactive
(`input-required`/`auth-required`) state makes the delegation step **fail
explicitly** (fed back to the policy as a step error), never a silent
success.

## Consequences

- The `agent` seam is real for outbound; 4c (plan mode) and future inbound
  work build on the same registry routing.
- The kernel now makes outbound network calls it fully controls — the
  narrow, enforceable case. Subprocess extensions (tool/memory) **still**
  have un-sandboxed egress; that is a different seam and remains staged
  (container `--network none` / WASM), documented honestly, not simulated.
- `CompositeActionSource` merges the MCP tool substrate and the A2A
  delegation source into one action space; the kernel's validation,
  journaling and durability apply uniformly to local tools and remote
  delegations.
- Interactive remote tasks are unsupported in v1 (delegation is
  fire-and-collect); resuming into a human-in-the-loop remote task is a
  later increment.
