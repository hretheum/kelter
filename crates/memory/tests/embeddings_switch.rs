//! Item D (Phase 8): the retrieval switch — BM25 (default) ↔ a REAL embedding
//! backend. The embedding path is exercised over actual HTTP against a mock
//! OpenAI-compatible `/v1/embeddings` server (deterministic vectors), so this
//! proves the adapter genuinely embeds + cosine-ranks over the wire — not a
//! stub. Both strategies must rank the intended document first.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::{routing::post, Json, Router};
use memory::{embeddings::LlamaEmbeddingBackend, Retrieval};

/// A mock `/v1/embeddings` server: it maps known texts to fixed vectors so
/// the ranking is deterministic. Any text it doesn't know embeds to zero.
async fn spawn_mock_embeddings() -> SocketAddr {
    let mut table: HashMap<String, Vec<f32>> = HashMap::new();
    // A tiny 3-dim "semantic" space: axis 0 = weather, 1 = finance, 2 = code.
    table.insert("what is the weather forecast".into(), vec![1.0, 0.0, 0.0]);
    table.insert(
        "sunny skies and warm temperatures today".into(),
        vec![0.95, 0.05, 0.0],
    );
    table.insert(
        "quarterly revenue and profit margins".into(),
        vec![0.0, 1.0, 0.0],
    );
    table.insert(
        "refactor the rust borrow checker code".into(),
        vec![0.0, 0.0, 1.0],
    );
    let table = Arc::new(table);

    let app = Router::new().route(
        "/v1/embeddings",
        post({
            let table = table.clone();
            move |Json(body): Json<serde_json::Value>| {
                let table = table.clone();
                async move {
                    let inputs = body["input"].as_array().cloned().unwrap_or_default();
                    let data: Vec<serde_json::Value> = inputs
                        .iter()
                        .enumerate()
                        .map(|(i, v)| {
                            let text = v.as_str().unwrap_or("");
                            let emb = table.get(text).cloned().unwrap_or_else(|| vec![0.0, 0.0, 0.0]);
                            serde_json::json!({ "object": "embedding", "index": i, "embedding": emb })
                        })
                        .collect();
                    Json(serde_json::json!({ "object": "list", "data": data }))
                }
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

fn docs() -> Vec<String> {
    vec![
        "quarterly revenue and profit margins".into(), // 0: finance
        "sunny skies and warm temperatures today".into(), // 1: weather
        "refactor the rust borrow checker code".into(), // 2: code
    ]
}

#[tokio::test]
async fn embeddings_strategy_ranks_semantically_over_real_http() {
    let addr = spawn_mock_embeddings().await;
    let backend = Arc::new(LlamaEmbeddingBackend::new(
        &format!("http://{addr}"),
        "mock-embed",
    ));
    let retrieval = Retrieval::Embeddings(backend);

    // The weather query is lexically disjoint from the weather doc (no shared
    // words), so BM25 would miss it — embeddings must still rank it first.
    let ranked = retrieval
        .rank("what is the weather forecast", &docs(), 3)
        .await;
    assert!(!ranked.is_empty(), "embeddings returned no hits");
    assert_eq!(ranked[0].0, 1, "weather doc must rank first via embeddings");
    assert!(ranked[0].1 > 0.0 && ranked[0].1 <= 1.0);
}

#[tokio::test]
async fn bm25_default_still_ranks_lexically() {
    let retrieval = Retrieval::Bm25;
    assert_eq!(retrieval.describe(), "bm25");
    // Lexical overlap: the query shares words with the finance doc.
    let ranked = retrieval.rank("quarterly profit margins", &docs(), 3).await;
    assert_eq!(ranked[0].0, 0, "finance doc must rank first via BM25");
}

#[tokio::test]
async fn embeddings_backend_error_falls_back_to_bm25() {
    // Point the backend at a dead port: rank() must degrade to BM25, not fail.
    let backend = Arc::new(LlamaEmbeddingBackend::new("http://127.0.0.1:1", "x"));
    let retrieval = Retrieval::Embeddings(backend);
    let ranked = retrieval.rank("quarterly profit margins", &docs(), 3).await;
    // BM25 fallback still finds the lexically-matching finance doc.
    assert_eq!(ranked[0].0, 0);
}
