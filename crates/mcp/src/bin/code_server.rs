//! Stdio entry point for the bundled code-tools MCP server (A1/C4/C5):
//! spawned by the daemon when `[tools.coding]` is configured. Default =
//! separated roles (`popraw_kod`, no content-writing tool);
//! `--merged-roles` exposes `zastosuj_patch` instead — the "before A1"
//! measurement configuration.

use mcp::server::{CodeServer, CodeServerMerged, CodeToolsCore};
use rmcp::transport::stdio;
use rmcp::ServiceExt;

fn arg(name: &str) -> Option<String> {
    std::env::args()
        .skip_while(|a| a != name)
        .nth(1)
        .filter(|v| !v.starts_with("--"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = arg("--root").unwrap_or_else(|| ".".into());
    let executor = arg("--executor-url").map(|base_url| mcp::code_tools::ExecutorConfig {
        base_url,
        model: arg("--executor-model").unwrap_or_else(|| "default".into()),
        max_tokens: arg("--executor-max-tokens")
            .and_then(|v| v.parse().ok())
            .unwrap_or(2048),
    });
    let allowed: Vec<String> = arg("--allow")
        .map(|v| v.split(',').map(str::to_string).collect())
        .unwrap_or_else(|| vec!["cargo".into(), "git".into()]);
    let timeout_secs = arg("--timeout-secs")
        .and_then(|v| v.parse().ok())
        .unwrap_or(180);
    let max_errors = arg("--summary-errors")
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let merged = std::env::args().any(|a| a == "--merged-roles");

    let core = CodeToolsCore::new(root, executor, allowed, timeout_secs, max_errors)?;
    if merged {
        let service = CodeServerMerged::new(core).serve(stdio()).await?;
        service.waiting().await?;
    } else {
        let service = CodeServer::new(core).serve(stdio()).await?;
        service.waiting().await?;
    }
    Ok(())
}
