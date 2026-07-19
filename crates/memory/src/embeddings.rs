//! Optional embedding-based retrieval (Phase 8, item D) behind the same
//! `MemoryProvider::retrieve` seam as BM25 (ADR 0005). BM25 stays the default;
//! this is a **real** backend — it embeds the query + candidate documents via
//! an OpenAI-compatible `/v1/embeddings` endpoint (e.g. a dedicated llama.cpp
//! embedding server) and ranks by cosine similarity. No stub, no fake vectors.
//!
//! Honest boundary: a llama.cpp server in `--embedding` mode is embedding-only
//! and needs a dedicated embedding model, so this points at a **separate**
//! embedding server from the generation model (verified against the llama.cpp
//! server docs; see BUILD-EVIDENCE Phase 8). If the backend errors at runtime,
//! retrieval falls back to BM25 rather than failing — a degraded ranking never
//! breaks a run.

use async_trait::async_trait;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum EmbeddingError {
    #[error("embedding backend http error: {0}")]
    Http(String),
    #[error("embedding backend returned {got} vectors for {want} inputs")]
    CountMismatch { want: usize, got: usize },
}

/// A source of text embeddings. One call embeds a batch of texts, returning
/// one vector per input, in input order.
#[async_trait]
pub trait EmbeddingBackend: Send + Sync {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError>;
    fn describe(&self) -> String;
}

/// The OpenAI-compatible `/v1/embeddings` client (llama.cpp server, or any
/// compatible endpoint). Request `{ "input": [...], "model": "..." }`,
/// response `{ "data": [ { "index": i, "embedding": [...] }, ... ] }`.
pub struct LlamaEmbeddingBackend {
    http: reqwest::Client,
    endpoint: String,
    model: String,
}

impl LlamaEmbeddingBackend {
    /// `base_url` is the server origin (e.g. `http://127.0.0.1:8090`); the
    /// `/v1/embeddings` path is appended.
    pub fn new(base_url: &str, model: impl Into<String>) -> Self {
        let endpoint = format!("{}/v1/embeddings", base_url.trim_end_matches('/'));
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("client builds"),
            endpoint,
            model: model.into(),
        }
    }
}

#[derive(Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingDatum>,
}

#[derive(Deserialize)]
struct EmbeddingDatum {
    #[serde(default)]
    index: usize,
    embedding: Vec<f32>,
}

#[async_trait]
impl EmbeddingBackend for LlamaEmbeddingBackend {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let body = serde_json::json!({ "input": texts, "model": self.model });
        let resp: EmbeddingsResponse = self
            .http
            .post(&self.endpoint)
            .json(&body)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| EmbeddingError::Http(e.to_string()))?
            .json()
            .await
            .map_err(|e| EmbeddingError::Http(e.to_string()))?;

        if resp.data.len() != texts.len() {
            return Err(EmbeddingError::CountMismatch {
                want: texts.len(),
                got: resp.data.len(),
            });
        }
        // Order by the response index so vectors line up with inputs.
        let mut data = resp.data;
        data.sort_by_key(|d| d.index);
        Ok(data.into_iter().map(|d| d.embedding).collect())
    }

    fn describe(&self) -> String {
        format!("embeddings:{} @ {}", self.model, self.endpoint)
    }
}

/// Cosine similarity of two vectors, guarded against zero norms.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// Rank `docs` against `query` by cosine similarity of their embeddings.
/// Returns `(index into docs, score>0)`, best first, at most `limit`. One
/// batched embed call covers the query + all docs.
pub async fn rank_embeddings(
    backend: &dyn EmbeddingBackend,
    query: &str,
    docs: &[String],
    limit: usize,
) -> Result<Vec<(usize, f64)>, EmbeddingError> {
    if docs.is_empty() || query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut inputs = Vec::with_capacity(docs.len() + 1);
    inputs.push(query.to_string());
    inputs.extend(docs.iter().cloned());

    let vectors = backend.embed(&inputs).await?;
    let (query_vec, doc_vecs) = vectors.split_first().expect("query + docs embedded");

    let mut scored: Vec<(usize, f64)> = doc_vecs
        .iter()
        .enumerate()
        .map(|(i, v)| (i, cosine(query_vec, v) as f64))
        .filter(|(_, s)| *s > 0.0)
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(limit);
    Ok(scored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_is_one_for_parallel_and_zero_for_orthogonal() {
        assert!((cosine(&[1.0, 0.0], &[2.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    struct StaticBackend(std::collections::HashMap<String, Vec<f32>>);

    #[async_trait]
    impl EmbeddingBackend for StaticBackend {
        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            Ok(texts
                .iter()
                .map(|t| {
                    self.0
                        .get(t)
                        .cloned()
                        .unwrap_or_else(|| vec![0.0, 0.0, 0.0])
                })
                .collect())
        }
        fn describe(&self) -> String {
            "static-test".into()
        }
    }

    #[tokio::test]
    async fn ranks_by_cosine_closest_first() {
        let mut m = std::collections::HashMap::new();
        m.insert("query".to_string(), vec![1.0, 0.0, 0.0]);
        m.insert("near".to_string(), vec![0.9, 0.1, 0.0]);
        // A small positive x-component so it still scores > 0 (just lower).
        m.insert("far".to_string(), vec![0.2, 0.98, 0.0]);
        let backend = StaticBackend(m);
        let docs = vec!["far".to_string(), "near".to_string()];
        let ranked = rank_embeddings(&backend, "query", &docs, 5).await.unwrap();
        assert_eq!(ranked[0].0, 1, "the cosine-closest doc must rank first");
        assert!(ranked[0].1 > ranked[1].1);
    }
}
