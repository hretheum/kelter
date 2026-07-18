//! Run + journal persistence trait and the in-memory reference
//! implementation (tests, ephemeral topologies). The durable SQLite
//! implementation lives in the `storage` crate.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::engine::{Run, RunStatus};
use crate::journal::JournalEntry;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("run {0} not found")]
    NotFound(Uuid),
    #[error("store backend error: {0}")]
    Backend(String),
}

#[async_trait]
pub trait RunStore: Send + Sync {
    async fn put(&self, run: Run) -> Result<(), StoreError>;
    async fn get(&self, id: Uuid) -> Result<Run, StoreError>;
    async fn list(&self) -> Result<Vec<Run>, StoreError>;

    /// Append one journal entry for a run, durably, in order.
    async fn append_journal(&self, run_id: Uuid, entry: JournalEntry) -> Result<(), StoreError>;
    /// The full journal for a run, in append order.
    async fn journal(&self, run_id: Uuid) -> Result<Vec<JournalEntry>, StoreError>;
    /// Runs that never reached a terminal state (candidates for resume).
    async fn unfinished_runs(&self) -> Result<Vec<Uuid>, StoreError>;
}

#[derive(Default)]
pub struct InMemoryRunStore {
    runs: Arc<RwLock<HashMap<Uuid, Run>>>,
    journals: Arc<RwLock<HashMap<Uuid, Vec<JournalEntry>>>>,
}

impl InMemoryRunStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl RunStore for InMemoryRunStore {
    async fn put(&self, run: Run) -> Result<(), StoreError> {
        self.runs.write().await.insert(run.id, run);
        Ok(())
    }

    async fn get(&self, id: Uuid) -> Result<Run, StoreError> {
        self.runs
            .read()
            .await
            .get(&id)
            .cloned()
            .ok_or(StoreError::NotFound(id))
    }

    async fn list(&self) -> Result<Vec<Run>, StoreError> {
        let mut runs: Vec<Run> = self.runs.read().await.values().cloned().collect();
        runs.sort_by_key(|r| r.created_at);
        Ok(runs)
    }

    async fn append_journal(&self, run_id: Uuid, entry: JournalEntry) -> Result<(), StoreError> {
        self.journals
            .write()
            .await
            .entry(run_id)
            .or_default()
            .push(entry);
        Ok(())
    }

    async fn journal(&self, run_id: Uuid) -> Result<Vec<JournalEntry>, StoreError> {
        Ok(self
            .journals
            .read()
            .await
            .get(&run_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn unfinished_runs(&self) -> Result<Vec<Uuid>, StoreError> {
        Ok(self
            .runs
            .read()
            .await
            .values()
            .filter(|r| matches!(r.status, RunStatus::Pending | RunStatus::Running))
            .map(|r| r.id)
            .collect())
    }
}
