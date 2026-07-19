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
