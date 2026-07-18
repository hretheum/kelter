//! MCP client: spawns configured tool servers (stdio child processes),
//! converts their tool schemas into the kernel's action space, and executes
//! validated actions as MCP tool calls. This crate is the only place where
//! the kernel's `ActionSource` seam meets a real protocol.

use std::collections::HashMap;

use async_trait::async_trait;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use kernel_core::{
    ActionSchema, ActionSource, ActionSpec, ExecutionError, Latitude, ValidatedAction,
};

/// One MCP server to spawn over stdio.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSpec {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory for the spawned server (extensions run in their
    /// own directory).
    #[serde(default)]
    pub cwd: Option<String>,
    /// Extra environment for the spawned server. The daemon passes
    /// `KELTER_WORKSPACE` here so extensions resolve workspace-relative
    /// paths the same way the reference tools do.
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

impl ServerSpec {
    /// The bundled reference tool server, expected next to the current
    /// executable (both binaries are built from this workspace).
    pub fn bundled_tools_server(workspace: &str) -> std::io::Result<Self> {
        let exe = std::env::current_exe()?;
        let dir = exe.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "executable has no parent dir")
        })?;
        let candidate = dir.join("tools-server");
        Ok(Self {
            name: "reference-tools".into(),
            command: candidate.to_string_lossy().into_owned(),
            args: vec!["--workspace".into(), workspace.to_string()],
            cwd: None,
            env: Vec::new(),
        })
    }
}

type Client = RunningService<RoleClient, ()>;

/// The kernel's tool substrate: all tools from all configured MCP servers,
/// flattened into one action space.
pub struct McpToolSource {
    clients: Vec<Client>,
    /// tool name → (client index, description, parameters schema)
    tools: HashMap<String, ToolEntry>,
}

struct ToolEntry {
    client_index: usize,
    description: String,
    parameters: serde_json::Value,
}

impl McpToolSource {
    /// Serve the bundled reference tool server *in-process* over an
    /// in-memory duplex stream — still the full MCP protocol, no child
    /// process. Used by the standalone desktop topology.
    pub async fn connect_in_process(workspace: &str) -> Result<Self, ExecutionError> {
        let server = crate::server::ToolServer::new(workspace)
            .map_err(|e| ExecutionError::Substrate(format!("workspace init failed: {e}")))?;

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server_io);
        tokio::spawn(async move {
            match server.serve((server_read, server_write)).await {
                Ok(running) => {
                    let _ = running.waiting().await;
                }
                Err(e) => tracing::error!("in-process tool server failed to start: {e}"),
            }
        });

        let (client_read, client_write) = tokio::io::split(client_io);
        let client = ().serve((client_read, client_write)).await.map_err(|e| {
            ExecutionError::Substrate(format!("in-process MCP handshake failed: {e}"))
        })?;
        Self::from_clients(vec![("in-process-tools".to_string(), client)]).await
    }

    /// Spawn each configured server as a stdio child process and register
    /// its tools.
    pub async fn connect(specs: Vec<ServerSpec>) -> Result<Self, ExecutionError> {
        let mut named_clients = Vec::new();
        for spec in specs {
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
                ExecutionError::Substrate(format!(
                    "cannot spawn MCP server '{}' ({}): {e}",
                    spec.name, spec.command
                ))
            })?;

            let client = ().serve(transport).await.map_err(|e| {
                ExecutionError::Substrate(format!("MCP handshake with '{}' failed: {e}", spec.name))
            })?;
            named_clients.push((spec.name, client));
        }
        Self::from_clients(named_clients).await
    }

    /// Register all tools exposed by already-connected clients.
    async fn from_clients(named: Vec<(String, Client)>) -> Result<Self, ExecutionError> {
        let mut clients = Vec::new();
        let mut tools = HashMap::new();

        for (name, client) in named {
            let listed = client.list_all_tools().await.map_err(|e| {
                ExecutionError::Substrate(format!("list_tools on '{name}' failed: {e}"))
            })?;

            let client_index = clients.len();
            for tool in listed {
                let name = tool.name.to_string();
                if tools.contains_key(&name) {
                    tracing::warn!("duplicate MCP tool '{name}' — keeping the first registration");
                    continue;
                }
                let parameters = serde_json::to_value(tool.input_schema.as_ref())
                    .unwrap_or_else(|_| serde_json::json!({"type": "object"}));
                tools.insert(
                    name,
                    ToolEntry {
                        client_index,
                        description: tool.description.as_deref().unwrap_or_default().to_string(),
                        parameters,
                    },
                );
            }
            clients.push(client);
        }

        if tools.is_empty() {
            return Err(ExecutionError::Substrate(
                "no tools registered by any MCP server".into(),
            ));
        }
        Ok(Self { clients, tools })
    }

    pub fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tools.keys().cloned().collect();
        names.sort();
        names
    }
}

/// Convert a CallToolResult into the kernel's step-result JSON.
fn result_to_json(result: CallToolResult) -> Result<serde_json::Value, ExecutionError> {
    let is_error = result.is_error.unwrap_or(false);
    let value = if let Some(structured) = result.structured_content {
        structured
    } else {
        // Fold text content blocks; serialize via serde to stay independent
        // of the ContentBlock enum's exact variant shapes.
        let texts: Vec<String> = result
            .content
            .iter()
            .filter_map(|block| {
                serde_json::to_value(block)
                    .ok()
                    .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(String::from))
            })
            .collect();
        let joined = texts.join("\n");
        // Many servers (e.g. FastMCP returning a plain dict) send JSON as
        // text content — surface it structured when it parses.
        match serde_json::from_str::<serde_json::Value>(&joined) {
            Ok(parsed) if parsed.is_object() || parsed.is_array() => parsed,
            _ => serde_json::json!({ "text": joined }),
        }
    };
    if is_error {
        Err(ExecutionError::Tool(value.to_string()))
    } else {
        Ok(value)
    }
}

#[async_trait]
impl ActionSource for McpToolSource {
    async fn action_schema(&self, latitude: Latitude) -> Result<ActionSchema, ExecutionError> {
        let mut actions: Vec<ActionSpec> = self
            .tools
            .iter()
            .map(|(name, entry)| {
                let mut parameters = entry.parameters.clone();
                // Constrained latitude tightens the space: unknown argument
                // keys become schema violations the kernel will reject.
                if latitude == Latitude::Constrained {
                    if let Some(obj) = parameters.as_object_mut() {
                        obj.entry("additionalProperties")
                            .or_insert(serde_json::Value::Bool(false));
                    }
                }
                ActionSpec {
                    name: name.clone(),
                    description: entry.description.clone(),
                    parameters,
                }
            })
            .collect();
        actions.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(ActionSchema { actions })
    }

    async fn execute(&self, action: &ValidatedAction) -> Result<serde_json::Value, ExecutionError> {
        let entry = self.tools.get(action.action()).ok_or_else(|| {
            ExecutionError::Tool(format!("tool '{}' is not registered", action.action()))
        })?;
        let client = &self.clients[entry.client_index];

        let arguments = action.arguments().as_object().cloned().unwrap_or_default();

        let mut params = CallToolRequestParams::new(action.action().to_string());
        params.arguments = Some(arguments);

        let result = client
            .call_tool(params)
            .await
            .map_err(|e| ExecutionError::Tool(format!("MCP call failed: {e}")))?;
        result_to_json(result)
    }
}
