//! The memory-extension protocol, server side: `MemoryProvider` exposed as
//! three reserved MCP tools, plus the optional `memory_episode_history`
//! (ADR 0038). The built-in durable memory ships as exactly this server (dogfooding, ADR 0006/0008): the daemon spawns it through
//! the same registry path as any third-party memory extension — no
//! privileged in-process shortcut.

use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

use memory::{Episode, GraphUpsert, MemoryProvider};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct WriteParams {
    /// The full episode object (provenance-tagged by the kernel).
    pub episode: serde_json::Value,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RetrieveParams {
    pub task: String,
    pub limit: usize,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct UpsertParams {
    /// An entity or relation upsert (tagged with `kind`).
    pub item: serde_json::Value,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct HistoryParams {
    /// The run's id (a UUID).
    pub run_id: String,
}

/// Serves any `MemoryProvider` over the reserved memory tools.
#[derive(Clone)]
pub struct MemoryToolServer {
    provider: Arc<dyn MemoryProvider>,
}

impl MemoryToolServer {
    pub fn new(provider: Arc<dyn MemoryProvider>) -> Self {
        Self { provider }
    }
}

fn ok(value: serde_json::Value) -> CallToolResult {
    CallToolResult::structured(value)
}

fn fail(e: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::structured_error(serde_json::json!({ "error": e.to_string() }))
}

#[tool_router(server_handler)]
impl MemoryToolServer {
    #[tool(description = "Store one episode (reserved memory-capability tool).")]
    async fn memory_write(&self, Parameters(p): Parameters<WriteParams>) -> CallToolResult {
        let episode: Episode = match serde_json::from_value(p.episode) {
            Ok(e) => e,
            Err(e) => return fail(format!("invalid episode: {e}")),
        };
        match self.provider.write(episode).await {
            Ok(()) => ok(serde_json::json!({ "written": true })),
            Err(e) => fail(e),
        }
    }

    #[tool(description = "Rank stored episodes against a task (reserved memory-capability tool).")]
    async fn memory_retrieve(&self, Parameters(p): Parameters<RetrieveParams>) -> CallToolResult {
        match self.provider.retrieve(&p.task, p.limit).await {
            Ok(hits) => ok(serde_json::json!({ "results": hits })),
            Err(e) => fail(e),
        }
    }

    #[tool(description = "Upsert a graph entity or relation (reserved memory-capability tool).")]
    async fn memory_upsert(&self, Parameters(p): Parameters<UpsertParams>) -> CallToolResult {
        let item: GraphUpsert = match serde_json::from_value(p.item) {
            Ok(i) => i,
            Err(e) => return fail(format!("invalid upsert item: {e}")),
        };
        match self.provider.upsert(item).await {
            Ok(()) => ok(serde_json::json!({ "upserted": true })),
            Err(e) => fail(e),
        }
    }

    #[tool(
        description = "Every stored version of a run's episodes, oldest first (optional memory-capability tool, ADR 0038)."
    )]
    async fn memory_episode_history(
        &self,
        Parameters(p): Parameters<HistoryParams>,
    ) -> CallToolResult {
        let run_id: uuid::Uuid = match p.run_id.parse() {
            Ok(id) => id,
            Err(e) => return fail(format!("invalid run_id: {e}")),
        };
        match self.provider.episode_history(run_id).await {
            Ok(episodes) => ok(serde_json::json!({ "episodes": episodes })),
            Err(e) => fail(e),
        }
    }
}
