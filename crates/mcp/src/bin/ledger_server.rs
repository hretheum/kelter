//! Stdio entry point for the ledger-only MCP server (durability smoke).

use mcp::server::LedgerServer;
use rmcp::transport::stdio;
use rmcp::ServiceExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let workspace = std::env::args()
        .skip_while(|a| a != "--workspace")
        .nth(1)
        .unwrap_or_else(|| "./work".to_string());
    let service = LedgerServer::new(workspace)?.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
