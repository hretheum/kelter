#!/usr/bin/env bash
# Smoke test, local leg: the demo slice end-to-end on a local llama.cpp
# model with GBNF constrained decoding.
#
# Prereq: llama-server running on 127.0.0.1:8080, e.g.:
#   llama-server -hf Qwen/Qwen2.5-0.5B-Instruct-GGUF:q4_k_m --port 8080 -c 4096
#
# Usage: scripts/smoke-local.sh [evidence-dir]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="${1:-}"
PORT=7511
TOKEN="smoke-$(date +%s)"
RUN_DIR="$(mktemp -d)"
BASE="http://127.0.0.1:${PORT}"

log() { printf '[smoke-local] %s\n' "$*"; }
die() { printf '[smoke-local] FAIL: %s\n' "$*" >&2; exit 1; }

curl -fsS http://127.0.0.1:8080/health >/dev/null 2>&1 \
  || die "llama-server is not running on :8080 (see header of this script)"

log "building workspace"
cargo build --workspace --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null

cat > "$RUN_DIR/config.toml" <<EOF
[server]
bind = "127.0.0.1:${PORT}"
token = "${TOKEN}"

[policy]
engine = "local:qwen2.5-0.5b-instruct"

[kernel]
max_steps = 8
reflection = true

[tools]
workspace = "${RUN_DIR}/work"
EOF

log "starting daemon on :${PORT}"
(cd "$RUN_DIR" && exec "$REPO_ROOT/target/debug/kelter" --config config.toml \
  > "$RUN_DIR/daemon.log" 2>&1) &
DAEMON_PID=$!
trap 'kill "$DAEMON_PID" 2>/dev/null || true' EXIT

for _ in $(seq 1 50); do
  curl -fsS "$BASE/healthz" >/dev/null 2>&1 && break
  sleep 0.2
done
curl -fsS "$BASE/healthz" >/dev/null || die "daemon did not come up"

# Capture the WebSocket event stream during the run (proof of live
# streaming), if node with a WebSocket client is available.
WS_LOG="$RUN_DIR/events.ndjson"
if node -e 'new WebSocket("ws://x")' >/dev/null 2>&1 || node -e 'WebSocket' >/dev/null 2>&1; then
  node -e "
    const ws = new WebSocket('ws://127.0.0.1:${PORT}/api/v1/events?token=${TOKEN}');
    ws.onmessage = (m) => console.log(m.data);
    setTimeout(() => process.exit(0), 120000);
  " > "$WS_LOG" 2>/dev/null &
  WS_PID=$!
else
  WS_PID=""
  log "node WebSocket client unavailable — skipping WS capture"
fi

TASK="Read the file ${REPO_ROOT}/examples/sample.md and save it as raw.md. Then extract the structure of raw.md into structure.json. Then write a markdown report from structure.json to report.md. Then finish."

log "submitting wedge task"
RUN_ID=$(curl -fsS -X POST "$BASE/api/v1/runs" \
  -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' \
  -d "{\"task\": \"${TASK}\"}" | sed -E 's/.*"run_id":"([^"]+)".*/\1/')
log "run: $RUN_ID"

STATUS=""
for _ in $(seq 1 90); do
  BODY=$(curl -fsS "$BASE/api/v1/runs/$RUN_ID" -H "Authorization: Bearer ${TOKEN}")
  STATUS=$(printf '%s' "$BODY" | sed -E 's/.*"status":"([a-z]+)".*/\1/')
  [ "$STATUS" = "completed" ] || [ "$STATUS" = "failed" ] && break
  sleep 2
done

printf '%s' "$BODY" > "$RUN_DIR/run.json"
[ -n "$WS_PID" ] && { sleep 1; kill "$WS_PID" 2>/dev/null || true; }

log "final status: $STATUS"
echo "--- steps ---"
python3 - "$RUN_DIR/run.json" <<'EOF'
import json, sys
r = json.load(open(sys.argv[1]))
for s in r["steps"]:
    print(f'  step {s["index"]}: {s["action"]} {json.dumps(s["arguments"])} ok={s["ok"]}')
print("  output:", json.dumps(r.get("output")))
EOF

[ "$STATUS" = "completed" ] || die "run did not complete (see $RUN_DIR/run.json, $RUN_DIR/daemon.log)"
[ -f "$RUN_DIR/work/report.md" ] || die "report.md was not produced"
grep -q '^# Report:' "$RUN_DIR/work/report.md" || die "report.md has unexpected content"

echo "--- report.md ---"
cat "$RUN_DIR/work/report.md"

if [ -n "$EVIDENCE_DIR" ]; then
  mkdir -p "$EVIDENCE_DIR"
  # Sanitize machine-specific paths out of committed evidence.
  scrub() { sed "s#${REPO_ROOT}#<repo>#g" "$1" > "$2"; }
  scrub "$RUN_DIR/run.json" "$EVIDENCE_DIR/smoke-local-run.json"
  scrub "$RUN_DIR/work/report.md" "$EVIDENCE_DIR/smoke-local-report.md"
  [ -f "$WS_LOG" ] && scrub "$WS_LOG" "$EVIDENCE_DIR/smoke-local-events.ndjson"
  scrub "$RUN_DIR/daemon.log" "$EVIDENCE_DIR/smoke-local-daemon.log"
  log "evidence copied to $EVIDENCE_DIR"
fi

log "PASS: local leg completed end-to-end"
