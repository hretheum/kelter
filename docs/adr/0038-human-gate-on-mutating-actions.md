---
provenance: ai_authored
provenance_metadata:
  agent: claude-code
  session_date: 2026-10-05
  human_validated: false
  session_context: "KR1.2: an operator could not require a human before a state-changing action ran. Skills (ADR 0021) and delegations (ADR 0023/0025/0026) already had a durable pause → decision → resume; ordinary tools — the ones that write files, run commands, append ledgers — had none. This ADR puts mutating actions behind an allow/ask/deny policy on the same mechanism."
---

# ADR 0038: A human gate on mutating actions

Status: accepted (open points for the maintainer under "Open") · Date: 2026-10-05 · Builds on: ADR 0021/0023/0025/0026 (the durable human pause), ADR 0031 (what "mutating" means), ADR 0036 (refusals in the journal) · Touches: ADR 0007 (journal, exactly-once), ADR 0010/0030 (plan mode), ADR 0018 (principals), ADR 0033/0037 (verdict)

## Context

The kernel already lets a human stop a *skill activation* (ADR 0021) and a
*delegation* (ADR 0023, plan mode ADR 0025, a remote's follow-up ADR 0026)
before it happens: the run pauses durably, the decision is journaled with a
principal, a sweeper turns silence into a deny, and the resumed run replays
the model's original proposal instead of asking again. Ordinary tools had
nothing of the kind. `zastosuj_patch`, `uruchom_komende`, `append_ledger`,
`write_report` executed the moment they validated. An operator who wanted
"ask me before you change anything" had no switch.

## Decision

### 1. A policy per action, in config, for mutating actions only

```toml
[approval]
default = "allow"          # for mutating actions with no entry
ask_timeout_secs = 300     # no decision in time = deny; 0 = wait forever
[approval.actions]
zastosuj_patch = "ask"
uruchom_komende = "deny"
review_verdict = "ask"     # plan mode: review nodes become a human verdict
```

`kernel_core::action_gate::ActionGateConfig::policy_for(action, mutating)`
(`action_gate.rs:70`). **Mutating** is exactly ADR 0031's criterion: the
action's spec declares `idempotency_key` support. It is the tool author's own
durable declaration — already what makes exactly-once resume work — so no new
manifest field and nothing the model can influence. Consequences, by design:

- read-only tools (`czytaj_plik`, `git_status`, `git_diff`) are never gated,
  even when the config names them — the daemon logs a WARN at startup for
  such an entry (`log_governance`, `daemon/src/main.rs`);
- an *unkeyed* side-effect tool is conservatively **not** gated — the same
  conservative reading ADR 0031 takes, and the same fix: declare the key;
- `delegate_to_*` is never gated here (it declares no key, and it has its own
  gate, ADR 0023);
- kernel-owned actions (`finish`, `activate_skill`) are never gated.

The policy values are a serde enum: a typo (`"aks"`) is a **startup error**,
not a silent allow (`deny_unknown_fields` on the section too). A safety gate
fails closed. No `[approval]` section = `default = "allow"`, no entries — the
pre-0038 behaviour, byte for byte.

### 2. `ask` reuses the existing pause; only the vocabulary is new

The mechanism is the one ADR 0021 introduced and 0023/0025/0026 reused, step
for step:

| piece | skills (0021) | delegation (0023/0025) | **actions (this ADR)** |
|---|---|---|---|
| journaled request | `skill_activation_requested` | `agent_delegation_requested` | `action_approval_requested` |
| paused status | `awaiting_skill_decision` | `awaiting_delegation_decision` | `awaiting_action_approval` |
| record snapshot | `pending_skill_activation` | `pending_delegation` | `pending_action_approval` |
| early return on resume | `still_awaiting_skill_decision` | `still_awaiting_delegation_decision` | `still_awaiting_action_approval` |
| decision | `decide_skill_activation` | `decide_delegation` | `decide_action_approval` |
| journaled decision | `skill_activation_decided` (+principal) | `agent_delegation_decided` (+principal) | `action_approval_decided` (+principal) |
| endpoint | `POST …/skill-activation` | `POST …/delegation` | `POST …/action-approval` |
| sweeper | `skill_sweep` | `delegation_sweep` | `approval_sweep` |
| allow → | synthesized `Decision`, replay branch | same | same |
| deny → | `last_error`, fresh proposal | same (plan: node fails) | same (plan: node fails) |

**Why not literally reuse the delegation entries?** The stop condition of the
brief: reusing `agent_delegation_requested` for a tool would change what that
entry and its WS event mean to every existing reader (Glassbox, the web UI,
the `AllowAlways` trust elevation that calls the agent registry). The
*machine* is reused — the early-return check before the `Running` flip, the
`unfinished_runs` allowlist, the "allow synthesizes the journaled proposal as
the step's `Decision`" trick, drain-then-pause in plan mode, the
`system:timeout` sweeper — and no control-flow shape was added. The status
enum gains one variant, the fourth of the same kind. ADR 0023 made the same
"mirror, don't reuse" call for the same reason.

**Two-way decision.** `allow` | `deny`, no `allow_always`: there is no
persistent trust registry for tools, and the standing policy is the config
file the operator edits — never something a click on one run rewrites.

### 3. Step mode (`engine.rs:3695`)

The gate runs on a **fresh** proposal, after schema validation and the
duplicate/delegation/skill gates, **before** the `Decision` is journaled:

- `deny` → `Kernel::reject` with `source: action_policy`, `subject:
  proposal` (ADR 0036 — journaled informationally, live `rejected` event),
  `last_error` tells the model "the operator's action policy denies 'X' — it
  changes state and is not permitted in this deployment; pick a different
  approach". It consumes one attempt, like any kernel refusal.
- `ask` → `pause_for_approval` (`engine.rs:1983`) journals
  `action_approval_requested {step, action, arguments, requested_at}`, sets
  `awaiting_action_approval` + `pending_action_approval` (with `expires_at`),
  emits `action_approval_requested`, returns. **No `Decision`, no `Intent`.**

On resume after a decision (`engine.rs:3153`): `allow` journals the original
proposal as the step's `Decision` **once** (guarded by `folded.decision.is_none()`,
so a crash after that append does not journal a second one) and the existing
replay branch dispatches it through `Intent → execute → Receipt`; `deny`
seeds `last_error` for a fresh proposal. The replay path never re-runs the
gate (replay is law — the human decided).

### 4. Plan mode (`engine.rs:5216`), including `review` nodes

`plan.rs` has a `review` node type (a kernel-owned `review_verdict` model
call over upstream outputs), so the brief's point 4 applies.

- A mutating `tool` node under `deny` is refused at admission
  (`action_policy`, `subject: plan_node`) and fails like any refused node
  (re-plan if ADR 0030 budget allows).
- Under `ask` (and for a `review` node when `review_verdict = "ask"`) the
  ADR 0025 **drain-then-pause** applies unchanged: no new admissions,
  in-flight siblings drain to receipts, then the run pauses with
  `branch` = the node id. One request at a time.
- On resume the recorded decision is consumed, the policy is not re-read:
  a tool node's `allow` dispatches it, `deny` fails it (`operator_denied`,
  the ADR 0025 source). For a **human review**, the decision *is* the
  verdict: journaled as the node's `Receipt` — `{"approve": true|false,
  "reason": "approved|rejected by a human reviewer", "reviewer": "<principal>"}`
  — in the same shape a model verdict has; the model is not called. The
  request carries `{"node", "deps", "upstream"}` (upstream outputs bounded
  by the ADR 0036 8 KiB rule) so the human sees what they approve.
- The approval is read from the full journal, not the per-step fold, so a
  node retry (ADR 0030, clean per-step view) of an approved node is not asked
  again; a node of a *new* plan generation is a new step and is asked again.

### 5. Exactly-once and restart

Nothing executes before a decision: the pause returns before any `Decision`
or `Intent` exists. After `allow`, the action goes through the unchanged
ADR 0007 boundary. Proven:

- `ask_pauses_then_allow_executes_exactly_once_without_requerying_the_model`
  — re-executing while paused calls neither model nor tool; after allow, one
  effect, two model calls total (proposal + finish);
- `crash_after_the_allowed_decision_is_journaled_still_executes_once` — a
  crash after the Allow's `Decision` and `Intent` were journaled resumes on
  the dangling-intent path with the journaled key; one `Decision`, one effect;
- `paused_approval_survives_a_restart_and_executes_once_after_allow`
  (`crates/daemon/tests/human_gate_restart_test.rs`) — the real SQLite store
  across three kernel instances; `unfinished_runs` returns the paused run, the
  resume sweep re-surfaces it (no model call, no effect), allow → once, a
  further restart resumes nothing;
- `scripts/smoke-human-gate-budgets.sh` leg A — the same on the real daemon
  with a real restart and the real ledger file.

`unfinished_runs` gained `awaiting_action_approval` in both stores; the
SQLite query is a status-string literal, so no migration. The audit H-1
regression test now lists the new status.

### 6. Timeout = deny

`daemon::approval_sweep::sweep_timed_out_action_approvals` (every 30 s, only
when some policy can ask and `ask_timeout_secs > 0`) calls
`decide_action_approval(…, Deny, "system:timeout")` and resumes the run.
`ask_timeout_secs = 0` disables it — then a pause waits for a human
indefinitely; that is an explicit operator choice, not the default (300 s).

### 7. API and events, additive only

- `POST /api/v1/runs/{id}/action-approval` `{"action": "...", "decision":
  "allow"|"deny"}` — bearer; 422 unknown decision, 409 when the run is not
  paused or `action` is not the pending one (a stale view cannot approve a
  different proposal), 404 unknown run; re-spawns the run like the other
  decision routes.
- `GET /api/v1/approvals` — the decision queue: every run paused on a human,
  across all four kinds (`action_approval`, `skill_activation`, `delegation`,
  `delegation_input`), oldest first, with the route that resolves each.
- WS: `action_approval_requested {step, action, arguments, branch,
  expires_at}`, `action_approval_decided {step, action, decision, principal,
  branch}` (emitted by the decision call itself, `seq` 0, like the other
  decided events). A2A maps the new status to `TASK_STATE_INPUT_REQUIRED`.
- Run record: `pending_action_approval`, `principal` (ADR 0039).

### 8. No model path

The policy is config. The decision is an authenticated API call (principal
journaled) or the sweeper's deny. Approval is no action in any schema; the
model's only influence is what it proposes, and that is what is gated.

## Consequences

- An operator can require a human before any state change, per tool, or
  forbid a tool outright, without touching the tool.
- `GET /api/v1/approvals` is the first cross-run view of "what is waiting on
  me"; it also lists the three older pause kinds.
- The web UI renders an inline prompt (`ActionApprovalPrompt.tsx`, allow once
  / deny, the arguments, the auto-deny time) and the `◉ Awaiting your
  decision` badge. A dedicated queue view over `/approvals` is not built.

## Known limits

- **Unkeyed side-effect tools are not gated** (see §1). A WASM or MCP tool
  that changes state without declaring `idempotency_key` passes untouched.
  The startup log lists configured names that are not mutating; it cannot
  find tools that should have been.
- **Concurrent decisions** (a human and the sweeper in the same instant) can
  both journal a decision; the later one wins in the fold. Shared with the
  three older decision paths — none takes a per-run lock.
- **The policy applies to fresh proposals.** A `Decision` journaled before a
  config change is replayed as decided (replay is law), and an undecided
  pause stays a pause after the policy for that action is relaxed — decide it.
- The decided event rides `seq` 0, which a seq-deduping client may drop
  (true of the older decided events too); the web UI therefore renders the
  prompt from the run record, not only the stream.

## Open (for the maintainer)

- Whether `ask_timeout_secs = 0` (wait forever) should be allowed at all.
- Whether an `[approval.actions]` entry naming a non-mutating action should
  be a startup error rather than a WARN.

### For Glassbox — the exact contract (input to KR4.5)

- New run status `awaiting_action_approval` — non-terminal; show "waiting for
  a decision". `run.pending_action_approval`: `{step, action, arguments,
  branch|null, requested_at, expires_at|null}`.
- Journal kinds: `action_approval_requested {step, action, arguments,
  branch?, requested_at}`; `action_approval_decided {step, action, decision:
  "allow"|"deny", principal, branch?}`. Pair by `(step, action)`. Not
  informational (they shape resume) but a reader may render them freely.
- WS: `action_approval_requested`, `action_approval_decided` (fields above,
  `branch` explicit `null` in step mode).
- `rejected.source` gains `action_policy` (config deny, `subject: proposal`
  in step mode, `plan_node` in plan mode); a human/timeout deny in plan mode
  is `operator_denied`. A human deny in step mode is not a `rejected` row —
  the `action_approval_decided` row is the record (ADR 0036's rule for human
  decisions).
- A human review verdict is a `receipt` whose result has `reviewer`.
- `GET /api/v1/approvals` for a queue view.
