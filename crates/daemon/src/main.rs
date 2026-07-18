//! Headless daemon: assembles memory + policy + tools + kernel and serves
//! the one stable API contract (HTTP + WS) with the embedded web UI.

mod config;

use std::sync::Arc;

use anyhow::Context;
use rand::Rng;

use config::Config;
use kernel_core::{Kernel, KernelConfig};
use memory::InMemoryProvider;

fn generate_token() -> String {
    let bytes: [u8; 24] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config_path = std::env::args()
        .skip_while(|a| a != "--config")
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());
    let cfg = Config::load(&config_path)?;

    // Policy: the adapter + model choice is one line in the config file.
    let (oracle, latitude) = policy::build_oracle(&cfg.policy)
        .context("failed to build policy adapter from [policy] config")?;
    tracing::info!("policy engine: {}", oracle.describe());

    // Tool substrate: spawn MCP servers (default: the bundled reference
    // tool server, found next to this binary).
    let servers = if cfg.tools.servers.is_empty() {
        vec![mcp::ServerSpec::bundled_tools_server(&cfg.tools.workspace)?]
    } else {
        cfg.tools.servers.clone()
    };
    let tools = mcp::McpToolSource::connect(servers)
        .await
        .context("failed to start MCP tool servers")?;
    tracing::info!("mcp tools online: {}", tools.tool_names().join(", "));

    let kernel = Arc::new(Kernel::new(
        oracle,
        Arc::new(tools),
        Arc::new(InMemoryProvider::new()),
        Arc::new(kernel_core::InMemoryRunStore::new()),
        KernelConfig {
            max_steps: cfg.kernel.max_steps,
            latitude,
            reflection_enabled: cfg.kernel.reflection,
        },
    ));

    let token = cfg.server.token.clone().unwrap_or_else(|| {
        let t = generate_token();
        // Printed once at startup on purpose: pair a client by copying it.
        println!("generated api token: {t}");
        t
    });

    let addr: std::net::SocketAddr = cfg
        .server
        .bind
        .parse()
        .with_context(|| format!("invalid bind address '{}'", cfg.server.bind))?;

    println!("ui:  http://{addr}/");
    println!("api: http://{addr}/api/v1  (Authorization: Bearer <token>)");
    api::serve(kernel, token, addr).await?;
    Ok(())
}
