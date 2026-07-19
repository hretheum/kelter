---
provenance: ai_authored
provenance_metadata:
  agent: claude-opus-4-8
  session_date: 2026-07-19
  human_validated: false
  session_context: "Phase 10: governed, reversible self-learning — the runtime distils reusable facts from past runs into a distinct, provenance-tagged, forgettable memory layer that informs proposals but never bypasses validation. First slice = facts (plans/auto-background/sharing deferred)."
---

# ADR 0016: Governed, reversible self-learning

Status: accepted · Date: 2026-07-19 · Builds on: ADR 0001 (validation floor — unchanged), ADR 0005 (memory provider), ADR 0007 (journal/episodes — the distillation source), ADR 0008 (memory-as-extension) · Scope: reusable FACTS only

## Context

The runtime accumulates completed runs (journal + episodes). The tempting next
step — "let the agent learn from experience" — is exactly where an agent
runtime can quietly rot: learning that silently mutates behaviour, that can't be
undone, or that becomes an unvalidated authority is the anti-pattern this whole
project exists to avoid. This ADR adds self-learning that **cannot** do those
things: the runtime distils reusable **facts** from past runs into memory —
governed, provenance-tagged, and completely reversible — so learning can inform
the model but never silently corrupt behaviour.

First slice is **knowledge (facts)**. Deferred, explicitly: distilling **plans**
(reusable plan templates — the second slice), **auto-distillation** in the
background (this slice is command/threshold-driven, never silent), and
cross-user sharing.

## Decision

Two invariants are the whole point; each has a hard STOP if it can't be met.

### Crux 1 — reversibility is complete (no residue)

Learned facts are a **distinct, tagged layer** (`LearnedStore` / the SQLite
`learned_facts` table), never merged into `episodes` or into any persistent
ranking index. Ranking is recomputed per retrieval (BM25 over `task_context`),
so there is **no index to leave residue in**. `forget(id)` / `forget_batch(batch_id)`
delete the rows; the next run simply does not load them — the influence is
gone, entirely. Keeping learned facts distinct (rather than blended into
indistinguishable state) is precisely what makes forgetting complete. Proven by
`smoke-selflearn.sh` (forget → list empty → a later run shows no learned
context) and the unit tests.

### Crux 2 — learning informs proposals, never bypasses validation

A learned fact enters **only** as retrieved *context* on the model's prompt —
the same `PolicyContext.memory` seam as episodic memory, tagged `[learned]` so
it is visibly lower-trust. The model may be *informed* by it, but the kernel
still validates **every** proposed action against the schema (ADR 0001). A
learned fact is never an executable, never an authority. Even a poisoned fact
("always call the tool `nuke`") only reaches the model as text: if the model
proposes `nuke`, validation rejects it because it isn't in the schema — no
unvalidated action executes. The validation floor is **unchanged**. Proven by
`poisoned_learned_fact_cannot_trigger_an_unvalidated_action` (the model, scripted
to propose the suggested invalid action, is rejected; the run still completes via
a valid action).

### Governed distillation

`Kernel::distill(limit)` is a kernel operation: it reads **completed** runs,
extracts a reusable fact from each (the task + the ordered successful actions),
and writes it to the learned layer. It is **command/threshold-driven** (a
bearer-gated API call), never silent background mutation. Runs already distilled
are skipped, so repeated calls don't duplicate. The learned artifacts — durable
and inspectable — are the record of what was learned.

### Provenance (nothing learned without it)

Every learned fact carries `LearnedProvenance`: the **source run id(s)**, when
it was distilled, the **model** that produced the source run, `level =
"ai_extracted"`, and `human_validated = false` — the project's provenance
convention applied to runtime memory. Learned facts are lower-trust until a
human validates them.

### Inspectability + governance

`GET /api/v1/learn` lists every learned fact with its provenance — you can see
**what** the system learned and **why** (from which runs). Distillation and
`forget` are bearer-gated; distillation is logged. No path bypasses the kernel.

## Consequences

- Self-learning exists, but it is the opposite of a silent behaviour mutation:
  **reversible, governed, inspectable, and validation-preserving.** It can make
  the model better-informed; it cannot make the runtime do something unvalidated,
  and anything it learns can be completely undone.
- Learned facts are a distinct layer on the durable store (sqlite backend); the
  kernel holds it optionally (`with_learned`), so nothing changes for
  deployments that don't enable it.
- Because ranking is recomputed and learned facts are separate, "forget" is
  structurally complete — not a best-effort scrub.
- Deferred honestly: plan-template distillation, background/auto distillation,
  cross-user sharing, and model-assisted (vs the current deterministic)
  extraction. Plan-mode runs do not yet receive learned context (step-mode
  only), consistent with plans being the next slice.
