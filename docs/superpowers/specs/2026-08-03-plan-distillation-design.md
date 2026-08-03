---
provenance: ai_authored
provenance_metadata:
  agent: claude-opus-5
  session_date: 2026-08-03
  human_validated: false
  session_context: "Design for plan distillation — the second self-learning slice deferred by ADR 0016. A completed plan-mode run's plan becomes a retrievable, structured learned artifact that the model may adopt as a candidate, re-validated through the same floor as any plan and completely reversible."
---

# Design: plan distillation (second self-learning slice)

Status: design, approved in conversation 2026-08-03 · Builds on: ADR 0001
(validation floor), ADR 0010 (plan mode), ADR 0016 (governed self-learning —
this is its own deferred second slice), ADR 0030 (plan-mode re-planning) ·
Supersedes nothing

## Problem

ADR 0016 shipped self-learning for **facts** and explicitly deferred
**plans**: "distilling plans (reusable plan templates — the second slice)".
Today a completed plan-mode run's plan is journaled once
(`JournalEntry::PlanProposed`) and then forgotten as far as future runs are
concerned. Every plan-mode run re-derives its plan from scratch, even for a
task the runtime has already planned and executed successfully.

The distilled *fact* a step-mode run produces today is prose: "For a task like
X, a successful approach used these actions in order: a → b." That informs the
model but cannot be checked, only read. A plan is structured, so it can be
**re-validated** — which is the whole reason this slice is worth building as
something more than nicer prose.

## Non-goals

Carried forward from ADR 0016, still deferred and not addressed here:

- background / automatic distillation (this stays command-driven),
- cross-user sharing of learned artifacts,
- model-assisted extraction (the selection rule here is deterministic),
- learning from failed runs.

## Decision summary

A completed plan-mode run's final plan is distilled into a **structured,
provenance-tagged, forgettable** `LearnedPlan`. On a later plan-mode run with a
similar task, surviving candidates are offered to the model as a second
proposable action, `adopt_learned_plan`. An adopted plan goes through the
identical `plan::validate` floor as a freshly proposed one and is journaled
**by value**. Nothing executes without the model explicitly choosing it, and
nothing is privileged by having been learned.

## 1. Artifact and storage

New type in `crates/kernel-core/src/learned.rs`:

```rust
pub struct LearnedPlan {
    pub id: Uuid,
    /// What task this plan is relevant to — the text retrieval ranks against.
    pub task_context: String,
    /// The full DAG, arguments included.
    pub plan: crate::plan::Plan,
    pub provenance: LearnedProvenance,
}
```

`LearnedProvenance` is reused unchanged: `source_run_ids`, `distilled_at`,
`model`, `level = "ai_extracted"`, `human_validated = false`, `batch_id`. A
learned plan is therefore lower-trust by construction, exactly as a learned
fact is.

The existing `LearnedStore` trait gains `put_plan`, `retrieve_plans(task,
limit)` and `list_plans`. `forget` and `forget_batch` are extended to cover
**both** artifact kinds, so there stays exactly one reversal path and one
`batch_id` per distillation call. A new `learned_plans` table is added to all
three implementations: `InMemoryLearnedStore`, the SQLite store in `storage`,
and `PgLearnedStore` in `pgstore`. The last one means a Postgres schema
migration and a test that needs a live database, which joins the existing
`#[ignore]`d group.

Rejected alternatives: an optional `plan` field on `LearnedFact` (one type
meaning two things, and retrieval would have to pretend both rank alike, when a
plan wants a single best candidate rather than a top-3); and a fully separate
`LearnedPlanStore` with its own endpoints (duplicates the entire governance
surface — two backends, two endpoint sets, two reversal paths — when the
coherence of reversal is the thesis being defended).

## 2. Distillation

`Kernel::distill(limit)` (`engine.rs:1048`) gains one branch. For a completed
run, the kernel reads `store.journal(run.id)` and looks for `PlanProposed`
entries. If present, it takes the **last generation** and stores it as a
`LearnedPlan`; otherwise the run is distilled to a fact exactly as today.

The last generation is the correct source because it is the plan that actually
carried the run to completion. Earlier generations are plans that failed and
triggered a re-plan under ADR 0030 — learning from those would be learning
failure.

The `already` set (`engine.rs:1056`) still guarantees a run is distilled at
most once, and must now be built from both facts and plans.

This is a deliberate behaviour change for existing deployments: a completed
plan-mode run currently distils to a prose fact (its `StepRecord`s carry the
executed node actions), and after this change it distils to a `LearnedPlan`
instead. One run yields one artifact, never both. Facts already distilled from
past plan-mode runs are left alone — they remain valid, lower-trust context.

No new configuration. The learned layer is already optional via `with_learned`;
plans ride the same switch.

## 3. Retrieval and adoption

Everything happens at one seam, the proposal site in `engine.rs:3289–3353`.

**Context for plan mode.** `PolicyContext.memory` — an empty vector today at
`engine.rs:3329` — receives the same block step mode builds: episodic memory
plus `[learned]`-tagged facts. This closes a real inconsistency between the two
modes (plan-mode runs currently receive no retrieved context of any kind), and
is in scope by explicit decision. Degradation matches step mode: a failing
memory backend is a logged warning, never a failed run.

**Candidate retrieval.** The kernel retrieves up to 3 learned plans matching
the task — the same limit and the same ranking the fact layer already uses
(BM25 in the durable backends, word overlap in the in-memory reference) — and
immediately runs each through `plan::validate` against the live
`validation_schema`. Every survivor is offered; there may be none. A plan
referencing a tool no longer in the registry simply does not exist for the
model, which is the structural analogue of forgetting, driven by the action
space itself.

**The choice.** `propose_schema` (`engine.rs:3298`) gains a second action:

```
adopt_learned_plan { plan_id: string }
```

Candidate contents — id plus the node list with actions — are rendered into the
`guidance` text, so the model chooses while seeing what it is adopting rather
than blind on an opaque id.

Rejected alternative: a kernel-level reuse decision outside the action space.
That would add a second planning surface next to the existing one; a plan must
remain one proposed action (ADR 0010).

**Generation zero only.** Adoption is offered on the first proposal and never
on a re-plan. A plan that just failed must not return as a candidate — that is
a direct route to a loop — and confining adoption to the first generation keeps
the ADR 0030 re-plan budget untouched.

**On adoption.** The kernel materializes the `Plan` from the store by id and
feeds it into the same `plan::validate` call at `engine.rs:3363`. Not one
branch fewer. An adopted plan is privileged in nothing: same structural
validation against the same pristine `validation_schema`, same per-node
argument validation at execution time.

**Journaled by value, not by reference.** `PlanProposed { plan }` records the
fully materialized plan. Two consequences, both load-bearing:

1. Crash-resume replays the plan from the journal and never consults the
   learned layer, so `forget` cannot break an in-flight or resuming run.
2. A completed run's journal keeps the plan it executed. That is history, not
   residue — the same way a fact that informed a completed run stays in that
   run's record. `forget` removes future influence; it does not rewrite the
   past. This is stated explicitly here and must be stated in the ADR, so it is
   not misread as a breach of ADR 0016's zero-residue crux.

**Poisoning.** A poisoned learned plan can at most name actions that exist,
because anything else is filtered at retrieval. It cannot introduce an action
outside the schema: `ActionSchema::validate` and `plan::validate` both stand
before execution. ADR 0016's second crux — learning informs proposals, never
bypasses validation — is unchanged, only extended from prose to structure.

## 4. Node arguments

Distilled plans keep their arguments verbatim. Only then is adoption
meaningful: the model receives a genuinely executable candidate. The risk is a
task that is similar but not identical, where an argument is type-correct and
substantively wrong (the wrong filename, say). Two things bound it: retrieval
ranks on task similarity, and the model sees the plan's contents before
adopting it. Per-node arguments are still re-validated against the real tool
schema at execution time, so this is a relevance risk, never a validation
bypass.

Rejected alternative: storing DAG structure only, without arguments. That
leaves a non-executable candidate, which collapses adoption back into a prose
hint — the option this design already rejected.

## 5. Error handling

**Unknown plan id.** The model may hallucinate an id; this is a real case, not
a theoretical one. Today an invalid first-generation plan fails the run
(`engine.rs:3355–3362`), but killing a run over a hallucinated id is
disproportionate. One bounded branch: log it, skip adoption, and re-ask the
model **exactly once** with `propose_plan` alone. No loop, and no re-plan
budget consumed. If that second proposal is also invalid, the run fails as it
does today.

**Learned store fails on retrieve.** Logged warning, zero candidates, the model
plans normally — the same degradation memory already has at `engine.rs:2027`.

**Adoption racing a forget.** Not an execution hazard, because journaling is by
value. If `forget` lands between retrieval and materialization, materialization
returns nothing and control enters the unknown-id branch above.

## 6. Testing

Unit:

- distillation takes the last journaled generation, not the first and not all;
- a step-mode run still yields a fact, a plan-mode run yields a plan, neither
  yields both;
- `forget_batch` removes facts and plans from the same batch;
- retrieval filters out a plan whose action is no longer in the schema;
- adoption is not offered when the generation is greater than zero.

Integration:

- an adopted plan executes and is journaled by value, and a later `forget` does
  not break resume;
- a poisoned learned plan never reaches execution, mirroring the existing
  `poisoned_learned_fact_cannot_trigger_an_unvalidated_action`;
- an unknown plan id produces exactly one re-ask, not a loop.

Beyond `cargo test`: `scripts/smoke-selflearn.sh` gains a plan leg against a
live daemon. The `pgstore` test for `learned_plans` joins the `#[ignore]`d
group, so hermetic and `--include-ignored` counts both rise. Exact numbers are
reported after implementation rather than guessed here.

## Consequences

- The second self-learning slice exists on the same terms as the first:
  provenance-tagged, inspectable, completely reversible, and incapable of
  producing an unvalidated action.
- A learned plan is structurally checkable, not merely readable. Staleness is
  handled by the action space rather than by a policy: a plan that no longer
  fits is not offered.
- Plan mode gains retrieved context (episodic + learned facts) it never had,
  closing an inconsistency with step mode.
- `LearnedStore` grows; three backends must implement the additions, and
  Postgres needs a migration.
- Still deferred, unchanged: background distillation, cross-user sharing,
  model-assisted extraction, learning from failures.
