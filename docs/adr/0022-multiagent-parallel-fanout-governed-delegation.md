---
provenance: ai_authored
provenance_metadata:
  agent: claude-sonnet-5
  session_date: 2026-07-20
  human_validated: false
  session_context: "Parallel plan fan-out (P1) + governed delegation limits (P2) — concurrent execution of independent plan-mode nodes, and a per-run delegation budget/depth/cycle guard the kernel enforces the same way it enforces the action schema. Extends ADR 0009 (governed durable A2A) and ADR 0010 (plan mode); mirrors the ADR 0020/0021 skill-gate Deny pattern for the governance-refusal shape."
---

# ADR 0022: Parallel plan fan-out + governed delegation (budget, depth, cycles)

Status: accepted · Date: 2026-07-20 · Extends: ADR 0009 (governed durable
A2A), ADR 0010 (plan mode), ADR 0013 (A2A inbound)

## Context

Plan mode (ADR 0010) already expresses independence between DAG nodes —
`ValidatedPlan` computes a topological order, and `deps_of(index)` exposes
each node's dependencies. What it didn't do until now: execute independent
nodes CONCURRENTLY. `execute_plan` was a strict sequential `for` loop,
`.await`ing one node to completion before starting the next — a fan-out of
independent tool/A2A/review work ran no faster than the slowest possible
serialization of it, even when nothing depended on anything else.

Parallelism makes a second, previously-optional gap load-bearing:
delegation had no budget, no chain-depth limit, and no cycle detection.
Sequentially, a runaway A→B→A loop was already possible in principle but
slow to manifest and easy to notice; concurrently, the same runaway is
easier to trigger and harder to see coming. The source plan
(`docs/multi-agent-expansion-plan.md`, P1+P2) states this explicitly:
parallelism and governance ship TOGETHER, not as two independent features,
because shipping only the first without the second changes the risk
profile of the network a Kelter instance can be embedded in.

## Decision

### The concurrency-confined-to-validated-plan invariant

The model still proposes exactly ONE plan; `crate::plan::validate` still
structurally validates the whole DAG (cycles, unknown actions, node-type
mismatches) BEFORE any node executes — completely unchanged from ADR 0010.
The only thing that changed is the EXECUTOR: `execute_plan`
(`crates/kernel-core/src/engine.rs`) is now a readiness-driven admission
loop instead of a `for` loop. A single orchestrator (`&mut Run`, never
shared across concurrent tasks — no locking needed) admits every node whose
dependencies are already satisfied, dispatches each through
`tokio::spawn(dispatch_plan_node_execution(...))`, and applies each
completion's bookkeeping (`record_node_step`, `RunEvent::Executed`,
`node_outputs`) sequentially as results arrive via
`futures::stream::FuturesUnordered`. Every node — concurrent or not — still
goes through `ActionSchema::validate` exactly as before; the model never
gets a path to spawn unvalidated work. This is the SAME trust boundary ADR
0010 established, just executed with more concurrency inside it.

Proof, not just reasoning: `dependent_node_waits_for_its_dependency`
(`crates/kernel-core/tests/parallel_plan_test.rs`) asserts a downstream
node cannot even be ADMITTED until its dependency's result is in
`node_outputs` — a 150ms delay on the upstream node makes this
deterministically assertable (not timing-flaky), and
`parallel_independent_nodes_run_concurrently` proves real wall-clock
concurrency (3 independent 250ms-delayed A2A nodes complete in ~250-350ms,
not the ~750ms a sequential executor would need).

### Split: governance-and-admission (orchestrator) vs the durable tail (spawned)

`execute_plan_node` (the old sequential per-node function) was split along
its existing natural seam: everything through building the node's
`ValidatedAction` and delegation governance (needs `&mut Run`/orchestrator
state) stays on the orchestrator, synchronous with admission; the durable
Intent → execute → Receipt tail became a free function,
`dispatch_plan_node_execution`, taking no `&mut Run` — only owned/`Arc`
clones (`run_id`, `step`, the node, `store`, `tools`, `oracle`, and the
run's fixed `a2a_depth`/`a2a_ancestry`) — so the future is `'static` as
`tokio::spawn` requires. Per-node durability (idempotency keys, A2A
reconnect via `ExecutionContext::note`) is unchanged — it was already
step-indexed, so it composes with concurrent dispatch for free.

### Governed delegation: budget, depth, cycle detection

`KernelConfig` gains `delegation: DelegationLimits { max_delegations_per_run,
max_depth }` (defaults 10 / 3 — see "Defaults, not derived" below). `Run`
gains two fixed-at-creation fields mirroring the `active_skills` pattern:
`a2a_depth: u32` (this run's own position in an inbound delegation chain)
and `a2a_ancestry: Vec<String>` (origins of every Kelter hop that led here).
A new `Kernel::authorize_delegation(&self, run, attempted, target_origin)`
checks all three limits and returns `Result<(), String>`; on `Err`, the
kernel emits a `RunEvent::Rejected` (branch-tagged for a plan node),
journals a failed `StepRecord` via `record_node_step`, and fails the whole
plan — mirroring the ADR 0020/0021 skill-gate `Deny` pattern exactly: a
governance refusal is always loud (emitted + recorded), never a silent
skip, and it fails the run the SAME way an ordinary node failure already
does (ADR 0010's fail-fast, no-adaptive-replanning stance — a governance
refusal gets no special-cased behavior). Step mode gets the identical check
at its existing fresh-proposal decision point (mirroring where
`ACTIVATE_SKILL_ACTION` is gated) — it does NOT run on the journal-replay
path, for the same reason the skill-activation gate doesn't: replay is law,
the decision was authorized once.

**One deliberate refinement over the literal source-plan pseudocode:** the
budget check does not recompute "attempts so far" from `run.steps` on every
call. In the concurrent plan executor, several A2A nodes can be admitted in
the SAME admission pass before any of them completes and appends a
`StepRecord` — recomputing from `run.steps` would under-count those
in-flight siblings and let more delegations through than the budget
allows. The admission loop instead threads an explicit local counter,
incremented the moment each A2A node is CONSIDERED (admitted or refused),
strictly before any `tokio::spawn` — this is the concurrency-safety-critical
point: the check-and-increment happens single-threaded, in program order,
before any concurrent work starts, so a race on the counter is structurally
impossible. Proven by `delegation_budget_exhausted_is_governed_rejected`: a
3-node all-independent plan with `max_delegations_per_run = 2` admits
exactly 2, refuses the 3rd with `"delegation budget exhausted (2/2 this
run)"`, in a SINGLE admission pass (the scenario that would have broken a
`run.steps`-recompute approach). Depth and cycle checks need no such
counter — `run.a2a_depth`/`run.a2a_ancestry` are fixed for a run's whole
lifetime, so they can't be under- or over-counted by concurrent admission.

A node with a DANGLING Intent (in-flight when a crash happened) skips the
governance CHECK entirely on resume — re-checking could spuriously refuse
an already-in-flight, maybe already-fired-over-the-wire delegation and
orphan it, the same "replay is law" reasoning as everywhere else in this
codebase. Lineage VALUES for injection, however, are always recomputed
fresh, unconditionally, right before any A2A dispatch (fresh or reconnect)
— this is safe because they're a pure function of the run's own fixed
depth/ancestry plus the target's origin, never of in-flight budget state,
and the a2a crate only actually reads them when a genuinely fresh
`SendMessage` goes out (a reconnect polls an existing remote task and never
re-sends).

### Ancestry tokens are origins, not display names

`A2aClient::allowed_origin()` (the same `scheme://host:port` string already
used for SSRF pinning) is the ancestry token — not the agent's manifest
display name. Display names are locally chosen per deployment and not
globally unique; origins are the right identity for CROSS-DEPLOYMENT cycle
detection, and the SSRF-pinning machinery already computes them, so this
reuses an existing trust primitive rather than inventing a second identity
system.

### Lineage rides the actual A2A wire protocol — no side channel

Depth/ancestry are injected into the VALIDATED action's execution-copy
arguments AFTER schema validation — reserved keys
`__a2a_depth`/`__a2a_ancestry` (`kernel_core::A2A_DEPTH_ARG`/
`A2A_ANCESTRY_ARG`), mirroring `inject_idempotency_key`'s precedent exactly
(`ValidatedAction::inject_a2a_lineage`, `action.rs`) — never part of what
the model proposes or what the schema checks.
`A2aActionSource::execute` reads them and passes them to
`A2aClient::send_message(text, lineage)`, which embeds
`{"kelter_a2a_depth": depth, "kelter_a2a_ancestry": [...]}"` as a
`metadata` object on the outbound A2A `message` — additive to the wire
shape (an unrecognized JSON field is expected to be ignored by any
compliant peer, not a protocol break).
`a2a::server::message_lineage` (mirroring `message_text`'s exact style)
parses it back on the inbound side, defaulting to `(0, [])` when absent —
correctly treated as "the start of a fresh chain from this server's
perspective," not evidence of anything to refuse.
`crates/api/src/lib.rs`'s `a2a_send` threads the parsed lineage into a new
`Kernel::create_run_with_lineage(task, depth, ancestry)` (mirrors
`create_run`, which is now a thin wrapper around it with `depth=0,
ancestry=[]`).

Proof: `delegation_lineage_rides_the_outbound_a2a_metadata`
(`crates/a2a/tests/a2a_client_test.rs`) captures the actual outbound
`SendMessage` request body against a mock A2A server and asserts the
`metadata` object carries the correct depth/ancestry.
`delegation_depth_limit_enforced_on_inbound_seeded_run`
(`crates/kernel-core/tests/parallel_plan_test.rs`) proves the INBOUND
half: a run seeded via `create_run_with_lineage` at `a2a_depth = 3`
(matching `max_depth`) is refused with `"delegation depth limit exceeded
(would reach 4, max 3)"` the moment it tries one more delegation — the
same code path an inbound `a2a/message-send` carrying
`kelter_a2a_depth: 3` would seed through `server.rs::message_lineage` +
`a2a_send`.

### Durable multi-branch resume — proven, not just claimed

Per-node durability was already fully self-contained and step-indexed
before this ADR (`Intent`/`Receipt`/`Note` keyed by the node's own index).
"Reconnect to ALL in-flight branches on resume, never re-delegate any of
them" falls out of the SAME readiness-driven admission loop used for fresh
execution — on a fresh `execute_plan` call after a crash, every node with
a `Receipt` short-circuits inline; every node with a dangling `Intent`
whose deps are satisfied is admitted (skipping the governance re-check, as
above) and re-dispatched; if MULTIPLE nodes are simultaneously
dangling-intent, they are ALL admitted in the SAME first admission pass
(deps-satisfied is independent of dangling-intent-ness) and reconnect
CONCURRENTLY. This is not asserted by inspection: 
`durable_multi_branch_resume_reconnects_all_in_flight`
(`crates/daemon/tests/multiagent_durable_resume_test.rs`) stands up TWO
real mock A2A HTTP servers (not a mock `ActionSource`), drives a 2-branch
plan against a real `SqliteRunStore`, waits until BOTH branches have
journaled their remote task id (both durably in-flight, `TASK_STATE_WORKING`
held open by the mock), aborts the in-flight `execute_run` task (simulated
crash — no Receipt for either branch), reopens the SAME SQLite file in a
fresh `Kernel`/`A2aActionSource`, calls `execute_run` again, and asserts
the run completes AND each mock agent's `SendMessage` counter is still
exactly 1 (no re-delegation) across both sessions combined.

## Honest limitations, stated plainly

**Determinism, stated exactly once, not inflated.** The kernel's DECISIONS
— which plan is accepted, which nodes are admitted, what limits apply, what
gets journaled — remain deterministic and reproducible from the journal,
unchanged by this ADR. REMOTE EXECUTION was always nondeterministic
(network timing) even under the old sequential executor; parallelism does
not add a new CATEGORY of nondeterminism, it only lets several
already-nondeterministic remote calls overlap in wall-clock time. This ADR
does not claim more than that.

**Depth/cycle enforcement is a Kelter-to-Kelter guarantee, not a
cross-protocol one.** Only a chain of Kelter instances can actually ENFORCE
`kelter_a2a_depth`/`kelter_a2a_ancestry` — a third-party A2A agent in the
middle of a chain has no obligation to honor or forward an unrecognized
metadata field, and is exactly as opaque to depth/cycle tracking as it
already was to observability before this ADR. This is a real, disclosed
scope boundary, not a bug: the guarantee holds across however much of a
delegation chain is Kelter-to-Kelter, and stops being enforceable the
instant a non-Kelter hop sits in the middle.

**No new concurrency throttle beyond `MAX_NODES` (already 32, `plan.rs`)
and the delegation budget.** The source plan does not ask for a separate
"max concurrent branches" cap distinct from delegation governance, and none
was added — a deliberate non-goal, not an oversight.

**Defaults are reasonable, not derived — flagged for sign-off.**
`max_delegations_per_run = 10` and `max_depth = 3` are sane-sounding
numbers, not the output of any measurement or capacity analysis. They are
presented here as the honest starting point for Eryk's sign-off, not as
principled derivations — the daemon config (`[delegation]` section,
mirroring `[skills]`'s pattern) makes both operator-tunable per
deployment.

## Consequences

- Fan-out plans (independent tool/A2A/review work) now complete in
  wall-clock time close to the SLOWEST branch, not the sum of all branches
  — proven with real timing, not estimated (see
  `parallel_independent_nodes_run_concurrently`'s evidence-log entry).
- Every `delegate_to_*` attempt a run makes — admitted or refused — is
  now governed by a hard, journaled/emitted limit, closing the "nothing
  stops A→B→A" and "nothing stops 200 delegations" gaps the source plan
  named as the actual differentiator (P2, "the killer feature, not the
  fan-out itself").
- `RunEvent::Proposed`/`Rejected`/`Executed` gain `branch: Option<String>`
  (the plan node's own id; `None` for ordinary step-mode events and the
  plan-level proposal event) — a purely additive field; `crates/api`
  needed zero code changes since `stream_events` already serializes
  whatever `RunEvent` is verbatim (confirmed by grep — no hand-written JSON
  shape duplicates `RunEvent`'s fields anywhere in the api crate).
- `Run` gains `a2a_depth`/`a2a_ancestry` (fixed at creation, `#[serde(default)]`
  so existing persisted runs deserialize with depth 0 / empty ancestry — a
  correct default, since a pre-ADR-0022 run was, definitionally, not part
  of a tracked chain).
- `ActionSource` gains a defaulted `a2a_origin(&self, action) -> Option<String>`
  method — additive and non-breaking for every existing implementor;
  `A2aActionSource` overrides it, `CompositeActionSource` forwards to
  whichever source declares the action.

## Evidence

See `docs/evidence/multiagent-fanout.log` (backend section) for: real
measured parallel-vs-sequential timing, each governance-refusal transcript
with its exact reason string, the durable-multi-branch-resume transcript,
and a sample `branch`-carrying event JSON.
