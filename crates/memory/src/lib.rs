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
    /// Monotonic version (ADR 0016 reversibility). Reflection writes v1; a
    /// human's confirmation or revocation (ADR 0038) writes the next version
    /// of the SAME `id` — see [`MemoryProvider::write`] for the contract.
    pub version: u32,
}

/// How an episode's run ended, as far as memory may treat it (ADR 0005,
/// three states since ADR 0038).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeOutcome {
    /// A verified success: the run's success contract held (ADR 0037), or a
    /// human confirmed it. Episodes written before ADR 0038 also carry this
    /// value for any `completed` run and are left as they are.
    Success,
    Failure,
    /// The run ended `completed` but nothing verified it — no contract, no
    /// human confirmation yet (ADR 0038). Retrieved into context, marked as
    /// such, never presented as a success pattern.
    Unverified,
}

/// A retrieved piece of context, ranked by relevance to the current task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankedContext {
    pub episode_id: Uuid,
    pub summary: String,
    /// Relevance score in [0, 1]. Higher is more relevant.
    pub score: f64,
}

/// The canonical response contract for `retrieve` when a backend lives on
/// the other side of an untrusted boundary (a subprocess MCP extension, a
/// WASM module). This is the kernel-side gate of ADR 0008: every transport
/// validates the backend's response against THIS schema before anything
/// reaches the policy context — the contract belongs to the interface, not
/// to any one transport.
pub fn retrieve_response_schema() -> &'static jsonschema::Validator {
    static SCHEMA: std::sync::OnceLock<jsonschema::Validator> = std::sync::OnceLock::new();
    SCHEMA.get_or_init(|| {
        jsonschema::validator_for(&serde_json::json!({
            "type": "object",
            "properties": {
                "results": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "episode_id": {"type": "string"},
                            "summary": {"type": "string"},
                            "score": {"type": "number", "minimum": 0.0, "maximum": 1.0}
                        },
                        "required": ["episode_id", "summary", "score"]
                    }
                }
            },
            "required": ["results"]
        }))
        .expect("static schema compiles")
    })
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
    /// The backend does not implement an optional operation (e.g. episode
    /// history on a WASM or subprocess backend, ADR 0038).
    #[error("memory backend does not support {0}")]
    Unsupported(&'static str),
}

/// The stable provider protocol: `write(episode)`, `retrieve(task) ->
/// ranked_context`, `upsert(entity|relation)`. Backends are swappable; the
/// kernel never depends on a concrete store.
#[async_trait]
pub trait MemoryProvider: Send + Sync {
    /// Store an episode. Versioning contract (ADR 0038): a write whose `id`
    /// is already stored and whose `version` is HIGHER is a new revision —
    /// it becomes what `retrieve` sees, and the earlier versions are kept as
    /// history (never deleted by a revision). A write of an already-stored
    /// `(id, version)` replaces that version (idempotent re-write). A lower
    /// version never displaces a higher one.
    async fn write(&self, episode: Episode) -> Result<(), MemoryError>;
    /// Rank the CURRENT version of each episode against `task`.
    async fn retrieve(&self, task: &str, limit: usize) -> Result<Vec<RankedContext>, MemoryError>;
    async fn upsert(&self, item: GraphUpsert) -> Result<(), MemoryError>;
    /// Every stored version of every episode of `run_id`, ordered by
    /// `(id, version)` ascending (ADR 0038). Optional: a backend that cannot
    /// answer returns [`MemoryError::Unsupported`], and the kernel then never
    /// writes revisions to it (an unverified episode there stays unverified).
    async fn episode_history(&self, run_id: Uuid) -> Result<Vec<Episode>, MemoryError> {
        let _ = run_id;
        Err(MemoryError::Unsupported("episode history"))
    }
}

/// The current version of each episode in `history` (highest `version` per
/// `id`), in the order the ids first appear. Shared by backends and the
/// kernel so "current" means one thing everywhere (ADR 0038).
pub fn current_versions(history: &[Episode]) -> Vec<Episode> {
    let mut order: Vec<Uuid> = Vec::new();
    let mut latest: HashMap<Uuid, &Episode> = HashMap::new();
    for e in history {
        match latest.get(&e.id) {
            None => {
                order.push(e.id);
                latest.insert(e.id, e);
            }
            Some(prev) if e.version >= prev.version => {
                latest.insert(e.id, e);
            }
            Some(_) => {}
        }
    }
    order.into_iter().map(|id| latest[&id].clone()).collect()
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
    /// The current version of each episode (what retrieval ranks).
    episodes: Vec<Episode>,
    /// Every version ever written, append-only except for an idempotent
    /// re-write of the same `(id, version)` (ADR 0038).
    history: Vec<Episode>,
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

    /// The current version of each stored episode.
    pub async fn episodes(&self) -> Vec<Episode> {
        self.inner.read().await.episodes.clone()
    }
}

/// One-line summary of an episode for prompt context. An unverified episode
/// is marked so the model never reads it as a success pattern (ADR 0038);
/// `Success`/`Failure` summaries are unchanged from before.
pub fn episode_summary(e: &Episode) -> String {
    let outcome = match e.outcome {
        EpisodeOutcome::Unverified => {
            "Unverified (completed, but no contract or human verified it — not a success pattern)"
                .to_string()
        }
        o => format!("{o:?}"),
    };
    format!(
        "task: {} | outcome: {} | steps: {}",
        e.task,
        outcome,
        e.trace.len()
    )
}

#[async_trait]
impl MemoryProvider for InMemoryProvider {
    async fn write(&self, episode: Episode) -> Result<(), MemoryError> {
        let mut store = self.inner.write().await;
        match store
            .history
            .iter_mut()
            .find(|e| e.id == episode.id && e.version == episode.version)
        {
            Some(same) => *same = episode.clone(),
            None => store.history.push(episode.clone()),
        }
        match store.episodes.iter_mut().find(|e| e.id == episode.id) {
            Some(cur) if episode.version >= cur.version => *cur = episode,
            Some(_) => {}
            None => store.episodes.push(episode),
        }
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

    async fn episode_history(&self, run_id: Uuid) -> Result<Vec<Episode>, MemoryError> {
        let store = self.inner.read().await;
        let mut out: Vec<Episode> = store
            .history
            .iter()
            .filter(|e| e.run_id == run_id)
            .cloned()
            .collect();
        out.sort_by_key(|e| (e.id, e.version));
        Ok(out)
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

    // ---- ADR 0038: three outcome states + versioned revisions ----

    #[test]
    fn pre_adr_0038_record_still_reads_and_unverified_round_trips() {
        // A record exactly as every pre-0038 backend stored it.
        let old = r#"{"id":"6f1c2a52-8d0e-4c43-9b3a-0c5d2f6e7a11","run_id":"0b7f8a8e-1d2c-4e3f-8a9b-1c2d3e4f5a6b","task":"t","trace":["finish: ok"],"outcome":"success","provenance":"reflection","version":1}"#;
        let e: Episode = serde_json::from_str(old).unwrap();
        assert_eq!(e.outcome, EpisodeOutcome::Success);
        assert_eq!(e.version, 1);

        let mut u = e.clone();
        u.outcome = EpisodeOutcome::Unverified;
        let json = serde_json::to_value(&u).unwrap();
        assert_eq!(json["outcome"], "unverified");
        let back: Episode = serde_json::from_value(json).unwrap();
        assert_eq!(back.outcome, EpisodeOutcome::Unverified);
    }

    #[test]
    fn unverified_summary_is_marked_and_others_unchanged() {
        let mut e = episode("send the report");
        assert_eq!(
            episode_summary(&e),
            "task: send the report | outcome: Success | steps: 1"
        );
        e.outcome = EpisodeOutcome::Unverified;
        let s = episode_summary(&e);
        assert!(s.contains("Unverified"), "{s}");
        assert!(s.contains("not a success pattern"), "{s}");
    }

    #[tokio::test]
    async fn higher_version_supersedes_for_retrieval_and_history_is_kept() {
        let mem = InMemoryProvider::new();
        let mut v1 = episode("archive the invoices");
        v1.outcome = EpisodeOutcome::Unverified;
        mem.write(v1.clone()).await.unwrap();

        let mut v2 = v1.clone();
        v2.version = 2;
        v2.outcome = EpisodeOutcome::Success;
        v2.provenance = Provenance::Human;
        mem.write(v2.clone()).await.unwrap();

        // A stale lower version never displaces the current one.
        mem.write(v1.clone()).await.unwrap();

        let hits = mem.retrieve("archive invoices", 5).await.unwrap();
        assert_eq!(hits.len(), 1, "one episode, not one per version");
        assert!(hits[0].summary.contains("outcome: Success"));

        let hist = mem.episode_history(v1.run_id).await.unwrap();
        assert_eq!(
            hist.iter()
                .map(|e| (e.version, e.outcome))
                .collect::<Vec<_>>(),
            vec![
                (1, EpisodeOutcome::Unverified),
                (2, EpisodeOutcome::Success)
            ]
        );
        assert_eq!(current_versions(&hist)[0].version, 2);
    }
}
