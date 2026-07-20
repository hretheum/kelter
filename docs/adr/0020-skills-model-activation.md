---
provenance: ai_authored
provenance_metadata:
  agent: claude-sonnet-5
  session_date: 2026-07-20
  human_validated: false
  session_context: "Model self-activation of advisory skills as a kernel-owned action, run-scoped and ephemeral — never granting capability."
---

# ADR 0020: Skills — model self-activation, still advisory, still never authorizing

Status: accepted · Date: 2026-07-20 · Builds on: ADR 0019 (skills — advisory
procedural knowledge, never authorization)

## Context

ADR 0019 made skill activation an **operator** act: `kelter skill activate
<name>` writes to the registry's `active.toml`, and the always-visible catalog
(name + description) is the only thing every run sees regardless. That is
deliberately conservative — an operator decides what advisory context a run
gets — but it also means the model itself has no way to reach for a skill it
recognizes from the catalog mid-task. A model that sees `careful-notes:
formatting rules for the notes file` in the catalog cannot act on that
recognition; only the operator can flip it on, globally, for every run until
turned off again.

The natural next step is letting the **model** activate a skill for itself,
mid-run, when it decides the catalog entry is relevant. The risk is obvious
and exactly the one ADR 0019 exists to foreclose: if "activate" became a path
by which the model could grant itself anything, the kernel's central claim —
*nothing outside the validated action space executes* — would have a hole in
it shaped like "but skills are special." This ADR closes that gap the same
way ADR 0019 did: by keeping activation on the knowledge side of the
propose→validate→execute loop, never the capability side, and making the
kernel the one and only place that decides what is safe to do about it.

## Decision

`activate_skill` and `deactivate_skill` become **kernel-owned actions**,
handled inline in the engine loop exactly like `finish` — never routed
through `ActionSource` (the tool substrate). This is the structural
guarantee: an action that never reaches `ActionSource::execute` cannot grant
capability, by construction, regardless of what the skill's own instructions
say. Their only effect is which advisory skill instructions the model sees on
the **next step**. The kernel re-validates every subsequent proposal against
the same `ActionSchema` regardless of which skills are active — the ADR 0019
invariant is unchanged; this ADR only adds a second way to reach the same
set of "active skills," gated the same way at the point that matters.

### The seam: `SkillGate` (kernel-core stays skills-agnostic)

kernel-core must not depend on the `skills` crate, so the model-activation
gate is a trait in `kernel_core::skill_gate`, mirroring the
`LearnedStore`/`with_learned` optional-subsystem pattern (ADR 0016):

```rust
pub trait SkillGate: Send + Sync {
    fn any_activatable(&self) -> bool;
    fn authorize_activation(&self, name: &str, run_active: &[String])
        -> Result<String, String>;
}
```

`Kernel` gets `skills: Option<Arc<dyn SkillGate>>` and a builder
`with_skills(...)`. Absent → `activate_skill`/`deactivate_skill` are never
offered, so the model can never even propose them; the feature is completely
inert unless a daemon wires a gate in.

The `skills` crate implements the seam: `RegistrySkillGate` wraps the
existing `SkillRegistry` and a `ModelActivationPolicy` (`TrustedOnly` |
`All`). `authorize_activation` checks, in order: (1) the skill exists, (2)
the policy permits its trust label, (3) the shared advisory-context budget
still holds over the union of the operator-active set, the run's own
already-active set, and the candidate — `≤ MAX_TOTAL_ACTIVE_BYTES` (48 KiB,
unchanged from ADR 0019). `Ok(trust)` on success (the trust label is
journaled/returned for audit); `Err(reason)` on refusal — never a panic,
never a silent truncation.

### Where authorization runs, and where it does not

Authorization runs **once**, at the point a *fresh* model proposal for
`activate_skill` passes schema validation and the duplicate-action check, and
strictly **before** the Decision is journaled. On `Err`, the kernel emits
`RunEvent::Rejected` with the gate's reason, sets `last_error` so the model
gets feedback, and retries — exactly like a duplicate-action rejection: no
journaled Decision, no state change. `deactivate_skill` needs no gate at
all — turning advisory context off is always safe.

Authorization does **not** run on the journal-replay path. A replayed
Decision was authorized when it was first made; the journal answering
instead of re-querying the model is the project's replay law (ADR 0003), and
it applies here unchanged. On replay, the kernel-owned handler re-derives the
trust label (the gate call is pure and safe to call again) purely for the
result payload; if the gate now errs — e.g. the operator's policy changed
between the run and a resume — the kernel still applies the activation
(replay is law) but records the trust as `"unknown"` rather than fabricating
a value.

### The handler itself: knowledge only, no Intent/Receipt

`activate_skill`/`deactivate_skill` are handled in the engine loop
immediately after the `finish` check and before the tool-execution path. The
entire effect is a mutation of `Run.active_skills` — a `Vec<String>` on the
run record, `#[serde(default)]` so old runs deserialize with an empty set:

- `activate_skill`: push the name if not already present; the result JSON
  carries `{"activated": name, "trust": <label>, "run_active": [...]}`.
- `deactivate_skill`: retain everything except the name; result carries
  `{"deactivated": name, "run_active": [...]}`.

Both push a `StepRecord` (so the action is visible in the run's step list
like any other), emit `RunEvent::Executed`, persist the run, and `continue`
the step loop. **No `Intent`/`Receipt` journal entries** — there is no
external side effect to protect against a crash mid-execution, and a set
insert/remove is repeatable: replaying the same `Decision` after a crash
re-applies the same mutation with the same result. Exactly-once by
construction, the same way the existing `finish` handling needs no
Intent/Receipt boundary either.

### Run-scoped and ephemeral — the hard requirement

Self-activation must never touch the operator's persistent state. It does
not: the handler only ever writes to `run.active_skills` via the durable
store's `RunStore::put`, and nothing in this feature calls
`SkillRegistry::activate`/`deactivate` (the operator-facing API that writes
`active.toml`) or touches a skill's trust sidecar. A run that self-activates
a skill leaves the registry's `active_names()` and every skill's `trust`
byte-for-byte unchanged; a fresh run always starts with `active_skills =
[]`. Proven by test (c),
`self_activation_is_run_scoped_and_ephemeral`.

### Rendering: the union, budgeted the same way twice

`SkillContextOracle::propose` now unions `context.active_skills` (the
run-scoped set, threaded through `PolicyContext`) with
`registry.active_names()` (the operator set) and renders instructions for
that union via a new `SkillRegistry::active_instructions_for(&[String])` —
the same per-skill (16 KiB) and total (48 KiB) budgets as the
operator-only path, which `active_instructions()` now delegates to. This is
the **render-time backstop**: even if authorization were somehow bypassed,
the rendered context can never exceed the budget, because the same capped
renderer is the only thing that ever produces it.

### Schema construction: additive and after the narrowing

`activate_skill_spec()`/`deactivate_skill_spec()` (params: `{name: string}`,
required, `additionalProperties: false`) are pushed onto the schema
alongside `finish_spec()`, gated on `self.skills.as_ref().is_some_and(|g|
g.any_activatable())` — no gate, or a gate with nothing activatable under its
policy, means the actions are never offered (no noise). In the step loop this
push happens **after** `Latitude::Constrained`'s `narrow_action_space` retain
(which drops actions that already succeeded), so a model may activate and
deactivate repeatedly across a run even under the harder decomposition
latitude.

### Policy knob: default `trusted`, an open decision for the maintainer

Every skill is `"untrusted"` at install (ADR 0019, unchanged); only a
sidecar edit — never any API in either crate — moves a skill to trusted.
`[skills] model_activation = "trusted" | "all"` (default `"trusted"`) picks
`ModelActivationPolicy::TrustedOnly` or `::All`. An unrecognized value falls
back to `"trusted"` with a warning rather than failing closed silently or
open loudly.

**`TrustedOnly` (default) makes the feature inert until an operator
explicitly trusts a skill** — the safe opt-in posture, consistent with
"activation adds knowledge, not capability" but acknowledging that the
*residual* risk here is the same one ADR 0019 named: **steering**, not
capability. An untrusted `SKILL.md` cannot make the kernel execute anything
it would otherwise refuse, but it can nudge the model toward an
allowed-but-unwanted in-space action — and self-activation gives it a new
lever to pull that lever without an operator's say-so. `All` removes that
gate entirely (any installed skill, any trust label). **This default is an
open decision left for the maintainer, not resolved silently by this
change** — both settings are capability-safe; the trade-off is entirely
about how much steering latitude the model gets on its own initiative.

### Not yet wired: desktop

`apps/desktop` wires neither the skills registry nor the learned-fact store
today (ADR 0016's note applies unchanged here) — `Kernel::with_skills` is
daemon-only for this phase. Desktop wiring is a small, mechanical follow-up
once the daemon-side shape is validated in practice.

## Consequences

- **Proven invariants** (full detail: `docs/evidence/skills-model-activation.log`,
  `crates/skills/tests/skills_test.rs`, `crates/skills/examples/model-activation-demo.rs`):
  a model can self-activate a skill and the urged in-space action it then
  proposes executes through the unchanged validate→execute path
  (`model_self_activates_skill_then_urged_action_executes`); a
  self-activated skill urging an out-of-space action is still rejected —
  the ADR 0019 invariant holds regardless of who flipped the skill on
  (`invariant_holds_under_model_self_activation`); self-activation is
  run-scoped and ephemeral, never mutating the operator's persistent active
  set or any skill's trust (`self_activation_is_run_scoped_and_ephemeral`);
  the budget is a hard floor — refused, never silently overflowed
  (`budget_refuses_activation_that_would_exceed_the_cap`); deactivation
  removes instructions from the very next step
  (`deactivate_removes_instructions_next_step`); and the trust policy gates
  correctly (`trust_policy_trusted_only_refuses_untrusted_all_permits`).
- kernel-core gains one new trait and two new kernel-owned actions but stays
  skills-agnostic — no dependency on the `skills` crate, and the
  `ActionSchema`/`ValidatedAction` trust boundary (ADR 0002) is untouched;
  every proposed action, self-activation included, is still re-validated
  against the schema the kernel itself constructed.
- `Run` gains `active_skills: Vec<String>` (`#[serde(default)]`, so existing
  persisted runs deserialize unaffected) and `PolicyContext` gains
  `active_skills: Vec<String>`, threaded through all three construction
  sites in the engine (step loop, plan proposal, plan review) plus the
  `policy` crate's test fixture and the OpenRouter mock test fixture.
- **Open for the maintainer:** the `trusted` vs `all` default for
  `[skills] model_activation` (leaning `trusted`, already the shipped
  default) — a steering-risk trade-off, not a capability one, and
  deliberately not resolved unilaterally here.
- **Deferred:** desktop wiring; a dedicated `RunEvent` variant for
  activation (reusing `Executed` with `trust` in `result` was judged
  sufficient — keeps the event surface small); per-run activation limits
  beyond the shared byte budget (e.g. a count cap) if operators want one.
