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
    let provider = Arc::new(storage::SqliteMemoryProvider::new(db));
    let service = MemoryToolServer::new(provider).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
