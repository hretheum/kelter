---
provenance: ai_authored
provenance_metadata:
  agent: claude-opus-4-8
  session_date: 2026-07-19
  human_validated: false
  session_context: "Phase 7: A2A inbound — Kelter as an A2A server. Agent Card + message/send + tasks/get + tasks/cancel, bearer auth, and the crux: an inbound task IS a durable kernel run, so tasks/get is journal-backed and survives a server crash. Closes A2A out+in."
---

# ADR 0013: A2A inbound — Kelter as an A2A server

Status: accepted · Date: 2026-07-19 · Realizes: ADR 0008 (`agent` capability, inbound direction) · Completes: ADR 0009 (outbound) · Scope: INBOUND only

## Context

ADR 0009 made Kelter an A2A **client** (outbound delegation). This ADR makes
Kelter an A2A **server**: it publishes an Agent Card and accepts delegated
tasks from other agents. That closes the A2A loop (out + in) and, more
importantly, lets a remote delegator inherit Kelter's two defining properties —
**governance** (every action validated + journaled) and **durability** (a task
survives a crash). A delegated task is not a side channel around the kernel; it
is an ordinary kernel run wearing an A2A envelope.

Two things make an inbound server dangerous if done naively, and both are the
point:

1. **It executes untrusted input from the network.** A task from a remote
   caller must go through the same kernel validation as any run — no privileged
   path, no shortcut.
2. **It is an open door.** An anonymous, unauthenticated task-executing server
   is a footgun (a remote code-execution surface). Auth is not optional.

Explicitly out of scope (separate increments): SSE streaming (`message/stream`),
push notifications / webhooks, per-tool A2A skills, and exposing plan mode over
A2A. Kept deliberately narrow: `message/send` + `tasks/get` + `tasks/cancel` +
auth.

## Decision

**The design crux — an inbound task IS a durable run.** The A2A `task.id` is
the Kelter `run_id`. There is **no separate task store**: `message/send` calls
`Kernel::create_run` (durable, journaled from creation, ADR 0007) and spawns
execution; `tasks/get` reads the run store by id and maps the run status onto
the A2A task state. Because the run is durable, this buys crash-durability for
free:

- A crash mid inbound-task leaves a `Pending`/`Running` run in the journal.
- On restart the daemon's existing resume path (`unfinished_runs` → `execute_run`)
  replays the journal and finishes the run **exactly once** — the model is not
  re-queried, side effects are not repeated (idempotency keys / receipt gate).
- `tasks/get` by the same id keeps working across the crash, now reflecting the
  resumed/completed run.

This is the differentiator: a remote delegate gets a task that survives the
executor crashing, with no bespoke task-recovery machinery — it reuses Phase 3
wholesale. (Failure mode considered: if the mapping could not be made
journal-backed, the honest fallback was an inbound task that dies *explicitly*
on crash rather than faking continuity. It was not needed — the run store is
already the durable task store.)

**Governance is inherited, not re-implemented.** The inbound task text is
untrusted input fed to `create_run`; every action the model then proposes goes
through the same kernel validation and journal as a locally-submitted run.
There is no new code path that bypasses the kernel.

**Auth is mandatory (the security surface).** The A2A JSON-RPC endpoint
(`/a2a/rpc`) requires the API bearer token; an anonymous task call is refused
with `401` before any method runs. Only Agent Card **discovery**
(`/.well-known/agent-card.json`) is unauthenticated — a client must be able to
learn the auth scheme before it can authenticate. The card declares the scheme
explicitly (`securitySchemes.bearer` = HTTP bearer, `security` marks it
globally required). The caller is **attributed** in the run's journal (a
`Note` recording the caller label + that a bearer authorised it), the inbound
mirror of ADR 0009's per-agent outbound accounting.

**Wire shape follows the live A2A v1.0 spec** (verified for this phase; see
BUILD-EVIDENCE Phase 7). The JSON-RPC method names are the v1.0 proto-derived
`SendMessage` / `GetTask` / `CancelTask`; the pre-1.0 JSON-RPC spellings
`message/send` / `tasks/get` / `tasks/cancel` are **also accepted**, so both
client generations interop. Task states are the canonical `TASK_STATE_*` enum;
the state map is `Pending→SUBMITTED`, `Running→WORKING`, `Completed→COMPLETED`,
`Failed→FAILED`, `Canceled→CANCELED`. The Agent Card emits both `interfaces`
(v1.0) and `supportedInterfaces` (the field Kelter's own 4b client reads) with
identical content.

**`tasks/cancel` is cooperative and durable.** A new terminal `RunStatus::Canceled`
is added. Cancel stops the in-flight execution (aborting the spawned task this
process owns), then writes a `Terminal { Canceled }` journal entry and sets the
run canceled — so the decision survives a restart (the resume path finalises
from that terminal entry instead of re-executing) and `unfinished_runs` never
resumes it. A cancel on an already-terminal task is a no-op returning its state
(A2A permits a cancel to be refused/too-late).

**Reuse over rebuild.** The server-side wire helpers live in the existing
`a2a` crate (`a2a::server`) — Agent Card construction, method normalization,
run-status→task-state mapping — transport-agnostic and unit-tested. The HTTP
surface is mounted in the existing `api` crate behind the same bearer auth as
the rest of the API; no new server, no new port, no new auth mechanism.

**Authenticated outbound, for the loop.** To let one Kelter delegate to
another Kelter's (authenticated) inbound endpoint, the 4b outbound client gains
an **optional** per-agent bearer token, supplied via an env-var name in the
manifest (`[agent] token_env`) — the token itself never touches the tree
(secrets are env-only). An open endpoint (the 4b toy target) still works with
no token.

## Consequences

- The A2A seam is now bidirectional: Kelter delegates out (ADR 0009) and
  accepts delegations in, both governed and durable, over one hand-rolled,
  dep-light client/server pair (no A2A SDK in the trust path — the same posture
  as ADR 0009).
- A remote delegator inherits durability: proven by crashing the server
  mid inbound-task and watching `tasks/get` reconnect to the resumed run with
  the side effect applied exactly once (`scripts/smoke-a2a-inbound.sh`).
- Interop is real, not private: a generic A2A client (urllib, no Kelter code,
  no SDK) resolves the well-known Agent Card and drives a task to completion;
  and a Kelter↔Kelter loop runs on localhost (A delegates to B, authenticated).
- The inbound endpoint is closed by default to anonymous callers; the only
  unauthenticated route is card discovery, which leaks nothing but the auth
  scheme and capabilities.
- Deferred, honestly: streaming (`message/stream`), push/webhooks, A2A skills
  per tool, and plan-mode-over-A2A. Interactive inbound states
  (`input-required`) are likewise not modelled — an inbound task is
  fire-and-durably-complete.
