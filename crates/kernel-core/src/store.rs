//! Run persistence trait + reference in-memory implementation.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::engine::Run;

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
}

#[derive(Default)]
pub struct InMemoryRunStore {
    runs: Arc<RwLock<HashMap<Uuid, Run>>>,
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
}
