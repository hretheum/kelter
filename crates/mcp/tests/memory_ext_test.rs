//! Memory-as-extension over the real MCP protocol:
//! - the built-in durable memory (memory-server bin, SQLite) round-trips
//!   write → retrieve → upsert through a spawned child process — the same
//!   dogfooded path the daemon uses;
//! - governance: a backend returning interface-violating responses is
//!   rejected by the kernel-side schema gate, never trusted.

use std::sync::Arc;

use async_trait::async_trait;
use rmcp::ServiceExt;
use uuid::Uuid;

use mcp::memory_server::MemoryToolServer;
use mcp::{McpMemoryProvider, ServerSpec};
use memory::{
    Episode, EpisodeOutcome, GraphUpsert, MemoryError, MemoryProvider, Provenance, RankedContext,
};

fn episode(task: &str) -> Episode {
    Episode {
        id: Uuid::new_v4(),
        run_id: Uuid::new_v4(),
        task: task.into(),
        trace: vec!["step".into()],
        outcome: EpisodeOutcome::Success,
        provenance: Provenance::Reflection,
        version: 1,
    }
}

#[tokio::test]
async fn builtin_memory_roundtrips_over_real_mcp_child_process() {
    let dir = std::env::temp_dir().join(format!("kelter-memext-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("mem.db");

    let provider = McpMemoryProvider::connect(ServerSpec {
        name: "builtin-memory".into(),
        command: env!("CARGO_BIN_EXE_memory-server").into(),
        args: vec!["--db".into(), db.to_string_lossy().into_owned()],
        cwd: None,
        env: Vec::new(),
    })
    .await
    .expect("builtin memory extension should start");

    provider
        .write(episode("extract links from a webpage"))
        .await
        .unwrap();
    provider
        .write(episode("append an entry to the audit ledger"))
        .await
        .unwrap();

    let hits = provider.retrieve("audit ledger entry", 3).await.unwrap();
    assert!(!hits.is_empty());
    assert!(hits[0].summary.contains("audit ledger"));
    assert!(hits[0].score > 0.0 && hits[0].score <= 1.0);

    provider
        .upsert(GraphUpsert::Entity {
            name: "ledger".into(),
            attributes: serde_json::json!({"kind": "file"}),
        })
        .await
        .unwrap();

    // Durability of the extension-backed memory: a SECOND spawn against the
    // same db still sees the episodes (restart survival through the seam).
    let provider2 = McpMemoryProvider::connect(ServerSpec {
        name: "builtin-memory".into(),
        command: env!("CARGO_BIN_EXE_memory-server").into(),
        args: vec!["--db".into(), db.to_string_lossy().into_owned()],
        cwd: None,
        env: Vec::new(),
    })
    .await
    .unwrap();
    let hits = provider2
        .retrieve("extract webpage links", 3)
        .await
        .unwrap();
    assert!(hits[0].summary.contains("extract links"));
}

/// A hostile/broken backend: correct tool names, interface-violating
/// retrieve payload (score way outside [0,1]).
#[derive(Default)]
struct RogueProvider;

#[async_trait]
impl MemoryProvider for RogueProvider {
    async fn write(&self, _e: Episode) -> Result<(), MemoryError> {
        Ok(())
    }
    async fn retrieve(&self, _t: &str, _l: usize) -> Result<Vec<RankedContext>, MemoryError> {
        Ok(vec![RankedContext {
            episode_id: Uuid::new_v4(),
            summary: "trust me".into(),
            score: 42.0, // violates the interface schema (score ≤ 1)
        }])
    }
    async fn upsert(&self, _i: GraphUpsert) -> Result<(), MemoryError> {
        Ok(())
    }
}

#[tokio::test]
async fn interface_violating_backend_is_rejected_by_the_schema_gate() {
    // Serve the rogue backend over an in-process duplex (still real MCP).
    let server = MemoryToolServer::new(Arc::new(RogueProvider));
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (sr, sw) = tokio::io::split(server_io);
    tokio::spawn(async move {
        if let Ok(running) = server.serve((sr, sw)).await {
            let _ = running.waiting().await;
        }
    });
    let (cr, cw) = tokio::io::split(client_io);
    let client = ().serve((cr, cw)).await.unwrap();

    let provider = McpMemoryProvider::from_client(client, "rogue".into())
        .await
        .unwrap();

    let err = provider.retrieve("anything", 3).await.unwrap_err();
    assert!(
        err.to_string().contains("interface-violating"),
        "schema gate must reject the malformed response, got: {err}"
    );
}
