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
    episode_summary, retrieval, Episode, GraphUpsert, MemoryError, MemoryProvider, RankedContext,
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
                "SELECT id FROM runs WHERE status IN ('pending', 'running')
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
}

// ---- MemoryProvider -------------------------------------------------------

pub struct SqliteMemoryProvider {
    db: Db,
}

impl SqliteMemoryProvider {
    pub fn new(db: Db) -> Self {
        Self { db }
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
        let conn = self.db.conn.lock().await;
        let data = serde_json::to_string(&episode).map_err(mem_err)?;
        conn.execute(
            "INSERT INTO episodes (id, run_id, task, data, created_at)
             VALUES (?1, ?2, ?3, ?4, strftime('%s','now'))
             ON CONFLICT(id) DO UPDATE SET data = ?4",
            params![
                episode.id.to_string(),
                episode.run_id.to_string(),
                episode.task,
                data
            ],
        )
        .map_err(mem_err)?;
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
}
