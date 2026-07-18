#!/usr/bin/env bash
# WASM extension mechanism smoke (Phase 5A):
#   1) a WASM tool (word_count, compiled from Rust) registers via a manifest
#      with mechanism = "wasm" and runs IN-PROCESS through wasmtime — the
#      daemon spawns no subprocess for it; a real model uses it in a run;
#   2) SANDBOX MONEY SHOT: a WASM module that imports a network capability it
#      was NOT granted is STRUCTURALLY blocked (wasmtime refuses to
#      instantiate it — the import is undefined), not "blocked at runtime";
#   3) governance: the WASM tool's output flows into the run through the same
#      kernel-side validation as any tool.
#
#   scripts/smoke-wasm.sh [evidence-dir]
#
# Needs llama-server on :8080. The demo .wasm modules are checked in; rebuild
# with scripts/build-wasm-demo.sh after editing the guest source.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="${1:-}"
PORT=7601
BASE="http://127.0.0.1:${PORT}"
TOKEN="smoke-wasm-$$"
EXT_DIR="$REPO_ROOT/examples/extensions/wordcount-wasm"

log() { printf '[smoke-wasm] %s\n' "$*"; }
die() { printf '[smoke-wasm] FAIL: %s\n' "$*" >&2; exit 1; }

curl -fsS http://127.0.0.1:8080/health >/dev/null 2>&1 \
  || die "llama-server is not running on :8080"

[ -f "$EXT_DIR/wordcount_wasm.wasm" ] \
  || die "demo wasm missing — run scripts/build-wasm-demo.sh"

log "building workspace"
cargo build --workspace --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null

RUN_DIR="$(mktemp -d)"
# Isolate: an extension dir containing ONLY the wordcount-wasm manifest.
EXTS="$RUN_DIR/exts"
mkdir -p "$EXTS/wordcount-wasm"
cp "$EXT_DIR"/*.wasm "$EXT_DIR/kelter-extension.toml" "$EXTS/wordcount-wasm/"

cat > "$RUN_DIR/config.toml" <<EOF
[server]
bind = "127.0.0.1:${PORT}"
token = "${TOKEN}"

[policy]
engine = "local:qwen2.5-0.5b-instruct"

[kernel]
max_steps = 4

[storage]
backend = "memory"

[tools]
workspace = "$RUN_DIR/work"
bundled_tools = false
extension_dirs = ["$EXTS"]
EOF

DAEMON_PID=""
( cd "$RUN_DIR" && RUST_LOG=info exec "$REPO_ROOT/target/debug/kelter" --config config.toml \
  > "$RUN_DIR/daemon.log" 2>&1 ) &
DAEMON_PID=$!
trap 'kill "$DAEMON_PID" 2>/dev/null || true' EXIT
for _ in $(seq 1 60); do
  curl -fsS "$BASE/healthz" >/dev/null 2>&1 && break
  kill -0 "$DAEMON_PID" 2>/dev/null || die "daemon died (see $RUN_DIR/daemon.log)"
  sleep 0.3
done

log "assert 1: WASM tool registered in-process (no subprocess spawned)"
grep -q "WASM extension 'wordcount-wasm' (in-process, sandboxed" "$RUN_DIR/daemon.log" \
  || die "wasm extension was not registered in-process"
TOOLS=$(curl -fsS "$BASE/api/v1/tools" -H "Authorization: Bearer ${TOKEN}")
printf '%s' "$TOOLS" | grep -q '"word_count"' \
  || die "word_count not in the action space: $TOOLS"
log "  action space: $(printf '%s' "$TOOLS" | python3 -c 'import json,sys; print(", ".join(t["name"] for t in json.load(sys.stdin)["tools"]))')"

log "assert 2: the model uses the WASM tool in a real run"
TASK="Use word_count with input set to hello world from wasm. Then finish."
BODY=$(python3 -c 'import json,sys; print(json.dumps({"task": sys.argv[1]}))' "$TASK")
STATUS=""
for _ in 1 2 3; do
  RUN=$(curl -fsS -X POST "$BASE/api/v1/runs" -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' -d "$BODY" | sed -E 's/.*"run_id":"([^"]+)".*/\1/')
  for _ in $(seq 1 40); do
    STATUS=$(curl -fsS "$BASE/api/v1/runs/$RUN" -H "Authorization: Bearer ${TOKEN}" \
      | sed -E 's/.*"status":"([a-z]+)".*/\1/')
    { [ "$STATUS" = "completed" ] || [ "$STATUS" = "failed" ]; } && break
    sleep 1
  done
  [ "$STATUS" = "completed" ] && break
done
RUNJSON=$(curl -fsS "$BASE/api/v1/runs/$RUN" -H "Authorization: Bearer ${TOKEN}")
[ "$STATUS" = "completed" ] || die "wasm-tool run did not complete ($STATUS): $RUNJSON"
printf '%s' "$RUNJSON" | python3 -c '
import json,sys
r = json.load(sys.stdin)
steps = [s for s in r["steps"] if s["action"] == "word_count" and s["ok"]]
assert steps, "no successful word_count step"
res = steps[0]["result"]
assert "words" in res and "chars" in res, res
print("  word_count result (from the WASM module, in-process):", json.dumps(res))
' || die "the WASM tool did not produce a valid result in the run"

kill "$DAEMON_PID" 2>/dev/null || true; wait "$DAEMON_PID" 2>/dev/null || true

# ---- 3: SANDBOX MONEY SHOT — ungranted capability structurally blocked ---
log "=== sandbox: a WASM module wanting the network WITHOUT the grant ==="
BAD="$RUN_DIR/bad/netpeek-wasm"
mkdir -p "$BAD"
cp "$EXT_DIR/netpeek_wasm.wasm" "$BAD/"
cat > "$BAD/kelter-extension.toml" <<EOF
[extension]
name = "netpeek-wasm"
description = "probes the network — should be sandboxed"
provides = ["tools"]
mechanism = "wasm"
# NO [permissions] network — the module cannot get the capability.

[wasm]
module = "netpeek_wasm.wasm"
tool = "net_peek"
EOF
cat > "$RUN_DIR/bad-config.toml" <<EOF
[server]
bind = "127.0.0.1:${PORT}"
token = "${TOKEN}"
[policy]
engine = "local:qwen2.5-0.5b-instruct"
[kernel]
max_steps = 3
[storage]
backend = "memory"
[tools]
workspace = "$RUN_DIR/work2"
bundled_tools = false
extension_dirs = ["$RUN_DIR/bad"]
EOF
( cd "$RUN_DIR" && RUST_LOG=info exec "$REPO_ROOT/target/debug/kelter" --config bad-config.toml \
  > "$RUN_DIR/bad-daemon.log" 2>&1 ) &
DAEMON_PID=$!
for _ in $(seq 1 60); do
  curl -fsS "$BASE/healthz" >/dev/null 2>&1 && break
  kill -0 "$DAEMON_PID" 2>/dev/null || die "bad daemon died"
  sleep 0.3
done
# The daemon starts (the module loads); the sandbox bites at INSTANTIATION,
# i.e. when the tool is actually called. Drive the tool and assert failure.
TASK2="Use net_peek with input set to example.com. Then finish."
BODY2=$(python3 -c 'import json,sys; print(json.dumps({"task": sys.argv[1]}))' "$TASK2")
RUN2=$(curl -fsS -X POST "$BASE/api/v1/runs" -H "Authorization: Bearer ${TOKEN}" \
  -H 'Content-Type: application/json' -d "$BODY2" | sed -E 's/.*"run_id":"([^"]+)".*/\1/')
for _ in $(seq 1 40); do
  S2=$(curl -fsS "$BASE/api/v1/runs/$RUN2" -H "Authorization: Bearer ${TOKEN}" \
    | sed -E 's/.*"status":"([a-z]+)".*/\1/')
  { [ "$S2" = "completed" ] || [ "$S2" = "failed" ]; } && break
  sleep 1
done
RUN2JSON=$(curl -fsS "$BASE/api/v1/runs/$RUN2" -H "Authorization: Bearer ${TOKEN}")
# Every net_peek step must have failed with a structural instantiation error.
printf '%s' "$RUN2JSON" | python3 -c '
import json,sys
r = json.load(sys.stdin)
peeks = [s for s in r["steps"] if s["action"] == "net_peek"]
assert peeks, "the model never called net_peek — inconclusive"
for s in peeks:
    assert not s["ok"], f"net_peek SUCCEEDED without the network grant — sandbox BREACH: {s}"
    err = json.dumps(s["result"])
    assert ("instantiated" in err) or ("import" in err), f"unexpected failure (not a capability block): {err}"
print("  net_peek structurally blocked:", peeks[0]["result"].get("error","")[:110])
' || die "sandbox proof failed — an ungranted capability was NOT structurally blocked"
log "PASS sandbox: ungranted network capability is structurally unreachable (wasmtime refused to instantiate)"
kill "$DAEMON_PID" 2>/dev/null || true; wait "$DAEMON_PID" 2>/dev/null || true

if [ -n "$EVIDENCE_DIR" ]; then
  mkdir -p "$EVIDENCE_DIR"
  scrub() { sed "s#${REPO_ROOT}#<repo>#g;s#${RUN_DIR}#<run>#g"; }
  printf '%s' "$RUNJSON" | scrub > "$EVIDENCE_DIR/wasm-tool-run.json"
  printf '%s' "$RUN2JSON" | scrub > "$EVIDENCE_DIR/wasm-sandbox-run.json"
  grep -E 'WASM extension|wasm' "$RUN_DIR/daemon.log" | scrub > "$EVIDENCE_DIR/wasm-registration.log" || true
fi

log "PASS: WASM mechanism — in-process sandboxed tool works; ungranted capability structurally blocked"
