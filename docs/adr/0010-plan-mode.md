---
provenance: ai_authored
provenance_metadata:
  agent: claude-fable-5
  session_date: 2026-07-18
  human_validated: false
  session_context: "Phase 4c: plan mode — the model proposes a validated durable DAG; the kernel validates it before execution and runs it durably by composing existing seams"
---

# ADR 0010: Plan mode — validated, durable DAG

Status: accepted · Date: 2026-07-18 · Builds on: ADR 0001 (action space), ADR 0007 (journal), ADR 0008 (registry), ADR 0009 (A2A)

## Context

Step mode (Phases 1–4b) has the model pick one action at a time. Plan mode
lets the model propose the whole shape up front — a DAG of typed nodes — so
the kernel can validate the structure before running anything and execute it
durably. The requirement is composition, not a new execution surface: a plan
node must run through an *existing* seam and inherit its governance.

The crux is the durable-execution model: how to journal a plan so a
crash-resume is deterministic and the model is never re-queried about a plan
it already proposed.

## Decision

**A plan is one proposed action.** In plan mode the first (and only)
decision point offers a single `propose_plan` action whose arguments are the
plan DAG (schema-constrained via GBNF, so even a small model emits a
structurally valid plan — a one-node plan is valid). Nodes are typed:

- `tool` → an action in the MCP action space,
- `a2a` → a `delegate_to_<name>` action (still an action in the space) —
  inheriting Phase 4b egress governance (origin-pin/SSRF, durable
  reconnect),
- `review` → a kernel-owned approve/reject decision over upstream outputs.

**Validation is an additional layer, before any execution.** The kernel
checks: unique ids; deps reference existing nodes; **no cycles** (Kahn
topological sort, which also yields the deterministic execution order);
allowed node types; `tool`/`a2a` nodes reference an action that exists in
the registry's action space; and the declared node type matches how the
registry classifies the action (a `tool` node can't point at a remote
agent). An invalid plan fails the run **before the plan is journaled and
before any node runs** — no partial side effects (proven live: the smoke
sees a self-referential-dep plan rejected pre-execution).

**The plan is a memoized decision (the durability model).** This is the
Phase 3 replay law generalized from a linear sequence to a DAG:

- The validated plan is journaled once as `PlanProposed`. On resume it is
  replayed from the journal and **the model is NOT re-queried for it**.
- Each node reuses the per-action durable machinery — `Intent → execute →
  Receipt`, idempotency keys, A2A reconnect `Note`s — **keyed by the node's
  index**. A node with a journaled `Receipt` is skipped on resume; a
  dangling `Intent` re-dispatches with the journaled idempotency key
  (exactly-once). A `review` node's verdict is journaled as its receipt, so
  its model call is memoized too.

So the model is consulted at most once for the plan plus once per review
node; tool/a2a nodes involve no model call at all. Resume is fully
deterministic. (Proven deterministically in
`crates/kernel-core/tests/plan_test.rs`, including: resume replays the
journaled plan without re-querying the model and does not re-run a completed
node.)

**Governance is not bypassed per node.** Every node still goes through
kernel-side schema validation and journaling at execution time; plan
validation is *in addition*. An `a2a` node is the same governed A2A
delegation as in step mode.

**Adaptive re-planning is deferred (scoped honestly).** On a node failure
the plan fails explicitly (fail-on-node-error), fed back as a run error —
the kernel does **not** silently ask for a revised plan. Adaptive
re-planning (re-propose the remaining work on failure, re-validate,
re-journal) is a documented follow-up, per the phase's failure guidance:
never ship an uncertain re-plan that pretends to work.

> **Follow-up delivered:** ADR 0030 adds exactly that follow-up — bounded
> per-node retry and bounded adaptive re-planning, re-validated through this
> ADR's `propose_plan` floor and journaled for deterministic resume. Off by
> default; this ADR's fail-on-node-error remains the default behavior.

## Consequences

- Plan mode composes tools, A2A delegations and review into one validated,
  durable DAG without a new action surface or new governance path.
- The journal model extends cleanly: one new `PlanProposed` entry; nodes
  reuse the existing per-step entries keyed by node index.
- A GBNF limitation surfaced and was handled: a generic (recursive) object
  grammar for node `arguments` makes constrained generation pathologically
  slow on a 0.5B, so the plan schema constrains `arguments` to a shallow
  string→string map under decoding while the kernel re-validates each
  node's arguments against the real tool schema at execution. Tools needing
  non-string arguments in a plan are a documented follow-up; kernel-side
  validation is the guarantee either way (ADR 0003 spirit).
- Live plan *proposal* on a small local model is slow/unreliable (the model
  rambles or self-references); that is a model-capability limit, not a
  kernel one. The kernel-side validation gate — which rejects those bad
  plans before execution — is exercised live; the durable-execution money
  shot is proven by the deterministic test harness.
