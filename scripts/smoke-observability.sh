#!/usr/bin/env bash
# Observability + cost smoke (Phase 8, items C & B).
#   C) OTel metrics + logs (not just traces) actually export over OTLP/HTTP —
#      captured by a tiny local OTLP sink that records the request bodies;
#   B) a run's dollar cost is exposed via the API: a real number for a priced
#      (local → $0) model (`null` for an unpriced one).
#
#   scripts/smoke-observability.sh [evidence-dir]
#
# Dep-light: a stdlib-only Python OTLP sink (no docker). OTLP/HTTP bodies are
# protobuf; string fields (metric names, log bodies, span names) appear verbatim
# in the bytes, so grepping the captured payloads proves each pillar exported.
# Needs llama-server on :8080 (a real run to generate telemetry).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="${1:-}"
PORT=7621
OTLP_PORT=4319
BASE="http://127.0.0.1:${PORT}"
TOKEN="smoke-obs-$$"
BIN="$REPO_ROOT/target/debug/kelter"

log() { printf '[smoke-obs] %s\n' "$*"; }
die() { printf '[smoke-obs] FAIL: %s\n' "$*" >&2; exit 1; }

curl -fsS http://127.0.0.1:8080/health >/dev/null 2>&1 || die "llama-server not on :8080"

log "building workspace"
cargo build --workspace --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null

WORK="$(mktemp -d)"
cleanup() { kill -9 "${DPID:-0}" "${SINK:-0}" 2>/dev/null || true; }
trap cleanup EXIT

# ---- a stdlib OTLP sink: capture each POST body per signal ---------------
cat > "$WORK/sink.py" <<PY
import http.server, sys
OUT = sys.argv[1]
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get('content-length', 0))
        body = self.rfile.read(n)
        sig = self.path.rsplit('/', 1)[-1]  # traces | metrics | logs
        with open(f"{OUT}/otlp-{sig}.bin", "ab") as f:
            f.write(body)
        self.send_response(200); self.send_header('content-type','application/x-protobuf')
        self.send_header('content-length','0'); self.end_headers()
    def log_message(self, *a): pass
http.server.HTTPServer(('127.0.0.1', ${OTLP_PORT}), H).serve_forever()
PY
python3 "$WORK/sink.py" "$WORK" &
SINK=$!
sleep 1

# ---- daemon: observability ON + a known-priced (local) model -------------
cat > "$WORK/config.toml" <<EOF
[server]
bind = "127.0.0.1:${PORT}"
token = "${TOKEN}"
[policy]
engine = "local:qwen2.5-0.5b-instruct"
[kernel]
max_steps = 4
[storage]
backend = "sqlite"
path = "$WORK/kelter.db"
[otel]
endpoint = "http://127.0.0.1:${OTLP_PORT}"
service_name = "kelter"
[tools]
workspace = "$WORK/work"
bundled_tools = false
extension_dirs = []
[[tools.servers]]
name = "ledger"
command = "${REPO_ROOT}/target/debug/ledger-server"
args = ["--workspace", "$WORK/work"]
EOF
( cd "$WORK" && RUST_LOG=info "$BIN" --config config.toml > "$WORK/daemon.log" 2>&1 ) &
DPID=$!
for _ in $(seq 1 60); do curl -fsS "$BASE/healthz" >/dev/null 2>&1 && break; sleep 0.5; done
curl -fsS "$BASE/healthz" >/dev/null 2>&1 || { cat "$WORK/daemon.log"; die "daemon did not start"; }
grep -q "traces + metrics + logs" "$WORK/daemon.log" || die "all three OTel pillars were not enabled"
log "daemon up with OTLP export (traces + metrics + logs) → sink :${OTLP_PORT}"

# ---- a real run to generate telemetry + a cost ---------------------------
RUN=$(curl -fsS -X POST "$BASE/api/v1/runs" -H "Authorization: Bearer ${TOKEN}" \
  -H 'Content-Type: application/json' \
  -d '{"task":"Append the entry obs-smoke to the ledger using append_ledger. Then finish."}' \
  | sed -E 's/.*"run_id":"([^"]+)".*/\1/')
for _ in $(seq 1 45); do
  S=$(curl -fsS "$BASE/api/v1/runs/$RUN" -H "Authorization: Bearer ${TOKEN}" | sed -E 's/.*"status":"([a-z]+)".*/\1/')
  { [ "$S" = completed ] || [ "$S" = failed ]; } && break; sleep 2
done
RUNJSON=$(curl -fsS "$BASE/api/v1/runs/$RUN" -H "Authorization: Bearer ${TOKEN}")

# --- item B: dollar cost exposed on the run (local = priced $0, non-null) --
COST=$(printf '%s' "$RUNJSON" | python3 -c 'import json,sys; print(json.dumps(json.load(sys.stdin).get("cost_usd")))')
log "run.cost_usd (known/local model) = ${COST}"
[ "$COST" != "null" ] || die "cost_usd was null for a priced (local) model"

# Wait for one metrics export cycle (10s reader) + trace/log batches.
log "waiting for a metrics export cycle…"
sleep 13
kill -9 "$DPID" 2>/dev/null || true

# --- item C: metrics + logs + traces reached the sink ---------------------
[ -f "$WORK/otlp-metrics.bin" ] || die "no OTLP metrics POST received"
[ -f "$WORK/otlp-logs.bin" ]    || die "no OTLP logs POST received"
grep -qa "kelter.runs.started" "$WORK/otlp-metrics.bin" || die "metric kelter.runs.started not exported"
grep -qa "kelter.tool_calls"   "$WORK/otlp-metrics.bin" || die "metric kelter.tool_calls not exported"
# A log body from the run appears verbatim in the logs payload.
grep -qa "kelter" "$WORK/otlp-logs.bin" || die "no OTel logs exported"
log "sink captured metrics (kelter.*) + logs + $( [ -f "$WORK/otlp-traces.bin" ] && echo traces || echo 'no-traces')"

if [ -n "$EVIDENCE_DIR" ]; then
  mkdir -p "$EVIDENCE_DIR"
  scrub() { sed "s#${REPO_ROOT}#<repo>#g;s#${WORK}#<work>#g;s#${TOKEN}#<token>#g"; }
  {
    echo "=== OTLP export capture (item C) — bytes received per signal ==="
    for sig in traces metrics logs; do
      f="$WORK/otlp-$sig.bin"
      [ -f "$f" ] && echo "  $sig: $(wc -c < "$f" | tr -d ' ') bytes"
    done
    echo "=== metric instrument names found in the OTLP metrics payload ==="
    strings "$WORK/otlp-metrics.bin" | grep -E '^kelter\.' | sort -u
    echo "=== a sample of log/trace strings in the payloads ==="
    { strings "$WORK/otlp-logs.bin" 2>/dev/null | grep -iE 'kelter|run|resume' | head -6; }
  } | scrub > "$EVIDENCE_DIR/otel-metrics-logs-sample.log" || true
  printf '%s' "$RUNJSON" | scrub > "$EVIDENCE_DIR/cost-run-known.json"
fi

log "PASS: OTel metrics + logs exported (item C); run cost \$ exposed non-null for a priced model (item B)"
