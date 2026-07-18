#!/usr/bin/env bash
# Smoke for memory-as-a-registered-extension (Phase 4a):
#   1) a Python memory backend (~30 lines, examples/extensions/memstore)
#      registers via its manifest and the daemon routes the kernel's memory
#      seam to it — no recompilation;
#   2) a real model run writes a reflection episode THROUGH the extension,
#      and a second run retrieves that context back from it;
#   3) governance: a manifest declaring an unimplemented capability makes
#      startup fail loudly.
#
#   scripts/smoke-memory-extension.sh [evidence-dir]
#
# Needs: python3 >= 3.10; llama-server on :8080.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="${1:-}"
PORT=7561
BASE="http://127.0.0.1:${PORT}"
TOKEN="smoke-mem-$$"
RUN_DIR="$(mktemp -d)"

log() { printf '[smoke-memory-ext] %s\n' "$*"; }
die() { printf '[smoke-memory-ext] FAIL: %s\n' "$*" >&2; exit 1; }

curl -fsS http://127.0.0.1:8080/health >/dev/null 2>&1 \
  || die "llama-server is not running on :8080 — start it first:
       llama-server -hf Qwen/Qwen2.5-0.5B-Instruct-GGUF:q4_k_m --port 8080 -c 4096"

log "building workspace (kernel is NOT recompiled for the extension)"
cargo build --workspace --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null

# Fresh backend state for determinism.
rm -f "$REPO_ROOT/examples/extensions/memstore/memstore.json"

cat > "$RUN_DIR/config.toml" <<EOF
[server]
bind = "127.0.0.1:${PORT}"
token = "${TOKEN}"

[policy]
engine = "local:qwen2.5-0.5b-instruct"

[kernel]
max_steps = 4
reflection = true

[storage]
backend = "sqlite"
path = "$RUN_DIR/kelter.db"

[tools]
workspace = "$RUN_DIR/work"
bundled_tools = false
extension_dirs = ["${REPO_ROOT}/examples/extensions"]

[[tools.servers]]
name = "ledger"
command = "${REPO_ROOT}/target/debug/ledger-server"
args = ["--workspace", "$RUN_DIR/work"]
EOF

DAEMON_PID=""
start_daemon() {
  ( cd "$RUN_DIR" && RUST_LOG=info \
    exec "$REPO_ROOT/target/debug/kelter" --config config.toml \
    > "$RUN_DIR/daemon.log" 2>&1 ) &
  DAEMON_PID=$!
  for _ in $(seq 1 120); do
    curl -fsS "$BASE/healthz" >/dev/null 2>&1 && return 0
    kill -0 "$DAEMON_PID" 2>/dev/null || return 1
    sleep 0.5
  done
  return 1
}
trap 'kill "$DAEMON_PID" 2>/dev/null || true' EXIT

log "starting daemon (first start creates the memstore venv)…"
start_daemon || die "daemon did not come up (see $RUN_DIR/daemon.log)"

log "assert 1: kernel memory routed to the Python extension"
grep -q "memory: extension 'memstore' (external backend)" "$RUN_DIR/daemon.log" \
  || die "daemon did not register memstore as the memory backend"

run_task() { # $1 task → status
  local run_id status="" body
  run_id=$(curl -fsS -X POST "$BASE/api/v1/runs" \
    -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' \
    -d "{\"task\": \"$1\"}" | sed -E 's/.*"run_id":"([^"]+)".*/\1/')
  for _ in $(seq 1 45); do
    body=$(curl -fsS "$BASE/api/v1/runs/$run_id" -H "Authorization: Bearer ${TOKEN}")
    status=$(printf '%s' "$body" | sed -E 's/.*"status":"([a-z]+)".*/\1/')
    { [ "$status" = "completed" ] || [ "$status" = "failed" ]; } && break
    sleep 2
  done
  printf '%s' "$status"
}

log "assert 2: a real run's reflection is WRITTEN through the extension"
STATUS=$(run_task "Append the entry 'memory-proof' to the ledger using append_ledger. Then finish.")
[ "$STATUS" = "completed" ] || die "run 1 did not complete ($STATUS)"
MEMFILE="$REPO_ROOT/examples/extensions/memstore/memstore.json"
[ -f "$MEMFILE" ] || die "extension backend file was never created"
grep -q "memory-proof" "$MEMFILE" \
  || die "reflection episode did not reach the Python backend"
log "  episode stored by the Python backend: $(python3 -c "import json;d=json.load(open('$MEMFILE'));print(len(d['episodes']),'episode(s)')")"

log "assert 3: a second run RETRIEVES that context back from the extension"
STATUS=$(run_task "Append the entry 'memory-proof-again' to the ledger using append_ledger. Then finish.")
[ "$STATUS" = "completed" ] || die "run 2 did not complete ($STATUS)"
grep -q "memory: retrieved context from the registered provider" "$RUN_DIR/daemon.log" \
  || die "kernel never retrieved context from the extension"

kill "$DAEMON_PID" 2>/dev/null || true; wait "$DAEMON_PID" 2>/dev/null || true

log "assert 4: unimplemented capability in a manifest fails startup loudly"
BAD_DIR="$(mktemp -d)"
mkdir -p "$BAD_DIR/exts/futuristic"
cat > "$BAD_DIR/exts/futuristic/kelter-extension.toml" <<EOF
[extension]
name = "futuristic"
provides = ["agent"]

[server]
command = "true"
EOF
sed "s#extension_dirs = .*#extension_dirs = [\"$BAD_DIR/exts\"]#" "$RUN_DIR/config.toml" \
  > "$BAD_DIR/config.toml"
set +e
( cd "$BAD_DIR" && "$REPO_ROOT/target/debug/kelter" --config config.toml \
  > "$BAD_DIR/daemon.log" 2>&1 )
RC=$?
set -e
[ "$RC" -ne 0 ] || die "daemon should have refused to start"
grep -q "does not implement yet" "$BAD_DIR/daemon.log" \
  || die "expected a loud unimplemented-capability error, got: $(tail -2 "$BAD_DIR/daemon.log")"
log "  startup refused with: $(grep -o 'extension .futuristic. declares capability .agent..*' "$BAD_DIR/daemon.log" | head -1 | cut -c1-90)…"

if [ -n "$EVIDENCE_DIR" ]; then
  mkdir -p "$EVIDENCE_DIR"
  sed "s#${REPO_ROOT}#<repo>#g;s#${RUN_DIR}#<run>#g" "$RUN_DIR/daemon.log" \
    > "$EVIDENCE_DIR/memory-extension-daemon.log"
  sed "s#${REPO_ROOT}#<repo>#g;s#${BAD_DIR}#<bad>#g" "$BAD_DIR/daemon.log" \
    > "$EVIDENCE_DIR/memory-extension-badmanifest.log"
  sed "s#${REPO_ROOT}#<repo>#g" "$MEMFILE" > "$EVIDENCE_DIR/memory-extension-store.json"
fi

log "PASS: Python memory backend registered via manifest, written to and retrieved from by real runs — no kernel recompile"
