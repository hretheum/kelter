//! The bundled reference MCP tool server (read → extract → report), usable
//! over stdio (child process) or an in-process duplex stream. Either way it
//! is a normal MCP server — the kernel client speaks the same protocol.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

use crate::tools::{self, Workspace};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReadSourceParams {
    /// URL (http/https) or local file path to read.
    pub input: String,
    /// Workspace-relative file name to store the raw content under.
    pub save_as: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ExtractParams {
    /// Workspace-relative file previously stored by read_source.
    pub source: String,
    /// Workspace-relative file name for the extracted JSON structure.
    pub save_as: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReportParams {
    /// Workspace-relative extraction JSON produced by extract_structure.
    pub source: String,
    /// Workspace-relative output file for the final report.
    pub output: String,
    /// Report format: "markdown" or "json".
    pub format: String,
}

#[derive(Clone)]
pub struct ToolServer {
    ws: Workspace,
}

impl ToolServer {
    pub fn new(workspace: impl Into<std::path::PathBuf>) -> std::io::Result<Self> {
        Ok(Self {
            ws: Workspace::new(workspace)?,
        })
    }
}

fn ok(value: serde_json::Value) -> CallToolResult {
    CallToolResult::structured(value)
}

fn fail(e: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::structured_error(serde_json::json!({ "error": e.to_string() }))
}

#[tool_router(server_handler)]
impl ToolServer {
    #[tool(
        description = "Read a source document (URL or local file path) and store its raw content in the workspace under save_as."
    )]
    async fn read_source(&self, Parameters(p): Parameters<ReadSourceParams>) -> CallToolResult {
        match tools::read_source(&self.ws, &p.input, &p.save_as).await {
            Ok(v) => ok(v),
            Err(e) => fail(e),
        }
    }

    #[tool(
        description = "Extract structured information (title, headings, links, word count) from a stored source file and save it as JSON under save_as."
    )]
    async fn extract_structure(&self, Parameters(p): Parameters<ExtractParams>) -> CallToolResult {
        match tools::extract_structure(&self.ws, &p.source, &p.save_as).await {
            Ok(v) => ok(v),
            Err(e) => fail(e),
        }
    }

    #[tool(
        description = "Render the extracted structure JSON into a final report file (markdown or json) at output."
    )]
    async fn write_report(&self, Parameters(p): Parameters<ReportParams>) -> CallToolResult {
        match tools::write_report(&self.ws, &p.source, &p.output, &p.format).await {
            Ok(v) => ok(v),
            Err(e) => fail(e),
        }
    }
}
