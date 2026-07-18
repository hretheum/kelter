//! Headless daemon: assembles memory + policy + tools + kernel and serves
//! the one stable API contract (HTTP + WS) with the embedded web UI.

mod config;

use std::sync::Arc;

use anyhow::Context;
use rand::Rng;

use config::Config;
use kernel_core::{Kernel, KernelConfig, RunStore};
use memory::{InMemoryProvider, MemoryProvider};

fn generate_token() -> String {
    let bytes: [u8; 24] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Initialize logging; when `[otel] endpoint` is set, also bridge tracing
/// spans (run/step/model/validate/tool) to an OTLP/HTTP exporter.
fn init_tracing(
    otel: &config::OtelConfig,
) -> anyhow::Result<Option<opentelemetry_sdk::trace::SdkTracerProvider>> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let fmt_layer = tracing_subscriber::fmt::layer();

    match &otel.endpoint {
        Some(endpoint) => {
            use opentelemetry::trace::TracerProvider as _;
            use opentelemetry_otlp::WithExportConfig as _;
            let exporter = opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .with_endpoint(format!("{}/v1/traces", endpoint.trim_end_matches('/')))
                .build()
                .map_err(|e| anyhow::anyhow!("otlp exporter: {e}"))?;
            let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
                .with_batch_exporter(exporter)
                .with_resource(
                    opentelemetry_sdk::Resource::builder()
                        .with_service_name(
                            otel.service_name.clone().unwrap_or_else(|| "kelter".into()),
                        )
                        .build(),
                )
                .build();
            opentelemetry::global::set_tracer_provider(provider.clone());
            let tracer = provider.tracer("kelter");
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt_layer)
                .with(tracing_opentelemetry::layer().with_tracer(tracer))
                .init();
            tracing::info!("otel: exporting traces to {endpoint} (otlp/http)");
            Ok(Some(provider))
        }
        None => {
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt_layer)
                .init();
            Ok(None)
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config_path = std::env::args()
        .skip_while(|a| a != "--config")
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());
    let cfg = Config::load(&config_path)?;
    let otel_provider = init_tracing(&cfg.otel)?;

    // Policy: the adapter + model choice is one line in the config file.
    let (oracle, latitude) = policy::build_oracle(&cfg.policy)
        .context("failed to build policy adapter from [policy] config")?;
    tracing::info!("policy engine: {}", oracle.describe());

    // Tool substrate: spawn MCP servers (default: the bundled reference
    // tool server, found next to this binary).
    let mut servers = cfg.tools.servers.clone();
    if cfg.tools.bundled_tools {
        servers.insert(
            0,
            mcp::ServerSpec::bundled_tools_server(&cfg.tools.workspace)?,
        );
    }

    // Extensions: manifest-declared MCP servers, registered at startup with
    // no recompilation (ADR 0006). Invalid manifests fail loudly.
    let (extensions, manifest_errors) = mcp::extensions::discover(&cfg.tools.extension_dirs);
    if let Some(err) = manifest_errors.first() {
        anyhow::bail!("extension manifest error: {err}");
    }
    let workspace_abs = std::fs::canonicalize(&cfg.tools.workspace)
        .or_else(|_| {
            std::fs::create_dir_all(&cfg.tools.workspace)
                .and_then(|_| std::fs::canonicalize(&cfg.tools.workspace))
        })
        .context("cannot resolve tools workspace directory")?;
    for mut ext in extensions {
        // Runtime contract: extensions see the run workspace via env, so
        // they resolve relative paths the same way the reference tools do.
        ext.env.push((
            "KELTER_WORKSPACE".into(),
            workspace_abs.to_string_lossy().into_owned(),
        ));
        tracing::info!("extension registered: {} ({})", ext.name, ext.command);
        servers.push(ext);
    }

    let tools = mcp::McpToolSource::connect(servers)
        .await
        .context("failed to start MCP tool servers")?;
    tracing::info!("mcp tools online: {}", tools.tool_names().join(", "));

    // Storage: durable SQLite by default; runs, journal and memory share
    // one database file and survive restarts.
    let (store, memory): (Arc<dyn RunStore>, Arc<dyn MemoryProvider>) =
        match cfg.storage.backend.as_str() {
            "sqlite" => {
                let db = storage::Db::open(&cfg.storage.path)
                    .with_context(|| format!("cannot open database '{}'", cfg.storage.path))?;
                tracing::info!("storage: sqlite at {}", cfg.storage.path);
                (
                    Arc::new(storage::SqliteRunStore::new(db.clone())),
                    Arc::new(storage::SqliteMemoryProvider::new(db)),
                )
            }
            "memory" => {
                tracing::warn!("storage: in-memory backend — nothing survives a restart");
                (
                    Arc::new(kernel_core::InMemoryRunStore::new()),
                    Arc::new(InMemoryProvider::new()),
                )
            }
            other => anyhow::bail!(
                "unknown storage backend '{other}' (expected \"sqlite\" or \"memory\")"
            ),
        };

    let kernel = Arc::new(Kernel::new(
        oracle,
        Arc::new(tools),
        memory,
        store.clone(),
        KernelConfig {
            max_steps: cfg.kernel.max_steps,
            latitude,
            reflection_enabled: cfg.kernel.reflection,
        },
    ));

    // Durable execution: any run interrupted by a previous crash/restart is
    // resumed from its journal before we accept new work.
    match store.unfinished_runs().await {
        Ok(ids) if !ids.is_empty() => {
            tracing::info!("resuming {} unfinished run(s) from the journal", ids.len());
            for run_id in ids {
                let kernel = kernel.clone();
                tokio::spawn(async move {
                    match kernel.execute_run(run_id).await {
                        Ok(run) => tracing::info!(%run_id, status = ?run.status, "resume finished"),
                        Err(e) => tracing::error!(%run_id, "resume failed: {e}"),
                    }
                });
            }
        }
        Ok(_) => {}
        Err(e) => tracing::error!("cannot scan for unfinished runs: {e}"),
    }

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
    let serve = api::serve(kernel, token, addr);
    tokio::select! {
        res = serve => res?,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
        }
    }
    // Flush buffered spans — the batch exporter does not auto-flush on exit.
    if let Some(provider) = otel_provider {
        let _ = provider.shutdown();
    }
    Ok(())
}
