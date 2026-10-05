---
provenance: ai_authored
provenance_metadata:
  agent: claude-code
  session_date: 2026-10-05
  human_validated: false
  session_context: "KR1.3: a run could spend without bound — tokens, dollars, wall time — limited only by max_steps, and a principal could start any number of them. Cost was computed (Phase 8) but only reported, after the fact. This ADR adds per-run and per-principal-daily ceilings that the kernel enforces before each model call and action, so a limit is never crossed by the call it refuses."
---

# ADR 0039: Cost budgets, enforced before they are crossed

Status: accepted (maintainer decisions 2026-10-05 recorded under "Decided") · Date: 2026-10-05 · Builds on: Phase 8 item B (pricing, `cost.rs`), ADR 0022 (the governed-refusal shape of the delegation budget), ADR 0036 (refusals journaled), ADR 0018 (principals) · Touches: ADR 0033/0037 (terminal statuses), ADR 0038 (pauses excluded from wall time)

## Context

`cost_usd` exists since Phase 8, computed from journaled token usage — at
the end of the run, for the report. Nothing stopped a run from spending: a
weak model looping on refused proposals burned tokens for `max_steps ×
max_retries` calls; a slow tool or model held a run for as long as it took;
a principal could start runs without limit. ADR 0022 had already shown the
shape a governed limit takes in this kernel — a journaled, emitted refusal,
never a silent stop — for delegation attempts. This ADR applies it to cost.

## Decision

### 1. What is limited

```toml
[budget]
max_usd = 0.50                 # per run, under the [pricing] table
max_tokens = 200000            # per run: prompt + completion, all model calls
max_wall_secs = 1800           # per run: wall clock from creation − human pauses
daily_usd_per_principal = 5.0  # per API principal, per UTC day
prompt_overhead_tokens = 1024
fallback_completion_tokens = 4096
```

Per run, overridable at creation: `POST /api/v1/runs {"budget":
{"max_usd", "max_tokens", "max_wall_secs"}}`. **A configured limit is a
ceiling**: the override may lower it, never raise it (422 naming the field);
a dimension the config leaves open may be set to any positive value
(`BudgetLimits::with_override`, `budget.rs:42`). The effective limits are
fixed on the run record at creation (`run.budget`) — for API runs *and* for
runs created otherwise (A2A inbound, CLI), which get the config's per-run
limits. No `[budget]`, no override → `run.budget = null`, no check runs at
all: the pre-0039 behaviour.

### 2. Before, not after — the call bound

Before every model call the kernel asks the adapter for the call's **worst
case**: `PolicyOracle::call_bound(context, schema) -> CallBound
{prompt_tokens, completion_tokens}` (`oracle.rs:125`), a new trait method
with a default. The kernel adds `prompt_overhead_tokens` and, when the
adapter reports no completion bound, `fallback_completion_tokens`
(`Kernel::call_bound`, `engine.rs:2022`). The call is made only if

- `used_tokens + worst_case ≤ max_tokens`,
- `cost(used) + cost(worst_case) ≤ max_usd` (priced models only, §4),
- `principal_spent_today + cost(used) + cost(worst_case) ≤ daily_usd`,
- `elapsed < max_wall_secs`.

Otherwise the call is not made. The predicate is the pure function
`budget::check` (`budget.rs:199`).

**Why the bound is honest.** The three adapters render their real prompt in
`call_bound` (`policy/src/{openrouter,local}.rs`) — the same
`system_prompt*`/`user_prompt_with_limit` text and `tools` payload `propose`
sends — and return its **byte count** plus 16 per message for the chat
template, and their real `max_tokens` (default 512 local GBNF, 1024
native/OpenRouter). Every tokenizer these backends run (byte-level BPE:
GPT/Llama 3/Qwen; SentencePiece with byte fallback: Gemma, Llama 2) emits at
most one token per byte of text, so bytes bound tokens from above. What an
adapter cannot see — a provider's hidden tool-use preamble (Anthropic adds a
few hundred tokens), an OpenRouter-side template — is what
`prompt_overhead_tokens` (default 1024) covers. `SwappableOracle` and the
skills wrapper (`SkillContextOracle`, which adds skill text to the prompt)
delegate, the wrapper with its augmented context, so the bound sees the
prompt actually sent. The bound is deliberately pessimistic (real usage is
typically a quarter of the byte count): a budget stops early rather than
late.

**Every call counts.** `run.usage` used to aggregate only *accepted*
proposals: a proposal refused by schema, the duplicate rule or a gate, and
one that paused for a human, never reached it — so a looping model was
invisible to anything built on `usage`. Usage is now added at the moment the
model answers (`engine.rs:3362`), before validation. **This is the one
change visible without any new config**: `usage`/`cost_usd` of runs with
refused proposals now report what they actually consumed (the journaled
`Decision.usage` is unchanged). Pinned by
`refused_proposals_count_against_the_token_budget`.

### 3. Where the checks run

| checkpoint | mode | file:line | subject of the `rejected` row |
|---|---|---|---|
| before each proposal call (every attempt) | step | `engine.rs:3300` | `none` |
| before dispatching an action (replayed or fresh) | step | `engine.rs:3899` | `decision` |
| before a plan / re-plan proposal | plan | `engine.rs:4607` | `none` |
| before admitting a node (a review node = a model call, its bound reserved) | plan | `engine.rs:5366` | `plan_node` |

Plan-mode review nodes run concurrently (ADR 0022): at admission their bound
is **reserved** against siblings still in flight and released when their
real usage lands, so a fan-out of reviews cannot jointly overcommit. A
budget stop in plan mode is fail-fast (no new admissions, in-flight siblings
drain) and suppresses ADR 0030 node retries and re-plans — a re-plan is a
model call the budget just refused.

**A dangling intent is never refused.** Before an action the check is
skipped when the step already has an `Intent` (in-flight before a crash) or a
`Receipt`: the effect may have happened, and finishing it beats leaving it
ambiguous — the same "replay is law" reading as ADR 0022's governance skip.

### 4. A model without a price

`PricingTable::cost` returns `None` for a model neither built in nor in
`[pricing]` — the kernel never fabricates a price. For such a run `max_usd`
and the daily ceiling **cannot be evaluated and are not enforced**;
`max_tokens` and `max_wall_secs` are. It is recorded, not guessed:
`run.budget.usd_enforced = false`, and a WARN at creation when `max_usd` is
set. Pinned by `unpriced_model_disables_the_dollar_limit_but_not_the_token_limit`.
An operator who needs a dollar ceiling on such a model prices it in
`[pricing]`, or caps tokens.

### 5. Wall time

`now − created_at − paused_secs`. `paused_secs` accumulates the time every
human pause took (all four decision paths add `decided_at − requested_at`),
so a run waiting on an approval (ADR 0038) does not burn its budget. Wall
time cannot be reserved ahead, so it is checked as "already ≥ max" — before
each model call and each action. A single long call or tool that starts
within budget can overrun it; the next checkpoint stops the run. It is not
interrupted mid-flight: cancelling a tool mid-effect would open exactly the
crash window ADR 0007 closes. Time the daemon is down counts (it is wall
time).

### 6. Per principal per day — derived, not a counter

`daily_usd_per_principal` sums, over the run records, what the principal's
**other** runs created in the current UTC day have spent — `cost_usd` for
finished runs, the price of the recorded `usage` for in-flight ones — plus
this run's own usage (`principal_spend_today`, `engine.rs:2039`). The run
record gains `principal` (set by the API from the bearer's principal;
`"default"` in the single-token flow). Nothing new is stored and the SQLite
schema is unchanged: `principal` lives inside the run's JSON `data`, like
every other `Run` field. The stop condition "a counter needs a schema
migration" was therefore not hit — by not building a counter. Re-read before
every model call, so a concurrent run's spend is seen at the next call.

**Decided (maintainer, 2026-10-05): the derived counter stays.** Its known
inaccuracy is accepted for now: when **one principal runs several runs in
parallel**, each run sees the others' spend only at its own next checkpoint,
so runs checking in at the same moment can together overshoot the daily
ceiling (by up to one worst-case call each); and retention pruning that
day's runs would hide their spend. A separate per-principal ledger table —
exact under concurrency and pruning, with its schema migration — comes with
**KR1.4**, not in this ADR.

### 7. Stopping = a recorded refusal and `failed` with a structured reason

On a stop (`record_budget_stop`, `engine.rs:2101`):

1. `rejected {source: "budget", subject: none|decision|plan_node, reason}` —
   journaled (ADR 0036) and the live `rejected` event;
2. live `budget_exceeded {step, limit, max, used, requested, checkpoint,
   branch}`;
3. `run.budget_stop = {limit: usd|tokens|wall_time|daily_usd, max, used,
   requested, checkpoint: model_call|action, step, branch?}` — persisted
   **before** the terminal write, so a crash in between cannot lose it;
4. the run finalizes `failed`, `error` = the reason, e.g. *"token budget
   exceeded before a model call at step 1: used 400 tokens + worst case 4758
   tokens > max 4549 tokens — stopped before crossing it"*.

**Why `failed`, not a new terminal status.** `failed` is already what the
other run-level limit, `max_steps`, ends in ("step budget exhausted"); a
budget is the same kind of operator-imposed end. A new terminal status would
have to be added to every terminal-status list — retention's SQL
(`status IN ('completed','failed','incomplete','canceled')`), the A2A task
mapping, the web UI, Glassbox — and an old client matching exhaustively would
break; `failed` + `budget_stop` is additive and machine-distinguishable.
`incomplete` keeps its ADR 0033/0037 meaning (finished, success not earned):
a budget stop is not a verdict on success. **Decided (maintainer,
2026-10-05): `failed` + `budget_stop` stays** — no new terminal status.

## Consequences

- An operator can cap any run, and any principal's day, in dollars, tokens
  and time; a caller can ask for a tighter cap per run.
- `GET /runs/{id}` and `GET /runs/{id}/events` carry `budget`,
  `budget_stop` (and `principal`); WS carries `budget_exceeded`.
- `usage`/`cost_usd` now include refused and paused proposals (§2).

## Known limits

- **Remote work is not priced.** A delegation's remote token spend is the
  remote's; only local model calls count. `agent_usage` (count, ms) is
  unchanged.
- **Concurrent runs of one principal** each see the others' spend at their
  next call, not continuously: two runs checking in the same instant can
  together exceed the daily ceiling by up to one worst-case call each — the
  daily counter can be wrong under parallel runs of one principal. Accepted
  until KR1.4 adds a ledger table (§6).
- **The day is the UTC day of creation.** A run created at 23:59 UTC spends
  against that day. Retention pruning today's runs (only if `max_runs` is
  tiny) would hide their spend.
- **Runs without a principal** (A2A inbound, CLI) are not held to the daily
  ceiling — only API runs carry one. Per-run limits still apply to them.
- **The bound is only as good as its assumption** (bytes ≥ tokens, hidden
  overhead ≤ `prompt_overhead_tokens`). A tokenizer producing more tokens
  than bytes, or a provider preamble larger than the allowance, could let one
  call overshoot; the next checkpoint then stops the run with `used > max`
  visible in `budget_stop`.

## Decided (maintainer, 2026-10-05)

1. **Terminal state on a budget stop**: `failed` + `budget_stop`, as built —
   no new terminal status (§7).
2. **The daily counter**: stays derived from run records (no migration). It
   can be wrong when one principal runs runs in parallel (§6, Known limits);
   a durable per-principal ledger table with its migration comes with
   **KR1.4**.

### For Glassbox — the exact contract (input to KR4.5)

- `run.budget`: `{max_usd?, max_tokens?, max_wall_secs?, usd_enforced}` or
  `null` (no limits). `run.principal`: string or `null`.
- `run.budget_stop`: `null`, or `{limit: "usd"|"tokens"|"wall_time"|"daily_usd",
  max, used, requested, checkpoint: "model_call"|"action", step, branch?}`
  (dollars for usd/daily_usd, tokens, seconds). Present ⇒ `status: failed`.
- `GET /runs/{id}/events` header: `budget`, `budget_stop`, `principal`.
- Journal: `rejected.source = "budget"`, `subject` per §3.
- WS: `budget_exceeded {step, limit, max, used, requested, checkpoint,
  branch}`, then `failed {error}`.
