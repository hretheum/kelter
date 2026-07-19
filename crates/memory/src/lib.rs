//! Memory provider interface and a reference in-memory implementation.
//!
//! Memory is a *provider interface*, not a storage engine: the kernel talks to
//! `MemoryProvider` and any backend (knowledge graph, vector store, hybrid)
//! can be plugged in behind it. Retrieval is a tested component with a
//! relevance metric, not "dump everything into the prompt".

pub mod embeddings;
pub mod retrieval;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use embeddings::EmbeddingBackend;

/// The retrieval-scoring strategy behind `MemoryProvider::retrieve` — the
/// Phase 3 seam made switchable (Phase 8, item D). BM25 is the default;
/// `Embeddings` uses a real embedding backend and cosine similarity, falling
/// back to BM25 if the backend errors (degraded ranking, never a broken run).
#[derive(Clone, Default)]
pub enum Retrieval {
    #[default]
    Bm25,
    Embeddings(Arc<dyn EmbeddingBackend>),
}

impl Retrieval {
    pub fn describe(&self) -> String {
        match self {
            Retrieval::Bm25 => "bm25".into(),
            Retrieval::Embeddings(b) => b.describe(),
        }
    }

    /// Rank `docs` against `query`, returning normalized `(index, score)`
    /// pairs (best first, ≤ `limit`) — the same shape both strategies produce.
    pub async fn rank(&self, query: &str, docs: &[String], limit: usize) -> Vec<(usize, f64)> {
        match self {
            Retrieval::Bm25 => retrieval::normalize(&retrieval::rank_bm25(query, docs, limit)),
            Retrieval::Embeddings(backend) => {
                match embeddings::rank_embeddings(backend.as_ref(), query, docs, limit).await {
                    Ok(scored) => retrieval::normalize(&scored),
                    Err(e) => {
                        tracing::warn!(
                            "retrieval: embedding backend failed ({e}); falling back to BM25"
                        );
                        retrieval::normalize(&retrieval::rank_bm25(query, docs, limit))
                    }
                }
            }
        }
    }
}
use uuid::Uuid;

/// Where a memory entry came from. Every write is provenance-tagged so the
/// self-improvement loop stays auditable and reversible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Written by the kernel's post-run reflection step.
    Reflection,
    /// Written by a human operator.
    Human,
    /// Imported from an external system.
    Import,
}

/// A single episodic record: what task ran, what happened, how it ended.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    pub id: Uuid,
    pub run_id: Uuid,
    pub task: String,
    /// Compact step-by-step trace (action name + outcome summary per step).
    pub trace: Vec<String>,
    pub outcome: EpisodeOutcome,
    pub provenance: Provenance,
    /// Monotonic version for reversibility; in-memory impl always stores v1.
    pub version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeOutcome {
    Success,
    Failure,
}

/// A retrieved piece of context, ranked by relevance to the current task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankedContext {
    pub episode_id: Uuid,
    pub summary: String,
    /// Relevance score in [0, 1]. Higher is more relevant.
    pub score: f64,
}

/// An entity or relation upsert for graph-shaped backends.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GraphUpsert {
    Entity {
        name: String,
        attributes: serde_json::Value,
    },
    Relation {
        from: String,
        to: String,
        label: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("memory backend error: {0}")]
    Backend(String),
}

/// The stable provider protocol: `write(episode)`, `retrieve(task) ->
/// ranked_context`, `upsert(entity|relation)`. Backends are swappable; the
/// kernel never depends on a concrete store.
#[async_trait]
pub trait MemoryProvider: Send + Sync {
    async fn write(&self, episode: Episode) -> Result<(), MemoryError>;
    async fn retrieve(&self, task: &str, limit: usize) -> Result<Vec<RankedContext>, MemoryError>;
    async fn upsert(&self, item: GraphUpsert) -> Result<(), MemoryError>;
}

/// Reference in-memory implementation. Retrieval ranks episodes by token
/// overlap with the query task — deliberately simple, but it is a *scored*
/// retrieval with unit tests, which is the contract real backends must keep.
#[derive(Default)]
pub struct InMemoryProvider {
    inner: Arc<RwLock<Store>>,
    retrieval: Retrieval,
}

#[derive(Default)]
struct Store {
    episodes: Vec<Episode>,
    entities: HashMap<String, serde_json::Value>,
    relations: Vec<(String, String, String)>,
}

impl InMemoryProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use a specific retrieval strategy (Phase 8, item D) instead of BM25.
    pub fn with_retrieval(retrieval: Retrieval) -> Self {
        Self {
            retrieval,
            ..Self::default()
        }
    }

    /// Number of stored episodes (used by tests and the reflection smoke path).
    pub async fn episode_count(&self) -> usize {
        self.inner.read().await.episodes.len()
    }

    pub async fn episodes(&self) -> Vec<Episode> {
        self.inner.read().await.episodes.clone()
    }
}

/// One-line summary of an episode for prompt context.
pub fn episode_summary(e: &Episode) -> String {
    format!(
        "task: {} | outcome: {:?} | steps: {}",
        e.task,
        e.outcome,
        e.trace.len()
    )
}

#[async_trait]
impl MemoryProvider for InMemoryProvider {
    async fn write(&self, episode: Episode) -> Result<(), MemoryError> {
        self.inner.write().await.episodes.push(episode);
        Ok(())
    }

    async fn retrieve(&self, task: &str, limit: usize) -> Result<Vec<RankedContext>, MemoryError> {
        let store = self.inner.read().await;
        let docs: Vec<String> = store.episodes.iter().map(|e| e.task.clone()).collect();
        let ranked = self.retrieval.rank(task, &docs, limit).await;
        Ok(ranked
            .into_iter()
            .map(|(i, score)| RankedContext {
                episode_id: store.episodes[i].id,
                summary: episode_summary(&store.episodes[i]),
                score,
            })
            .collect())
    }

    async fn upsert(&self, item: GraphUpsert) -> Result<(), MemoryError> {
        let mut store = self.inner.write().await;
        match item {
            GraphUpsert::Entity { name, attributes } => {
                store.entities.insert(name, attributes);
            }
            GraphUpsert::Relation { from, to, label } => {
                store.relations.push((from, to, label));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn episode(task: &str) -> Episode {
        Episode {
            id: Uuid::new_v4(),
            run_id: Uuid::new_v4(),
            task: task.to_string(),
            trace: vec!["read_source: ok".into()],
            outcome: EpisodeOutcome::Success,
            provenance: Provenance::Reflection,
            version: 1,
        }
    }

    #[tokio::test]
    async fn write_then_retrieve_ranks_by_relevance() {
        let mem = InMemoryProvider::new();
        mem.write(episode("summarize the quarterly sales report"))
            .await
            .unwrap();
        mem.write(episode("extract links from a webpage"))
            .await
            .unwrap();

        let hits = mem
            .retrieve("extract structured links from webpage", 5)
            .await
            .unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].summary.contains("extract links"));
        assert!(hits[0].score > 0.0);
    }

    #[tokio::test]
    async fn retrieve_respects_limit_and_filters_irrelevant() {
        let mem = InMemoryProvider::new();
        for i in 0..10 {
            mem.write(episode(&format!("download file number {i}")))
                .await
                .unwrap();
        }
        let hits = mem.retrieve("download file", 3).await.unwrap();
        assert_eq!(hits.len(), 3);
        let none = mem.retrieve("zzz qqq xxx", 3).await.unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn upsert_entities_and_relations() {
        let mem = InMemoryProvider::new();
        mem.upsert(GraphUpsert::Entity {
            name: "report".into(),
            attributes: serde_json::json!({"format": "markdown"}),
        })
        .await
        .unwrap();
        mem.upsert(GraphUpsert::Relation {
            from: "report".into(),
            to: "webpage".into(),
            label: "derived_from".into(),
        })
        .await
        .unwrap();
    }
}
