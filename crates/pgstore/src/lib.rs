//! Governed **shared** memory backend: Postgres + pgvector behind the existing
//! memory seams (ADR 0028; builds on ADR 0005 provider / ADR 0008
//! memory-as-extension / ADR 0024 pluggable-backend precedent / ADR 0016
//! governed self-learning).
//!
//! Two independent kernels (desktop + server) point at the SAME Postgres and
//! thereby SHARE episodic memory + learned facts — where a single-file SQLite
//! store cannot have two concurrent writers. Everything that made Phase 10
//! governance safe is preserved, and re-proven here on the shared store:
//!
//! - **Reversibility (complete).** There is NO persistent ranking index. BM25
//!   ranking is recomputed per query in Rust; the optional pgvector path is an
//!   EXACT per-query cosine scan (`ORDER BY embedding <=> $q`) with NO ANN
//!   index (no `hnsw`/`ivfflat`). The embedding is a per-row column, so a
//!   `forget` (row `DELETE`) removes the fact AND its embedding — zero
//!   residue. The only indexes are a PK and a btree on `batch_id` (a lookup,
//!   maintained transactionally by Postgres, exactly as SQLite's `learned_batch`
//!   index) — never a relevance index.
//! - **Provenance.** Learned facts carry `LearnedProvenance` (source run ids,
//!   `ai_extracted`, `human_validated:false`, batch id) verbatim in the row
//!   `data` — the kernel still tags every fact; this store never invents one.
//! - **Dumb store.** Storage + retrieval only. ZERO belief-formation, ZERO
//!   auto-consolidation, ZERO "smart memory": the kernel owns distillation,
//!   provenance, and reversibility. That is a deliberate line (see the ADR).
//!
//! Concurrency rides Postgres MVCC + a `deadpool-postgres` pool: each op is
//! one short statement on a pooled connection, safe under concurrent
//! instances with no cross-instance corruption.

use std::sync::Arc;

use async_trait::async_trait;
use deadpool_postgres::{Config, ManagerConfig, Pool, RecyclingMethod, Runtime};
use memory::embeddings::EmbeddingBackend;
use memory::{
    episode_summary, retrieval, Episode, GraphUpsert, MemoryError, MemoryProvider, RankedContext,
};
use pgvector::Vector;
use tokio_postgres::NoTls;
use uuid::Uuid;

use kernel_core::{LearnedFact, LearnedPlan, LearnedStore, StoreError};

/// A fixed advisory-lock key that serializes schema application across
/// concurrently-connecting instances. `CREATE TABLE IF NOT EXISTS` is NOT
/// race-safe in Postgres — two connections running it at the same time
/// collide on the `pg_type` catalog (`duplicate key … pg_type`) — so the two
/// independent kernels this backend exists for would hit exactly that on a
/// simultaneous first connect. The migration runs inside a transaction that
/// first takes `pg_advisory_xact_lock`, released automatically at COMMIT, so
/// migrators serialize and the loser simply finds everything already there.
const MIGRATE_LOCK_KEY: i64 = 4_915_823_075;

/// The schema, applied idempotently on connect (forward-only `IF NOT EXISTS`).
/// Deliberately NO vector ANN index — see the module docs on reversibility.
const SCHEMA: &str = "
CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE IF NOT EXISTS episodes (
    id         UUID PRIMARY KEY,
    run_id     UUID NOT NULL,
    task       TEXT NOT NULL,
    data       TEXT NOT NULL,
    embedding  vector,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS graph_entities (
    name       TEXT PRIMARY KEY,
    attributes TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS graph_relations (
    id        BIGSERIAL PRIMARY KEY,
    from_name TEXT NOT NULL,
    to_name   TEXT NOT NULL,
    label     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS learned_facts (
    id           UUID PRIMARY KEY,
    task_context TEXT NOT NULL,
    batch_id     UUID NOT NULL,
    data         TEXT NOT NULL,
    embedding    vector,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS learned_batch ON learned_facts(batch_id);

-- Plan distillation (the second self-learning slice): a distinct reversible
-- layer for distilled PLANS, mirroring `learned_facts` exactly so `forget`/
-- `forget_batch` reverse both artifact kinds with one path.
CREATE TABLE IF NOT EXISTS learned_plans (
    id           UUID PRIMARY KEY,
    task_context TEXT NOT NULL,
    batch_id     UUID NOT NULL,
    data         TEXT NOT NULL,
    embedding    vector,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS learned_plan_batch ON learned_plans(batch_id);

-- Versioned episodes (ADR 0038): `episodes` keeps the CURRENT version of each
-- episode (what retrieval ranks; a pre-existing row is version 1), and every
-- version ever written also lands in the append-only `episode_versions`. A row
-- from before this existed is only in `episodes`; `episode_history` reads
-- both, and a write copies such a row into history before revising it.
-- The column is added only when missing: `ALTER TABLE … ADD COLUMN IF NOT
-- EXISTS` takes an ACCESS EXCLUSIVE lock on `episodes` even when the column is
-- there, and an instance connecting while another writes an episode then
-- deadlocks with that writer (measured: the writer holds `episode_versions`
-- and waits on `episodes`, the migrator holds `episodes` and waits on the
-- `episode_versions` index). Checked in the catalog, the steady-state migrate
-- takes no lock on `episodes` at all.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'episodes' AND column_name = 'version'
    ) THEN
        ALTER TABLE episodes ADD COLUMN version INTEGER NOT NULL DEFAULT 1;
    END IF;
END
$$;
CREATE TABLE IF NOT EXISTS episode_versions (
    id         UUID NOT NULL,
    version    INTEGER NOT NULL,
    run_id     UUID NOT NULL,
    data       TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (id, version)
);
CREATE INDEX IF NOT EXISTS episode_versions_run ON episode_versions(run_id);
";

/// A shared handle to one Postgres database: a connection pool plus the
/// optional embedding backend that turns on the pgvector retrieval path.
#[derive(Clone)]
pub struct PgStore {
    pool: Pool,
    embed: Option<Arc<dyn EmbeddingBackend>>,
}

#[derive(Debug, thiserror::Error)]
pub enum PgError {
    #[error("postgres connection string is invalid: {0}")]
    BadUrl(String),
    #[error("postgres pool error: {0}")]
    Pool(String),
    #[error("postgres error: {0}")]
    Db(#[from] tokio_postgres::Error),
}

impl PgStore {
    /// Connect (pooled) and apply the schema. `embed` present → the pgvector
    /// cosine retrieval path; absent → BM25, ranked per query in Rust. The
    /// connection string (with any password) comes from the caller — read
    /// from the environment by the daemon, never from the config file/logs.
    pub async fn connect(
        url: &str,
        embed: Option<Arc<dyn EmbeddingBackend>>,
    ) -> Result<Self, PgError> {
        let pg_config: tokio_postgres::Config = url
            .parse()
            .map_err(|e: tokio_postgres::Error| PgError::BadUrl(e.to_string()))?;
        let mut cfg = Config::new();
        cfg.manager = Some(ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        });
        // Reuse the parsed libpq settings verbatim (host/user/dbname/etc.).
        cfg.host = pg_config.get_hosts().first().and_then(|h| match h {
            tokio_postgres::config::Host::Tcp(s) => Some(s.clone()),
            _ => None,
        });
        cfg.port = pg_config.get_ports().first().copied();
        cfg.user = pg_config.get_user().map(String::from);
        cfg.password = pg_config
            .get_password()
            .map(|p| String::from_utf8_lossy(p).into_owned());
        cfg.dbname = pg_config.get_dbname().map(String::from);

        let pool = cfg
            .create_pool(Some(Runtime::Tokio1), NoTls)
            .map_err(|e| PgError::Pool(e.to_string()))?;
        let store = Self { pool, embed };
        store.migrate().await?;
        Ok(store)
    }

    /// Build from an existing deadpool `Pool` (tests share one live database
    /// across the two "instances" without re-applying the schema each time).
    pub fn from_pool(pool: Pool, embed: Option<Arc<dyn EmbeddingBackend>>) -> Self {
        Self { pool, embed }
    }

    async fn client(&self) -> Result<deadpool_postgres::Object, PgError> {
        self.pool
            .get()
            .await
            .map_err(|e| PgError::Pool(e.to_string()))
    }

    /// Apply the schema, concurrency-safe across instances: a transaction
    /// takes `pg_advisory_xact_lock` first, so two kernels connecting at once
    /// never race on `CREATE TABLE IF NOT EXISTS` (auto-released at COMMIT).
    pub async fn migrate(&self) -> Result<(), PgError> {
        let sql =
            format!("BEGIN;\nSELECT pg_advisory_xact_lock({MIGRATE_LOCK_KEY});\n{SCHEMA}\nCOMMIT;");
        self.client().await?.batch_execute(&sql).await?;
        Ok(())
    }

    /// Whether the pgvector retrieval path is active for this handle.
    pub fn uses_embeddings(&self) -> bool {
        self.embed.is_some()
    }

    async fn embed_one(&self, text: &str) -> Option<Vector> {
        let backend = self.embed.as_ref()?;
        match backend.embed(&[text.to_string()]).await {
            Ok(mut v) if !v.is_empty() => Some(Vector::from(v.remove(0))),
            Ok(_) => None,
            Err(e) => {
                // Degraded ranking, never a broken run — mirrors the BM25
                // fallback the in-tree `Retrieval::Embeddings` path uses.
                tracing::warn!(
                    "pgstore: embedding failed ({e}); this row/query goes without a vector"
                );
                None
            }
        }
    }
}

fn mem_err<E: std::fmt::Display>(e: E) -> MemoryError {
    MemoryError::Backend(e.to_string())
}
fn store_err<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Backend(e.to_string())
}

// ---- episodic memory ------------------------------------------------------

/// `MemoryProvider` on Postgres. Shared across instances; reversible.
pub struct PgMemoryProvider {
    store: PgStore,
}

impl PgMemoryProvider {
    pub fn new(store: PgStore) -> Self {
        Self { store }
    }

    pub async fn episode_count(&self) -> Result<u64, MemoryError> {
        let client = self.store.client().await.map_err(mem_err)?;
        let row = client
            .query_one("SELECT COUNT(*)::bigint FROM episodes", &[])
            .await
            .map_err(mem_err)?;
        let n: i64 = row.get(0);
        Ok(n as u64)
    }
}

#[async_trait]
impl MemoryProvider for PgMemoryProvider {
    async fn write(&self, episode: Episode) -> Result<(), MemoryError> {
        let data = serde_json::to_string(&episode).map_err(mem_err)?;
        let embedding = self.store.embed_one(&episode.task).await;
        let version = episode.version as i32;
        let mut client = self.store.client().await.map_err(mem_err)?;
        // One transaction: history row + current row (ADR 0038). The current
        // row only advances; a lower version never displaces a higher one.
        let tx = client.transaction().await.map_err(mem_err)?;
        // A current row from before versioning has no history row yet.
        tx.execute(
            "INSERT INTO episode_versions (id, version, run_id, data, created_at)
             SELECT id, version, run_id, data, created_at FROM episodes WHERE id = $1
             ON CONFLICT (id, version) DO NOTHING",
            &[&episode.id],
        )
        .await
        .map_err(mem_err)?;
        tx.execute(
            "INSERT INTO episode_versions (id, version, run_id, data)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (id, version) DO UPDATE SET data = $4",
            &[&episode.id, &version, &episode.run_id, &data],
        )
        .await
        .map_err(mem_err)?;
        tx.execute(
            "INSERT INTO episodes (id, run_id, task, data, embedding, version)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (id) DO UPDATE SET data = $4, embedding = $5, version = $6
             WHERE episodes.version <= $6",
            &[
                &episode.id,
                &episode.run_id,
                &episode.task,
                &data,
                &embedding,
                &version,
            ],
        )
        .await
        .map_err(mem_err)?;
        tx.commit().await.map_err(mem_err)?;
        Ok(())
    }

    async fn episode_history(&self, run_id: Uuid) -> Result<Vec<Episode>, MemoryError> {
        let client = self.store.client().await.map_err(mem_err)?;
        let rows = client
            .query(
                "SELECT data FROM episode_versions WHERE run_id = $1
                 UNION ALL
                 SELECT data FROM episodes WHERE run_id = $1",
                &[&run_id],
            )
            .await
            .map_err(mem_err)?;
        let mut out: Vec<Episode> = Vec::new();
        for row in rows {
            let data: String = row.get("data");
            let e: Episode = serde_json::from_str(&data).map_err(mem_err)?;
            if !out.iter().any(|o| o.id == e.id && o.version == e.version) {
                out.push(e);
            }
        }
        out.sort_by_key(|e| (e.id, e.version));
        Ok(out)
    }

    async fn retrieve(&self, task: &str, limit: usize) -> Result<Vec<RankedContext>, MemoryError> {
        let client = self.store.client().await.map_err(mem_err)?;
        // pgvector path: an EXACT per-query cosine scan (no ANN index), so a
        // forgotten row leaves nothing behind. Falls back to BM25 when this
        // store has no embedding backend, or the query could not be embedded.
        if let Some(qvec) = self.store.embed_one(task).await {
            let rows = client
                .query(
                    "SELECT data, (1 - (embedding <=> $1))::float8 AS score
                     FROM episodes
                     WHERE embedding IS NOT NULL
                     ORDER BY embedding <=> $1
                     LIMIT $2",
                    &[&qvec, &(limit as i64)],
                )
                .await
                .map_err(mem_err)?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let data: String = row.get("data");
                let score: f64 = row.get("score");
                let ep: Episode = serde_json::from_str(&data).map_err(mem_err)?;
                out.push(RankedContext {
                    episode_id: ep.id,
                    summary: episode_summary(&ep),
                    score: score.clamp(0.0, 1.0),
                });
            }
            return Ok(out);
        }

        // BM25 path: load rows, rank fresh in Rust (identical contract to the
        // SQLite backend). No persistent index → reversible by construction.
        let episodes = load_episodes(&client).await.map_err(mem_err)?;
        let docs: Vec<String> = episodes.iter().map(|e| e.task.clone()).collect();
        let ranked = retrieval::normalize(&retrieval::rank_bm25(task, &docs, limit));
        Ok(ranked
            .into_iter()
            .map(|(i, score)| RankedContext {
                episode_id: episodes[i].id,
                summary: episode_summary(&episodes[i]),
                score,
            })
            .collect())
    }

    async fn upsert(&self, item: GraphUpsert) -> Result<(), MemoryError> {
        let client = self.store.client().await.map_err(mem_err)?;
        match item {
            GraphUpsert::Entity { name, attributes } => {
                client
                    .execute(
                        "INSERT INTO graph_entities (name, attributes) VALUES ($1, $2)
                         ON CONFLICT (name) DO UPDATE SET attributes = $2",
                        &[&name, &attributes.to_string()],
                    )
                    .await
                    .map_err(mem_err)?;
            }
            GraphUpsert::Relation { from, to, label } => {
                client
                    .execute(
                        "INSERT INTO graph_relations (from_name, to_name, label)
                         VALUES ($1, $2, $3)",
                        &[&from, &to, &label],
                    )
                    .await
                    .map_err(mem_err)?;
            }
        }
        Ok(())
    }
}

async fn load_episodes(
    client: &deadpool_postgres::Object,
) -> Result<Vec<Episode>, tokio_postgres::Error> {
    let rows = client
        .query(
            "SELECT data FROM episodes ORDER BY created_at ASC, id ASC",
            &[],
        )
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let data: String = row.get("data");
        if let Ok(ep) = serde_json::from_str::<Episode>(&data) {
            out.push(ep);
        }
    }
    Ok(out)
}

// ---- learned facts + plans (governed self-learning, ADR 0016 / plan
// distillation) ------------------------------------------------------------

/// `LearnedStore` on Postgres: the distinct, reversible learned layer — facts
/// AND plans. A row `DELETE` on `forget`/`forget_batch` is complete — ranking
/// is recomputed per query, so nothing lingers.
pub struct PgLearnedStore {
    store: PgStore,
}

impl PgLearnedStore {
    pub fn new(store: PgStore) -> Self {
        Self { store }
    }

    async fn load_all(&self) -> Result<Vec<LearnedFact>, StoreError> {
        let client = self.store.client().await.map_err(store_err)?;
        let rows = client
            .query(
                "SELECT data FROM learned_facts ORDER BY created_at DESC, id ASC",
                &[],
            )
            .await
            .map_err(store_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let data: String = row.get("data");
            out.push(serde_json::from_str(&data).map_err(store_err)?);
        }
        Ok(out)
    }

    async fn load_all_plans(&self) -> Result<Vec<LearnedPlan>, StoreError> {
        let client = self.store.client().await.map_err(store_err)?;
        let rows = client
            .query(
                "SELECT data FROM learned_plans ORDER BY created_at DESC, id ASC",
                &[],
            )
            .await
            .map_err(store_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let data: String = row.get("data");
            out.push(serde_json::from_str(&data).map_err(store_err)?);
        }
        Ok(out)
    }
}

#[async_trait]
impl LearnedStore for PgLearnedStore {
    async fn put(&self, fact: LearnedFact) -> Result<(), StoreError> {
        let data = serde_json::to_string(&fact).map_err(store_err)?;
        let embedding = self.store.embed_one(&fact.task_context).await;
        let client = self.store.client().await.map_err(store_err)?;
        client
            .execute(
                "INSERT INTO learned_facts (id, task_context, batch_id, data, embedding)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (id) DO UPDATE SET data = $4, embedding = $5",
                &[
                    &fact.id,
                    &fact.task_context,
                    &fact.provenance.batch_id,
                    &data,
                    &embedding,
                ],
            )
            .await
            .map_err(store_err)?;
        Ok(())
    }

    async fn retrieve(&self, task: &str, limit: usize) -> Result<Vec<LearnedFact>, StoreError> {
        let client = self.store.client().await.map_err(store_err)?;
        if let Some(qvec) = self.store.embed_one(task).await {
            let rows = client
                .query(
                    "SELECT data FROM learned_facts
                     WHERE embedding IS NOT NULL
                     ORDER BY embedding <=> $1
                     LIMIT $2",
                    &[&qvec, &(limit as i64)],
                )
                .await
                .map_err(store_err)?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let data: String = row.get("data");
                out.push(serde_json::from_str(&data).map_err(store_err)?);
            }
            return Ok(out);
        }

        let facts = self.load_all().await?;
        if facts.is_empty() {
            return Ok(Vec::new());
        }
        let docs: Vec<String> = facts.iter().map(|f| f.task_context.clone()).collect();
        let ranked = retrieval::rank_bm25(task, &docs, limit);
        Ok(ranked.into_iter().map(|(i, _)| facts[i].clone()).collect())
    }

    async fn list(&self) -> Result<Vec<LearnedFact>, StoreError> {
        self.load_all().await
    }

    async fn put_plan(&self, plan: LearnedPlan) -> Result<(), StoreError> {
        let data = serde_json::to_string(&plan).map_err(store_err)?;
        let embedding = self.store.embed_one(&plan.task_context).await;
        let client = self.store.client().await.map_err(store_err)?;
        client
            .execute(
                "INSERT INTO learned_plans (id, task_context, batch_id, data, embedding)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (id) DO UPDATE SET data = $4, embedding = $5",
                &[
                    &plan.id,
                    &plan.task_context,
                    &plan.provenance.batch_id,
                    &data,
                    &embedding,
                ],
            )
            .await
            .map_err(store_err)?;
        Ok(())
    }

    async fn retrieve_plans(
        &self,
        task: &str,
        limit: usize,
    ) -> Result<Vec<LearnedPlan>, StoreError> {
        let client = self.store.client().await.map_err(store_err)?;
        if let Some(qvec) = self.store.embed_one(task).await {
            let rows = client
                .query(
                    "SELECT data FROM learned_plans
                     WHERE embedding IS NOT NULL
                     ORDER BY embedding <=> $1
                     LIMIT $2",
                    &[&qvec, &(limit as i64)],
                )
                .await
                .map_err(store_err)?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let data: String = row.get("data");
                out.push(serde_json::from_str(&data).map_err(store_err)?);
            }
            return Ok(out);
        }

        let plans = self.load_all_plans().await?;
        if plans.is_empty() {
            return Ok(Vec::new());
        }
        let docs: Vec<String> = plans.iter().map(|p| p.task_context.clone()).collect();
        let ranked = retrieval::rank_bm25(task, &docs, limit);
        Ok(ranked.into_iter().map(|(i, _)| plans[i].clone()).collect())
    }

    async fn list_plans(&self) -> Result<Vec<LearnedPlan>, StoreError> {
        self.load_all_plans().await
    }

    async fn forget(&self, id: Uuid) -> Result<bool, StoreError> {
        let client = self.store.client().await.map_err(store_err)?;
        let facts_removed = client
            .execute("DELETE FROM learned_facts WHERE id = $1", &[&id])
            .await
            .map_err(store_err)?;
        let plans_removed = client
            .execute("DELETE FROM learned_plans WHERE id = $1", &[&id])
            .await
            .map_err(store_err)?;
        Ok(facts_removed > 0 || plans_removed > 0)
    }

    async fn forget_batch(&self, batch_id: Uuid) -> Result<u64, StoreError> {
        let client = self.store.client().await.map_err(store_err)?;
        let facts_removed = client
            .execute(
                "DELETE FROM learned_facts WHERE batch_id = $1",
                &[&batch_id],
            )
            .await
            .map_err(store_err)?;
        let plans_removed = client
            .execute(
                "DELETE FROM learned_plans WHERE batch_id = $1",
                &[&batch_id],
            )
            .await
            .map_err(store_err)?;
        Ok(facts_removed + plans_removed)
    }
}
