---
provenance: ai_authored
provenance_metadata:
  agent: claude-opus-5
  session_date: 2026-08-03
  human_validated: false
  session_context: "Phase 23: plan distillation — the second self-learning slice ADR 0016 deferred. A completed plan-mode run's final generation becomes a structured, forgettable LearnedPlan; a later run may adopt it as a candidate, re-validated through the same floor as any plan and journaled by value."
---

# ADR 0032: Plan distillation (self-learning, second slice)

Status: accepted · Date: 2026-08-03 · Builds on: ADR 0001 (validation floor — unchanged), ADR 0010 (plan mode), ADR 0016 (governed self-learning — this is its own deferred second slice), ADR 0030 (plan-mode re-planning) · Scope: reusable PLANS

## Context

ADR 0016 shipped self-learning for **facts** and named what it was deferring:
"distilling plans (reusable plan templates — the second slice)". This is that
slice.

The distilled fact a step-mode run produces is prose — "for a task like X these
actions worked, in this order". It informs the model and cannot be checked,
only read. A plan is structured, so it can be **re-validated**. That is the
only reason this slice is worth building as something more than nicer prose,
and it is what makes the governance argument below possible at all.

## Decision

Three properties carry the design; each is a hard requirement, not a nicety.

### Crux 1 — a learned plan is re-validated, never trusted

A candidate is retrieved by task similarity and immediately run through
`plan::validate` against the **live** action space. Only survivors are offered.
A plan referencing a tool the registry no longer carries does not reach the
model at all.

This is the structural analogue of forgetting: there is no expiry field and no
staleness policy to keep correct, because the action space itself does the
pruning. If a capability goes away, every learned plan that depended on it
stops existing for the planner in the same instant.

Adoption then goes through the *identical* `plan::validate` call as a freshly
composed plan, against the same pristine `validation_schema`, with per-node
argument validation still happening at execution. An adopted plan is privileged
in nothing. A poisoned learned plan can at most name actions that exist; it
cannot introduce one outside the schema. **ADR 0001's floor is unchanged.**

### Crux 2 — adoption is a choice the model makes, and it is journaled

The candidate is offered as a second proposable action, `adopt_learned_plan`,
alongside `propose_plan` — never instead of it. The candidates' **contents**
(id plus the node list) go into the guidance, so the model chooses seeing what
it would adopt rather than blind on an opaque handle. The choice is an ordinary
validated action and lands in the journal like any other.

This is what keeps the slice on the right side of ADR 0016's line. The runtime
is not silently substituting learned behaviour; it is offering a candidate that
something has to explicitly pick, and the pick is inspectable afterwards.

Adoption is offered on **generation zero only**. A plan that just failed must
not return as a candidate on the re-plan — that is a direct route to a loop —
and confining the offer to the first proposal leaves ADR 0030's re-plan budget
untouched.

### Crux 3 — journaled by value: forgetting removes influence, not history

`PlanProposed` records the fully materialized plan, not a reference to the
learned artifact. Two consequences, both load-bearing:

1. Crash-resume replays the plan from the journal and never consults the
   learned layer, so `forget` cannot break an in-flight or resuming run.
2. A completed run's journal keeps the plan it executed.

The second reads at first like a breach of ADR 0016's zero-residue crux. It is
not, and the distinction matters enough to state plainly: **forget removes
future influence; it does not rewrite the past.** The learned layer is what is
emptied. A run that already happened keeps its record of what it did — exactly
as a fact that informed a completed run stays in that run's history today. If
forgetting rewrote journals, the journal would stop being a truthful account of
what the system did, which is the only thing it is for.

### Distillation source

The **last** journaled generation. It is the plan that carried the run to
completion; earlier generations failed and triggered a re-plan (ADR 0030), so
learning from them would be learning failure. One run yields one artifact —
plan mode a plan, step mode a fact, never both.

### Storage and reversal

`LearnedPlan` reuses `LearnedProvenance` unchanged: source run ids, when,
which model, `level = "ai_extracted"`, `human_validated = false`, batch id. A
learned plan is lower-trust by construction, exactly as a learned fact is.

The new methods live on the existing `LearnedStore` trait rather than a
parallel one, so there stays exactly **one** reversal path: `forget` and
`forget_batch` cover both artifact kinds, and one `batch_id` spans a whole
distillation call. Coherent reversal is the thesis being defended; a second
store would have meant two ways to get it wrong.

`GET /api/v1/learn/plans` was added alongside the existing facts listing.
Without it a learned plan has no readable id, which would make the per-plan
`forget` unreachable from the API and ADR 0016's "inspectable" property untrue
for plans.

## Consequences

- The second self-learning slice exists on the same terms as the first:
  provenance-tagged, inspectable, completely reversible, incapable of producing
  an unvalidated action.
- Plan mode gains retrieved context (episodic + `[learned]` facts) it never
  had — `execute_plan` branched before step mode's memory block, so plans were
  composed against an empty `PolicyContext.memory`. An inconsistency between
  the modes, closed here by explicit decision.
- A **behaviour change for existing deployments**: a completed plan-mode run
  previously distilled to a prose fact and now distils to a plan instead. Facts
  already distilled from past plan-mode runs are left alone; they remain valid,
  lower-trust context.
- `LearnedStore` grew; three backends implement the additions and Postgres
  gained a `learned_plans` table (self-migrating via `CREATE TABLE IF NOT
  EXISTS`).
- Still deferred, unchanged from ADR 0016: background/automatic distillation,
  cross-user sharing, model-assisted extraction, learning from failed runs.
- Not built here: a web-UI surface for learned plans (facts have one, plans do
  not), and the plan leg of `scripts/smoke-selflearn.sh`. Both are noted rather
  than quietly skipped.
