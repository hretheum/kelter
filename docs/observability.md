---
provenance: ai_authored
provenance_metadata:
  agent: claude-fable-5
  session_date: 2026-07-18
  human_validated: false
  session_context: "Phase 3: how to export and inspect Kelter traces (OTLP)"
---

# Observability

Kelter emits `tracing` spans for everything the kernel does:

| Span | Attributes | Covers |
|---|---|---|
| `kelter.run` | `run_id`, `policy` | one whole run (incl. resume legs) |
| `kelter.step` | `step` | one decision + execution cycle |
| `kelter.model_call` | `step`, `attempt` | one policy-oracle (LLM) call |
| `kelter.validate` | `step` | kernel-side schema validation |
| `kelter.tool_call` | `step`, `tool` | one MCP tool execution |

Per-run token accounting is separate from tracing: every model call's usage
is journaled and aggregated on the run record — `GET /api/v1/runs/{id}`
returns `usage: {prompt_tokens, completion_tokens, calls}` and, when the model
is priced (ADR 0014), a `cost_usd` dollar figure (`null` for an unpriced
model — never a fabricated number).

## Metrics + logs (Phase 8, ADR 0014)

Beside traces, Kelter exports the other two OpenTelemetry pillars over the same
OTLP/HTTP endpoint — also **off by default**, enabled by the same `[otel]`
config.

**Metrics** (instruments, translated from the kernel's run-event stream):

| Instrument | Kind | Meaning |
|---|---|---|
| `kelter.runs.started` | counter | runs created |
| `kelter.runs.finished` | counter | runs reaching a terminal state (attr `outcome`) |
| `kelter.steps.executed` | counter | executed decision/action steps |
| `kelter.tool_calls` | counter | tool/action executions (attr `ok`) |
| `kelter.validation.rejections` | counter | kernel validation rejections |
| `kelter.run.latency` | histogram (ms) | run wall-clock, creation → terminal |

**Logs**: `tracing` events are bridged to OTel logs
(`opentelemetry-appender-tracing`) and exported to `…/v1/logs`.

The OTLP exporter uses a **blocking** HTTP client on purpose — the metrics
PeriodicReader and the batch processors run on their own threads (no tokio
reactor), where an async client would panic. Shutdown flush is bounded (3s) so
an unreachable collector never wedges Ctrl-C. A captured sample of exported
metrics + logs lives in `docs/evidence/otel-metrics-logs-sample.log`.

## Exporting (OTLP)

Off by default. Enable in the config:

```toml
[otel]
endpoint = "http://127.0.0.1:4318"   # OTLP over HTTP
# service_name = "kelter"
```

Spans are batched; the daemon flushes them on Ctrl-C shutdown.

## Seeing spans locally

Any OTLP-capable backend works. The minimal loop with the official
collector and its debug exporter:

```sh
cat > /tmp/otel-config.yaml <<'EOF'
receivers:
  otlp:
    protocols:
      http:
        endpoint: 0.0.0.0:4318
exporters:
  debug:
    verbosity: detailed
service:
  pipelines:
    traces:
      receivers: [otlp]
      exporters: [debug]
EOF
docker run -d --name otel -p 127.0.0.1:4318:4318 \
  -v /tmp/otel-config.yaml:/etc/otelcol/config.yaml \
  otel/opentelemetry-collector:0.156.0
docker logs -f otel     # spans print here as runs execute
```

A captured sample lives in `docs/evidence/otel-trace-sample.log`.

Version note (verified live 2026-07-18): opentelemetry/opentelemetry_sdk/
opentelemetry-otlp 0.32.x pair with tracing-opentelemetry 0.33.x; the
OTLP crate's default transport is HTTP (`http-proto`) as of 0.32.

## Logging: destination, levels, redaction (Phase 9, ADR 0017)

**Destination = stdout** (twelve-factor). The daemon does **not** write log
files itself — the platform captures and rotates stdout. This is deliberate;
there is no in-app rolling file logger.

**Levels.** `tracing` with an `EnvFilter`; the default is `info`. Override with
`RUST_LOG`, e.g.:

```sh
RUST_LOG=info                      # default
RUST_LOG=debug                     # verbose (dev only — not for production)
RUST_LOG=info,kernel_core=debug    # per-crate
```

Avoid a blanket `debug` in production (firehose). Fields are structured
(`run_id`, `step`, `tool`, …).

**Redaction.** By audit, no secret value is passed to a log macro: auth
headers, request/response bodies, the OpenRouter key and A2A payloads are never
logged. The daemon logs only a **masked fingerprint** of the active bearer
token (`kernel_core::redact::redact_secret`). `redact::scrub_secrets` strips
`Bearer …`/`sk-…`/`key-…` runs from any string before logging. The one
intentional full-token output is the one-time **pairing** print of a
*generated* token to stdout (copy it once; set `KELTER_TOKEN`/`[server] token`
to avoid it). Enforced by a capture test (`redaction_test.rs`) asserting a
secret never appears in captured log output.

## Bounding log size (deployment, ADR 0017)

Since logs go to stdout, cap them at the platform — not in the app.

**Quadlet / Podman** (`deploy/kelter.container`):

```ini
[Container]
LogDriver=k8s-file
LogOpt=max-size=10mb
LogOpt=max-file=3      # total ≤ 3 × 10 MB = 30 MB, rotated by podman
```

Validate with the podman quadlet generator: `quadlet -dryrun -user` (the
generated `podman run` carries `--log-driver` + `--log-opt max-size/max-file`).
Alternative: `LogDriver=journald` and cap via journald
(`/etc/systemd/journald.conf` → `SystemMaxUse=`). **Bare daemon** (no
container): redirect stdout to a file and rotate it with `logrotate`, e.g.
`kelter --config config.toml >> /var/log/kelter.log 2>&1`.

## Store retention (data-lifecycle, ADR 0017)

The durable SQLite store is bounded by a retention policy that prunes **terminal**
runs only (never an in-flight run — durability + exactly-once are untouched):

```toml
[retention]
prune_on_start = true     # prune once on start (default); false to disable
max_runs = 10000          # keep at most this many terminal runs (0 = unlimited)
# max_age_days = 30       # optional: also prune terminal runs older than N days
```

On-demand: `kelter --config config.toml prune` (prints a JSON report of what was
removed). Pruning deletes a victim run's journal + episodes + row in one
transaction, so a run and its markers are pruned together. Proof:
`scripts/smoke-retention.sh` + `docs/evidence/retention-prune.log`.
