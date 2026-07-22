---
provenance: ai_authored
provenance_metadata:
  agent: claude-fable-5
  session_date: 2026-07-22
  human_validated: true
  session_context: "Plan-mode resilience: bounded per-node retry for transient failures + bounded adaptive re-planning (a failed plan is repaired by a model-proposed REVISED plan through the same propose_plan validation, journaled as a new plan generation), with exactly-once and deterministic crash-resume preserved."
---

# ADR 0030: Plan-mode resilience — bounded retry + adaptive re-planning

Status: accepted · Date: 2026-07-22 · Builds on: ADR 0010 (plan mode — this
is its documented "adaptive re-planning" follow-up), ADR 0007 (durability),
ADR 0001 (validation floor) · Related: ADR 0023 (governed resilience for
delegation — the retry-re-enters-governance rule), ADR 0025 (plan-mode
drain-then-pause)

## Context

Plan mode was deliberately fail-fast: one failed node failed the whole run
("no new node admitted once the plan has failed", ADR 0010). Honest, but
brittle — a transient tool error or one bad node choice threw away every
completed sibling. This ADR adds two bounded recovery mechanisms without
weakening any of the three invariants that make plan mode trustworthy:
validation floor, bounded execution, exactly-once durability.

## Decision

**A. Bounded per-node retry** (`plan_resilience.max_node_retries`, default
0). A failed **tool/a2a** node is re-dispatched up to N extra times before
its failure counts. Review nodes never retry — a rejection is a verdict, not
a transient fault. An a2a retry re-enters the FULL admission governance
(budget, depth/cycle, trust gate) exactly like a fresh dial — the ADR 0023
rule, unchanged.

**B. Bounded adaptive re-planning** (`plan_resilience.max_replans`, default
0). When a generation of the plan fails (a node exhausted its retries, a
review rejected, governance refused), the kernel asks the model for a
**revised plan** instead of failing the run — informed by a **bounded**
failure context: which node(s) failed and why (error truncated), which nodes
completed with what outputs (truncated, ≤ MAX_NODES entries — the failed
plan's own size), and the attempt count. The context is rebuilt per failure,
never accumulated.

### Crux 1 — the validation floor holds through re-planning

A re-plan is a **new `propose_plan` action through the same validation** as
the original: the same schema-constrained proposal surface, the same
`plan::validate` (structure/cycles/types/action-existence) against the same
**pristine tool schema** (the kernel-owned `review_verdict` is part of the
execution schema only, never proposable). There is no privileged path. A
revised plan proposing an invalid action is **rejected**; the rejection
consumes a re-plan slot and, with budget left, the model is re-asked with
the validation error quoted — budget exhausted, the run fails cleanly.
Per-node argument validation at execution time is likewise unchanged.

### Crux 2 — bounded, provably terminating

`max_replans` caps re-plan **proposals** (accepted and rejected alike).
Accepted re-plans are journaled, so the count **survives crash-resume**
(`replans_used` seeds from the number of journaled plans − 1); rejected
proposals count session-locally. Retry is capped per node by
`max_node_retries`. Worst case per session: `1 + max_replans` plan
proposals, each executing ≤ MAX_NODES nodes ≤ `1 + max_node_retries` times.
No re-plan→fail→re-plan loop is possible.

### Crux 3 — exactly-once + deterministic resume through retry/re-plan

*Journal keying.* Each accepted plan is appended as another `PlanProposed`
entry — generation g = its index; the **last** entry is the active plan.
Generation g's node at plan-index i journals under step
`g × PLAN_GENERATION_STRIDE + i` (stride 100 > MAX_NODES 32), so receipts,
intents and notes from different generations can never collide. Generation
0's keys equal the plain node index — **every pre-ADR-0030 journal resumes
unchanged**.

*Retry.* The idempotency key stays a pure function of (run, step), so every
retry reuses the SAME key: a keyed tool whose effect somehow landed before
the failure **dedupes instead of double-executing**. A retry dispatches with
a clean per-step view — its failed receipt is a definitive outcome, not a
crash window — and on resume the attempt count is re-seeded by counting that
step's journaled receipts, so the retry budget also holds across a crash.

*Re-planning (carry-forward).* A node a prior generation completed
successfully — identified by identical **(id, action, arguments)** — is
never re-executed: the revised plan re-including it verbatim gets the
journaled result applied (and the model is told to reuse completed nodes
exactly this way). The carry-forward map is rebuilt deterministically from
the journal alone. Review verdicts are never carried — a review re-judges
the NEW plan's upstream state.

*Crash windows.* Crash after a re-plan was journaled → resume replays both
plans, queries the model **zero** times, re-executes **zero** completed
nodes. Crash before the re-plan was journaled → the failure is durable (the
failed receipt), the proposal is not — resume re-asks the model, which is
the deterministic behavior (identical to step mode's crash-before-Decision),
and completed work still carries forward.

## Defaults and configuration

Both knobs default to **0** — plan mode behaves exactly as before this ADR
(fail-on-node-error). Opt-in via `[kernel] plan_node_retries` /
`plan_replans` (daemon config) → `KernelConfig::plan_resilience`.

## Deliberately deferred

- Model-driven fallback-path selection beyond re-planning (e.g. "try agent B
  if agent A's node fails") — the sanctioned-fallback list of ADR 0023
  already covers the delegation case inside a node.
- Cross-plan learning (feeding re-plan outcomes into the learned store).
- Retrying `review` nodes or carrying verdicts across generations (a verdict
  is state-dependent judgment, not a memoizable side effect).

## Consequences

- `run.steps` indices are now generation-scoped (0-31, 100-131, …); the
  failed node of an old generation stays in the record — honest history.
- The a2a budget seed scans all journaled generations, so re-planned runs
  cannot reset their delegation budget.
- A carried node's step is re-recorded per generation from the journaled
  receipt — no new journal entries are written for carry-forward, keeping
  resume a pure fold of the journal.
- Proof: `crates/kernel-core/tests/plan_resilience_test.rs` (10 tests) —
  retry success + sibling exactly-once, retry exhaustion, defaults preserve
  fail-fast, re-plan repairs with bounded informed context, invalid re-plan
  rejected (floor holds) then corrected / or fails cleanly on empty budget,
  bounded no-infinite-loop, resume-after-journaled-re-plan (0 model calls, 0
  re-executions), resume-before-journaled-re-plan (1 model call, completed
  work carried), re-plan budget holds across resume — plus
  `crates/daemon/tests/plan_replan_durable_resume_test.rs`: the two-session
  SQLite idiom, a crash at the exact re-plan boundary resumed by a fresh
  kernel over the same DB file (1 model call, carried node not re-executed,
  run completes).
