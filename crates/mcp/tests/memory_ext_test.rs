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
    /// Hands back ANOTHER run's episode, as if it were the asked run's.
    async fn episode_history(&self, _run_id: Uuid) -> Result<Vec<Episode>, MemoryError> {
        Ok(vec![episode("someone else's run")])
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

    // ADR 0040: a history naming another run is refused, so an extension
    // cannot steer which episode the kernel revises.
    let err = provider.episode_history(Uuid::new_v4()).await.unwrap_err();
    assert!(err.to_string().contains("for run"), "{err}");
}

/// ADR 0040 through the subprocess seam (the daemon's default SQLite memory
/// path): the built-in memory extension stores an `unverified` episode but
/// never retrieves it, a higher `success` version is retrieved, and
/// `memory_episode_history` returns every version.
#[tokio::test]
async fn builtin_memory_carries_unverified_and_versions_over_mcp() {
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
    .unwrap();
    let mut e = episode("reconcile the ledger");
    e.outcome = EpisodeOutcome::Unverified;
    provider.write(e.clone()).await.unwrap();
    assert!(provider
        .retrieve("reconcile ledger", 3)
        .await
        .unwrap()
        .is_empty());

    let mut v2 = e.clone();
    v2.version = 2;
    v2.outcome = EpisodeOutcome::Success;
    v2.provenance = Provenance::Human;
    provider.write(v2).await.unwrap();
    let hits = provider.retrieve("reconcile ledger", 3).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].summary.contains("outcome: Success"));
    let hist = provider.episode_history(e.run_id).await.unwrap();
    assert_eq!(
        hist.iter()
            .map(|e| (e.version, e.outcome))
            .collect::<Vec<_>>(),
        vec![
            (1, EpisodeOutcome::Unverified),
            (2, EpisodeOutcome::Success)
        ]
    );
    assert!(provider
        .episode_history(Uuid::new_v4())
        .await
        .unwrap()
        .is_empty());
}

/// ADR 0040: the memory tools — `memory_episode_history` included — are the
/// kernel's private protocol with its memory backend, never model actions.
/// Even the memory-server misconfigured as a TOOL server (next to a real one)
/// contributes nothing to the action space, so the model can neither see
/// nor call them.
#[tokio::test]
async fn memory_tools_never_enter_the_action_space() {
    use kernel_core::{ActionSource, Latitude};
    use mcp::McpToolSource;

    let dir = std::env::temp_dir().join(format!("kelter-memtools-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = McpToolSource::connect(vec![
        ServerSpec {
            name: "reference-tools".into(),
            command: env!("CARGO_BIN_EXE_tools-server").into(),
            args: vec!["--workspace".into(), dir.to_string_lossy().into_owned()],
            cwd: None,
            env: Vec::new(),
        },
        ServerSpec {
            name: "memory-as-tools".into(),
            command: env!("CARGO_BIN_EXE_memory-server").into(),
            args: vec![
                "--db".into(),
                dir.join("mem.db").to_string_lossy().into_owned(),
            ],
            cwd: None,
            env: Vec::new(),
        },
    ])
    .await
    .unwrap();
    let names: Vec<String> = source
        .action_schema(Latitude::Standard)
        .await
        .unwrap()
        .actions
        .into_iter()
        .map(|a| a.name)
        .collect();
    assert!(!names.is_empty(), "the real tool server still registers");
    for reserved in [
        "memory_write",
        "memory_retrieve",
        "memory_upsert",
        "memory_episode_history",
    ] {
        assert!(
            !names.iter().any(|n| n == reserved),
            "{reserved} leaked: {names:?}"
        );
        assert!(mcp::client::is_reserved_memory_tool(reserved));
    }
    assert_eq!(names, source.tool_names());
}

/// ADR 0040 end to end on the daemon's default memory path — the kernel over
/// the built-in memory-server (SQLite) as a real child process: the model's
/// context never holds the unverified episode, and holds it once confirmed.
#[tokio::test]
async fn sqlite_memory_server_keeps_unverified_out_of_model_context() {
    let dir = std::env::temp_dir().join(format!("kelter-memext-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let provider = McpMemoryProvider::connect(ServerSpec {
        name: "builtin-memory".into(),
        command: env!("CARGO_BIN_EXE_memory-server").into(),
        args: vec![
            "--db".into(),
            dir.join("mem.db").to_string_lossy().into_owned(),
        ],
        cwd: None,
        env: Vec::new(),
    })
    .await
    .unwrap();
    let p = kernel_core::testkit::episode_context_probe(
        Arc::new(provider),
        "probe sqlite reconcile ledger",
    )
    .await;
    assert!(
        p.before_confirmation
            .iter()
            .all(|m| !m.contains("probe sqlite")),
        "{:?}",
        p.before_confirmation
    );
    assert_eq!(
        p.after_confirmation,
        vec!["task: probe sqlite reconcile ledger | outcome: Success | steps: 1".to_string()]
    );
}

/// ADR 0040, end to end through the kernel (in-memory backend): the model's
/// context never holds the unverified episode, and holds it once confirmed.
/// (Lives here rather than in kernel-core, which cannot enable its own
/// `testkit` feature without a self dev-dependency.)
#[tokio::test]
async fn unverified_episode_reaches_model_context_only_after_confirmation() {
    let p = kernel_core::testkit::episode_context_probe(
        Arc::new(memory::InMemoryProvider::new()),
        "probe inmem reconcile ledger",
    )
    .await;
    assert!(
        p.before_confirmation
            .iter()
            .all(|m| !m.contains("probe inmem")),
        "{:?}",
        p.before_confirmation
    );
    assert_eq!(
        p.after_confirmation,
        vec!["task: probe inmem reconcile ledger | outcome: Success | steps: 1".to_string()]
    );
}
