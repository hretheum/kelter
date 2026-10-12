---
provenance: ai_authored
provenance_metadata:
  agent: claude-code
  session_date: 2026-10-05
  human_validated: false
  session_context: "KR1.1, gap L8: on a deployment the task 'send an e-mail' — no mail tool configured — was 'done' by writing a file. An effective action, so ADR 0033's floor granted `completed`, and distillation would have learned it. This ADR adds a caller-supplied, kernel-checked success contract and keeps unverified successes out of learning until a human confirms them."
---

# ADR 0037: The success contract, and `verification` on every success

Status: accepted (historical-data rule decided by the maintainer, 2026-10-05) · Date: 2026-10-05 · Builds on: ADR 0001 (kernel decides), ADR 0033 (success requires an effective action) · Touches: ADR 0007/0036 (journal), ADR 0010/0030 (plan mode), ADR 0013 (A2A inbound), ADR 0016/0032 (self-learning), ADR 0018 (principals), ADR 0021/0023 (human decisions)

## Context

ADR 0033 made the kernel, not the model, decide success — but with the only
predicate the kernel could check without knowing the task: *some dispatched
action came back ok*. ADR 0033 named the remaining gap itself: "a model that
lists a directory and then claims to have sent an e-mail still ends
`completed`." That gap was hit on a deployment: "send an e-mail" with no mail
tool ended `completed` after the model wrote a file. And `completed` feeds
forward: distillation (ADR 0016/0032) learns facts and plans from exactly
those runs, so a hallucinated success becomes a learned approach.

The kernel cannot infer what a task requires from its prose — that would need
a model, and a model's opinion is what is being checked. But the **caller**
often knows: "this task is done when `send_email` succeeded." The kernel can
check that deterministically against the trace it already records.

## Decision

### 1. An optional success contract, given at creation

`POST /api/v1/runs` takes an additive field `success_contract`:

```json
{
  "task": "Send the report to Bob",
  "success_contract": {
    "require": [
      { "kind": "action_succeeded", "action": "send_email" },
      { "kind": "result_equals",   "action": "send_email", "pointer": "/status", "equals": "sent" },
      { "kind": "result_contains", "action": "send_email", "pointer": "/id",     "contains": "m-" }
    ]
  }
}
```

A contract is a **conjunction**; each condition is **existential over the
run's steps** — some step of the named action that came back ok satisfies it:

| kind | holds when |
|---|---|
| `action_succeeded` | a step of `action` was dispatched and is `ok` |
| `result_equals` | some ok step of `action` has `result` whose value at the JSON Pointer `pointer` (RFC 6901, `""` = whole result) is JSON-equal to `equals` |
| `result_contains` | same, the value is a string containing `contains` (case-sensitive) |

That is the whole language. Every condition is a pure function of
`run.steps` — `action`, `ok`, `result` as recorded (after the ADR 0017 / step
redaction the trace already gets). No code from the contract runs, no regex
engine, no model call. Anything richer ("the e-mail body mentions the Q3
numbers", "the recipient is a real person") needs interpretation of content
and is **out of scope**: it would make a model the judge again. A caller who
needs it can expose a checking tool, and require that tool's success.

**Validation at creation** (`kernel_core::contract::validate`, called by
`Kernel::create_run_with_contract`), so a bad contract is a `422` before any
run exists — never a silent `incomplete` later:

- malformed JSON shape, unknown `kind`, unknown field (`deny_unknown_fields`)
  → `422 invalid success_contract: …`;
- empty `require`, more than 16 conditions, an expected value over 4 KiB;
- an `action` not in the action space the daemon offers right now (the same
  list as `GET /api/v1/tools`) → `422 … unknown action \`x\``. This is also
  what the deployment case gets: with no mail tool, "send an e-mail" with a
  `send_email` contract is refused up front, which is the honest answer;
- a kernel-owned action (`finish`, `activate_skill`, `deactivate_skill`,
  `review`) — it never dispatches anything, so it cannot evidence success;
- a `pointer` that is neither empty nor starts with `/`.

The action space can change while a run is in flight (an extension removed,
an agent deregistered). The contract is not re-validated: an action that is
gone simply never succeeds, and the run ends `incomplete` naming it.

**The model does not see the contract.** It is not in `PolicyContext`, the
system prompt or any action schema. Reasons: the check must not be something
the model can "satisfy by declaration" — `finish` arguments are never read by
the verdict, so that is already impossible — and showing it would make the
contract part of the instruction, which is the task's job. A caller who wants
the model to know "use `send_email`" says so in the task. Revisit if weak
models demonstrably need the hint; the change would be additive.

### 2. The verdict: ADR 0033 floor, then the contract

`outcome::verdict(steps, contract)` is the single success predicate, used at
both points where a run can succeed (unchanged from ADR 0033):

1. no effective step → `incomplete` with the ADR 0033 reason, contract or not;
2. a contract → `completed` iff every condition holds; otherwise
   **`incomplete`** with `error` naming every failing condition, e.g.
   `success contract not met (1 of 1 condition(s) failed): action
   \`send_email\` succeeded — \`send_email\` was never executed (the model's
   summary is kept as output, unverified)`;
3. no contract → `completed`, exactly as before.

**Plan mode uses the same, run-level semantics.** The contract is evaluated
over `run.steps` — every node of every generation that was recorded (a review
node is recorded as `review`, ADR 0033, so it can never impersonate a tool).
This matches ADR 0033's floor, which is also run-level, and needs no per-node
mapping. Per-node contracts are not part of this ADR.

`incomplete` keeps its ADR 0033 meaning ("the kernel withheld success; the
model's summary is kept as output, unverified"); a failed contract is a new
*reason*, not a new status.

### 3. `verification` — every success says how it was judged

Additive, on the run record, the terminal journal entry and the terminal
WS/SSE event:

| field | where | values |
|---|---|---|
| `verification` | `Run`, `JournalEntry::Terminal`, `RunEvent::Completed`, `RunEvent::Incomplete`, `GET /runs/{id}/events` top level | `"contract"` — checked against the caller's contract; `"none"` — no contract, ADR 0033 floor only (**unverified**) |
| `contract_report` | `Run`, `Terminal`, `Incomplete` event | `{ satisfied, conditions: [{ condition, met, detail }] }` |
| `success_contract` | `Run`, `GET /runs/{id}/events` | the contract as given |
| `confirmation` | `Run`, `GET /runs/{id}/events` | `{ principal, at_ms }` or `null` |

`verification` is set on every run whose success the kernel judged
(`completed`, `incomplete`); it is `null` while in flight, for
`failed`/`canceled`, and for runs that ended before this ADR. The terminal
journal entry carries it, so the crash-after-terminal finalise path restores
it without recomputing, and a replayed `finish` recomputes the same verdict
from the same trace.

### 4. Learning takes only verified or human-confirmed successes

`Kernel::distill` (facts and plans) now admits a run iff `Run::learnable()`:

- `status == completed` **and**
- `verification == contract`, **or** a human `confirmation` (for an
  unverified run: `verification == none`, or absent — historical, below).

Confirmation is a human decision with the ADR 0021/0023 shape:

- `POST /api/v1/runs/{id}/confirmation` confirms; `DELETE` revokes. Bearer
  (ADR 0018); the principal comes from the token.
- The kernel (`Kernel::decide_success_confirmation`) journals
  `kind: "success_confirmation_decided"` `{ decision: confirm|revoke,
  principal, at_ms }` **before** changing the run record, emits WS
  `success_confirmation_decided`, and returns the run.
- Only an unverified `completed` run (`Run::is_unverified_success`:
  `verification` `none` or absent) can be confirmed (`409` otherwise): a
  contract-verified run needs nothing; an `incomplete` run cannot be
  confirmed into success (that would be a human overriding the kernel's
  verdict, a different feature). Confirming does not invent a verification
  basis: `verification` stays as it was.
  Confirming twice / revoking nothing is `409` — every journaled decision
  changes state.
- **Revoking also forgets** every learned fact or plan whose
  `source_run_ids` is exactly `[this run]` (distillation creates one
  artifact per run, so that is all of them), and returns their ids as
  `forgotten_learned_ids`. A revoked success leaves no learning behind; the
  existing `forget` is reused, not changed. Re-confirming makes the run
  learnable again.

**No model path.** Confirmation is not an action, appears in no action
schema, and is reachable only through the authenticated API. A model that
proposes `confirm_run` is refused as an unknown action; a `finish` that
claims "verified" changes nothing (tested).

**Known boundary: "a human" means "a holder of an API token".** The kernel
records the principal `auth_gate` resolves from the bearer token (ADR 0018);
it cannot tell a person from a program holding that token. An API client
running as an agent — including an orchestrating model given the token — can
technically confirm. What this ADR guarantees is narrower: the *model inside
this run* has no path, and every confirmation is attributed to a principal
in the journal. With one operator that is acceptable; a deployment that
hands tokens to automation should give it a named principal (so its
confirmations are distinguishable) and, if needed, a later ADR can restrict
confirmation to designated principals.

**Retention / `forget` do not collide.** Pruning a run removes its
confirmation with its journal; learned artifacts already survive their
source run's pruning (unchanged). `forget` of an artifact does not touch the
run's confirmation — the run stays learnable and a later `distill` may learn
from it again, exactly as for any other learnable run today.

`success_confirmation_decided` is **informational** for execution
(`JournalEntry::is_informational`): written after `Terminal`, read by no
resume, replay or exactly-once scan, listed in `fold_step`'s explicit
ignore arm.

### 5. Historical data — pre-ADR runs are unverified (maintainer's decision)

A `completed` run written before this change has no `verification` key. It
was judged by the ADR 0033 floor only, so it **is** unverified, and it is
treated exactly like `verification: none`: **not learnable until a human
confirms it**, and confirmable through the same endpoint (tested:
`pre_adr_0037_completed_runs_are_unverified_until_confirmed`). Decided by
Eryk on 2026-10-05 over the compatible alternative this ADR first proposed
("pre-change = learnable as before").

What this does and does not touch:
- **Already-learned facts and plans stay.** The gate only changes what
  *future* distillation reads; nothing in the learned store is removed by
  upgrading (tested: `already_learned_artifacts_survive_the_gate`).
- Future `distill` calls skip every old `completed` run until it is
  confirmed. A deployment that wants an old run's lesson re-learned confirms
  it; the `already` check still prevents a duplicate if it was learned before.
- Revoking a confirmation of an old run forgets every artifact learned from
  that run alone — including one distilled before this ADR. Revoke means
  "this success was not real"; its lesson goes with it.

### 6. A2A inbound (ADR 0013)

An inbound A2A task carries no contract (A2A has no field for one; a
`metadata` convention could add it later). So an inbound success is
`completed` with `verification: none` and maps to `TASK_STATE_COMPLETED` with
its artifact — no behaviour change for A2A callers. The basis rides along
additively as task `metadata: { "kelter.verification": "none" | "contract" }`.
An `incomplete` run (either reason) stays `TASK_STATE_FAILED` with the reason
as status message and no artifact (ADR 0033).

## What does not change

- Without a contract, statuses, outputs, events (apart from the new fields),
  exactly-once and resume behave exactly as before.
- Reflection episodes (`memory`, ADR 0005) still record an unverified
  `completed` run as `Success`. Episodic retrieval is not distillation; gating
  it is a separate decision and is listed as an open item.
  *(Addendum, 2026-10-05: closed by ADR 0040 — an unverified `completed`
  run's episode is now `unverified`, and the confirmation endpoint below
  moves it, versioned.)*
- `adopt_learned_plan` (ADR 0032) consumes learned plans as before — it simply
  sees fewer of them.

## Compatibility

- **API / WS:** additive only. New optional request field; new run fields
  (always present in new records, `null` when not applicable); new fields on
  `completed`/`incomplete` events; a new event type; a new route. Nothing
  existing changes shape.
- **Storage:** no migration. Runs are JSON in `runs.data`; new fields are
  `#[serde(default)]`. A pre-ADR `Terminal` entry parses (tested).
- **Downgrade** (same policy as ADR 0036 — not supported, rollback by
  `DELETE`): an older binary ignores the new fields, but cannot parse a
  `success_confirmation_decided` row — `GET /events` 500s and `distill`
  errors for a confirmed run. The rows are informational for execution, but
  **not** for learning: deleting them is lossless for resume and loses only
  the record of who confirmed. Before running an older binary:
  `DELETE FROM journal WHERE json_extract(entry, '$.kind') = 'success_confirmation_decided';`
  An older binary also learns from unverified runs again, since it has no gate.
- **Upgrade:** after this change `distill` learns nothing from runs that
  ended before it until they are confirmed (section 5); existing learned
  artifacts are untouched.
- **Desktop** (`apps/desktop/src-tauri`) constructs a `Run` literal in a
  test; updated, and its fmt/clippy/test re-run.

## Consequences

- With a contract, "wrote a file instead of sending the e-mail" is
  `incomplete`, names `send_email`, and is never learned.
- Without one, it is still `completed` — but labelled `verification: none`
  everywhere a client looks, and invisible to learning until a person vouches
  for it. Learning becomes opt-in per unverified run: deployments that relied
  on `distill` learning from every completed run must now either send
  contracts or confirm runs (`scripts/smoke-selflearn.sh` now confirms).
- The web UI shows the basis on a completed run, a "Confirm it did the task"
  / "Revoke confirmation" control for unverified ones, and "Success contract
  not met" on a contract-failed `incomplete`. It cannot yet *submit* a
  contract (the composer has no field) — an API-only feature for now.
