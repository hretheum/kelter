---
provenance: ai_authored
provenance_metadata:
  agent: claude-code
  session_date: 2026-10-04
  human_validated: false
  session_context: "K12 acceptance (2026-10-04), finding N2 'success without action': an infeasible task ended `completed` because the model called `finish` at step 0. The kernel now decides success; a run with no effective action ends in a new terminal status, `incomplete`."
---

# ADR 0033: Success requires an effective action — the `incomplete` terminal status

Status: accepted · Date: 2026-10-04 · Builds on: ADR 0001 (kernel decides, model proposes), ADR 0007 (journal), ADR 0010 / 0030 (plan mode), ADR 0016 / 0032 (self-learning reads only successful runs) · Trigger: K12 acceptance finding N2

## Context

On K12 (engine `local:gemma-3-27b`, `mode = "step"`) the task "send an
e-mail" — infeasible, there is no such tool — ended **`completed`**. The model
proposed `finish` as its very first action, with a summary claiming the work was
done. Nothing ran.

`finish`'s description already says "use only after the task's output has
actually been produced". That is an instruction to the model, and ADR 0001's
whole premise is that the kernel does not rely on the model following
instructions. Before this ADR the kernel turned any validated `finish` into
`RunStatus::Completed` unconditionally. So the one status a caller reads as
"done" was granted on the model's word alone — the inversion ADR 0001 exists to
prevent, at the exact point where it matters most.

The cost is not only a wrong badge. `completed` feeds forward:
reflection writes the episode as `Success`, distillation (ADR 0016 / 0032)
learns facts and plans only from `completed` runs, and an inbound A2A caller
(ADR 0013) receives `TASK_STATE_COMPLETED` with the summary as its artifact. A
hallucinated success becomes a learned success and a delegated success.

## Decision

**The model proposes `finish`; the kernel decides whether the run earned
success.** A run reaches `Completed` only if at least one **effective step**
happened. Otherwise it ends in a new terminal status, **`Incomplete`**.

### What counts as effective (`kernel_core::outcome`)

A step is effective when it is **ok** and it is **not kernel-owned** — that is,
the kernel actually dispatched it to an `ActionSource` (an MCP tool, a WASM
module, a remote A2A agent) and got a success back. Kernel-owned actions never
count, even when "ok": `finish`, `activate_skill`, `deactivate_skill`, and a plan
`review` verdict. They change advisory context or record a verdict; nothing
leaves the kernel.

The predicate lives in one place (`outcome::effective_steps`) and is applied at
both points where a run can succeed:

- **step mode** — when the validated action is `finish`;
- **plan mode** — when the last generation finishes with no failed node.

A plan that is only `review` nodes therefore ends `Incomplete`, exactly like a
step-mode run that only finishes. While wiring plan mode a loophole surfaced: a
review node that carried an `action` field (ignored at execution) was recorded
under that action's name, so a review "named" `send_email` would have looked
like an executed `send_email`. Review nodes are now always recorded as
`review`. That is an observability fix in its own right, and required for the
predicate to be sound.

### Why a new status, not "finish must carry a failure reason"

The alternative was to change `finish`'s schema so the model must state success
or a failure reason. Rejected, for three reasons:

1. **It still trusts the model.** The K12 model would have declared success; a
   self-report field moves the claim into a different argument without making
   the kernel check anything. The bug is that the *kernel* never checked.
2. **It changes the action space every adapter decodes.** `finish` is in every
   GBNF grammar (ADR 0003) and every native tool list; a required field changes
   what weak local models must emit on every run, for every task — exactly the
   population that produced N2. The status decision costs zero extra tokens and
   zero extra model calls.
3. **It is not additive.** An extra required argument breaks every client,
   prompt and learned plan that assumes the old `finish` shape. A new enum value
   is additive.

A third option — reject the premature `finish` and re-prompt ("you have not
done anything yet") — was also considered. On an infeasible task it only burns
the retry budget and ends `Failed` with a misleading "no valid action" error,
or pressures the model into an irrelevant tool call to get past the gate.
Deterministic, immediate `Incomplete` is honest and cheap.

### Shape of an `incomplete` run

- `status = "incomplete"` (terminal; excluded from `unfinished_runs`, so never
  resumed; prunable by retention like every terminal status).
- `output` = the model's own `finish` arguments (step mode) or the plan summary
  (plan mode), **kept but unverified** — useful to a human, never treated as a
  result.
- `error` = the kernel's reason, which distinguishes "nothing was attempted"
  from "N attempted, all failed".
- Journal: `JournalEntry::Terminal { status: Incomplete, .. }` — durable, and
  replay reaches the same verdict without the model.
- Event: a new `RunEvent::Incomplete { output, reason }` (WS/SSE `type:
  "incomplete"`), never `completed`.
- Reflection: episode outcome `Failure`. Distillation already reads only
  `Completed`, so nothing is learned from it.
- A2A inbound: `TASK_STATE_FAILED` with the reason as the status message and
  **no** artifact — the unverified summary is never served as a result. A2A has
  no "unverified" state, and anything else would let a caller read it as
  success.
- Metrics (OTel): `runs_finished{outcome="incomplete"}`.
- Web UI: an "Incomplete" badge and a result face that shows the kernel's
  verdict first and the model's summary only under "unverified".

### Compatibility

- **API.** Additive only: one new `RunStatus` value and one new `RunEvent` type.
  Every existing field and shape is unchanged. A client that switches on status
  must treat an unknown value as not-success — which a well-behaved client
  already does.
- **Behaviour.** This *is* a behaviour change, deliberately: runs that used to
  end `completed` with no successful action now end `incomplete`. Seventeen
  existing tests encoded the old behaviour incidentally (a refused delegation, a
  denied skill, a rejected out-of-space action, then `finish`). Each was read
  individually and updated with a comment; in every case the run had done
  nothing effective, so `incomplete` is the correct answer, not a weakened
  assertion. Cases where an action did succeed (an allowed delegation, a
  recovered circuit) still assert `completed`.
- **Database.** No migration. SQLite stores `status` as TEXT (serde
  snake_case); `'incomplete'` is a new value in an existing column. The
  retention query's terminal list gained `'incomplete'` (pinned by the existing
  SQLite status-list test, extended). `pgstore` holds memory and learned data
  only, no runs — untouched. A database written by this version is readable by
  an older binary *except* that an older binary cannot deserialize the new
  status value; downgrade is not supported, as with every previous status
  addition (ADR 0021/0023/0026).

## What this does NOT claim

The kernel cannot tell a read from a write. `ActionSpec` carries no side-effect
classification, and a tool's own "read-only" hint would be untrusted extension
input (ADR 0006). So "effective" means *the kernel executed something and it
succeeded* — the floor below which success is provably unearned — not *the
requested effect happened*. A model that lists a directory and then claims to
have sent an e-mail still ends `completed`. Closing that gap needs a verifier
(a review step against the task, or typed effect declarations the kernel can
check), not a counter. This ADR closes the K12 case — success with nothing
done — and names the rest instead of pretending it is closed.

Pure question-answering runs ("what is 2+2?") that finish without any tool now
end `incomplete`. That is intended: the kernel has nothing to vouch for, and
the answer is still there in `output`. A deployment that wants such runs to
count as success needs a tool that produces the answer (and so becomes the
effective step), not a kernel that takes the model's word.

## Evidence

- Unit: `crates/kernel-core/src/outcome.rs` (the predicate, 6 tests).
- Real runs with a scripted oracle (the pattern of
  `crates/kernel-core/tests/engine_test.rs`):
  `crates/kernel-core/tests/no_effect_finish_test.rs` — the exact K12 trace
  (finish at step 0 → `incomplete`, journal + event + reflection checked),
  finish after only failed actions, the unchanged success path, terminal/no
  resume, and plan mode (review-only plan, review node naming a tool).
- A2A mapping: `crates/a2a/src/server.rs` tests.
- End-to-end with a real WASM tool: `crates/wasmhost/tests/zapisz_plik_test.rs`
  (a refused write then `finish` → `incomplete`; a real write → `completed`).
- Build evidence: `docs/BUILD-EVIDENCE.md`, 2026-10-04 entry.
