---
provenance: ai_authored
provenance_metadata:
  agent: claude-code
  session_date: 2026-10-05
  human_validated: false
  session_context: "ADR 0037 follow-up (CLAUDE.md open item 4): reflection episodes still recorded an unverified `completed` run as `Success`, so episodic memory kept presenting hallucinated successes to the model as success patterns even though distillation no longer learned from them. This ADR adds a third episode state and makes the human confirmation move it, versioned."
---

# ADR 0040: Unverified reflection episodes

Status: accepted (retrieval rule in §4 decided by the maintainer, 2026-10-05) · Date: 2026-10-05 · Builds on: ADR 0005 (memory provider, reflection), ADR 0016 (versioned, provenance-tagged, reversible memory), ADR 0037 (success contract, `verification`, human confirmation) · Touches: ADR 0008 (memory extension protocol), ADR 0024 (WASM memory), ADR 0028 (shared Postgres memory)

## Context

ADR 0037 made the kernel say how every success was judged
(`verification: contract | none`) and kept unverified successes out of
**distillation** until a human confirms them. It deliberately left the other
consumer of `completed` alone and listed it as open: the reflection
**episode** (ADR 0005), written by `finalize_run` for every finished run.

That episode was `EpisodeOutcome::Success` for any `completed` run. Episodes
are retrieved into the model's context for the next similar task
(`retrieve(task) → summary`), and the summary reads `outcome: Success`. So
the "send an e-mail" run that wrote a file instead — `completed`,
`verification: none`, not learnable — was still shown to the next run as a
successful way to send an e-mail. Distillation was gated; the cheaper
learning channel was not.

## Inventory — where `EpisodeOutcome` lives (at `f9d5a50` → after)

| Site | Role | Before | After |
|---|---|---|---|
| `crates/memory/src/lib.rs:89` → `:93` | the enum | `Success`, `Failure` | + `Unverified` (serde `"unverified"`) |
| `crates/memory/src/lib.rs:209` → `:274` (`episode_summary`) | **read** — the text the model sees | `{:?}` of the outcome | unchanged |
| `crates/memory/src/lib.rs:204` (`in_model_context`, new) | **gate** — may it reach the model? | — | everything but `unverified` (§4) |
| `retrieve` in `memory` (in-memory), `storage` (SQLite, served to the daemon by `memory-server`), `pgstore` (BM25 + pgvector) | **read** → model context | every current episode | `unverified` filtered out before ranking (pgvector: in SQL, before `LIMIT`) |
| `crates/kernel-core/src/engine.rs:3645-3647` → `:3743-3751` (`finalize_run`) | **write** | `Completed` → `Success`, else `Failure` | `Completed` + `Contract` → `Success`; other `Completed` → `Unverified`; else `Failure` |
| `crates/kernel-core/src/engine.rs:1825` (`revise_episodes`, new) | **write** a new version | — | confirm / revoke (§3) |
| `crates/kernel-core/src/engine.rs:628` (`RunEvent::EpisodeRevised`, new) | WS | — | carries the new outcome |
| `crates/storage` (SQLite), `crates/pgstore` (Postgres) | store | opaque JSON in `episodes.data` | same JSON; + version table (§3) |
| `crates/mcp/src/memory_{client,server}.rs` (subprocess; the daemon's **default** SQLite memory path) | transport | opaque JSON | same; + optional `memory_episode_history` tool |
| `crates/wasmhost` + `examples/wasm-tools/memstore-wasm/src/lib.rs` | transport + guest | guest echoes `outcome` into its summary; no versions | guest: versions, skips `unverified` in retrieve, serves `memory_episode_history`; host: `episode_history` |
| `examples/extensions/memstore/server.py` (Python example extension) | external backend | stores/echoes | same three rules as the WASM guest |
| `crates/memory/src/retrieval.rs`, `embeddings.rs`, pgvector query | **ranking** | ranks on `task` only | unchanged — the outcome never enters the score |
| `crates/mcp/src/client.rs:165` (`is_reserved_memory_tool`, new) | action space | a tool server could expose `memory_*` as actions | reserved names refused (§6) |
| `crates/api/src/lib.rs:906` (`GET /runs/{id}/episodes`, new) | API read | — (no episode API existed) | current + versions |
| `apps/web-ui/src/lib/result.ts` (`episodeView`, new), `Result.tsx` | UI read | only counted `reflection_written` | one "Memory: …" line under verification |
| `apps/desktop/src-tauri/tests/durable_store.rs:63` | test literal | `Success` | unchanged (still valid) |

Ranking itself does not read the outcome. The filter removes `unverified`
episodes from the candidate set before ranking, which changes results only
where such episodes exist — i.e. only data written after this change; no
existing quality test moved (the stop condition in the brief did not
trigger).

## Decision

### 1. A third state, not a flag

`EpisodeOutcome::Unverified` — "ended `completed`, nothing verified it".
Chosen over a separate `verified: bool` field because:

- the outcome is the one thing every reader already looks at (retrieval
  filters, the WASM guest, any third-party memory extension); a new field
  next to an unchanged `Success` would leave every reader that ignores it
  still saying "success" — the exact failure this ADR exists to fix;
- an enum cannot express the nonsense combination `Failure` + `verified`;
- it is additive on the wire: old records (`"success"`/`"failure"`) parse
  unchanged (test `pre_adr_0040_record_still_reads_and_unverified_round_trips`).

### 2. What reflection writes

| Run ends | `verification` | Episode v1 |
|---|---|---|
| `completed` | `contract` | `success` |
| `completed` | `none` | **`unverified`** |
| `incomplete` (incl. unmet contract), `failed`, `canceled` | — | `failure` (as before) |

All v1 episodes carry `provenance: reflection`, as before. Runs with a
contract and failed runs are written exactly as before.

### 3. A human decision is a new version — nothing is overwritten or deleted

`POST /runs/{id}/confirmation` and `DELETE …` (ADR 0037) now also move the
run's episode, through `Kernel::revise_episodes`, called only from
`decide_success_confirmation`:

- **confirm**: current `unverified` → new version `success`;
- **revoke**: current `success` **with `provenance: human`** → new version
  `unverified`;
- anything else is left alone.

The new version keeps `id`, `run_id`, `task` and `trace`, bumps `version`,
and is tagged `provenance: human` — the principal and time are in the
journaled `success_confirmation_decided` entry (ADR 0037), which is the
audit record; the episode is the memory consequence. WS gets
`episode_revised {episode_id, version, outcome}`.

The revision is **best-effort**, like the reflection write it follows: the
decision is journaled and applied to the run first, and that is what governs
learning (`Run::learnable`). A memory error is logged, never turns the
decision into a 500.

**Versioning contract** (`MemoryProvider::write`, ADR 0016 made concrete): a
write of an already-stored `id` with a *higher* `version` is a revision — it
becomes what `retrieve` ranks, and earlier versions stay as history; a write
of the same `(id, version)` replaces that version (idempotent); a lower
version never displaces a higher one. History is read through a new,
optional trait method `episode_history(run_id)`; its default returns
`MemoryError::Unsupported`.

Per backend:

- **In-memory**: current list + append-only history list.
- **SQLite** (schema **v4**): `episodes` keeps the current version (new
  `version` column, default 1, so every pre-existing row is v1) and is still
  the only table `retrieve` reads; every write also lands in the new
  append-only `episode_versions (id, version) PK`. Pre-v4 rows are not
  backfilled: `episode_history` reads both tables, and a write first copies
  the current row into history if it is not there yet, so a pre-v4 v1 is
  never lost to a revision. `prune` (ADR 0017) deletes both.
- **Postgres** (ADR 0028): the same two-table shape, applied inside the
  existing advisory-locked idempotent migration. The `version` column is
  added by a catalog-checked `DO` block, **not** `ADD COLUMN IF NOT EXISTS`:
  the latter takes an ACCESS EXCLUSIVE lock on `episodes` on every connect,
  and an instance connecting while another wrote an episode deadlocked with
  that writer (measured in this suite's parallel run — `deadlock detected`;
  regression test `connecting_while_writing_episodes_does_not_deadlock_on_postgres`
  fails 3/3 with the unconditional `ALTER`, passes with the `DO` block). The
  pgvector path still scans `episodes` only, so it ranks the current version.
- **Subprocess / MCP** (the daemon's default for `storage = sqlite`): a
  fourth, **optional** tool `memory_episode_history {run_id}` →
  `{episodes: [...]}`. The built-in `memory-server` serves it. The client
  treats the response like `retrieve`'s — untrusted: it must parse as
  episodes and every episode must belong to the asked run, so an extension
  cannot steer the kernel into revising another run's episode. A third-party
  extension that does not serve the tool stays a valid memory backend and
  reports `Unsupported`.
- **WASM** (ADR 0024): an **optional** fourth op in the same JSON envelope,
  `{"op": "memory_episode_history", "run_id"}` → `{"episodes": [...]}`, with
  the same untrusted-boundary checks on the host. The in-repo `memstore`
  guest now keeps versions (current per id + every version) and serves it, so
  confirmation lifts a WASM episode like any other. A module answering
  `{"ok": false, "error": "unknown op …"}` reports `Unsupported` (no
  revision); a module that answers anything else malformed is refused by the
  gate (test `lying_module_history_is_refused_by_the_gate`). The `.wasm`
  is rebuilt from source and committed, as before.

### 4. Retrieval: unverified episodes never enter model context (maintainer's decision)

Decided by Eryk, 2026-10-05: an `unverified` episode is **not** retrieved
into the model's context, in any backend. It stays in the history, in
`GET /runs/{id}/episodes` and in the UI — it is a true record — but the model
never sees it, so a hallucinated success cannot become a pattern for the
next run even as a marked hint. Once a human confirms the run, its `success`
version is retrieved like any other; after a revoke it disappears from
context again.

Contract (`MemoryProvider::retrieve` doc): the current version of each
episode is ranked, **excluding** `unverified` ones, which are filtered out
*before* ranking (so they neither appear nor take a `limit` slot). One
predicate, `memory::in_model_context`, is used by the Rust backends:

- in-memory and SQLite (`storage`, which is also what the daemon's
  `memory-server` subprocess serves): filter, then rank;
- Postgres: BM25 path filters in Rust; pgvector path filters in SQL
  (`(data::jsonb ->> 'outcome') IS DISTINCT FROM 'unverified'`) before
  `LIMIT`;
- WASM `memstore` guest and the Python example extension: skip
  `"unverified"` in their own retrieve.

A third-party backend is untrusted for its retrieve content in general (ADR
0008/0024: its context can be poisoned, never authorizes); honouring this
rule is part of the interface contract it implements.

Summaries are unchanged for every state (`task: … | outcome: Success | …`) —
no "not a success pattern" annotation; an unverified summary never reaches a
prompt.

Tests, one per backend, each driving the real kernel through
`kernel_core::testkit::episode_context_probe` (run → second run's context →
confirm → third run's context): `unverified_episode_reaches_model_context_only_after_confirmation`
(in-memory), `sqlite_memory_server_keeps_unverified_out_of_model_context`
(memory-server child process), `postgres_keeps_unverified_out_of_model_context`
(+ `pgvector_path_never_retrieves_unverified`),
`wasm_memory_keeps_unverified_out_of_model_context`. On the real daemon,
`scripts/smoke-unverified-episodes.sh` reads the prompt the scripted model
actually received.

### 5. Historical data — left as it is

Episodes written before this change stay exactly as they were: a `completed`
run's episode is `success` with `provenance: reflection`, whether or not its
run was verified. This mirrors ADR 0037's rule for already-learned artifacts
("what was learned before stays"). Consequence, stated plainly: a pre-ADR-0040
episode of an unverified run keeps reading `outcome: Success` in context.

Confirming or revoking such a run (it is confirmable per ADR 0037 §5) governs
learning as before but does **not** touch its episode: confirm does nothing
because it is not `unverified`; revoke does nothing because its `success` was
written by reflection, not by a human. No duplicate, no downgrade (test
`pre_adr_0040_success_episode_is_left_as_it_is`). A backfill that rewrites old
episodes to `unverified` is possible later as a separate, explicit operation;
it is not done here.

### 6. No model path

The model cannot reach any of this. Episodes are written by `finalize_run`
(kernel) and `revise_episodes` (only from the authenticated confirmation
endpoint); neither is an action. Test `the_model_has_no_path_to_lifting_an_episode`:
a model that proposes `episode_revise`, `memory_write` and a "verified
success" `finish` leaves exactly one version, `unverified`, `provenance:
reflection`.

The memory-extension tools — `memory_write`/`retrieve`/`upsert` and the new
`memory_episode_history` — are the kernel's private protocol with its memory
backend. A `memory` extension is connected only as a `MemoryProvider`,
never as a tool source, so they are not in the action schema. The one way
they could have reached it was a misconfiguration: the memory-server listed
under `[[tools.servers]]` registered all four as model actions (measured:
the new test fails without the guard, listing them). `McpToolSource` now
refuses those names from any tool server (warning logged), so the model can
neither see nor call them under any configuration — test
`memory_tools_never_enter_the_action_space`.

## What does not change

- Runs with a contract and failed/incomplete runs: same status, same events,
  same episode (`success` / `failure`, v1, `reflection`).
- `Run::learnable` and distillation (ADR 0037) — the episode is not a
  learning input; the run is.
- Ranking itself, the `retrieve` response schema, the three required
  memory-extension tools / WASM ops (the history one is optional in both).

## Compatibility

- **API / WS — additive.** New `GET /api/v1/runs/{id}/episodes` (bearer;
  404 for an unknown run). New WS event `episode_revised`. The confirmation
  responses are unchanged. `outcome` gains the value `"unverified"`.
- **Storage — forward migration.** SQLite schema v3 → v4, Postgres
  idempotent DDL on connect. Both read pre-existing rows unchanged (tests
  `pre_v4_episode_reads_after_migration_and_survives_revision`,
  `pre_versioning_episode_row_reads_and_survives_revision_on_postgres`).
- **Downgrade.** An older binary ignores the new column and table, but an
  episode stored as `"unverified"` does not parse for it: SQLite `retrieve`
  then errors (the kernel logs it and runs without episodic context — not
  fatal), and Postgres BM25 skips the row (the pgvector path errors the same
  way). Same shape of hazard as ADR 0036's; rollback option: `UPDATE
  episodes SET data = replace(data, '"outcome":"unverified"', '"outcome":"success"')`
  — which re-creates exactly the pre-ADR behaviour.
- **Tool servers:** a `[[tools.servers]]` entry that exposes a tool named
  `memory_write`/`memory_retrieve`/`memory_upsert`/`memory_episode_history`
  no longer gets it registered as an action (logged at WARN). No in-repo
  tool server uses those names.
- **Desktop** links `memory`/`kernel-core`: no struct literal gained a field
  (`Episode` is unchanged), and `MemoryProvider` gained only a defaulted
  method.

### For Glassbox — the exact contract

- WS: new `type: "episode_revised"`, fields `episode_id` (UUID string),
  `version` (u32, ≥ 2), `outcome` (`"success"` on confirm, `"unverified"` on
  revoke). Emitted with `seq: 0`, like `success_confirmation_decided`. May be
  ignored safely.
- `reflection_written` is unchanged; the episode it names may now be
  `unverified`.
- `GET /api/v1/runs/{id}/episodes` →
  `{run_id, history_supported: bool, current: Episode[], versions: Episode[]}`;
  `Episode = {id, run_id, task, trace: string[], outcome: "success"|"failure"|"unverified", provenance: "reflection"|"human"|"import", version}`.
  `current` has one entry per episode id (highest version); `versions` has
  all, ordered by `(id, version)`. `history_supported: false` with empty
  lists on a backend without the optional history op.
- An `unverified` episode is visible here (and in `reflection_written` /
  `episode_revised`) but never in a model prompt.
- No journal kind is added: the decision is already `success_confirmation_decided`.

## Known limits

- **Third-party memory backends without the optional history op/tool:** an
  unverified episode there stays out of context for good (if the backend
  honours the retrieve rule), because a confirmation cannot write it a
  `success` version. The in-repo backends all serve history.
- **Honouring the retrieve rule is up to the backend.** The kernel sees only
  ranked summaries, so it cannot itself filter an untrusted backend's
  context; that backend could equally poison context in any other way (ADR
  0024's invariant: context, never authority).
- **The pgvector path** embeds `task`, which does not change across versions;
  a revision re-embeds it anyway (one extra embedding call per decision).
- The episode carries `provenance: human` but not the principal; the
  principal is in the journal. An `Episode` field for it was rejected because
  it changes the struct every backend, guest and extension serializes.

## Consequences

- An unverified success no longer reaches the model at all; it reaches it
  only after a human confirms it.
- The human confirmation is now the one switch for both learning channels —
  distillation (ADR 0037) and episodic memory (this ADR) — and it is
  reversible in both, with history kept.
- `scripts/smoke-unverified-episodes.sh` drives it on the real daemon:
  unverified v1 → confirm → success v2 → revoke → unverified v3, with the
  model's prompt checked at each stage (no episode → `outcome: Success` → no
  episode); a contract-verified run is `success` at once and cannot be
  confirmed. `scripts/smoke-memory-extension.sh` (needs a real model) now
  confirms its first run before expecting retrieval.
