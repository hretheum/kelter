//! The memory-extension protocol, client side: a `MemoryProvider` backed by
//! a registered memory-capability MCP server.
//!
//! Untrusted by construction: every response from the extension is
//! validated against the memory interface schema before the kernel sees
//! it — a backend returning a malformed shape is an error, never silently
//! trusted. Writes carry kernel-constructed, provenance-tagged episodes;
//! the extension cannot alter what gets attributed, only store it.

use async_trait::async_trait;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use tokio::process::Command;

use crate::client::ServerSpec;
use memory::{Episode, GraphUpsert, MemoryError, MemoryProvider, RankedContext};

pub const MEMORY_TOOLS: [&str; 3] = ["memory_write", "memory_retrieve", "memory_upsert"];

/// Optional fourth memory tool (ADR 0038): every stored version of a run's
/// episodes. An extension that does not serve it is still a valid memory
/// backend — it just reports `Unsupported`, and the kernel writes no episode
/// revisions to it.
pub const MEMORY_HISTORY_TOOL: &str = "memory_episode_history";

/// The kernel-side response gate — the canonical contract now lives in the
/// interface crate (`memory::retrieve_response_schema`), shared with the
/// WASM memory transport (ADR 0024) so both boundaries validate identically.
fn retrieve_schema() -> &'static jsonschema::Validator {
    memory::retrieve_response_schema()
}

pub struct McpMemoryProvider {
    client: RunningService<RoleClient, ()>,
    name: String,
    /// Whether the extension serves [`MEMORY_HISTORY_TOOL`].
    has_history: bool,
}

impl McpMemoryProvider {
    /// Spawn a registered memory extension and verify it actually serves
    /// the reserved memory tools.
    pub async fn connect(spec: ServerSpec) -> Result<Self, MemoryError> {
        let transport = TokioChildProcess::new(Command::new(&spec.command).configure(|cmd| {
            cmd.args(&spec.args);
            if let Some(cwd) = &spec.cwd {
                cmd.current_dir(cwd);
            }
            for (k, v) in &spec.env {
                cmd.env(k, v);
            }
        }))
        .map_err(|e| {
            MemoryError::Backend(format!(
                "cannot spawn memory extension '{}' ({}): {e}",
                spec.name, spec.command
            ))
        })?;
        let client = ().serve(transport).await.map_err(|e| {
            MemoryError::Backend(format!(
                "memory extension '{}' handshake failed: {e}",
                spec.name
            ))
        })?;
        Self::from_client(client, spec.name).await
    }

    /// Wrap an already-connected client (tests use an in-process duplex).
    pub async fn from_client(
        client: RunningService<RoleClient, ()>,
        name: String,
    ) -> Result<Self, MemoryError> {
        let tools = client
            .list_all_tools()
            .await
            .map_err(|e| MemoryError::Backend(format!("list_tools failed: {e}")))?;
        let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
        for required in MEMORY_TOOLS {
            if !names.iter().any(|n| n == required) {
                return Err(MemoryError::Backend(format!(
                    "memory extension '{name}' does not serve required tool '{required}' \
                     (found: {names:?})"
                )));
            }
        }
        let has_history = names.iter().any(|n| n == MEMORY_HISTORY_TOOL);
        Ok(Self {
            client,
            name,
            has_history,
        })
    }

    async fn call(
        &self,
        tool: &'static str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, MemoryError> {
        let mut params = CallToolRequestParams::new(tool);
        params.arguments = args.as_object().cloned();
        let result = self
            .client
            .call_tool(params)
            .await
            .map_err(|e| MemoryError::Backend(format!("{tool} failed: {e}")))?;
        if result.is_error.unwrap_or(false) {
            return Err(MemoryError::Backend(format!(
                "{tool} returned an error: {:?}",
                result.structured_content
            )));
        }
        result
            .structured_content
            .or_else(|| {
                // Text-content fallback (e.g. FastMCP): parse JSON text.
                let texts: Vec<String> = result
                    .content
                    .iter()
                    .filter_map(|b| {
                        serde_json::to_value(b)
                            .ok()
                            .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(String::from))
                    })
                    .collect();
                serde_json::from_str(&texts.join("\n")).ok()
            })
            .ok_or_else(|| MemoryError::Backend(format!("{tool} returned no structured content")))
    }
}

#[async_trait]
impl MemoryProvider for McpMemoryProvider {
    async fn write(&self, episode: Episode) -> Result<(), MemoryError> {
        let episode =
            serde_json::to_value(&episode).map_err(|e| MemoryError::Backend(e.to_string()))?;
        self.call("memory_write", serde_json::json!({ "episode": episode }))
            .await?;
        Ok(())
    }

    async fn retrieve(&self, task: &str, limit: usize) -> Result<Vec<RankedContext>, MemoryError> {
        let response = self
            .call(
                "memory_retrieve",
                serde_json::json!({ "task": task, "limit": limit }),
            )
            .await?;

        // Governance: the kernel-side gate. A malformed response from the
        // extension is rejected, not passed into the policy context.
        let violations: Vec<String> = retrieve_schema()
            .iter_errors(&response)
            .map(|e| e.to_string())
            .collect();
        if !violations.is_empty() {
            return Err(MemoryError::Backend(format!(
                "memory extension '{}' returned an interface-violating retrieve response: {}",
                self.name,
                violations.join("; ")
            )));
        }

        let results = response["results"].clone();
        serde_json::from_value(results).map_err(|e| {
            MemoryError::Backend(format!("retrieve results failed to deserialize: {e}"))
        })
    }

    async fn upsert(&self, item: GraphUpsert) -> Result<(), MemoryError> {
        let item = serde_json::to_value(&item).map_err(|e| MemoryError::Backend(e.to_string()))?;
        self.call("memory_upsert", serde_json::json!({ "item": item }))
            .await?;
        Ok(())
    }

    async fn episode_history(&self, run_id: uuid::Uuid) -> Result<Vec<Episode>, MemoryError> {
        if !self.has_history {
            return Err(MemoryError::Unsupported("episode history"));
        }
        let response = self
            .call(MEMORY_HISTORY_TOOL, serde_json::json!({ "run_id": run_id }))
            .await?;
        // The same untrusted-boundary stance as `retrieve`: the shape must
        // parse as episodes, and every one must belong to the asked run — an
        // extension cannot hand the kernel another run's episode to revise.
        let episodes: Vec<Episode> =
            serde_json::from_value(response["episodes"].clone()).map_err(|e| {
                MemoryError::Backend(format!(
                    "memory extension '{}' returned an interface-violating history response: {e}",
                    self.name
                ))
            })?;
        if let Some(stray) = episodes.iter().find(|e| e.run_id != run_id) {
            return Err(MemoryError::Backend(format!(
                "memory extension '{}' returned episode {} of run {} for run {run_id}",
                self.name, stray.id, stray.run_id
            )));
        }
        Ok(episodes)
    }
}
