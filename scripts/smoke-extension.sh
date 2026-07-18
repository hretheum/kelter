#!/usr/bin/env bash
# Smoke test for the "extend without Rust" proof (ADR 0006, first slice):
# a Python MCP tool registers with the runtime through its manifest — no
# recompilation — shows up in the action space, and gets used by the model
# in a live run.
#
#   scripts/smoke-extension.sh [evidence-dir]
#
# Needs: python3 (>=3.10); llama-server on :8080 for the model-driven part.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="${1:-}"
PORT=7531
TOKEN="smoke-ext-$$"
RUN_DIR="$(mktemp -d)"
BASE="http://127.0.0.1:${PORT}"

log() { printf '[smoke-extension] %s\n' "$*"; }
die() { printf '[smoke-extension] FAIL: %s\n' "$*" >&2; exit 1; }

PY_OK=0
for p in python3.13 python3.12 python3.11 python3.10 python3; do
  command -v "$p" >/dev/null 2>&1 || continue
  "$p" -c 'import sys; raise SystemExit(0 if sys.version_info >= (3,10) else 1)' 2>/dev/null \
    && { PY_OK=1; break; }
done
[ "$PY_OK" = 1 ] || die "no Python >= 3.10 found (the mcp SDK requires it) — e.g.: brew install python@3.12"

log "building workspace (kernel is NOT recompiled for the extension — this is the same binary)"
cargo build --workspace --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null

cat > "$RUN_DIR/config.toml" <<EOF
[server]
bind = "127.0.0.1:${PORT}"
token = "${TOKEN}"

[policy]
engine = "local:qwen2.5-0.5b-instruct"

[kernel]
max_steps = 6
reflection = true

[trust]
allow_unsandboxed_subprocess = ["wordstats"]

[tools]
workspace = "${RUN_DIR}/work"
# Only the extension: the runtime serves exactly the tools YOU registered.
bundled_tools = false
extension_dirs = ["${REPO_ROOT}/extensions"]
EOF

log "starting daemon with extension_dirs = [extensions/] (first start creates the extension's venv)"
(cd "$RUN_DIR" && exec "$REPO_ROOT/target/debug/kelter" --config config.toml \
  > "$RUN_DIR/daemon.log" 2>&1) &
DAEMON_PID=$!
trap 'kill "$DAEMON_PID" 2>/dev/null || true' EXIT

for _ in $(seq 1 200); do
  curl -fsS "$BASE/healthz" >/dev/null 2>&1 && break
  sleep 0.5
done
curl -fsS "$BASE/healthz" >/dev/null || die "daemon did not come up (see $RUN_DIR/daemon.log)"

log "assert 1: word_stats is in the live action space (GET /api/v1/tools)"
TOOLS=$(curl -fsS "$BASE/api/v1/tools" -H "Authorization: Bearer ${TOKEN}")
printf '%s\n' "$TOOLS" > "$RUN_DIR/tools.json"
printf '%s' "$TOOLS" | grep -q '"word_stats"' \
  || die "word_stats not registered — extension discovery failed. Tools: $TOOLS"
log "  registered: $(printf '%s' "$TOOLS" | python3 -c 'import json,sys; print(", ".join(t["name"] for t in json.load(sys.stdin)["tools"]))')"

if ! curl -fsS http://127.0.0.1:8080/health >/dev/null 2>&1; then
  log "llama-server not running — registration proven; skipping the model-driven part."
  log "PASS (registration only)"
  exit 0
fi

log "assert 2: the model actually uses the Python tool in a live run"
TASK="Use word_stats on the file ${REPO_ROOT}/examples/sample.md and save the statistics as stats.json. Then finish."
RUN_ID=$(curl -fsS -X POST "$BASE/api/v1/runs" \
  -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' \
  -d "{\"task\": \"${TASK}\"}" | sed -E 's/.*"run_id":"([^"]+)".*/\1/')

STATUS=""
for _ in $(seq 1 60); do
  BODY=$(curl -fsS "$BASE/api/v1/runs/$RUN_ID" -H "Authorization: Bearer ${TOKEN}")
  STATUS=$(printf '%s' "$BODY" | sed -E 's/.*"status":"([a-z]+)".*/\1/')
  { [ "$STATUS" = "completed" ] || [ "$STATUS" = "failed" ]; } && break
  sleep 2
done
printf '%s' "$BODY" > "$RUN_DIR/run.json"

python3 - "$RUN_DIR/run.json" <<'EOF'
import json, sys
r = json.load(open(sys.argv[1]))
for s in r["steps"]:
    print(f'  step {s["index"]}: {s["action"]} {json.dumps(s["arguments"])} ok={s["ok"]}')
print("  status:", r["status"])
EOF

[ "$STATUS" = "completed" ] || die "run did not complete (see $RUN_DIR/run.json)"

# The hard proof: a word_stats step executed OK and returned the extension's
# structured result ({words, unique, top5}) through MCP into the kernel's
# journal. (A weak model may later overwrite output files — the journaled
# step result is the authoritative evidence, not the final file.)
python3 - "$RUN_DIR/run.json" <<'EOF' || exit 1
import json, sys
r = json.load(open(sys.argv[1]))
steps = [s for s in r["steps"] if s["action"] == "word_stats" and s["ok"]]
if not steps:
    sys.exit("FAIL: no successful word_stats step in the run")
res = steps[0]["result"]
for key in ("words", "unique", "top5"):
    if key not in res:
        sys.exit(f"FAIL: word_stats result missing '{key}': {json.dumps(res)}")
print("  word_stats step result (from the Python extension, via MCP):")
print("  " + json.dumps(res))
EOF

if [ -n "$EVIDENCE_DIR" ]; then
  mkdir -p "$EVIDENCE_DIR"
  sed "s#${REPO_ROOT}#<repo>#g" "$RUN_DIR/run.json" > "$EVIDENCE_DIR/smoke-extension-run.json"
  sed "s#${REPO_ROOT}#<repo>#g" "$RUN_DIR/tools.json" > "$EVIDENCE_DIR/smoke-extension-tools.json"

fi

log "PASS: Python extension registered via manifest and used by the model — no kernel recompile"
