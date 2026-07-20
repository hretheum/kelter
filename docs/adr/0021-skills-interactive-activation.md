---
provenance: ai_authored
provenance_metadata:
  agent: claude-sonnet-5
  session_date: 2026-07-20
  human_validated: false
  session_context: "Interactive human-in-the-loop skill activation — a third SkillGate outcome (AskHuman) that durably pauses a run for a deny/allow-once/allow-always decision, plus an HTTP surface for skills management. Extends ADR 0020."
---

# ADR 0021: Skills — interactive activation, a bounded human-in-the-loop pause

Status: accepted · Date: 2026-07-20 · Builds on: ADR 0019 (skills advisory
knowledge), ADR 0020 (model self-activation)

## Context

ADR 0020 gave the model two silent outcomes when it self-activates a skill:
`TrustedOnly` (only skills an operator already trusted) or `All` (any
installed skill). Both are silent — the model gets an immediate allow or
deny, with no human in the loop for the specific request. Some deployments
want a third posture: **ask a human**, for THIS specific untrusted skill, on
THIS specific run, without the operator having to pre-decide every skill's
trust up front. That is what this ADR adds: `ModelActivationPolicy::Ask`, a
`SkillGate::authorize_activation` outcome that pauses the run durably
instead of allowing or denying outright, and a human decision — deny /
allow-once / allow-always — that resolves it through the same journaled,
replayable mechanism every other kernel decision already goes through.

A mid-flight addendum (Part 6, added once the frontend implementer correctly
flagged a gap rather than guessing at it) also gives skills management —
list/install/remove/activate/deactivate/policy — an HTTP surface, since
`SkillsSettings.tsx` (the Settings panel) had no backend to talk to before
this pass; previously this was CLI-only (`kelter skill …`).

## Decision

### A third gate outcome, still capability-safe

`kernel_core::skill_gate::SkillGate::authorize_activation` now returns
`ActivationAuthorization` (`Allow(trust)` / `Deny(reason)` /
`AskHuman(meta)`) instead of `Result<String, String>`. `AskHuman` is
capability-wise **identical to `Deny` right now** — nothing executes — it
only changes WHO decides and WHEN. The kernel's structural guarantee from
ADR 0020 is untouched: `activate_skill`/`deactivate_skill` remain
kernel-owned actions, never routed through `ActionSource`, so no decision
under any of the three paths ever grants capability. This is proven by
`invariant_holds_through_every_ask_decision`
(`crates/skills/tests/ask_activation_test.rs`) — a self-activated skill
urging `wipe_disk` is still rejected by the kernel identically under
deny/allow_once/allow_always.

### The pause: `RunStatus::AwaitingSkillDecision`, durable

A new, **non-terminal** `RunStatus` variant. On a fresh `AskHuman` gate
result, the engine journals `SkillActivationRequested` (the model's original
arguments, verbatim, plus the trust/description/provenance the prompt
needs), sets `run.pending_skill_activation`, emits
`RunEvent::SkillActivationRequested`, and returns — no `Decision` is
journaled, nothing executes. A human resolves it via the new
`Kernel::decide_skill_activation(run_id, skill, decision, principal)`
method, which journals `SkillActivationDecided` and flips the run back to
`Running`; the caller (the new `POST /api/v1/runs/{id}/skill-activation`
endpoint, or the timeout sweeper) is responsible for re-invoking
`execute_run` to actually resume — mirroring exactly how `create_run`
(returns an id) is always paired with a separate `execute_run` spawn.

Inside the step loop, only a DECIDED pending request is acted on (the
top-of-function early check below handles an undecided one first): `Deny`
seeds `last_error` and falls into the EXISTING fresh-proposal branch — a
fresh model call is required, since the model must be told and must propose
something else. `AllowOnce`/`AllowAlways` instead synthesize exactly what
the EXISTING journal-replay branch already expects (a `Decision` for
`activate_skill`, journaled), so the ~150-line attempt loop is never
duplicated for the Allow path either. Both decision paths reuse code that
already existed for entirely different reasons — no new control-flow shape
was introduced.

### Durability: the fix that makes a paused run survivable

`unfinished_runs()` uses a POSITIVE allowlist of statuses in both the
in-memory (`kernel_core::store::InMemoryRunStore`) and SQLite
(`storage::SqliteRunStore`) implementations. `AwaitingSkillDecision` is now
in that allowlist in both places — without this, a daemon restart while a
run is paused would silently orphan it forever, since the daemon's startup
resume sweep only ever calls `execute_run` on what `unfinished_runs()`
returns. A new top-of-`execute_run_inner` check
(`still_awaiting_skill_decision`, mirroring the existing Terminal-journal
early-check) scans the whole journal for an undecided
`SkillActivationRequested` and, if found, re-emits the pause and returns —
before the `Running` status flip, so there's no pointless flicker. This
makes BOTH "daemon restarted while paused" (the resume sweep calls
`execute_run` again) and "someone calls `execute_run` again before
deciding" (idempotent — no side effects, no model call) work for free.
Proven end-to-end (not just by code inspection) by
`paused_run_survives_a_restart` — the single most important test in this
extension — using the real `storage::SqliteRunStore` across two dropped-and-
reopened `Kernel` instances against the same on-disk file, the same
two-session idiom `apps/desktop/src-tauri/tests/durable_store.rs` already
established for the desktop shell.

`cancel_run_with_reason` needed **zero code changes** to cancel a paused
run — it already treats anything outside `{Completed, Failed, Canceled}` as
cancelable. Proven, not just assumed, by `cancel_works_on_a_paused_run`.

### Never hangs headless: `ask_fallback`

**Flagged plainly for Eryk's sign-off.** `[skills] ask_fallback` defaults to
`"deny"` — under this default, `model_activation = "ask"` alone behaves
EXACTLY like `TrustedOnly` (an ask-worthy self-activation is denied outright,
immediately, no pause) until an operator ALSO explicitly sets
`ask_fallback = "queue"`. This is intentional, not an oversight: a
deployment with no one to ask (e.g. a headless A2A worker) must never hang a
run waiting for a human who will never answer. Proven by
`ask_fallback_deny_never_pauses` — the run completes without the skill,
never enters `AwaitingSkillDecision`.

Under `ask_fallback = "queue"`, a pause is still bounded: a background
sweeper (`daemon::skill_sweep::sweep_timed_out_skill_activations`,
extracted as a standalone, unit-testable function rather than left inline in
`main()`) polls every 30 seconds and force-denies (principal
`"system:timeout"`) any run paused longer than `[skills] ask_timeout_secs`
(default 300s), then resumes it. Proven deterministically — not via a slow
wall-clock integration run — by `timeout_sweeper_denies_and_resumes`
(`crates/daemon/tests/skill_sweep_test.rs`), which injects `now`/`requested_at`
directly rather than sleeping.

### Explicit, deliberate amendment to ADR 0019

**Flagged plainly for Eryk's sign-off — this is the other thing a reviewer
most needs to see.** ADR 0019 states: *"[trust is] always `\"untrusted\"` at
install; only an operator edit of the sidecar file can change it — no API
in this crate does."* This ADR adds exactly such an API:
`SkillRegistry::set_trust` (public, new) and `SkillGate::elevate_trust`
(new trait method, called ONLY when applying a human's `allow_always`
decision — never from any model-visible path, never a skill self-elevating
via its own frontmatter, which stays ignored exactly as before).

This is a **considered widening of who may write the sidecar** — from
"only a human editing a file on disk" to "also a human's bearer-gated
`allow_always` click on a SPECIFIC pending activation request, applied by
the daemon" — not a silent one, and not a capability change: trust still
never grants execution, only whether the model may self-activate that skill
SILENTLY (without asking) on a future run. `elevate_trust` is deliberately
best-effort: a filesystem write failure is logged but the activation still
applies for the CURRENT run (a human already decided; losing a persistence
write must not un-pause a run that was already resolved). Proven by
`allow_always_activates_and_elevates_trust_persistently` — trust actually
flips to `"trusted"` in the registry, and a SECOND fresh run with the same
gate is allowed silently, no new ask, proving persistence actually took
effect (not just that the call succeeded).

`allow_once`, by contrast, changes nothing persistent — proven by
`allow_once_activates_for_this_run_only_no_trust_change`: trust is
unchanged after, and a second fresh run is asked again.

### Part 6 addendum: skills management over HTTP

**Scope decision, stated plainly, not an oversight:** install is **URL-only**
over HTTP (`SkillRegistry::install_from_url`). A browser cannot meaningfully
pick a server-side folder path or upload a `.tar.gz` bundle without a new
upload mechanism this pass doesn't need to build; folder/bundle install
stays CLI-only (`kelter skill add <dir>` already covers it).

`api::SkillsController` (new trait, mirrors `ModelConfigController`'s shape
exactly — decoupled from the `skills` crate's own types so `api` gains no
hard dependency on it) is implemented by `daemon::skills_controller::
DaemonSkillsController`, closing over the registry (list/install/remove/
activate/deactivate) and a CONCRETE `Arc<RegistrySkillGate>` handle (policy
get/set — kept alongside, not instead of, the `Arc<dyn SkillGate>` upcast
handed to `Kernel::with_skills`, both pointing at the same object). Seven
new routes under `/api/v1/skills*`, bearer-gated like every other route;
absence of a wired controller → 404 on every one, matching the frontend's
existing 404/405-off capability-flag pattern with zero special-casing.

`RegistrySkillGate`'s policy (`ModelActivationPolicy` + `AskFallback` +
`ask_timeout_secs`) is now runtime-mutable — bundled into one
`RwLock<AskPolicyConfig>` swapped atomically by the new `set_policy` method,
so a policy change takes effect on the very NEXT `authorize_activation`
call. Proven, not just asserted, by
`set_policy_takes_effect_on_next_authorize_activation_call`
(`crates/skills/src/gate.rs`): the same gate instance is asked before and
after a live `set_policy` call and the outcome actually changes (Deny →
AskHuman → Allow across three policy states), not merely that the getter
echoes the setter. A policy change persists the same way a runtime model
change already does (`model-config::apply_overlay` precedent): a small TOML
overlay (`skills-policy-override.toml`) beside the config file, applied
BEFORE the skills wiring block reads `cfg.skills.*` at startup.

**Noted limitation, not solved in this pass:** the timeout sweeper reads
`ask_timeout_secs` from its own startup-config snapshot, not live from the
gate's `RwLock`, and the sweeper's spawn condition (`model_activation ==
"ask" && ask_fallback == "queue"`) is likewise evaluated once at startup.
A runtime policy change that flips a deployment INTO needing a sweeper (from
`trusted`/`all`, or from `ask`+`deny`, to `ask`+`queue`) will not spin one
up until the next restart — the policy change still takes effect for
GATING (immediately), just not for the sweeper's own scheduling. This is a
real, deliberate scope line (dynamic task lifecycle management was judged
more invasive than this addendum's actual ask), not an oversight — worth
Eryk's attention if the `Ask`+`Queue` combination is expected to be toggled
live in production rather than set once in config.

## Consequences

- **Proven invariants** (full detail:
  `docs/evidence/skills-ask-and-ui.log`, backend section;
  `crates/skills/tests/ask_activation_test.rs`,
  `crates/daemon/tests/skill_sweep_test.rs`,
  `crates/api/tests/skills_controller_test.rs`,
  `crates/skills/src/gate.rs` unit tests): the pause and its surfacing
  (`ask_pauses_and_surfaces_the_request`); all three decisions' distinct
  effects (`deny_lets_the_model_continue_without_the_skill`,
  `allow_once_activates_for_this_run_only_no_trust_change`,
  `allow_always_activates_and_elevates_trust_persistently`); the invariant
  holding through every decision
  (`invariant_holds_through_every_ask_decision`); headless
  `ask_fallback = "deny"` never hanging
  (`ask_fallback_deny_never_pauses`); durability surviving a real restart
  against real SQLite (`paused_run_survives_a_restart`); cancel working on a
  paused run with zero code changes (`cancel_works_on_a_paused_run`); the
  timeout sweeper's bounded-wait guarantee, tested deterministically
  (`timeout_sweeper_denies_and_resumes`); and the addendum's CRUD +
  runtime-policy contract (`crates/api/tests/skills_controller_test.rs`'s 5
  tests + the gate's RwLock-threading unit test).
- `kernel-core` gains: `RunStatus::AwaitingSkillDecision`,
  `Run.pending_skill_activation: Option<PendingSkillActivation>`
  (`#[serde(default)]`, so existing persisted runs deserialize unaffected),
  two `JournalEntry` variants (`SkillActivationRequested`/`Decided`), two
  `RunEvent` variants of the same names, `KernelError::InvalidState`, and
  `Kernel::decide_skill_activation`. `SkillGate::authorize_activation`'s
  return type changed (`Result<String,String>` → `ActivationAuthorization`)
  — a breaking change to the trait, absorbed entirely within this repo (the
  only implementor, `RegistrySkillGate`, and the one direct test call site,
  are both updated in the same change).
- `api` gains one endpoint (`POST /runs/{id}/skill-activation`) plus, from
  the Part 6 addendum, seven more under `/api/v1/skills*`, a new
  `SkillsController` trait, and `ApiState.skills: Option<Arc<dyn
  SkillsController>>` threaded through a NEW `router_with_principals_and_skills`
  / `build_app_with_skills` pair — the existing `router_with_principals`/
  `build_app` (and every existing call site: `crates/api/tests/`, the
  desktop shell) are UNCHANGED, delegating to the new functions with
  `skills: None`, so this addendum touched zero existing call sites.
- `daemon` gains a `[lib]` target (`src/lib.rs`, exposing `config`,
  `skill_sweep`, `skills_controller`) alongside the existing `kelter` binary
  — added specifically so the timeout sweeper (and the addendum's
  persistence helpers) are unit-testable from `crates/daemon/tests/`
  rather than only reachable through the running binary.
- **Open for the maintainer:** `ask_fallback` default `"deny"` (stated
  plainly above, not hidden) and the ADR-0019 trust-elevation amendment
  (stated plainly above) are the two things this ADR most wants Eryk's eyes
  on. The sweeper's startup-snapshot limitation (above) is a secondary,
  lower-stakes item.
- **Deferred:** live (non-restart) sweeper reconfiguration; a folder/bundle
  upload endpoint for skills install over HTTP (URL-only is the deliberate
  scope for this pass); per-skill ask-timeout overrides (one global
  `ask_timeout_secs` today).
