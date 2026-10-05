---
provenance: ai_authored
provenance_metadata:
  agent: claude-code
  session_date: 2026-10-05
  human_validated: false
  session_context: "KR0.5: the run history (GET /runs/{id}/events) lacked refusals and resumes — both existed only on the live WS stream and in logs, so Glassbox had to pair a rejection with a proposal by seq+1 and could not see a resume at all. An export of 142 deployed runs (2026-10-05) held only decision/intent/receipt/note/terminal/trust_posture entries; zero refusals."
---

# ADR 0036: Refusals and resumes are journaled — informationally

Status: accepted (downgrade policy open for the maintainer — see "Compatibility") · Date: 2026-10-05 · Builds on: ADR 0007 (journal + resume) · Touches: ADR 0017 (retention), 0020/0022/0023/0025/0030/0031 (the refusal sources)

## Context

The journal (ADR 0007) records what was *decided* and what was *done*. What
the kernel *refused* never reached it: every `RunEvent::Rejected` was
emit-only. Neither did a resume: the replay path logged
`"resume: replaying journaled decision"` and streamed
`rationale: "replayed from journal"`, nothing durable. So the run history
`GET /api/v1/runs/{id}/events` — the only history a client gets after the
fact — showed a run that never stumbled and never restarted.

Glassbox (the visualiser) worked around this by pairing a live `rejected`
with the `proposed` at `seq - 1`. That is wrong for a whole class of
refusals: a delegation retry or fallback refused *after* the step's proposal
was accepted and dialed sits right after that accepted proposal on the stream
— the pairing marks an accepted proposal as rejected. The test
`retry_failures_and_fallback_refusals_refer_to_the_accepted_decision` pins
exactly that: the first refusal is at `accepted_seq + 1`.

## Inventory — every refusal path

17 `RunEvent::Rejected` emission sites in `crates/kernel-core/src/engine.rs`
(line numbers at `a143922` → after this change). Every one now goes through
one helper, `Kernel::reject`, which journals and emits. There are no others:
`grep -c 'RunEvent::Rejected {' engine.rs` is 1 — the helper.

| # | at a143922 | now | mode | what is refused | `source` | `subject` |
|---|---|---|---|---|---|---|
| 1 | 1636 | 1775 | step, resilience | a retry, by budget/depth/cycle | `delegation_limit` | `decision` |
| 2 | 1652 | 1791 | step, resilience | a retry, by the trust gate | `delegation_gate` | `decision` |
| 3 | 1685 | 1824 | step, resilience | a failed delegation attempt → retrying | `delegation_retry` | `decision` |
| 4 | 1719 | 1858 | step, resilience | a sanctioned fallback, circuit open | `circuit_open` | `decision` |
| 5 | 1739 | 1878 | step, resilience | a fallback, by budget/depth/cycle | `delegation_limit` | `decision` |
| 6 | 1754 | 1893 | step, resilience | a fallback, by the trust gate | `delegation_gate` | `decision` |
| 7 | 2381 | 2576 | step | nothing — the model call failed | `policy_error` | `none` |
| 8 | 2468 | 2688 | step | a proposal repeating one in the state epoch (ADR 0031) | `duplicate` | `proposal` |
| 9 | 2515 | 2735 | step | a delegation proposal, by budget/depth/cycle or no origin | `delegation_limit` | `proposal` |
| 10 | 2545 | 2765 | step | a delegation proposal, by the trust gate | `delegation_gate` | `proposal` |
| 11 | 2634 | 2854 | step | an `activate_skill` proposal, by the skill gate | `skill_gate` | `proposal` |
| 12 | 2727 | 2948 | step | a proposal failing schema validation | `schema` | `proposal` |
| 13 | 3594 | 3819 | plan | a revised plan failing plan validation (ADR 0030) | `plan_validation` | `proposal` |
| 14 | 3845 | 4097 | plan, resume | an a2a node the operator denied (ADR 0025) | `operator_denied` | `plan_node` |
| 15 | 3971 | 4229 | plan | an a2a node, by budget/depth/cycle/no origin — or circuit open | `delegation_limit` / `circuit_open` | `plan_node` |
| 16 | 4007 | 4260 | plan | an a2a node, by the trust gate | `delegation_gate` | `plan_node` |
| 17 | 4133 | 4387 | plan | a failed node attempt → retrying (ADR 0030) | `node_retry` | `plan_node` |

Site 15 used to merge two refusals into one string; the source is now kept
apart (`delegation_limit` vs `circuit_open`) — the live reason text is
unchanged.

Not refusals, and deliberately not in this list: a plan-mode proposal that
fails at the policy call or schema (`?` — the run errors, no `Rejected` was
ever emitted), an `adopt_learned_plan` naming an unknown id (re-asked, logged
only), a step-mode first dial into an open circuit (a failed *result*,
journaled as a `Receipt`), and human Deny decisions (already journaled as
`SkillActivationDecided` / `AgentDelegationDecided`).

Resume paths: one — `execute_run_inner` after the three "still awaiting"
early returns, for both modes (`execute_plan` is entered from there).

## Decision

### 1. `rejected` — one entry per refusal, with an explicit subject

```json
{"kind":"rejected","step":2,"attempt":1,"source":"delegation_retry",
 "subject":"decision","reason":"…","action":"delegate_to_primary","at_ms":1759651200000}
```

`subject` is the linkage, structural and never inferred:

| `subject` | refers to | carried inline |
|---|---|---|
| `proposal` | a fresh model proposal that was never accepted — journaled nowhere else | `action`, `arguments` (bounded) |
| `decision` | this `step`'s `Decision` entry (one per step — the accepted proposal) | `action` = the agent actually attempted (differs from the Decision's for a fallback) |
| `plan_node` | node `branch` of plan generation `step / 100` (`PLAN_GENERATION_STRIDE`) — i.e. the `PlanProposed` of that generation | `action` = the node's action |
| `none` | nothing was proposed (the model call failed) | — |

`attempt` is the live event's `attempt`, unchanged (it is not unique per
step: resilience retries and fallbacks each count from 1 — `source` plus
`reason` disambiguate, exactly as the web UI already dedupes). `branch` is
present for plan-node refusals. `at_ms` is wall-clock milliseconds.

Ordering is append order, so for a `proposal` refusal the entry precedes its
step's `Decision`; for a `decision` refusal it lies between that step's
`Intent` and `Receipt`.

No ambiguous path was found: every site knows statically which of the four
subjects it refuses. Nothing is linked by heuristic, and `seq` semantics are
untouched.

### 2. `resumed` — one entry per resumed execution

```json
{"kind":"resumed","at_step":0,"session":"b694c417-…","cause":"interrupted",
 "replayed_steps":[0],"at_ms":1791206366287}
```

- `session` — `journal::execution_session_id()`: a UUID generated once per
  process, shared by every kernel in it, and logged by the daemon at startup
  (`execution session session=…`). Two `resumed` rows of one run differ in
  session exactly when the process did.
- `at_step` — where execution continues: step mode, the first step not yet
  recorded; plan mode, the active generation's step base.
- `replayed_steps` — steps whose decision this execution takes from the
  journal instead of the model: step mode, `[at_step]` when a `Decision` (or
  a resolved Allow on a human ask, whose decision is the model's journaled
  original proposal) exists for it, else `[]`; plan mode, the active
  generation's base when a `PlanProposed` exists. This is exactly the set the
  replay branch will serve, computed from the same journal snapshot.
- `cause` — `pause_resolved` when the last non-informational entry resolves a
  human pause (`SkillActivationDecided`, `AgentDelegationDecided`, or the ADR
  0026 continuation note), else `interrupted` (crash, kill, restart).

A resume is "an earlier execution left durable state": any recorded step, or
any non-`Note` journal entry. The old test (`!journal.is_empty()`, which only
gated a log line) fired on every API-created run, because the API journals a
caller-attribution `Note` before the first execution; the new predicate
writes no `resumed` for a first execution
(`a_first_execution_writes_no_resume_even_after_a_caller_note`). Notes an
execution writes always follow an `Intent` or a `*Requested` entry, so the
predicate needs no list of note keys.

### 3. Live stream: one additive field

`RunEvent::Rejected` gains `proposal_seq: Option<u64>` — the `seq` of the
`Proposed` event it refers to in the same execution's stream (the refused
proposal; the step's accepted or replayed proposal for a `decision` refusal;
the plan-level proposal for a `plan_node` refusal), `null` when none exists
(a failed model call, or a revised plan rejected before it was announced).
Nothing else in any event changes shape; `reason` is byte-identical.

### 4. Informational: resume never reads them

The contract: no resume, replay, exactly-once or budget logic may depend on
these entries, and they never reach the model's context.

How it holds, concretely:
- `fold_step` has an explicit no-op arm for both kinds (so a future arm above
  cannot start matching them); every other journal scan in the kernel matches
  specific kinds (`PlanProposed`, `Receipt`, `TrustPosture`, `Terminal`, the
  `*Requested`/`*Decided` pairs, `Note` by key) — checked one by one.
- The `resumed` entry is appended *after* the execution loaded its journal
  snapshot, so the execution that writes it cannot see it.
- `JournalEntry::is_informational()` names the two kinds for any reader.
- Both are written best-effort: a failed append is logged
  (`journal: could not record …`) and the run continues — an informational
  write must not be able to change what the run does.

Proof by test (`crates/kernel-core/tests/journal_rejections_resumes_test.rs`):
the same crashed journals — step mode at three crash points (after the
decision, in the intent→receipt window, after the effect) and a plan-mode
crash mid-plan — resumed with and without informational entries interleaved
at every position (including ones naming the resumed step and a stale
`resumed` claiming a replay) give an identical run record, status, output,
model-call count, *everything the model was shown*, tool executions and
non-informational journal. The existing durability, plan-resilience and
delegation tests pass without any assertion change.

### 5. Size: bounded per entry, not capped per run

Refusals repeat when a weak model loops, so the count was considered. The
count is already bounded by the run's own budgets — at most `max_retries`
(≤ 4) per step × `max_steps`, plus `max_retries_per_delegation` + the
fallback list per delegation step, plus `max_node_retries` per node and
`max_replans` per plan — linear in configuration the operator chose. A count
cap or an "N more refusals" summary would hide exactly the loop pathology the
history exists to show, so there is none.

What is unbounded is the payload: a refused proposal's arguments are
model-generated. So each entry is bounded: `arguments` over 8 KiB serialized
become `{"truncated":true,"bytes":N,"preview":"<first 1024 chars>"}`;
`reason` is cut at 2000 characters. Both are passed through the E3 secret
scrubber (`redact`) first — a refused proposal is still model output, and a
retry reason can echo a remote error. The live event is not altered. Typical
entry: ~300 bytes; worst case per run at defaults (12 steps × 3 attempts) is
well under 0.5 MB.

`resumed` adds one row per resumed execution.

Retention (ADR 0017) prunes a terminal run's journal by `run_id`; the new rows
go with it, no special case (pinned in
`crates/storage/tests/journal_informational_test.rs`).

### 6. Where it is stored

Only SQLite (`storage::SqliteRunStore`) and the in-memory store hold
journals. The Postgres backend (ADR 0028, `pgstore`) shares memory and learned
facts only — it has no journal, so there is nothing to change there. Both
journal stores serialize `JournalEntry` as one JSON value per row; no schema
or migration change.

## Compatibility

**Backward (new binary, old journal): unchanged.** The two variants are
purely additive; every pre-0036 row parses and re-serializes byte-identically
(`a_pre_0036_journal_still_parses`). Resuming an old in-flight run behaves
exactly as before; the only difference is the `resumed` row it adds.

**Downgrade (old binary, new journal): breaks per run — decision for the
maintainer.** `JournalEntry` is an internally tagged enum without a catch-all,
so a binary at or before `a143922` fails to deserialize *the whole journal* of
any run containing a `rejected` or `resumed` row. Concretely, on that binary:
- `GET /runs/{id}/events` for such a run → 500;
- resuming such a run (startup sweep, decide, continue) → error, run stays
  unfinished;
- `POST distill` → errors for the whole batch if any completed run in it has
  such a row (it reads each run's journal).
Runs without the new rows are unaffected. Weak-model runs almost always have
a refusal, so after any real use most runs will.

This ADR does not add a forward-compatible catch-all (`#[serde(other)]`).
Silently skipping unknown kinds is only safe while every future kind is
informational; a future load-bearing kind skipped by an old binary would break
exactly-once silently instead of failing loudly. That trade belongs to the
maintainer.

Options, in the order I would recommend them:
1. **Accept, with a documented rollback.** Because the rows are
   informational, deleting them is lossless for execution. Before running an
   old binary on a database this version wrote:
   `DELETE FROM journal WHERE json_extract(entry, '$.kind') IN ('rejected','resumed');`
   (stop the daemon first). History loses the refusals/resumes; nothing else.
2. Add a catch-all variant *now*, scoped to informational kinds by
   convention, so the *next* additive kind downgrades safely — and accept the
   silent-skip risk above.
3. A config switch to stop writing the entries, for a staged rollout.
   Not built: it only helps if flipped before the rows exist.

## Consequences

- The run history now answers "what was refused, and what exactly" and "did
  this run restart, when, in which process, and what did it replay" without
  the live stream. Glassbox can drop the `seq + 1` pairing (contract below).
- The resume log line no longer fires on a fresh API run.
- The web UI's replay folds `rejected` entries into the step's retry list
  with the same dedupe key as live events, so a replayed axis shows retry
  detail; `resumed` is accepted and not rendered.
- `scripts/smoke-journal-history.sh` exercises both against the real daemon,
  SQLite and the ledger MCP server, with a scripted model stub (smoke scripts
  stay out of CI, per CLAUDE.md).

### For Glassbox — the exact contract

- New `kind`s: `"rejected"`, `"resumed"`; both may be ignored safely.
- `rejected`: `step` u32, `attempt` u32, `source` one of `policy_error`,
  `schema`, `duplicate`, `delegation_limit`, `delegation_gate`, `skill_gate`,
  `circuit_open`, `delegation_retry`, `node_retry`, `plan_validation`,
  `operator_denied`; `subject` one of `proposal`, `decision`, `plan_node`,
  `none`; `reason` string; optional `action` string, `arguments` any (only
  with `proposal`; may be the truncation object), `branch` string (plan
  nodes); `at_ms` u64. Absent optionals are omitted, never `null`.
- `resumed`: `at_step` u32, `session` string (UUID), `cause` one of
  `interrupted`, `pause_resolved`; `replayed_steps` u32[]; `at_ms` u64.
- WS `rejected` gains `proposal_seq` (number or `null`); pair by it, not by
  `seq - 1`. `proposal_seq` is scoped to one execution's stream (the kernel
  restarts `seq` at 1 per execution, as before).

## Known limits

- A run killed before its first durable write (empty trust posture, killed
  inside the very first model call) leaves no trace; its resume is
  indistinguishable from a first execution and writes no `resumed`. Nothing is
  replayed in that case, so `replayed_steps` would have been `[]` anyway.
- `cause` is derived from the journal: a crash right after a human decision
  but before the released execution wrote anything is labelled
  `pause_resolved` (the session id still shows the process changed).
- A plan-mode delegation refusal that is not followed by a journaled
  `Receipt` (fail-fast refusals record only a run step) is journaled again if
  the run crashes and the same node is refused again on resume — correct (it
  was refused twice), but two rows.
