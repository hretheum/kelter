---
provenance: ai_authored
provenance_metadata:
  agent: claude-fable-5
  session_date: 2026-07-21
  human_validated: true
  validation_note: "signed 2026-07-22 on Eryk's authorization; governance invariant mirrors independently-verified ADR 0024, but the live Postgres+pgvector path was NOT independently re-run at sign-off"
  session_context: "Shared, concurrent-safe memory backend (Postgres + pgvector) behind the existing memory seam, so two independent kernels (desktop + server) share episodic memory + learned facts — with Kelter's governance (provenance, complete reversibility, learned-as-validated-context) preserved and re-proven on the shared store. Opt-in; SQLite stays the default. Dumb store: zero opaque learning."
---

# ADR 0028: Shared Postgres + pgvector memory backend

Status: accepted · Date: 2026-07-21 · Builds on: ADR 0005 (memory provider
interface), ADR 0008 (memory-as-extension), ADR 0024 (WASM memory backend —
the pluggable-backend precedent), ADR 0016 (governed self-learning) · Related:
ADR 0004 (topologies)

## Context

Thin-client sharing already works (ADR 0004): the desktop app can be a
stateless view onto a server kernel, so they share everything by sharing one
kernel. But there is a second topology the project wants: **two INDEPENDENT
kernels** (a local desktop kernel AND a server kernel) that each run their own
loop, yet **share episodic memory and learned facts**. The durable backend
today is a single SQLite file (`storage`), which cannot have two concurrent
writers — so two kernels cannot share it safely.

The memory seam was already built for exactly this: `MemoryProvider`
(ADR 0005) and `LearnedStore` (ADR 0016) are provider interfaces, and ADR 0024
established the precedent of adding a whole new backend (WASM) behind them
without touching the kernel. This ADR adds a **shared** backend — Postgres +
pgvector — behind the SAME seams.

The hard requirement is that **none of Kelter's governance may weaken on a
shared store.** Phase 10 (ADR 0016) rests on three invariants — provenance
tagging, *complete* reversibility (a `forget` leaves zero residue), and
learned-facts-as-validated-context (a learned fact is never an unvalidated
authority). Those must hold identically on Postgres, and be *re-proven* there,
not assumed.

## Decision

**A new `pgstore` crate implements `MemoryProvider` + `LearnedStore` on
Postgres + pgvector, opt-in behind a build feature, with governance preserved
by construction and re-proven by integration tests against a real Postgres.**

1. **Same seams, new backend.** `PgMemoryProvider` and `PgLearnedStore`
   implement the existing traits verbatim — the kernel is unchanged and
   unaware. Selection is config: `memory.backend = "sqlite"` (default) |
   `"postgres"`.

2. **Reversibility is complete — NO persistent ranking index.** This is the
   crux of preserving ADR 0016 on Postgres. BM25 ranking is recomputed per
   query in Rust (identical to the SQLite backend). The pgvector path stores
   the embedding as a per-row `vector` column and retrieves with an **EXACT
   per-query cosine scan** (`ORDER BY embedding <=> $q`) and **NO ANN index**
   (no `hnsw`/`ivfflat`). So a `forget` (a row `DELETE`) removes the fact AND
   its embedding — there is no separate relevance index that could hold a
   ghost. (Verified live: `pg_indexes` shows only PK btrees + a `batch_id`
   lookup btree — the same lookup index SQLite has — never a relevance index.)
   The cost is that retrieval is O(rows) per query — the same scale
   characteristic the SQLite backend already has; an ANN index is a deliberate
   *non-goal* here because it would trade reversibility for speed.

3. **Provenance survives verbatim.** Learned facts serialize their full
   `LearnedProvenance` (source run ids, `ai_extracted`, `human_validated:false`,
   batch id) into the row; the store round-trips it and invents nothing.

4. **Dumb store — zero opaque learning.** The backend does storage +
   retrieval ONLY. No belief-formation, no auto-consolidation, no "smart
   memory". The kernel keeps *all* governance (distillation, provenance,
   reversibility). This is a deliberate line: we hold Kelter's assumptions and
   do NOT adopt Hindsight-style belief-formation, which would break the
   governed/reversible thesis. Adopting such a service as a *store* (behind
   this same seam, still dumb) is a possible future ADR; adopting its
   *learning* is out of scope by design.

5. **Concurrency = Postgres MVCC + a pooled driver.** Each operation is one
   short statement on a `deadpool-postgres` pooled connection. Two independent
   kernels (separate pools, same database) write/retrieve/forget concurrently
   with no cross-instance corruption and a consistent view — re-proven by an
   integration test with two independent `PgStore` handles. One concurrency
   subtlety was found and fixed: `CREATE TABLE IF NOT EXISTS` is NOT race-safe
   in Postgres (two simultaneous first-connects collide on the `pg_type`
   catalog), so schema application runs inside a transaction that first takes
   `pg_advisory_xact_lock` — migrators serialize, the loser finds everything
   already there.

6. **Driver = tokio-postgres (+ deadpool), NOT sqlx.** sqlx was the first
   choice (built-in pool, pgvector integration), but its facade crate
   unconditionally drags `sqlx-sqlite → libsqlite3-sys` into the workspace
   dependency graph, which collides on `links = "sqlite3"` with the `storage`
   crate's rusqlite (Cargo forbids two packages linking the same native lib).
   That is a hard, repo-specific blocker. tokio-postgres has zero sqlite
   involvement and composes cleanly; deadpool-postgres supplies the pool; the
   `pgvector` crate integrates via its native `postgres` feature. TLS to
   Postgres is `NoTls` for now (local/tailnet/loopback deployment) — a rustls
   connector is a documented follow-up, not a blocker for the shared-memory
   topology.

7. **Opt-in and non-breaking.** SQLite stays the default; the Postgres driver
   compiles ONLY under the daemon's `postgres` feature, so a normal
   `cargo build` never pulls it (verified: `cargo tree -p daemon` has no
   tokio-postgres/pgvector). A `memory.backend = "postgres"` config on a daemon
   built without the feature fails loudly, never silently degrading.

8. **Secrets in the environment only.** The connection string (with any
   password) is read from the env var *named* by `memory.postgres_url_env`
   (default `KELTER_MEMORY_POSTGRES_URL`) — never in the config file, never
   logged (only "Postgres backend active" + whether embeddings are on).

## Consequences

- Two independent kernels can share memory + learned facts through one
  Postgres, with governance intact — re-proven, not assumed.
- Retrieval is a per-query scan (BM25 or exact cosine) — the reversibility
  price. An ANN index is a non-goal (it would break "zero residue").
- Default deployments (single instance, SQLite) are entirely unchanged.
- Integration tests need a real Postgres (Docker/podman + pgvector); they skip
  when `KELTER_TEST_POSTGRES_URL` is unset, so `cargo test` stays hermetic.

## Deliberately deferred

- A Hindsight-as-store adapter (behind this same dumb-store seam).
- Cross-user sharing / ACL, multi-region.
- TLS to Postgres (rustls connector).
- An ANN index — permanently a non-goal under the reversibility invariant.

## Alternatives rejected

- **sqlx.** Cleaner API, but its facade's unconditional sqlx-sqlite dependency
  hard-collides with the repo's rusqlite on `links="sqlite3"` — see decision 6.
- **A pgvector ANN index for speed.** Rejected: it is a persistent relevance
  structure, which is exactly what ADR 0016's "no persistent ranking index /
  zero residue" invariant forbids.
- **Adopting belief-formation / "smart memory".** Rejected by design: it would
  break the governed, reversible thesis (decision 4).
