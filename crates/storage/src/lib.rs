//! Durable persistence: SQLite-backed implementations of the kernel's
//! `RunStore` and the `MemoryProvider` seam, sharing one database file.
//!
//! Design notes:
//! - Single `rusqlite::Connection` behind an async mutex. Kelter's write
//!   rate is one journal row per kernel step — microsecond statements, so
//!   a pool or `spawn_blocking` would be complexity without benefit at
//!   this scale (documented seam if that changes).
//! - WAL journal mode: readers never block the writer, and an OS crash
//!   leaves a consistent database.
//! - Migrations via `PRAGMA user_version` — versioned, forward-only.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::{params, Connection};
use tokio::sync::Mutex;
use uuid::Uuid;

use kernel_core::{JournalEntry, Run, RunStore, StoreError};
use memory::{
    episode_summary, in_model_context, Episode, GraphUpsert, MemoryError, MemoryProvider,
    RankedContext,
};

/// Forward-only migrations; index = schema version - 1.
const MIGRATIONS: &[&str] = &[
    // v1
    "CREATE TABLE runs (
         id TEXT PRIMARY KEY,
         status TEXT NOT NULL,
         created_at INTEGER NOT NULL,
         data TEXT NOT NULL
     );
     CREATE TABLE journal (
         id INTEGER PRIMARY KEY AUTOINCREMENT,
         run_id TEXT NOT NULL,
         entry TEXT NOT NULL
     );
     CREATE INDEX journal_run ON journal(run_id);
     CREATE TABLE episodes (
         id TEXT PRIMARY KEY,
         run_id TEXT NOT NULL,
         task TEXT NOT NULL,
         data TEXT NOT NULL,
         created_at INTEGER NOT NULL
     );
     CREATE TABLE graph_entities (
         name TEXT PRIMARY KEY,
         attributes TEXT NOT NULL
     );
     CREATE TABLE graph_relations (
         id INTEGER PRIMARY KEY AUTOINCREMENT,
         from_name TEXT NOT NULL,
         to_name TEXT NOT NULL,
         label TEXT NOT NULL
     );",
    // v2 — governed self-learning (ADR 0016): a DISTINCT, reversible layer of
    // distilled facts, kept separate from `episodes` so `forget` is complete
    // (delete the rows → gone; ranking is recomputed per retrieval, so there
    // is no residual index).
    "CREATE TABLE learned_facts (
         id TEXT PRIMARY KEY,
         task_context TEXT NOT NULL,
         batch_id TEXT NOT NULL,
         data TEXT NOT NULL,
         created_at INTEGER NOT NULL
     );
     CREATE INDEX learned_batch ON learned_facts(batch_id);",
    // v3 — plan distillation (the second self-learning slice): a distinct
    // reversible layer for distilled PLANS, mirroring `learned_facts` exactly
    // so `forget`/`forget_batch` reverse both artifact kinds with one path.
    "CREATE TABLE learned_plans (
         id TEXT PRIMARY KEY,
         task_context TEXT NOT NULL,
         batch_id TEXT NOT NULL,
         data TEXT NOT NULL,
         created_at INTEGER NOT NULL
     );
     CREATE INDEX learned_plan_batch ON learned_plans(batch_id);",
    // v4 — versioned episodes (ADR 0038): `episodes` keeps the CURRENT
    // version of each episode (what retrieval ranks, unchanged for every
    // pre-existing row — they become version 1); every version ever written
    // also lands in the append-only `episode_versions`. Rows from before v4
    // exist only in `episodes`, and `episode_history` reads both, so nothing
    // needs backfilling.
    "ALTER TABLE episodes ADD COLUMN version INTEGER NOT NULL DEFAULT 1;
     CREATE TABLE episode_versions (
         id TEXT NOT NULL,
         version INTEGER NOT NULL,
         run_id TEXT NOT NULL,
         data TEXT NOT NULL,
         created_at INTEGER NOT NULL,
         PRIMARY KEY (id, version)
     );
     CREATE INDEX episode_versions_run ON episode_versions(run_id);",
];

/// A shared handle to one SQLite database file.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

impl Db {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// In-memory database (tests).
    pub fn open_in_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory()?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn migrate(conn: &Connection) -> Result<(), DbError> {
        let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate() {
            let target = (i + 1) as i64;
            if current < target {
                tracing::info!("storage: applying schema migration v{target}");
                conn.execute_batch(sql)?;
                conn.pragma_update(None, "user_version", target)?;
            }
        }
        Ok(())
    }
}

fn backend_err<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Backend(e.to_string())
}

// ---- RunStore -------------------------------------------------------------

pub struct SqliteRunStore {
    db: Db,
}

impl SqliteRunStore {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

#[async_trait]
impl RunStore for SqliteRunStore {
    async fn put(&self, run: Run) -> Result<(), StoreError> {
        let conn = self.db.conn.lock().await;
        let data = serde_json::to_string(&run).map_err(backend_err)?;
        let status = serde_json::to_value(run.status)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        conn.execute(
            "INSERT INTO runs (id, status, created_at, data) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET status = ?2, data = ?4",
            params![run.id.to_string(), status, run.created_at as i64, data],
        )
        .map_err(backend_err)?;
        Ok(())
    }

    async fn get(&self, id: Uuid) -> Result<Run, StoreError> {
        let conn = self.db.conn.lock().await;
        let data: String = conn
            .query_row(
                "SELECT data FROM runs WHERE id = ?1",
                params![id.to_string()],
                |row| row.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => StoreError::NotFound(id),
                other => backend_err(other),
            })?;
        serde_json::from_str(&data).map_err(backend_err)
    }

    async fn list(&self) -> Result<Vec<Run>, StoreError> {
        let conn = self.db.conn.lock().await;
        let mut stmt = conn
            .prepare("SELECT data FROM runs ORDER BY created_at ASC, id ASC")
            .map_err(backend_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(backend_err)?;
        let mut runs = Vec::new();
        for row in rows {
            let data = row.map_err(backend_err)?;
            runs.push(serde_json::from_str(&data).map_err(backend_err)?);
        }
        Ok(runs)
    }

    async fn append_journal(&self, run_id: Uuid, entry: JournalEntry) -> Result<(), StoreError> {
        let conn = self.db.conn.lock().await;
        let entry = serde_json::to_string(&entry).map_err(backend_err)?;
        conn.execute(
            "INSERT INTO journal (run_id, entry) VALUES (?1, ?2)",
            params![run_id.to_string(), entry],
        )
        .map_err(backend_err)?;
        Ok(())
    }

    async fn journal(&self, run_id: Uuid) -> Result<Vec<JournalEntry>, StoreError> {
        let conn = self.db.conn.lock().await;
        let mut stmt = conn
            .prepare("SELECT entry FROM journal WHERE run_id = ?1 ORDER BY id ASC")
            .map_err(backend_err)?;
        let rows = stmt
            .query_map(params![run_id.to_string()], |row| row.get::<_, String>(0))
            .map_err(backend_err)?;
        let mut entries = Vec::new();
        for row in rows {
            let raw = row.map_err(backend_err)?;
            entries.push(serde_json::from_str(&raw).map_err(backend_err)?);
        }
        Ok(entries)
    }

    async fn unfinished_runs(&self) -> Result<Vec<Uuid>, StoreError> {
        let conn = self.db.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT id FROM runs WHERE status IN ('pending', 'running', 'awaiting_skill_decision', 'awaiting_delegation_decision', 'awaiting_delegation_input')
                 ORDER BY created_at ASC",
            )
            .map_err(backend_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(backend_err)?;
        let mut ids = Vec::new();
        for row in rows {
            let raw = row.map_err(backend_err)?;
            ids.push(Uuid::parse_str(&raw).map_err(backend_err)?);
        }
        Ok(ids)
    }

    async fn prune(
        &self,
        policy: &kernel_core::RetentionPolicy,
    ) -> Result<kernel_core::PruneReport, StoreError> {
        if policy.is_noop() {
            return Ok(kernel_core::PruneReport::default());
        }
        let mut conn = self.db.conn.lock().await;

        // Only TERMINAL runs are candidates — the SQL filter guarantees an
        // in-flight (pending/running) run can never be selected for deletion.
        let terminal: Vec<(Uuid, u64)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT id, created_at FROM runs
                     WHERE status IN ('completed', 'failed', 'incomplete', 'canceled')
                     ORDER BY created_at DESC",
                )
                .map_err(backend_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(backend_err)?;
            let mut out = Vec::new();
            for row in rows {
                let (id, created) = row.map_err(backend_err)?;
                out.push((Uuid::parse_str(&id).map_err(backend_err)?, created as u64));
            }
            out
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let victims = policy.select(&terminal, now);
        if victims.is_empty() {
            return Ok(kernel_core::PruneReport::default());
        }

        // One transaction: journal + episodes + the run row per victim, so a
        // run and its markers are pruned together (never half-pruned).
        let tx = conn.transaction().map_err(backend_err)?;
        let mut journal_entries_pruned = 0usize;
        let mut episodes_pruned = 0usize;
        for id in &victims {
            let id = id.to_string();
            journal_entries_pruned += tx
                .execute("DELETE FROM journal WHERE run_id = ?1", params![id])
                .map_err(backend_err)?;
            episodes_pruned += tx
                .execute("DELETE FROM episodes WHERE run_id = ?1", params![id])
                .map_err(backend_err)?;
            tx.execute(
                "DELETE FROM episode_versions WHERE run_id = ?1",
                params![id],
            )
            .map_err(backend_err)?;
            tx.execute("DELETE FROM runs WHERE id = ?1", params![id])
                .map_err(backend_err)?;
        }
        tx.commit().map_err(backend_err)?;

        Ok(kernel_core::PruneReport {
            runs_pruned: victims.len(),
            journal_entries_pruned,
            episodes_pruned,
        })
    }
}

// ---- MemoryProvider -------------------------------------------------------

pub struct SqliteMemoryProvider {
    db: Db,
    retrieval: memory::Retrieval,
}

impl SqliteMemoryProvider {
    pub fn new(db: Db) -> Self {
        Self {
            db,
            retrieval: memory::Retrieval::Bm25,
        }
    }

    /// Use a specific retrieval strategy (Phase 8, item D) instead of BM25 —
    /// e.g. `Retrieval::Embeddings` backed by a llama.cpp embedding server.
    pub fn with_retrieval(db: Db, retrieval: memory::Retrieval) -> Self {
        Self { db, retrieval }
    }

    pub async fn episode_count(&self) -> Result<u64, MemoryError> {
        let conn = self.db.conn.lock().await;
        conn.query_row("SELECT COUNT(*) FROM episodes", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(|n| n as u64)
        .map_err(|e| MemoryError::Backend(e.to_string()))
    }
}

fn mem_err<E: std::fmt::Display>(e: E) -> MemoryError {
    MemoryError::Backend(e.to_string())
}

#[async_trait]
impl MemoryProvider for SqliteMemoryProvider {
    async fn write(&self, episode: Episode) -> Result<(), MemoryError> {
        let mut conn = self.db.conn.lock().await;
        let data = serde_json::to_string(&episode).map_err(mem_err)?;
        let id = episode.id.to_string();
        let run_id = episode.run_id.to_string();
        // One transaction: the version row (history) and the current row move
        // together. The current row only advances — a lower version never
        // displaces a higher one (ADR 0038).
        let tx = conn.transaction().map_err(mem_err)?;
        // A pre-v4 current row has no history row yet: preserve it before a
        // revision can overwrite it.
        tx.execute(
            "INSERT OR IGNORE INTO episode_versions (id, version, run_id, data, created_at)
             SELECT id, version, run_id, data, created_at FROM episodes WHERE id = ?1",
            params![id],
        )
        .map_err(mem_err)?;
        tx.execute(
            "INSERT INTO episode_versions (id, version, run_id, data, created_at)
             VALUES (?1, ?2, ?3, ?4, strftime('%s','now'))
             ON CONFLICT(id, version) DO UPDATE SET data = ?4",
            params![id, episode.version, run_id, data],
        )
        .map_err(mem_err)?;
        tx.execute(
            "INSERT INTO episodes (id, run_id, task, data, created_at, version)
             VALUES (?1, ?2, ?3, ?4, strftime('%s','now'), ?5)
             ON CONFLICT(id) DO UPDATE SET data = ?4, version = ?5
             WHERE ?5 >= episodes.version",
            params![id, run_id, episode.task, data, episode.version],
        )
        .map_err(mem_err)?;
        tx.commit().map_err(mem_err)?;
        Ok(())
    }

    async fn retrieve(&self, task: &str, limit: usize) -> Result<Vec<RankedContext>, MemoryError> {
        // Load (id, task) pairs and rank with the shared BM25 contract. At
        // the current scale this is a full scan; a term index or embedding
        // backend slots in behind the same trait when needed.
        let episodes: Vec<Episode> = {
            let conn = self.db.conn.lock().await;
            let mut stmt = conn
                .prepare("SELECT data FROM episodes ORDER BY created_at ASC")
                .map_err(mem_err)?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(mem_err)?;
            let mut out = Vec::new();
            for row in rows {
                let raw = row.map_err(mem_err)?;
                out.push(serde_json::from_str(&raw).map_err(mem_err)?);
            }
            out
        };

        // ADR 0038: an `unverified` episode never reaches model context, nor
        // takes part in ranking.
        let episodes: Vec<Episode> = episodes.into_iter().filter(in_model_context).collect();
        let docs: Vec<String> = episodes.iter().map(|e| e.task.clone()).collect();
        let ranked = self.retrieval.rank(task, &docs, limit).await;
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
        let conn = self.db.conn.lock().await;
        match item {
            GraphUpsert::Entity { name, attributes } => {
                conn.execute(
                    "INSERT INTO graph_entities (name, attributes) VALUES (?1, ?2)
                     ON CONFLICT(name) DO UPDATE SET attributes = ?2",
                    params![name, attributes.to_string()],
                )
                .map_err(mem_err)?;
            }
            GraphUpsert::Relation { from, to, label } => {
                conn.execute(
                    "INSERT INTO graph_relations (from_name, to_name, label)
                     VALUES (?1, ?2, ?3)",
                    params![from, to, label],
                )
                .map_err(mem_err)?;
            }
        }
        Ok(())
    }

    async fn episode_history(&self, run_id: Uuid) -> Result<Vec<Episode>, MemoryError> {
        // `episode_versions` holds everything written since schema v4; a row
        // from before v4 exists only in `episodes` (as its version 1), so
        // read both and drop the duplicate current rows.
        let conn = self.db.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT data FROM episode_versions WHERE run_id = ?1
                 UNION ALL
                 SELECT data FROM episodes WHERE run_id = ?1",
            )
            .map_err(mem_err)?;
        let rows = stmt
            .query_map(params![run_id.to_string()], |row| row.get::<_, String>(0))
            .map_err(mem_err)?;
        let mut out: Vec<Episode> = Vec::new();
        for row in rows {
            let e: Episode = serde_json::from_str(&row.map_err(mem_err)?).map_err(mem_err)?;
            if !out.iter().any(|o| o.id == e.id && o.version == e.version) {
                out.push(e);
            }
        }
        out.sort_by_key(|e| (e.id, e.version));
        Ok(out)
    }
}

// ---- LearnedStore (governed self-learning, ADR 0016) ----------------------

use kernel_core::{LearnedFact, LearnedPlan, LearnedStore};

/// SQLite-backed learned layer: facts AND plans. Each is a DISTINCT table
/// from `episodes`; ranking reuses the shared BM25 contract, recomputed per
/// retrieval, so `forget` (a row delete) leaves zero residue.
pub struct SqliteLearnedStore {
    db: Db,
}

impl SqliteLearnedStore {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    async fn load_all(&self) -> Result<Vec<LearnedFact>, StoreError> {
        let conn = self.db.conn.lock().await;
        let mut stmt = conn
            .prepare("SELECT data FROM learned_facts ORDER BY created_at DESC, id ASC")
            .map_err(backend_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(backend_err)?;
        let mut out = Vec::new();
        for row in rows {
            let raw = row.map_err(backend_err)?;
            out.push(serde_json::from_str(&raw).map_err(backend_err)?);
        }
        Ok(out)
    }

    async fn load_all_plans(&self) -> Result<Vec<LearnedPlan>, StoreError> {
        let conn = self.db.conn.lock().await;
        let mut stmt = conn
            .prepare("SELECT data FROM learned_plans ORDER BY created_at DESC, id ASC")
            .map_err(backend_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(backend_err)?;
        let mut out = Vec::new();
        for row in rows {
            let raw = row.map_err(backend_err)?;
            out.push(serde_json::from_str(&raw).map_err(backend_err)?);
        }
        Ok(out)
    }
}

#[async_trait]
impl LearnedStore for SqliteLearnedStore {
    async fn put(&self, fact: LearnedFact) -> Result<(), StoreError> {
        let conn = self.db.conn.lock().await;
        let data = serde_json::to_string(&fact).map_err(backend_err)?;
        conn.execute(
            "INSERT INTO learned_facts (id, task_context, batch_id, data, created_at)
             VALUES (?1, ?2, ?3, ?4, strftime('%s','now'))
             ON CONFLICT(id) DO UPDATE SET data = ?4",
            params![
                fact.id.to_string(),
                fact.task_context,
                fact.provenance.batch_id.to_string(),
                data
            ],
        )
        .map_err(backend_err)?;
        Ok(())
    }

    async fn retrieve(&self, task: &str, limit: usize) -> Result<Vec<LearnedFact>, StoreError> {
        let facts = self.load_all().await?;
        if facts.is_empty() {
            return Ok(Vec::new());
        }
        // Rank fresh over task_context — no persistent index, so a forgotten
        // fact leaves nothing behind.
        let docs: Vec<String> = facts.iter().map(|f| f.task_context.clone()).collect();
        let ranked = memory::retrieval::rank_bm25(task, &docs, limit);
        Ok(ranked.into_iter().map(|(i, _)| facts[i].clone()).collect())
    }

    async fn list(&self) -> Result<Vec<LearnedFact>, StoreError> {
        self.load_all().await
    }

    async fn put_plan(&self, plan: LearnedPlan) -> Result<(), StoreError> {
        let conn = self.db.conn.lock().await;
        let data = serde_json::to_string(&plan).map_err(backend_err)?;
        conn.execute(
            "INSERT INTO learned_plans (id, task_context, batch_id, data, created_at)
             VALUES (?1, ?2, ?3, ?4, strftime('%s','now'))
             ON CONFLICT(id) DO UPDATE SET data = ?4",
            params![
                plan.id.to_string(),
                plan.task_context,
                plan.provenance.batch_id.to_string(),
                data
            ],
        )
        .map_err(backend_err)?;
        Ok(())
    }

    async fn retrieve_plans(
        &self,
        task: &str,
        limit: usize,
    ) -> Result<Vec<LearnedPlan>, StoreError> {
        let plans = self.load_all_plans().await?;
        if plans.is_empty() {
            return Ok(Vec::new());
        }
        // Rank fresh over task_context — no persistent index, so a forgotten
        // plan leaves nothing behind.
        let docs: Vec<String> = plans.iter().map(|p| p.task_context.clone()).collect();
        let ranked = memory::retrieval::rank_bm25(task, &docs, limit);
        Ok(ranked.into_iter().map(|(i, _)| plans[i].clone()).collect())
    }

    async fn list_plans(&self) -> Result<Vec<LearnedPlan>, StoreError> {
        self.load_all_plans().await
    }

    async fn forget(&self, id: uuid::Uuid) -> Result<bool, StoreError> {
        let conn = self.db.conn.lock().await;
        let facts_removed = conn
            .execute(
                "DELETE FROM learned_facts WHERE id = ?1",
                params![id.to_string()],
            )
            .map_err(backend_err)?;
        let plans_removed = conn
            .execute(
                "DELETE FROM learned_plans WHERE id = ?1",
                params![id.to_string()],
            )
            .map_err(backend_err)?;
        Ok(facts_removed > 0 || plans_removed > 0)
    }

    async fn forget_batch(&self, batch_id: uuid::Uuid) -> Result<u64, StoreError> {
        let conn = self.db.conn.lock().await;
        let facts_removed = conn
            .execute(
                "DELETE FROM learned_facts WHERE batch_id = ?1",
                params![batch_id.to_string()],
            )
            .map_err(backend_err)?;
        let plans_removed = conn
            .execute(
                "DELETE FROM learned_plans WHERE batch_id = ?1",
                params![batch_id.to_string()],
            )
            .map_err(backend_err)?;
        Ok((facts_removed + plans_removed) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel_core::{Run, RunStatus};
    use memory::{EpisodeOutcome, Provenance};

    fn run(status: RunStatus) -> Run {
        Run {
            id: Uuid::new_v4(),
            task: "test task".into(),
            status,
            steps: Vec::new(),
            output: None,
            error: None,
            policy: "mock".into(),
            created_at: 1,
            usage: Default::default(),
            agent_usage: Default::default(),
            trust_posture: Vec::new(),
            cost_usd: None,
            active_skills: Vec::new(),
            pending_skill_activation: None,
            pending_delegation: None,
            pending_delegation_input: None,
            a2a_depth: 0,
            a2a_ancestry: Vec::new(),
            max_steps: None,
            success_contract: None,
            verification: None,
            contract_report: None,
            confirmation: None,
        }
    }

    #[tokio::test]
    async fn runs_and_journal_roundtrip() {
        let store = SqliteRunStore::new(Db::open_in_memory().unwrap());
        let mut r = run(RunStatus::Running);
        let id = r.id;
        store.put(r.clone()).await.unwrap();

        store
            .append_journal(
                id,
                JournalEntry::Decision {
                    step: 0,
                    action: "x".into(),
                    arguments: serde_json::json!({"a": 1}),
                    usage: None,
                },
            )
            .await
            .unwrap();
        store
            .append_journal(
                id,
                JournalEntry::Receipt {
                    step: 0,
                    ok: true,
                    result: serde_json::json!({}),
                },
            )
            .await
            .unwrap();

        let journal = store.journal(id).await.unwrap();
        assert_eq!(journal.len(), 2);
        assert!(matches!(journal[0], JournalEntry::Decision { .. }));

        assert_eq!(store.unfinished_runs().await.unwrap(), vec![id]);
        r.status = RunStatus::Completed;
        store.put(r).await.unwrap();
        assert!(store.unfinished_runs().await.unwrap().is_empty());
        assert_eq!(store.list().await.unwrap().len(), 1);
    }

    /// Regression test for audit finding H-1 (docs/audit/AUDIT-2026-07-21.md):
    /// the SQLite `unfinished_runs` query's `status IN (...)` list had drifted
    /// from `RunStatus`'s actual non-terminal set — `awaiting_delegation_input`
    /// (ADR 0026) was missing, silently orphaning a paused interactive
    /// delegation across a daemon restart. `InMemoryRunStore` (kernel-core's
    /// reference store, matches every non-terminal variant structurally via
    /// `matches!`) never exhibited this because it can't drift the same way —
    /// this test exercises the REAL SqliteRunStore query string against every
    /// `RunStatus` variant, so a future status added to the enum without a
    /// matching SQL literal fails loudly here instead of only in production.
    #[tokio::test]
    async fn unfinished_runs_includes_every_non_terminal_status_sqlite() {
        let store = SqliteRunStore::new(Db::open_in_memory().unwrap());
        let non_terminal = [
            RunStatus::Pending,
            RunStatus::Running,
            RunStatus::AwaitingSkillDecision,
            RunStatus::AwaitingDelegationDecision,
            RunStatus::AwaitingDelegationInput,
        ];
        let mut expected: Vec<Uuid> = Vec::new();
        for status in non_terminal {
            let r = run(status);
            expected.push(r.id);
            store.put(r).await.unwrap();
        }
        // Terminal statuses must NOT appear in unfinished_runs.
        for status in [
            RunStatus::Completed,
            RunStatus::Failed,
            RunStatus::Incomplete,
            RunStatus::Canceled,
        ] {
            store.put(run(status)).await.unwrap();
        }

        let mut got = store.unfinished_runs().await.unwrap();
        got.sort();
        expected.sort();
        assert_eq!(
            got, expected,
            "every non-terminal RunStatus (incl. awaiting_delegation_input, ADR 0026) must \
             survive a restart via unfinished_runs — a status missing here orphans a paused \
             run on the production SQLite store (audit H-1)"
        );
    }

    #[tokio::test]
    async fn get_missing_is_not_found() {
        let store = SqliteRunStore::new(Db::open_in_memory().unwrap());
        assert!(matches!(
            store.get(Uuid::new_v4()).await,
            Err(StoreError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn episodes_persist_and_rank() {
        let db = Db::open_in_memory().unwrap();
        let mem = SqliteMemoryProvider::new(db);
        for task in [
            "extract links from a webpage",
            "append an entry to the audit ledger",
            "summarize the sales report",
        ] {
            mem.write(Episode {
                id: Uuid::new_v4(),
                run_id: Uuid::new_v4(),
                task: task.into(),
                trace: vec![],
                outcome: EpisodeOutcome::Success,
                provenance: Provenance::Reflection,
                version: 1,
            })
            .await
            .unwrap();
        }
        let hits = mem.retrieve("audit ledger entry", 2).await.unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].summary.contains("audit ledger"));

        mem.upsert(GraphUpsert::Entity {
            name: "ledger".into(),
            attributes: serde_json::json!({"kind": "file"}),
        })
        .await
        .unwrap();
    }

    fn ep(run_id: Uuid, task: &str, outcome: EpisodeOutcome) -> Episode {
        Episode {
            id: Uuid::new_v4(),
            run_id,
            task: task.into(),
            trace: vec!["finish: ok".into()],
            outcome,
            provenance: Provenance::Reflection,
            version: 1,
        }
    }

    /// ADR 0038: confirm (v2 Success) and revoke (v3 Unverified) are new
    /// versions of one episode; retrieval (= model context) never returns it
    /// while it is `unverified` and returns it once `success`; the history
    /// keeps all three, and a stale lower version never wins.
    #[tokio::test]
    async fn episode_transitions_are_versioned_and_history_is_kept() {
        let mem = SqliteMemoryProvider::new(Db::open_in_memory().unwrap());
        let run_id = Uuid::new_v4();
        let v1 = ep(run_id, "reconcile the ledger", EpisodeOutcome::Unverified);
        mem.write(v1.clone()).await.unwrap();
        assert!(mem
            .retrieve("reconcile ledger", 5)
            .await
            .unwrap()
            .is_empty());

        let mut v2 = v1.clone();
        v2.version = 2;
        v2.outcome = EpisodeOutcome::Success;
        v2.provenance = Provenance::Human;
        mem.write(v2.clone()).await.unwrap();
        let hits = mem.retrieve("reconcile ledger", 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].summary.contains("outcome: Success"));

        let mut v3 = v2.clone();
        v3.version = 3;
        v3.outcome = EpisodeOutcome::Unverified;
        mem.write(v3).await.unwrap();
        mem.write(v1.clone()).await.unwrap(); // stale re-write
        assert!(mem
            .retrieve("reconcile ledger", 5)
            .await
            .unwrap()
            .is_empty());

        let hist = mem.episode_history(run_id).await.unwrap();
        assert_eq!(
            hist.iter()
                .map(|e| (e.version, e.outcome))
                .collect::<Vec<_>>(),
            vec![
                (1, EpisodeOutcome::Unverified),
                (2, EpisodeOutcome::Success),
                (3, EpisodeOutcome::Unverified),
            ]
        );
        assert_eq!(mem.episode_count().await.unwrap(), 1);
        assert!(mem
            .episode_history(Uuid::new_v4())
            .await
            .unwrap()
            .is_empty());
    }

    /// A database written before schema v4 (ADR 0038): its episode row reads
    /// unchanged after the migration, counts as version 1 in the history, and
    /// is preserved there when a revision supersedes it.
    #[tokio::test]
    async fn pre_v4_episode_reads_after_migration_and_survives_revision() {
        let dir = std::env::temp_dir().join(format!("kelter-db-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kelter.db");
        let run_id = Uuid::new_v4();
        let old = ep(run_id, "rotate the logs", EpisodeOutcome::Success);
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..3] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 3).unwrap();
            conn.execute(
                "INSERT INTO episodes (id, run_id, task, data, created_at) VALUES (?1, ?2, ?3, ?4, 0)",
                params![
                    old.id.to_string(),
                    run_id.to_string(),
                    old.task,
                    serde_json::to_string(&old).unwrap()
                ],
            )
            .unwrap();
        }
        let mem = SqliteMemoryProvider::new(Db::open(&path).unwrap());
        let hits = mem.retrieve("rotate logs", 5).await.unwrap();
        assert!(hits[0].summary.contains("outcome: Success"));
        let hist = mem.episode_history(run_id).await.unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].outcome, EpisodeOutcome::Success);

        let mut v2 = old.clone();
        v2.version = 2;
        v2.outcome = EpisodeOutcome::Unverified;
        mem.write(v2).await.unwrap();
        let hist = mem.episode_history(run_id).await.unwrap();
        assert_eq!(
            hist.iter()
                .map(|e| (e.version, e.outcome))
                .collect::<Vec<_>>(),
            vec![
                (1, EpisodeOutcome::Success),
                (2, EpisodeOutcome::Unverified)
            ]
        );
    }

    #[test]
    fn migrations_are_idempotent_across_reopen() {
        let dir = std::env::temp_dir().join(format!("kelter-db-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kelter.db");
        {
            let _ = Db::open(&path).unwrap();
        }
        // Re-opening runs migrate() again over an up-to-date schema —
        // must be a no-op, not an error. (Safe to run repeatedly.)
        let _ = Db::open(&path).unwrap();
    }

    fn fact(task: &str, batch: Uuid) -> LearnedFact {
        LearnedFact {
            id: Uuid::new_v4(),
            fact: format!("approach for {task}"),
            task_context: task.into(),
            provenance: kernel_core::LearnedProvenance {
                source_run_ids: vec![Uuid::new_v4()],
                distilled_at: 0,
                model: "mock".into(),
                level: kernel_core::LEARNED_LEVEL.into(),
                human_validated: false,
                batch_id: batch,
            },
        }
    }

    #[tokio::test]
    async fn prune_removes_terminal_runs_journal_episodes_but_not_in_flight() {
        let db = Db::open_in_memory().unwrap();
        let store = SqliteRunStore::new(db.clone());
        let memory = SqliteMemoryProvider::new(db.clone());

        // 3 terminal runs (with journal + an episode each) + 1 in-flight.
        let mut terminal = Vec::new();
        for i in 0..3 {
            let mut r = run(RunStatus::Completed);
            r.created_at = i; // oldest-first
            let id = r.id;
            terminal.push(id);
            store.put(r).await.unwrap();
            store
                .append_journal(
                    id,
                    JournalEntry::Terminal {
                        status: RunStatus::Completed,
                        output: None,
                        error: None,
                        verification: None,
                        contract_report: None,
                    },
                )
                .await
                .unwrap();
            memory.write(episode_for(id)).await.unwrap();
        }
        let mut inflight = run(RunStatus::Running);
        inflight.created_at = 999;
        let inflight_id = inflight.id;
        store.put(inflight).await.unwrap();

        // Keep the newest 1 terminal → prune the 2 oldest.
        let report = store
            .prune(&kernel_core::RetentionPolicy {
                max_runs: Some(1),
                max_age_secs: None,
            })
            .await
            .unwrap();
        assert_eq!(report.runs_pruned, 2);
        assert_eq!(report.journal_entries_pruned, 2);
        assert_eq!(report.episodes_pruned, 2);

        // The in-flight run and its (empty) journal are untouched.
        assert!(store.get(inflight_id).await.is_ok());
        // The 2 oldest terminal runs + their journal are gone.
        assert!(store.get(terminal[0]).await.is_err());
        assert!(store.journal(terminal[0]).await.unwrap().is_empty());
        // Only 1 episode remains (the surviving terminal run's).
        assert_eq!(memory.episode_count().await.unwrap(), 1);
    }

    fn episode_for(run_id: Uuid) -> memory::Episode {
        memory::Episode {
            id: Uuid::new_v4(),
            run_id,
            task: "t".into(),
            trace: vec![],
            outcome: memory::EpisodeOutcome::Success,
            provenance: memory::Provenance::Reflection,
            version: 1,
        }
    }

    #[tokio::test]
    async fn sqlite_learned_store_ranks_and_forgets_without_residue() {
        let store = SqliteLearnedStore::new(Db::open_in_memory().unwrap());
        let batch = Uuid::new_v4();
        let a = fact("extract links from a webpage", batch);
        let a_id = a.id;
        store.put(a).await.unwrap();
        store
            .put(fact("summarise the quarterly sales report", batch))
            .await
            .unwrap();

        // BM25 over task_context ranks the relevant fact first.
        let hits = store.retrieve("webpage links extraction", 5).await.unwrap();
        assert_eq!(hits[0].task_context, "extract links from a webpage");

        // forget one → gone from retrieval (no residue), the other remains.
        assert!(store.forget(a_id).await.unwrap());
        let hits = store.retrieve("webpage links extraction", 5).await.unwrap();
        assert!(hits.iter().all(|f| f.id != a_id));
        assert_eq!(store.list().await.unwrap().len(), 1);

        // forget the batch → the layer is empty.
        assert_eq!(store.forget_batch(batch).await.unwrap(), 1);
        assert!(store.list().await.unwrap().is_empty());
    }

    fn plan_fixture(task: &str, batch: Uuid) -> LearnedPlan {
        LearnedPlan {
            id: Uuid::new_v4(),
            task_context: task.into(),
            plan: kernel_core::Plan {
                nodes: vec![
                    kernel_core::PlanNode {
                        id: "a".into(),
                        node_type: kernel_core::NodeType::Tool,
                        action: Some("read_source".into()),
                        arguments: serde_json::json!({"path": "report.csv"}),
                        deps: Vec::new(),
                    },
                    kernel_core::PlanNode {
                        id: "b".into(),
                        node_type: kernel_core::NodeType::Review,
                        action: None,
                        arguments: serde_json::Value::Null,
                        deps: vec!["a".into()],
                    },
                ],
            },
            provenance: kernel_core::LearnedProvenance {
                source_run_ids: vec![Uuid::new_v4()],
                distilled_at: 0,
                model: "mock".into(),
                level: kernel_core::LEARNED_LEVEL.into(),
                human_validated: false,
                batch_id: batch,
            },
        }
    }

    #[tokio::test]
    async fn sqlite_learned_plan_store_ranks_and_forgets_without_residue() {
        let store = SqliteLearnedStore::new(Db::open_in_memory().unwrap());
        let batch = Uuid::new_v4();
        let a = plan_fixture("extract links from a webpage", batch);
        let a_id = a.id;
        store.put_plan(a.clone()).await.unwrap();
        store
            .put_plan(plan_fixture("summarise the quarterly sales report", batch))
            .await
            .unwrap();

        store
            .put_plan(plan_fixture("draft a release changelog", batch))
            .await
            .unwrap();

        // BM25 over task_context ranks the relevant plan first.
        let hits = store
            .retrieve_plans("webpage links extraction", 5)
            .await
            .unwrap();
        assert_eq!(hits[0].task_context, "extract links from a webpage");

        // `limit` is respected even with more candidates available.
        assert_eq!(
            store
                .retrieve_plans("webpage links extraction", 1)
                .await
                .unwrap()
                .len(),
            1
        );

        // Round-trip: nodes, deps, and arguments survive put -> list -> retrieve.
        let round_tripped = store.list_plans().await.unwrap();
        let got = round_tripped.iter().find(|p| p.id == a_id).unwrap();
        assert_eq!(got.plan.nodes.len(), a.plan.nodes.len());
        assert_eq!(got.plan.nodes[0].arguments, a.plan.nodes[0].arguments);
        assert_eq!(got.plan.nodes[1].deps, a.plan.nodes[1].deps);

        // forget one -> gone from retrieval (no residue), the other two remain.
        assert!(store.forget(a_id).await.unwrap());
        let hits = store
            .retrieve_plans("webpage links extraction", 5)
            .await
            .unwrap();
        assert!(hits.iter().all(|p| p.id != a_id));
        assert_eq!(store.list_plans().await.unwrap().len(), 2);

        // forget the batch -> the layer is empty.
        assert_eq!(store.forget_batch(batch).await.unwrap(), 2);
        assert!(store.list_plans().await.unwrap().is_empty());
    }

    /// Load-bearing: `forget_batch` is the ONE reversal path for a whole
    /// distillation call, and a distillation call can produce BOTH facts and
    /// plans tagged with the same `batch_id`. A `forget_batch` that only
    /// cleared one artifact kind would leave the other as residue — exactly
    /// the bug this phase must not ship.
    #[tokio::test]
    async fn forget_batch_removes_facts_and_plans_from_the_same_batch() {
        let store = SqliteLearnedStore::new(Db::open_in_memory().unwrap());
        let batch = Uuid::new_v4();
        store.put(fact("a fact in the batch", batch)).await.unwrap();
        store
            .put_plan(plan_fixture("a plan in the batch", batch))
            .await
            .unwrap();
        // A fact/plan from an unrelated batch must survive.
        let other_batch = Uuid::new_v4();
        store
            .put(fact("unrelated fact", other_batch))
            .await
            .unwrap();

        let removed = store.forget_batch(batch).await.unwrap();
        assert_eq!(removed, 2, "one fact and one plan from the same batch");
        assert!(store
            .list()
            .await
            .unwrap()
            .iter()
            .all(|f| f.provenance.batch_id != batch));
        assert!(store.list_plans().await.unwrap().is_empty());
        assert_eq!(store.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn forget_on_a_plan_id_works_and_reports_whether_it_existed() {
        let store = SqliteLearnedStore::new(Db::open_in_memory().unwrap());
        let batch = Uuid::new_v4();
        let plan = plan_fixture("some recurring task", batch);
        let id = plan.id;
        store.put_plan(plan).await.unwrap();

        assert!(store.forget(id).await.unwrap());
        assert!(store.list_plans().await.unwrap().is_empty());
        // Forgetting again (or an id that never existed) reports false, not an error.
        assert!(!store.forget(id).await.unwrap());
        assert!(!store.forget(Uuid::new_v4()).await.unwrap());
    }
}
