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

    // Capability registry: manifest-declared extensions, routed by the
    // capability they provide (ADR 0008). Invalid manifests fail loudly.
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

    let mut memory_extensions: Vec<mcp::extensions::RegisteredExtension> = Vec::new();
    for mut ext in extensions {
        tracing::info!(
            "registry: extension '{}' capability={:?} permissions={:?}",
            ext.name,
            ext.capability,
            ext.permissions
        );
        // Permission scoping: the run workspace is handed over only when
        // the manifest asked for it.
        if ext.permissions.workspace_write {
            ext.spec.env.push((
                "KELTER_WORKSPACE".into(),
                workspace_abs.to_string_lossy().into_owned(),
            ));
        }
        if ext.permissions.network {
            tracing::warn!(
                "registry: extension '{}' requests network access — recorded; transport-level \
                 enforcement is a later slice",
                ext.name
            );
        }
        match ext.capability {
            mcp::extensions::Capability::Tools => servers.push(ext.spec),
            mcp::extensions::Capability::Memory => memory_extensions.push(ext),
            // load_manifest refuses agent/model-provider before this point.
            other => anyhow::bail!("registry routed an unimplemented capability: {other:?}"),
        }
    }

    let tools = mcp::McpToolSource::connect(servers)
        .await
        .context("failed to start MCP tool servers")?;
    tracing::info!("mcp tools online: {}", tools.tool_names().join(", "));

    // Storage for runs + journal: durable SQLite by default.
    let store: Arc<dyn RunStore> = match cfg.storage.backend.as_str() {
        "sqlite" => {
            let db = storage::Db::open(&cfg.storage.path)
                .with_context(|| format!("cannot open database '{}'", cfg.storage.path))?;
            tracing::info!("storage: sqlite at {}", cfg.storage.path);
            Arc::new(storage::SqliteRunStore::new(db))
        }
        "memory" => {
            tracing::warn!("storage: in-memory backend — nothing survives a restart");
            Arc::new(kernel_core::InMemoryRunStore::new())
        }
        other => {
            anyhow::bail!("unknown storage backend '{other}' (expected \"sqlite\" or \"memory\")")
        }
    };

    // Memory: always a registered extension over the reserved MCP tools
    // (ADR 0008). An external memory extension overrides the built-in one;
    // the built-in durable memory is itself dogfooded through the same
    // registry seam (spawned `memory-server`, no in-process shortcut).
    if memory_extensions.len() > 1 {
        anyhow::bail!(
            "multiple memory extensions registered ({:?}) — exactly one memory backend is \
             supported",
            memory_extensions
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>()
        );
    }
    let memory: Arc<dyn MemoryProvider> = if let Some(ext) = memory_extensions.pop() {
        tracing::info!("memory: extension '{}' (external backend)", ext.name);
        Arc::new(
            mcp::McpMemoryProvider::connect(ext.spec)
                .await
                .context("failed to start the registered memory extension")?,
        )
    } else if cfg.storage.backend == "sqlite" {
        let exe_dir = std::env::current_exe()?
            .parent()
            .map(|p| p.to_path_buf())
            .context("executable has no parent dir")?;
        let spec = mcp::ServerSpec {
            name: "builtin-memory".into(),
            command: exe_dir.join("memory-server").to_string_lossy().into_owned(),
            args: vec!["--db".into(), cfg.storage.path.clone()],
            cwd: None,
            env: Vec::new(),
        };
        tracing::info!("memory: builtin durable extension (dogfooded via the registry seam)");
        Arc::new(
            mcp::McpMemoryProvider::connect(spec)
                .await
                .context("failed to start the builtin memory extension")?,
        )
    } else {
        tracing::warn!("memory: in-memory provider (test backend)");
        Arc::new(InMemoryProvider::new())
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
