//! Headless daemon: assembles memory + policy + tools + kernel and serves
//! the one stable API contract (HTTP + WS) with the embedded web UI.

mod config;
mod model_config;

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

/// The three OpenTelemetry SDK providers, held for shutdown/flush. All are
/// `None` unless `[otel] endpoint` is configured (observability is off by
/// default — traces, metrics AND logs).
#[derive(Default)]
struct OtelGuard {
    tracer: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
    meter: Option<opentelemetry_sdk::metrics::SdkMeterProvider>,
    logger: Option<opentelemetry_sdk::logs::SdkLoggerProvider>,
}

impl OtelGuard {
    fn shutdown(self) {
        // Flush + shut down on a worker thread with a hard timeout: an OTLP
        // exporter whose collector is gone can otherwise block exit
        // indefinitely (Ctrl-C would hang). Best-effort telemetry must never
        // wedge the process.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Some(m) = self.meter {
                let _ = m.force_flush();
                let _ = m.shutdown();
            }
            if let Some(l) = self.logger {
                let _ = l.shutdown();
            }
            if let Some(t) = self.tracer {
                let _ = t.shutdown();
            }
            let _ = tx.send(());
        });
        if rx.recv_timeout(std::time::Duration::from_secs(3)).is_err() {
            tracing::warn!("otel: shutdown flush timed out (3s) — exiting anyway");
        }
    }
}

/// Kernel metrics instruments (Phase 8, item C). Recorded by a background task
/// that translates the kernel's run-event stream into OTLP metrics.
struct KernelMetrics {
    runs_started: opentelemetry::metrics::Counter<u64>,
    runs_finished: opentelemetry::metrics::Counter<u64>,
    steps: opentelemetry::metrics::Counter<u64>,
    tool_calls: opentelemetry::metrics::Counter<u64>,
    validation_rejections: opentelemetry::metrics::Counter<u64>,
    run_latency_ms: opentelemetry::metrics::Histogram<f64>,
}

impl KernelMetrics {
    fn new(meter: &opentelemetry::metrics::Meter) -> Self {
        Self {
            runs_started: meter
                .u64_counter("kelter.runs.started")
                .with_description("Runs created")
                .build(),
            runs_finished: meter
                .u64_counter("kelter.runs.finished")
                .with_description("Runs reaching a terminal state (attr: outcome)")
                .build(),
            steps: meter
                .u64_counter("kelter.steps.executed")
                .with_description("Executed decision/action steps")
                .build(),
            tool_calls: meter
                .u64_counter("kelter.tool_calls")
                .with_description("Tool/action executions (attr: ok)")
                .build(),
            validation_rejections: meter
                .u64_counter("kelter.validation.rejections")
                .with_description("Kernel validation rejections of proposed actions")
                .build(),
            run_latency_ms: meter
                .f64_histogram("kelter.run.latency")
                .with_unit("ms")
                .with_description("Wall-clock latency of a run from creation to terminal")
                .build(),
        }
    }
}

/// Translate the kernel's run-event broadcast into OTLP metrics until the
/// channel closes. Latency is measured from the `Created` event to the
/// terminal event per run.
async fn run_metrics_recorder(
    mut rx: tokio::sync::broadcast::Receiver<kernel_core::RunEventEnvelope>,
    m: KernelMetrics,
) {
    use kernel_core::{RunEvent, FINISH_ACTION};
    use opentelemetry::KeyValue;
    use std::collections::HashMap;
    use std::time::Instant;

    let mut starts: HashMap<uuid::Uuid, Instant> = HashMap::new();
    loop {
        match rx.recv().await {
            Ok(env) => match &env.event {
                RunEvent::Created { .. } => {
                    m.runs_started.add(1, &[]);
                    starts.insert(env.run_id, Instant::now());
                }
                RunEvent::Executed { action, ok, .. } => {
                    m.steps.add(1, &[]);
                    if action != FINISH_ACTION {
                        m.tool_calls.add(1, &[KeyValue::new("ok", *ok)]);
                    }
                }
                RunEvent::Rejected { .. } => m.validation_rejections.add(1, &[]),
                RunEvent::Completed { .. } | RunEvent::Failed { .. } => {
                    let outcome = if matches!(env.event, RunEvent::Completed { .. }) {
                        "completed"
                    } else {
                        "failed"
                    };
                    m.runs_finished.add(1, &[KeyValue::new("outcome", outcome)]);
                    if let Some(t) = starts.remove(&env.run_id) {
                        m.run_latency_ms.record(t.elapsed().as_millis() as f64, &[]);
                    }
                }
                _ => {}
            },
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Initialize logging; when `[otel] endpoint` is set, also export all three
/// OpenTelemetry pillars over OTLP/HTTP: **traces** (run/step/model/validate/
/// tool spans), **metrics** (run/step/tool/rejection counters + run-latency
/// histogram), and **logs** (tracing events bridged to OTel logs). Off by
/// default. Returns the SDK providers (for shutdown) and, when enabled, the
/// kernel metrics instruments.
fn init_observability(
    otel: &config::OtelConfig,
) -> anyhow::Result<(OtelGuard, Option<KernelMetrics>)> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let fmt_layer = tracing_subscriber::fmt::layer();

    let Some(endpoint) = otel.endpoint.as_deref() else {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .init();
        return Ok((OtelGuard::default(), None));
    };

    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::WithExportConfig as _;

    let endpoint = endpoint.trim_end_matches('/').to_string();
    let service_name = otel.service_name.clone().unwrap_or_else(|| "kelter".into());
    let resource = || {
        opentelemetry_sdk::Resource::builder()
            .with_service_name(service_name.clone())
            .build()
    };

    // --- traces ---
    let span_exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(format!("{endpoint}/v1/traces"))
        .build()
        .map_err(|e| anyhow::anyhow!("otlp span exporter: {e}"))?;
    let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(span_exporter)
        .with_resource(resource())
        .build();
    opentelemetry::global::set_tracer_provider(tracer_provider.clone());
    let tracer = tracer_provider.tracer("kelter");

    // --- metrics ---
    let metric_exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .with_endpoint(format!("{endpoint}/v1/metrics"))
        .build()
        .map_err(|e| anyhow::anyhow!("otlp metric exporter: {e}"))?;
    // A short reader interval so metrics are observable promptly (this is a
    // local-first runtime; a 10s cadence when telemetry is explicitly enabled
    // is fine and avoids a minute-long wait to see a counter move).
    let metric_reader = opentelemetry_sdk::metrics::PeriodicReader::builder(metric_exporter)
        .with_interval(std::time::Duration::from_secs(10))
        .build();
    let meter_provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
        .with_reader(metric_reader)
        .with_resource(resource())
        .build();
    opentelemetry::global::set_meter_provider(meter_provider.clone());
    let metrics = {
        use opentelemetry::metrics::MeterProvider as _;
        KernelMetrics::new(&meter_provider.meter("kelter"))
    };

    // --- logs (tracing events → OTel logs bridge) ---
    let log_exporter = opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .with_endpoint(format!("{endpoint}/v1/logs"))
        .build()
        .map_err(|e| anyhow::anyhow!("otlp log exporter: {e}"))?;
    let logger_provider = opentelemetry_sdk::logs::SdkLoggerProvider::builder()
        .with_batch_exporter(log_exporter)
        .with_resource(resource())
        .build();
    let logs_layer =
        opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&logger_provider);

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(tracing_opentelemetry::layer().with_tracer(tracer))
        .with(logs_layer)
        .init();

    tracing::info!("otel: exporting traces + metrics + logs to {endpoint} (otlp/http)");
    Ok((
        OtelGuard {
            tracer: Some(tracer_provider),
            meter: Some(meter_provider),
            logger: Some(logger_provider),
        },
        Some(metrics),
    ))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config_path = std::env::args()
        .skip_while(|a| a != "--config")
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());
    let mut cfg = Config::load(&config_path)?;
    let (otel_guard, kernel_metrics) = init_observability(&cfg.otel)?;

    // A UI-chosen model (Phase 8.5) persists as an overlay beside the config
    // and wins over the base [policy]; this also injects the resolved API key
    // (env or the restricted-perm secret file) — never from config.toml.
    model_config::apply_overlay(&config_path, &mut cfg.policy);

    // Policy: the adapter + model choice is one line in the config file — now
    // runtime-swappable behind a SwappableOracle (Phase 8.5).
    let (oracle, latitude) = policy::build_oracle(&cfg.policy)
        .context("failed to build policy adapter from [policy] config")?;
    tracing::info!("policy engine: {}", oracle.describe());
    let swappable = Arc::new(policy::SwappableOracle::new(oracle));

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

    let mut memory_extensions: Vec<mcp::ServerSpec> = Vec::new();
    let mut memory_extension_names: Vec<String> = Vec::new();
    let mut remote_agents: Vec<a2a::RemoteAgent> = Vec::new();
    let mut wasm_sources: Vec<Arc<dyn kernel_core::ActionSource>> = Vec::new();
    // Trust posture recorded on every run (ADR 0012).
    let mut trust_posture: Vec<(String, String)> = Vec::new();
    for ext in extensions {
        tracing::info!(
            "registry: extension '{}' capability={:?} trust={:?} permissions={:?}",
            ext.name,
            ext.capability,
            ext.trust,
            ext.permissions
        );

        // Operator-consent gate (ADR 0012), part two of the two-part opt-in.
        // The manifest declaring `subprocess-unsandboxed` is NOT enough — the
        // operator must list the extension here. A manifest cannot authorise
        // its own escape.
        if ext.requires_operator_consent()
            && !cfg
                .trust
                .allow_unsandboxed_subprocess
                .iter()
                .any(|n| n == &ext.name)
        {
            anyhow::bail!(
                "extension '{}' opted into subprocess-unsandboxed, but the operator has not \
                 allowed it. Add it to [trust] allow_unsandboxed_subprocess to run it — this is \
                 an explicit trust decision and does NOT sandbox the subprocess (ADR 0012).",
                ext.name
            );
        }
        if ext.trust == mcp::extensions::TrustMode::SubprocessUnsandboxed {
            // Honest labelling (ADR 0012): this is operator-trusted, NOT
            // contained. Say so plainly at startup.
            tracing::warn!(
                "trust: '{}' runs as an operator-trusted subprocess (unsandboxed — it has \
                 OS-level egress and is NOT contained; the operator explicitly accepted this)",
                ext.name
            );
        }
        trust_posture.push((ext.name.clone(), ext.trust.label().to_string()));

        match (ext.capability, ext.kind) {
            (
                mcp::extensions::Capability::Tools,
                mcp::extensions::ExtensionKind::Server(mut spec),
            ) => {
                // Permission scoping: the run workspace is handed over only
                // when the manifest asked for it.
                if ext.permissions.workspace_write {
                    spec.env.push((
                        "KELTER_WORKSPACE".into(),
                        workspace_abs.to_string_lossy().into_owned(),
                    ));
                }
                if ext.permissions.network {
                    tracing::warn!(
                        "registry: subprocess extension '{}' requests network access — \
                         recorded; transport-level sandboxing is a later slice",
                        ext.name
                    );
                }
                servers.push(spec);
            }
            (
                mcp::extensions::Capability::Memory,
                mcp::extensions::ExtensionKind::Server(mut spec),
            ) => {
                if ext.permissions.workspace_write {
                    spec.env.push((
                        "KELTER_WORKSPACE".into(),
                        workspace_abs.to_string_lossy().into_owned(),
                    ));
                }
                memory_extension_names.push(ext.name.clone());
                memory_extensions.push(spec);
            }
            (
                mcp::extensions::Capability::Agent,
                mcp::extensions::ExtensionKind::RemoteAgent(endpoint),
            ) => {
                // A2A egress is kernel-owned: the `network` permission was
                // already required at load; the client is pinned to the
                // declared card host (SSRF allowlist).
                // Optional bearer token for an auth-requiring remote endpoint
                // (e.g. another Kelter's inbound A2A, ADR 0013). The manifest
                // names an env var; the token itself is never in the tree.
                let token = endpoint
                    .token_env
                    .as_ref()
                    .and_then(|var| match std::env::var(var) {
                        Ok(t) if !t.is_empty() => Some(t),
                        _ => {
                            tracing::warn!(
                                "a2a: agent '{}' declares token_env='{}' but it is unset/empty — \
                                 dialing without auth",
                                ext.name,
                                var
                            );
                            None
                        }
                    });
                remote_agents.push(a2a::RemoteAgent {
                    name: ext.name.clone(),
                    card_url: endpoint.card_url,
                    token,
                });
            }
            (
                mcp::extensions::Capability::Tools,
                mcp::extensions::ExtensionKind::Wasm(endpoint),
            ) => {
                // In-process, capability-sandboxed (ADR 0011). The host
                // grants ONLY the capabilities the manifest declared —
                // default: no network, no disk. A module importing an
                // ungranted capability cannot be instantiated.
                let grants = wasmhost::Grants {
                    network: ext.permissions.network,
                };
                tracing::info!(
                    "registry: WASM extension '{}' (in-process, sandboxed; grants: {:?})",
                    ext.name,
                    grants
                );
                let source = wasmhost::WasmToolSource::load(
                    ext.name.clone(),
                    &endpoint.module_path,
                    endpoint.tool,
                    endpoint.description,
                    grants,
                )
                .with_context(|| format!("failed to load WASM extension '{}'", ext.name))?;
                wasm_sources.push(wasmhost::arc(source));
            }
            (capability, kind) => {
                anyhow::bail!("registry mis-routed capability {capability:?} with kind {kind:?}")
            }
        }
    }

    // Assemble the action space from every mechanism: MCP tools, WASM
    // tools (in-process, sandboxed), and A2A delegations — all merged into
    // one governed action space. The MCP source is only built when there
    // are servers (a WASM-only or A2A-only deployment has none).
    let mut sources: Vec<Arc<dyn kernel_core::ActionSource>> = Vec::new();
    if !servers.is_empty() {
        let tools = mcp::McpToolSource::connect(servers)
            .await
            .context("failed to start MCP tool servers")?;
        tracing::info!("mcp tools online: {}", tools.tool_names().join(", "));
        sources.push(Arc::new(tools));
    }
    for source in &wasm_sources {
        if let Ok(schema) = source.action_schema(latitude).await {
            tracing::info!(
                "wasm tools online: {}",
                schema
                    .actions
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    sources.extend(wasm_sources);
    if !remote_agents.is_empty() {
        for agent in &remote_agents {
            tracing::info!(
                "a2a: remote agent '{}' registered (card: {})",
                agent.name,
                agent.card_url
            );
        }
        let delegations = a2a::A2aActionSource::new(remote_agents.clone())
            .context("failed to configure the A2A delegation client")?;
        sources.push(Arc::new(delegations));
    }
    let action_source: Arc<dyn kernel_core::ActionSource> = match sources.len() {
        0 => anyhow::bail!(
            "no action sources configured — enable tools, a WASM or memory \
                            extension, or a remote agent"
        ),
        1 => sources.pop().unwrap(),
        _ => Arc::new(kernel_core::CompositeActionSource::new(sources)),
    };

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

    // Governed self-learning (Phase 10, ADR 0016): a DISTINCT, reversible
    // learned-fact layer on the same durable database (a second handle, like
    // the desktop store). Only for the sqlite backend — the layer must persist.
    let learned: Option<Arc<dyn kernel_core::LearnedStore>> = if cfg.storage.backend == "sqlite" {
        let db = storage::Db::open(&cfg.storage.path)
            .with_context(|| format!("cannot open database '{}'", cfg.storage.path))?;
        Some(Arc::new(storage::SqliteLearnedStore::new(db)))
    } else {
        None
    };

    // Memory: always a registered extension over the reserved MCP tools
    // (ADR 0008). An external memory extension overrides the built-in one;
    // the built-in durable memory is itself dogfooded through the same
    // registry seam (spawned `memory-server`, no in-process shortcut).
    if memory_extensions.len() > 1 {
        anyhow::bail!(
            "multiple memory extensions registered ({memory_extension_names:?}) — exactly one \
             memory backend is supported"
        );
    }
    let memory: Arc<dyn MemoryProvider> = if let Some(spec) = memory_extensions.pop() {
        tracing::info!(
            "memory: extension '{}' (external backend)",
            memory_extension_names
                .first()
                .map(String::as_str)
                .unwrap_or("?")
        );
        Arc::new(
            mcp::McpMemoryProvider::connect(spec)
                .await
                .context("failed to start the registered memory extension")?,
        )
    } else if cfg.storage.backend == "sqlite" {
        let exe_dir = std::env::current_exe()?
            .parent()
            .map(|p| p.to_path_buf())
            .context("executable has no parent dir")?;
        // Retrieval switch (Phase 8, item D): pass the embedding server to the
        // memory-server via env when configured; absent → BM25 default.
        let mut env: Vec<(String, String)> = Vec::new();
        if let Some(url) = cfg
            .memory
            .embeddings_url
            .as_deref()
            .filter(|u| !u.is_empty())
        {
            env.push(("KELTER_EMBEDDINGS_URL".into(), url.to_string()));
            if let Some(model) = cfg.memory.embeddings_model.as_deref() {
                env.push(("KELTER_EMBEDDINGS_MODEL".into(), model.to_string()));
            }
            tracing::info!("memory: embedding retrieval enabled (server {url})");
        }
        let spec = mcp::ServerSpec {
            name: "builtin-memory".into(),
            command: exe_dir.join("memory-server").to_string_lossy().into_owned(),
            args: vec!["--db".into(), cfg.storage.path.clone()],
            cwd: None,
            env,
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

    let mut kernel_built = Kernel::new(
        swappable.clone(),
        action_source,
        memory,
        store.clone(),
        KernelConfig {
            max_steps: cfg.kernel.max_steps,
            latitude,
            reflection_enabled: cfg.kernel.reflection,
            mode: match cfg.kernel.mode.as_str() {
                "plan" => kernel_core::RunMode::Plan,
                _ => kernel_core::RunMode::Step,
            },
            trust_posture,
            // Built-in dated prices, extended/overridden by [pricing] config.
            pricing: kernel_core::PricingTable::builtin().merged_with(cfg.pricing.clone()),
        },
    );
    if let Some(learned) = learned {
        kernel_built = kernel_built.with_learned(learned);
        tracing::info!("self-learning: governed distillation enabled (ADR 0016)");
    }
    let kernel = Arc::new(kernel_built);

    // Metrics recorder (Phase 8, item C): translate the kernel's run-event
    // stream into OTLP metrics. Only when observability is enabled.
    if let Some(metrics) = kernel_metrics {
        let rx = kernel.subscribe();
        tokio::spawn(run_metrics_recorder(rx, metrics));
    }

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

    // Token resolution (Phase 8, item F): env first (secrets stay out of the
    // config file / tree), then the config value, then a generated one.
    let token = std::env::var("KELTER_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .inspect(|_| tracing::info!("auth: using bearer token from KELTER_TOKEN env"))
        .or_else(|| cfg.server.token.clone())
        .unwrap_or_else(|| {
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

    // Runtime model-config controller (Phase 8.5): the UI reads/writes the
    // model here; the key is never held in the exposed state.
    let controller: Arc<dyn api::ModelConfigController> =
        Arc::new(model_config::DaemonModelConfig::new(
            &config_path,
            swappable.clone(),
            policy::PolicyConfig {
                api_key: None,
                ..cfg.policy.clone()
            },
        ));
    let serve = api::serve_with_config(kernel, token, addr, Some(controller));
    tokio::select! {
        res = serve => res?,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
        }
    }
    // Flush + shut down all OTel pillars — batch exporters do not auto-flush.
    otel_guard.shutdown();
    Ok(())
}
