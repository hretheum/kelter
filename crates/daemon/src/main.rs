//! Headless daemon: assembles memory + policy + tools + kernel and serves
//! the one stable API contract (HTTP + WS) with the embedded web UI.

use std::sync::Arc;

use anyhow::Context;
use rand::Rng;

use daemon::agents_controller;
use daemon::config::{self, Config};
use daemon::delegation_sweep;
use daemon::skill_sweep;
use daemon::skills_controller;
use kernel_core::{Kernel, KernelConfig, RunStore};
use memory::{InMemoryProvider, MemoryProvider};

fn generate_token() -> String {
    let bytes: [u8; 24] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The optional (memory provider, learned store) pair a memory backend yields.
type MemoryBackends = (
    Option<Arc<dyn MemoryProvider>>,
    Option<Arc<dyn kernel_core::LearnedStore>>,
);

/// Build the shared Postgres memory + learned backend (ADR 0028). Real only
/// when compiled with the `postgres` feature; otherwise a clear build-time
/// bail so a `backend = "postgres"` config never silently degrades. The
/// connection string is read from the env var named in the config — never
/// from the config file, never logged.
#[cfg(feature = "postgres")]
async fn build_postgres_memory(cfg: &Config) -> anyhow::Result<MemoryBackends> {
    let url = std::env::var(&cfg.memory.postgres_url_env).with_context(|| {
        format!(
            "memory.backend = \"postgres\" but env var '{}' (memory.postgres_url_env) is unset — \
             the connection string must live in the environment, never in the config file",
            cfg.memory.postgres_url_env
        )
    })?;
    // Optional pgvector cosine path: reuse the same embeddings config the
    // sqlite memory-server uses (BM25 when absent).
    let embed: Option<Arc<dyn memory::embeddings::EmbeddingBackend>> = cfg
        .memory
        .embeddings_url
        .as_deref()
        .filter(|u| !u.is_empty())
        .map(|u| {
            let model = cfg.memory.embeddings_model.clone().unwrap_or_default();
            Arc::new(memory::embeddings::LlamaEmbeddingBackend::new(u, model))
                as Arc<dyn memory::embeddings::EmbeddingBackend>
        });
    let store = pgstore::PgStore::connect(&url, embed)
        .await
        .context("cannot connect to the shared Postgres memory backend (ADR 0028)")?;
    tracing::info!(
        "memory: SHARED Postgres backend (ADR 0028){}",
        if store.uses_embeddings() {
            " with pgvector cosine retrieval"
        } else {
            " (BM25 retrieval)"
        }
    );
    let mem: Arc<dyn MemoryProvider> = Arc::new(pgstore::PgMemoryProvider::new(store.clone()));
    let learned: Arc<dyn kernel_core::LearnedStore> = Arc::new(pgstore::PgLearnedStore::new(store));
    Ok((Some(mem), Some(learned)))
}

#[cfg(not(feature = "postgres"))]
async fn build_postgres_memory(_cfg: &Config) -> anyhow::Result<MemoryBackends> {
    anyhow::bail!(
        "memory.backend = \"postgres\" but this daemon was built WITHOUT the `postgres` \
         feature — rebuild with `cargo build --features postgres` (or set memory.backend = \
         \"sqlite\")"
    )
}

/// The `prune` subcommand (ADR 0017): open the durable store and apply the
/// configured retention policy once, then exit. Terminal runs only — never an
/// in-flight run.
async fn run_prune(cfg: &Config) -> anyhow::Result<()> {
    let store: Arc<dyn RunStore> = match cfg.storage.backend.as_str() {
        "sqlite" => Arc::new(storage::SqliteRunStore::new(
            storage::Db::open(&cfg.storage.path)
                .with_context(|| format!("cannot open database '{}'", cfg.storage.path))?,
        )),
        other => anyhow::bail!("`prune` requires the sqlite backend (got '{other}')"),
    };
    let report = store.prune(&cfg.retention.to_policy()).await?;
    println!(
        "{}",
        serde_json::to_string(&report).unwrap_or_else(|_| "{}".into())
    );
    Ok(())
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

/// The `skill` subcommand family (ADR 0019). Everything here is registry
/// bookkeeping: install/list/remove/activate. No skill content is executed,
/// and nothing touches the action space — capability candidates (scripts/,
/// kelter-extension.toml) are only REPORTED, with the extension-registry
/// route named explicitly.
async fn run_skill_command(cfg: &Config, args: &[String]) -> anyhow::Result<()> {
    let reg = skills::SkillRegistry::open(&cfg.skills.dir)?;
    let sub = args.first().map(String::as_str).unwrap_or("list");
    match (sub, args.get(1)) {
        ("add", Some(src)) => {
            if src.starts_with("http://") || src.starts_with("https://") {
                let report = reg.install_from_url(src).await?;
                println!(
                    "installed {} skill(s) from {src} (provenance: url, trust: untrusted)",
                    report.installed.len()
                );
                for (dir, why) in &report.skipped {
                    println!("  skipped {dir}: {why}");
                }
            } else {
                let path = std::path::Path::new(src);
                if path.join("SKILL.md").is_file() {
                    let m = reg.install_from_dir(path)?;
                    println!("installed '{}' (trust: {})", m.name, m.provenance.trust);
                } else {
                    let report = reg.install_bundle(path)?;
                    println!(
                        "installed {} skill(s) from bundle {src} (trust: untrusted)",
                        report.installed.len()
                    );
                    for (dir, why) in &report.skipped {
                        println!("  skipped {dir}: {why}");
                    }
                }
            }
        }
        ("remove", Some(name)) => {
            reg.remove(name)?;
            println!("removed '{name}'");
        }
        ("activate", Some(name)) => {
            reg.activate(name)?;
            println!(
                "activated '{name}' — its instructions now enter the model's context. \
                 Activation adds knowledge, not capability: every proposed action is \
                 still validated by the kernel."
            );
        }
        ("deactivate", Some(name)) => {
            reg.deactivate(name)?;
            println!("deactivated '{name}'");
        }
        ("list", _) => {
            let active = reg.active_names();
            let skills_list = reg.list();
            if skills_list.is_empty() {
                println!("no skills installed (dir: {})", cfg.skills.dir);
            }
            for s in skills_list {
                let mark = if active.contains(&s.name) { "*" } else { " " };
                let mut flags = Vec::new();
                if s.has_scripts {
                    flags.push(
                        "scripts: candidate (inert — expose via [tools] extension_dirs + [trust])",
                    );
                }
                if s.has_extension_manifest {
                    flags.push("extension manifest: candidate for extension_dirs");
                }
                println!(
                    "{mark} {}  [{}, {}]  {}{}",
                    s.name,
                    s.provenance.trust,
                    s.provenance.source_kind,
                    truncate_line(&s.description, 80),
                    if flags.is_empty() {
                        String::new()
                    } else {
                        format!("\n     {}", flags.join("; "))
                    }
                );
            }
        }
        _ => {
            println!(
                "usage: kelter [--config X] skill <add <dir|url> | list | remove <name> | \
                 activate <name> | deactivate <name>>"
            );
        }
    }
    Ok(())
}

fn truncate_line(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut cut = max;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &s[..cut])
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config_path = std::env::args()
        .skip_while(|a| a != "--config")
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());
    let mut cfg = Config::load(&config_path)?;

    // `kelter --config X prune`: run store retention once and exit (ADR 0017).
    if std::env::args().skip(1).any(|a| a == "prune") {
        return run_prune(&cfg).await;
    }

    // `kelter --config X skill <add|list|remove|activate|deactivate> …`
    // (ADR 0019): manage the advisory skill registry and exit. Pure
    // file-level state — activation marks context injection, never execution.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(pos) = args.iter().position(|a| a == "skill") {
        return run_skill_command(&cfg, &args[pos + 1..]).await;
    }

    let (otel_guard, kernel_metrics) = init_observability(&cfg.otel)?;

    // A UI-chosen model (Phase 8.5) persists as an overlay beside the config
    // and wins over the base [policy]; this also injects the resolved API key
    // (env or the restricted-perm secret file) — never from config.toml.
    model_config::apply_overlay(&config_path, &mut cfg.policy);

    // A UI-chosen skills policy (ADR 0021 addendum) persists the same way —
    // an overlay beside the config, applied BEFORE the skills wiring below
    // reads `cfg.skills.*`.
    skills_controller::apply_overlay(&config_path, &mut cfg.skills);
    // Same overlay convention, for the agent-delegation policy (ADR 0023) —
    // must also run BEFORE the agents wiring block reads `cfg.delegation.
    // agent_*`.
    agents_controller::apply_overlay(&config_path, &mut cfg.delegation);

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
    // A WASM memory backend (ADR 0024) — at most one, counted together with
    // the subprocess memory extensions below.
    let mut wasm_memory: Option<Arc<dyn MemoryProvider>> = None;
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
                let tool = endpoint.tool.clone().with_context(|| {
                    format!(
                        "registry invariant broken: wasm tools extension '{}' has no tool name",
                        ext.name
                    )
                })?;
                let source = wasmhost::WasmToolSource::load(
                    ext.name.clone(),
                    &endpoint.module_path,
                    tool,
                    endpoint.description,
                    grants,
                )
                .with_context(|| format!("failed to load WASM extension '{}'", ext.name))?;
                wasm_sources.push(wasmhost::arc(source));
            }
            (
                mcp::extensions::Capability::Memory,
                mcp::extensions::ExtensionKind::Wasm(endpoint),
            ) => {
                // WASM memory backend (ADR 0024): in-process, capability-
                // sandboxed, persistent instance. Loading instantiates —
                // an ungranted import refuses the daemon start loudly here,
                // not on first retrieval.
                let grants = wasmhost::Grants {
                    network: ext.permissions.network,
                };
                tracing::info!(
                    "registry: WASM memory extension '{}' (in-process, sandboxed; grants: {:?})",
                    ext.name,
                    grants
                );
                let provider = wasmhost::WasmMemoryProvider::load(
                    ext.name.clone(),
                    &endpoint.module_path,
                    grants,
                )
                .with_context(|| format!("failed to load WASM memory extension '{}'", ext.name))?;
                memory_extension_names.push(ext.name.clone());
                wasm_memory = Some(Arc::new(provider) as Arc<dyn MemoryProvider>);
            }
            (capability, kind) => {
                anyhow::bail!("registry mis-routed capability {capability:?} with kind {kind:?}")
            }
        }
    }

    // Fail-fast check BEFORE the A2A source is built: an always-present,
    // possibly EMPTY A2aActionSource (below, for ADR 0023 dynamic discovery)
    // must never quietly satisfy "at least one action source configured" —
    // that would silently offer the model zero actions instead of bailing
    // loudly.
    if servers.is_empty() && wasm_sources.is_empty() && remote_agents.is_empty() {
        anyhow::bail!(
            "no action sources configured — enable tools, a WASM or memory \
             extension, or a remote agent"
        );
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
    for agent in &remote_agents {
        tracing::info!(
            "a2a: remote agent '{}' registered (card: {})",
            agent.name,
            agent.card_url
        );
    }
    // Always constructed (even with zero configured agents) so dynamic
    // discovery (ADR 0023 Part 3) can register the FIRST agent at runtime
    // without a restart, in a deployment that started with none configured.
    // The fail-fast bail above already ran on the PRE-discovery state, so
    // this never silently satisfies it.
    let a2a_source = Arc::new(
        a2a::A2aActionSource::new(remote_agents.clone())
            .context("failed to configure the A2A delegation client")?
            .with_circuit_breaker(
                cfg.delegation.circuit_failure_threshold,
                std::time::Duration::from_secs(cfg.delegation.circuit_cooldown_secs),
            )
            .with_streaming(cfg.delegation.stream_progress),
    );
    sources.push(a2a_source.clone());
    let action_source: Arc<dyn kernel_core::ActionSource> = match sources.len() {
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

    // Shared Postgres memory backend (ADR 0028) — opt-in via
    // `memory.backend = "postgres"` AND the `postgres` build feature. When
    // active it provides BOTH the episodic `MemoryProvider` and the learned
    // layer, SHARED across independent kernels. Built once here; the memory
    // selection below prefers it over the sqlite builtin.
    let (pg_memory, pg_learned): MemoryBackends = if cfg.memory.backend == "postgres" {
        build_postgres_memory(&cfg).await?
    } else {
        (None, None)
    };

    // Governed self-learning (Phase 10, ADR 0016): a DISTINCT, reversible
    // learned-fact layer. Shared Postgres (ADR 0028) when configured;
    // otherwise the local sqlite layer on the same durable database (a second
    // handle, like the desktop store). The layer must persist, so never for
    // the ephemeral in-memory storage backend.
    let learned: Option<Arc<dyn kernel_core::LearnedStore>> = if let Some(l) = pg_learned {
        Some(l)
    } else if cfg.storage.backend == "sqlite" {
        let db = storage::Db::open(&cfg.storage.path)
            .with_context(|| format!("cannot open database '{}'", cfg.storage.path))?;
        Some(Arc::new(storage::SqliteLearnedStore::new(db)))
    } else {
        None
    };

    // Memory: always a registered extension — subprocess over the reserved
    // MCP tools (ADR 0008) or an in-process WASM module (ADR 0024). An
    // external memory extension overrides the built-in one; the built-in
    // durable memory is itself dogfooded through the same registry seam
    // (spawned `memory-server`, no in-process shortcut). Exactly one external
    // backend, counted across BOTH mechanisms.
    if memory_extensions.len()
        + usize::from(wasm_memory.is_some())
        + usize::from(pg_memory.is_some())
        > 1
    {
        anyhow::bail!(
            "multiple memory backends configured (extensions {memory_extension_names:?}, \
             wasm={}, postgres={}) — exactly one memory backend is supported",
            wasm_memory.is_some(),
            pg_memory.is_some()
        );
    }
    let memory: Arc<dyn MemoryProvider> = if let Some(provider) = wasm_memory.take() {
        tracing::info!(
            "memory: WASM extension '{}' (in-process, sandboxed; NOT the durable builtin — \
             persistence is whatever the module implements within its grants)",
            memory_extension_names
                .first()
                .map(String::as_str)
                .unwrap_or("?")
        );
        provider
    } else if let Some(spec) = memory_extensions.pop() {
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
    } else if let Some(provider) = pg_memory {
        // Shared Postgres backend (ADR 0028): in-process, not dogfooded via
        // the memory-server subprocess (that transport is SQLite-file-shaped).
        provider
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

    // Skills (ADR 0019): a PolicyOracle decorator that injects the skill
    // catalog (always) and activated instructions (progressive disclosure)
    // into the oracle's context — and passes the kernel's action schema
    // through untouched, so skill content can never widen the action space.
    // It wraps the SWAPPABLE oracle, so a runtime model swap keeps the layer.
    //
    // Model self-activation (ADR 0020, Phase 12): a separate seam — the
    // `SkillGate` the kernel calls to authorize a MODEL-proposed
    // `activate_skill`. Hoisted above the block so it can also be handed to
    // the Kernel builder below (`.with_skills`); `None` when skills are
    // disabled entirely.
    let mut skill_gate: Option<Arc<dyn kernel_core::SkillGate>> = None;
    // Concrete handle (ADR 0021 addendum): the SAME object as `skill_gate`
    // (via Arc::clone before the upcast), kept so `DaemonSkillsController`
    // can call the plain `set_policy`/`get_policy` methods the `SkillGate`
    // trait itself deliberately does not expose (kernel-facing only).
    let mut skill_gate_concrete: Option<Arc<skills::RegistrySkillGate>> = None;
    let mut skill_reg_handle: Option<Arc<skills::SkillRegistry>> = None;
    let policy_oracle: Arc<dyn kernel_core::PolicyOracle> = if cfg.skills.enabled {
        let skill_reg = Arc::new(
            skills::SkillRegistry::open(&cfg.skills.dir)
                .context("failed to open the skills registry")?,
        );
        for name in &cfg.skills.active {
            if let Err(e) = skill_reg.activate(name) {
                tracing::warn!("skills: cannot activate '{name}' from config: {e}");
            }
        }
        let n = skill_reg.list().len();
        if n > 0 {
            tracing::info!(
                "skills: {n} installed ({} active) — advisory context only (ADR 0019)",
                skill_reg.active_names().len()
            );
        }
        let activation_policy = skills::parse_model_activation_policy(&cfg.skills.model_activation);
        // Only meaningful under `Ask` (ADR 0021): whether an ask-worthy
        // self-activation actually pauses, or is denied outright when there
        // is no one configured to ask.
        let ask_fallback = skills::parse_ask_fallback(&cfg.skills.ask_fallback);
        let gate_concrete = Arc::new(
            skills::RegistrySkillGate::new(skill_reg.clone(), activation_policy)
                .with_ask_fallback(ask_fallback)
                .with_ask_timeout_secs(cfg.skills.ask_timeout_secs),
        );
        let gate: Arc<dyn kernel_core::SkillGate> = gate_concrete.clone();
        skill_gate = Some(gate);
        skill_gate_concrete = Some(gate_concrete);
        skill_reg_handle = Some(skill_reg.clone());
        tracing::info!(
            "skills: model self-activation enabled (policy={}, ask_fallback={})",
            cfg.skills.model_activation,
            cfg.skills.ask_fallback
        );
        Arc::new(skills::SkillContextOracle::new(
            swappable.clone(),
            skill_reg,
        ))
    } else {
        swappable.clone()
    };

    // Skills management over HTTP (ADR 0021 addendum): only when skills are
    // actually wired (both handles present) — `None` otherwise reports 404
    // on every `/api/v1/skills*` route, the frontend's existing
    // capability-flag signal.
    let skills_ctrl: Option<Arc<dyn api::SkillsController>> =
        match (skill_reg_handle, skill_gate_concrete) {
            (Some(reg), Some(gate)) => Some(Arc::new(
                daemon::skills_controller::DaemonSkillsController::new(reg, gate, &config_path),
            )),
            _ => None,
        };

    // Agent trust/policy/ask + governed resilience (ADR 0023): an
    // `AgentRegistry` seeded from the statically-configured `remote_agents`
    // — config-declared agents start TRUSTED (the ADR's disclosed
    // default-trust split: putting an agent in your own config file is the
    // operator's trust act, exactly parallel to a skill sidecar edit) — a
    // `RegistryDelegationGate` mirroring `RegistrySkillGate` exactly, and an
    // HTTP controller for dynamic discovery + policy management.
    let agent_registry = Arc::new(
        a2a::AgentRegistry::open(&cfg.delegation.agents_registry_path, &remote_agents)
            .context("failed to open the agent registry")?,
    );
    let agent_policy = a2a::parse_agent_delegation_policy(&cfg.delegation.agent_policy);
    let agent_ask_fallback = a2a::parse_agent_ask_fallback(&cfg.delegation.agent_ask_fallback);
    let delegation_gate_concrete = Arc::new(
        a2a::RegistryDelegationGate::new(agent_registry.clone(), agent_policy)
            .with_ask_fallback(agent_ask_fallback)
            .with_ask_timeout_secs(cfg.delegation.agent_ask_timeout_secs),
    );
    let delegation_gate: Arc<dyn kernel_core::DelegationGate> = delegation_gate_concrete.clone();
    tracing::info!(
        "agents: {} registered, delegation policy={} ask_fallback={}",
        agent_registry.list().len(),
        cfg.delegation.agent_policy,
        cfg.delegation.agent_ask_fallback
    );
    let agents_ctrl: Arc<dyn api::AgentsController> =
        Arc::new(daemon::agents_controller::DaemonAgentsController::new(
            agent_registry.clone(),
            delegation_gate_concrete,
            a2a_source,
            &config_path,
        ));

    // Sanctioned-only resilience fallback (ADR 0023): normalize the
    // operator-declared bare agent names (both keys and values, `[delegation.
    // fallback]`) to `delegate_to_*` action names — kernel-core stays
    // a2a-agnostic and only ever deals in action names, exactly like every
    // other seam here (`a2a_origin`, `DelegationGate::authorize_delegation_to`).
    let mut fallback: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for (primary, alternates) in &cfg.delegation.fallback {
        fallback.insert(
            a2a::action_name(primary),
            alternates.iter().map(|a| a2a::action_name(a)).collect(),
        );
    }

    let mut kernel_built = Kernel::new(
        policy_oracle,
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
            delegation: kernel_core::DelegationLimits {
                max_delegations_per_run: cfg.delegation.max_per_run,
                max_depth: cfg.delegation.max_depth,
            },
            resilience: kernel_core::ResilienceConfig {
                max_retries_per_delegation: cfg.delegation.max_retries_per_delegation,
                fallback,
            },
        },
    );
    if let Some(learned) = learned {
        kernel_built = kernel_built.with_learned(learned);
        tracing::info!("self-learning: governed distillation enabled (ADR 0016)");
    }
    if let Some(gate) = skill_gate {
        kernel_built = kernel_built.with_skills(gate);
    }
    kernel_built = kernel_built.with_delegation_gate(delegation_gate);
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

    // Store retention on start (ADR 0017): prune terminal runs beyond the
    // policy window. Runs AFTER resume, and only ever touches terminal runs, so
    // in-flight/just-resumed runs are never at risk.
    let policy = cfg.retention.to_policy();
    if cfg.retention.prune_on_start && !policy.is_noop() {
        match store.prune(&policy).await {
            Ok(r) if r.runs_pruned > 0 => tracing::info!(
                runs = r.runs_pruned,
                journal = r.journal_entries_pruned,
                episodes = r.episodes_pruned,
                "retention: pruned terminal runs on start"
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!("retention: prune-on-start failed: {e}"),
        }
    }

    // The `ask`-activation timeout sweeper (ADR 0021): only when a paused run
    // can actually occur (`model_activation == "ask"` AND `ask_fallback ==
    // "queue"`) — under the default `ask_fallback = "deny"` a run never
    // pauses, so no sweeper is needed. Bounds every pause: no paused run can
    // wait longer than `ask_timeout_secs` before being force-denied and
    // resumed.
    if cfg.skills.enabled
        && cfg.skills.model_activation == "ask"
        && cfg.skills.ask_fallback == "queue"
    {
        let kernel = kernel.clone();
        let timeout_secs = cfg.skills.ask_timeout_secs;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tick.tick().await;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                for run_id in
                    skill_sweep::sweep_timed_out_skill_activations(&kernel, timeout_secs, now).await
                {
                    let kernel = kernel.clone();
                    tokio::spawn(async move {
                        if let Err(e) = kernel.execute_run(run_id).await {
                            tracing::error!(%run_id, "resume after ask-timeout failed: {e}");
                        }
                    });
                }
            }
        });
        tracing::info!(
            "skills: ask-activation timeout sweeper active (timeout={}s)",
            timeout_secs
        );
    }

    // The delegation ask-authorization timeout sweeper (ADR 0023): same
    // never-hangs contract as the skills sweeper above, for a paused
    // `delegate_to_*` request. A SECOND small sweeper rather than a
    // generalization of the one above — see `delegation_sweep.rs`'s module
    // docs for why.
    if cfg.delegation.agent_policy == "ask" && cfg.delegation.agent_ask_fallback == "queue" {
        let kernel = kernel.clone();
        let timeout_secs = cfg.delegation.agent_ask_timeout_secs;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tick.tick().await;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                for run_id in delegation_sweep::sweep_timed_out_delegation_decisions(
                    &kernel,
                    timeout_secs,
                    now,
                )
                .await
                {
                    let kernel = kernel.clone();
                    tokio::spawn(async move {
                        if let Err(e) = kernel.execute_run(run_id).await {
                            tracing::error!(%run_id, "resume after delegation ask-timeout failed: {e}");
                        }
                    });
                }
            }
        });
        tracing::info!(
            "agents: delegation ask-authorization timeout sweeper active (timeout={}s)",
            timeout_secs
        );
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
    // Logs carry only a MASKED fingerprint — never the token value (ADR 0017).
    tracing::info!(
        "auth: bearer token active (fingerprint {})",
        kernel_core::redact::redact_secret(&token)
    );

    let addr: std::net::SocketAddr = cfg
        .server
        .bind
        .parse()
        .with_context(|| format!("invalid bind address '{}'", cfg.server.bind))?;

    let scheme = if cfg.tls.is_some() { "https" } else { "http" };
    println!("ui:  {scheme}://{addr}/");
    println!("api: {scheme}://{addr}/api/v1  (Authorization: Bearer <token>)");

    // Named principals (Phase 11, ADR 0018): the default token is principal
    // "default"; named ones resolve their token from env (never the tree). The
    // single-token flow is unchanged when none are configured.
    let mut named: Vec<(String, String)> = Vec::new();
    for p in &cfg.auth.principals {
        match std::env::var(&p.token_env) {
            Ok(t) if !t.is_empty() => named.push((p.name.clone(), t)),
            _ => tracing::warn!(
                "auth: principal '{}' token_env '{}' unset/empty — skipping",
                p.name,
                p.token_env
            ),
        }
    }
    if !named.is_empty() {
        tracing::info!("auth: {} named principal(s) + default", named.len());
    }
    let principals = api::Principals::with_named(token, named);

    // Runtime model-config controller (Phase 8.5): the UI reads/writes the
    // model here; the key is never held in the exposed state. The health-check
    // is SSRF-guarded by the allowlist (Phase 11).
    let controller: Arc<dyn api::ModelConfigController> =
        Arc::new(model_config::DaemonModelConfig::new(
            &config_path,
            swappable.clone(),
            policy::PolicyConfig {
                api_key: None,
                ..cfg.policy.clone()
            },
            cfg.auth.test_allowlist.clone(),
        ));

    // Opt-in TLS termination (Phase 11, ADR 0018): serve HTTPS with a config
    // cert; default stays plain (a tailnet provides transport security).
    let app = api::build_app_with_skills_and_agents(
        kernel,
        principals,
        Some(controller),
        skills_ctrl,
        Some(agents_ctrl),
    );
    let serve = async {
        match &cfg.tls {
            Some(tls) => {
                // Pin the crypto provider (the tree also has ring via reqwest;
                // rustls 0.23 panics on an ambiguous default otherwise).
                let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
                let rustls =
                    axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.cert, &tls.key)
                        .await
                        .with_context(|| {
                            format!("cannot load TLS cert '{}' / key '{}'", tls.cert, tls.key)
                        })?;
                tracing::info!("api serving over HTTPS (rustls) on {addr}");
                axum_server::bind_rustls(addr, rustls)
                    .serve(app.into_make_service())
                    .await
                    .map_err(anyhow::Error::from)
            }
            None => {
                let listener = tokio::net::TcpListener::bind(addr).await?;
                axum::serve(listener, app)
                    .await
                    .map_err(anyhow::Error::from)
            }
        }
    };
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
