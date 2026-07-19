//! Stdio entry point for the built-in durable memory extension: the
//! Phase 3 SQLite `MemoryProvider` served over the reserved memory tools.
//! Spawned by the daemon through the SAME registry path as any third-party
//! memory extension (dogfooding).

use std::sync::Arc;

use mcp::memory_server::MemoryToolServer;
use rmcp::transport::stdio;
use rmcp::ServiceExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let db_path = std::env::args()
        .skip_while(|a| a != "--db")
        .nth(1)
        .unwrap_or_else(|| "./kelter.db".to_string());
    let db = storage::Db::open(&db_path)?;

    // Retrieval switch (Phase 8, item D): BM25 by default; when the daemon
    // configures an embedding server, KELTER_EMBEDDINGS_URL is set and this
    // uses a real embedding backend (cosine similarity) instead.
    let provider = match std::env::var("KELTER_EMBEDDINGS_URL") {
        Ok(url) if !url.is_empty() => {
            let model = std::env::var("KELTER_EMBEDDINGS_MODEL")
                .unwrap_or_else(|_| "embedding-model".to_string());
            let backend = Arc::new(memory::embeddings::LlamaEmbeddingBackend::new(&url, model));
            eprintln!("memory: retrieval = embeddings ({url})");
            Arc::new(storage::SqliteMemoryProvider::with_retrieval(
                db,
                memory::Retrieval::Embeddings(backend),
            ))
        }
        _ => {
            eprintln!("memory: retrieval = bm25 (default)");
            Arc::new(storage::SqliteMemoryProvider::new(db))
        }
    };
    let service = MemoryToolServer::new(provider).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
