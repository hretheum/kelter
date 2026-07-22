---
provenance: ai_authored
provenance_metadata:
  agent: claude-fable-5
  session_date: 2026-07-22
  human_validated: false
  session_context: "A2 of the oculink agentic-coding brief, implemented after the explicit human gate: refine the anti-stall duplicate rule and Constrained action-space narrowing to be STATE-EPOCH-scoped, so verification after a mutation is legal while stateless repeats stay rejected."
---

# ADR 0031: State-epoch duplicate rule (verification after mutation is legal)

Status: accepted · Date: 2026-07-22 · Refines: the anti-stall rule and
Constrained narrowing in the step engine (ADR 0001-era) · Human gate:
explicitly approved before work started (oculink brief §10.2) · Evidence:
`~/dev/llm/oculink-testy/results/kelter/` (step-zero + live series)

## Context

The deterministic anti-stall rule rejected an exact repeat of (action,
arguments) against the WHOLE run history, and Constrained latitude removed a
once-succeeded tool from the action space permanently. Both exist for a good
reason — a weak model that stalls in a propose-repeat loop burns the step
budget for nothing — but both are STATE-BLIND: re-running the test suite
after an edit, or re-reading a file after a patch, is not a stall; it is the
verification the coding loop is MADE of. Step zero confirmed both deaths
deterministically on the live kernel and three separate live loop-deaths
with gemma-3-27b (identical test re-run, re-verification after a failed fix,
re-read after crash-resume).

## Decision

**A successful MUTATING step starts a new state epoch, and both rules are
scoped to the current epoch.** A step is mutating iff its action's spec
declares `idempotency_key` support — the kernel's existing side-effect
convention (a tool declares the key exactly because it has effects that must
not double-fire). No new manifest field, no model-facing change, no trust in
the model's claims: the classification is the tool author's own durable
declaration, already load-bearing for exactly-once resume.

- **Duplicate rule (all latitudes below Wide):** an exact (action,
  arguments) repeat is rejected only against steps from the last successful
  mutating step ONWARD — **inclusive** of that step, so repeating the
  mutation itself with identical arguments is still a stall. A repeat with a
  mutation in between executes normally.
- **Constrained narrowing:** succeeded tools are dropped from the action
  space only within the current epoch, **exclusive** of the mutation itself
  (or the mutating tool could never run twice; its exact-argument repeat is
  still caught by the duplicate rule). A mutation restores dropped tools —
  the edit→verify loop is possible at Constrained.
- **The prompt** no longer says an unconditional "do NOT repeat": it states
  the actual rule (a repeat is rejected unless a state-changing action ran
  in between), and the Constrained guidance is reworded to match. The
  deterministic rule stays kernel-side; the prompt merely stops instructing
  the model against legal verification.

## What deliberately did NOT change

- **Wide latitude**: still no duplicate rejection at all.
- **Stateless repeats still die** (the refined rule's own test pins this):
  read→read, test→test, edit(X)→edit(X) with nothing mutating in between
  are rejected with the same feedback as before.
- **An unkeyed side-effect tool** is conservatively non-mutating: its
  repeats stay rejected exactly as pre-A2 (no false epoch resets); declaring
  the key — which such a tool should do anyway for resume safety (C4) — is
  what opens the epoch semantics.
- **Plan mode** is untouched (nodes never went through the duplicate rule).
- **Journal/replay**: no journal shape change; resume folds the same
  entries. Durability re-proven post-change (test + live smoke).

## Consequences

- The measured loop-killer is gone: edit→test→edit'→test completes; the
  post-resume re-read (the third live death) is legal because the journaled
  edit anchors an epoch.
- A model can now burn budget alternating two mutating actions with
  fresh arguments — but that was always possible with fresh arguments;
  `max_steps` remains the hard bound, unchanged.
- Proof: `crates/kernel-core/tests/agentic_loop_step_zero_test.rs` (6
  tests — the money shot, both still-rejected flavors, the identical
  re-edit stall, both narrowing halves, and the resume/exactly-once
  re-proof) + `a2_epoch_window_test.rs` (failed attempts do not anchor an
  epoch), `scripts/smoke-durability.sh` re-run green post-change, and the
  N=24 before/after live series (gemma-3-27b) in the oculink evidence
  pack: duplicate-rule deaths 5/12 pre vs 0/12 post (Fisher one-tailed
  p≈0.019); a forced test→fix→retest trajectory completes on this kernel
  and died 5/6 on the previous one; premature finish 0/24.
