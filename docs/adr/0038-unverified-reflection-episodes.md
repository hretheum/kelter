---
provenance: ai_authored
provenance_metadata:
  agent: claude-code
  session_date: 2026-10-05
  human_validated: false
  session_context: "ADR 0037 follow-up (CLAUDE.md open item 4): reflection episodes still recorded an unverified `completed` run as `Success`, so episodic memory kept presenting hallucinated successes to the model as success patterns even though distillation no longer learned from them. This ADR adds a third episode state and makes the human confirmation move it, versioned."
---

# ADR 0038: Unverified reflection episodes

Status: proposed (the retrieval rule in §4 is the maintainer's to accept) · Date: 2026-10-05 · Builds on: ADR 0005 (memory provider, reflection), ADR 0016 (versioned, provenance-tagged, reversible memory), ADR 0037 (success contract, `verification`, human confirmation) · Touches: ADR 0008 (memory extension protocol), ADR 0024 (WASM memory), ADR 0028 (shared Postgres memory)

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
| `crates/memory/src/lib.rs:209` → `:263` (`episode_summary`) | **read** — the text the model sees | `{:?}` of the outcome | `Unverified` gets an explicit mark; `Success`/`Failure` text unchanged |
| `crates/kernel-core/src/engine.rs:3645-3647` → `:3743-3751` (`finalize_run`) | **write** | `Completed` → `Success`, else `Failure` | `Completed` + `Contract` → `Success`; other `Completed` → `Unverified`; else `Failure` |
| `crates/kernel-core/src/engine.rs:1825` (`revise_episodes`, new) | **write** a new version | — | confirm / revoke (§3) |
| `crates/kernel-core/src/engine.rs:628` (`RunEvent::EpisodeRevised`, new) | WS | — | carries the new outcome |
| `crates/storage` (SQLite), `crates/pgstore` (Postgres) | store | opaque JSON in `episodes.data` | same JSON; + version table (§3) |
| `crates/mcp/src/memory_{client,server}.rs` (subprocess; the daemon's **default** SQLite memory path) | transport | opaque JSON | same; + optional `memory_episode_history` tool |
| `crates/wasmhost` + `examples/wasm-tools/memstore-wasm/src/lib.rs:67` | transport + guest | guest reads `outcome` as a string into its summary | unchanged — `outcome: unverified` passes through |
| `crates/memory/src/retrieval.rs`, `embeddings.rs`, pgvector query | **ranking** | ranks on `task` only | unchanged — the outcome never enters the score |
| `crates/api/src/lib.rs:906` (`GET /runs/{id}/episodes`, new) | API read | — (no episode API existed) | current + versions |
| `apps/web-ui/src/lib/result.ts` (`episodeView`, new), `Result.tsx` | UI read | only counted `reflection_written` | one "Memory: …" line under verification |
| `apps/desktop/src-tauri/tests/durable_store.rs:63` | test literal | `Success` | unchanged (still valid) |

Ranking does not read the outcome anywhere, so no retrieval score and no
existing quality test moves (the stop condition in the brief did not
trigger).

## Decision

### 1. A third state, not a flag

`EpisodeOutcome::Unverified` — "ended `completed`, nothing verified it".
Chosen over a separate `verified: bool` field because:

- the outcome is the one thing every reader already looks at (the summary,
  the WASM guest, any third-party memory extension that echoes it); a new
  field next to an unchanged `Success` would leave every one of those readers
  still saying "success" — the exact failure this ADR exists to fix;
- an enum cannot express the nonsense combination `Failure` + `verified`;
- it is additive on the wire: old records (`"success"`/`"failure"`) parse
  unchanged (test `pre_adr_0038_record_still_reads_and_unverified_round_trips`).

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
- **WASM** (ADR 0024): the guest ABI is unchanged; there is no history op,
  so `Unsupported`. The episode is still stored as `unverified` — the guest
  stores the JSON it is given and its summary says `outcome: unverified` —
  but a confirmation does not lift it (logged). This is a known limit (below).

### 4. Retrieval: unverified episodes stay in context, marked (proposed default)

An `unverified` episode is still retrieved — it is a true record of what was
tried for a similar task, and dropping it would also drop a legitimate
not-yet-confirmed success. It is never presented as a success pattern: its
summary reads

```
task: … | outcome: Unverified (completed, but no contract or human verified it — not a success pattern) | steps: N
```

`Success` and `Failure` summaries are byte-for-byte unchanged. Ranking is
unchanged (the outcome is not a ranking input). After a confirmation the
same episode is summarized as `Success`; after a revoke, `Unverified` again —
one entry per episode, never one per version.

**Open for the maintainer:** the stricter alternative — exclude `unverified`
episodes from context entirely — is a one-line filter in each backend's
`retrieve`. This ADR proposes "in, marked" and implements it; Eryk decides.

### 5. Historical data — left as it is

Episodes written before this change stay exactly as they were: a `completed`
run's episode is `success` with `provenance: reflection`, whether or not its
run was verified. This mirrors ADR 0037's rule for already-learned artifacts
("what was learned before stays"). Consequence, stated plainly: a pre-ADR-0038
episode of an unverified run keeps reading `outcome: Success` in context.

Confirming or revoking such a run (it is confirmable per ADR 0037 §5) governs
learning as before but does **not** touch its episode: confirm does nothing
because it is not `unverified`; revoke does nothing because its `success` was
written by reflection, not by a human. No duplicate, no downgrade (test
`pre_adr_0038_success_episode_is_left_as_it_is`). A backfill that rewrites old
episodes to `unverified` is possible later as a separate, explicit operation;
it is not done here.

### 6. No model path

The model cannot reach any of this. Episodes are written by `finalize_run`
(kernel) and `revise_episodes` (only from the authenticated confirmation
endpoint); neither is an action, and memory-extension tools are never in the
action schema. Test `the_model_has_no_path_to_lifting_an_episode`: a model
that proposes `episode_revise`, `memory_write` and a "verified success"
`finish` leaves exactly one version, `unverified`, `provenance: reflection`.

## What does not change

- Runs with a contract and failed/incomplete runs: same status, same events,
  same episode (`success` / `failure`, v1, `reflection`).
- `Run::learnable` and distillation (ADR 0037) — the episode is not a
  learning input; the run is.
- Ranking, the `retrieve` response schema, the WASM guest ABI, the three
  required memory-extension tools.

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
  lists on a WASM or history-less extension backend.
- No journal kind is added: the decision is already `success_confirmation_decided`.

## Known limits

- **WASM and third-party subprocess memory without `memory_episode_history`:**
  the unverified state is stored and shown, but confirmation cannot lift it.
  Closing that needs a guest ABI op (WASM) — deferred.
- **The pgvector path** embeds `task`, which does not change across versions;
  a revision re-embeds it anyway (one extra embedding call per decision).
- The episode carries `provenance: human` but not the principal; the
  principal is in the journal. An `Episode` field for it was rejected because
  it changes the struct every backend, guest and extension serializes.

## Consequences

- An unverified success no longer reaches the model as a success pattern;
  it reaches it as what it is.
- The human confirmation is now the one switch for both learning channels —
  distillation (ADR 0037) and episodic memory (this ADR) — and it is
  reversible in both, with history kept.
- `scripts/smoke-unverified-episodes.sh` drives it on the real daemon:
  unverified v1 → confirm → success v2 → revoke → unverified v3; a
  contract-verified run is `success` at once and cannot be confirmed.
