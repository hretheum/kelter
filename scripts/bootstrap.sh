#!/usr/bin/env bash
# Kelter bootstrap: one command from a fresh clone to a first successful
# agent run, with the UI open at the end.
#
#   scripts/bootstrap.sh [--no-open]
#
# What it does: preflight checks → start the local llama.cpp sidecar
# (downloads a ~0.4 GB model on first run) → build → start the daemon →
# run the demo task end-to-end → print the UI URL + token.
set -euo pipefail

NO_OPEN=0
[ "${1:-}" = "--no-open" ] && NO_OPEN=1

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STATE_DIR="$REPO_ROOT/.bootstrap"
PORT=7466
MODEL_PORT=8080
MODEL="Qwen/Qwen2.5-0.5B-Instruct-GGUF:q4_k_m"
BASE="http://127.0.0.1:${PORT}"
START_TS=$SECONDS

say()  { printf '\033[1m[kelter]\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31m[kelter]\033[0m %s\n' "$*" >&2; exit 1; }

# ---------- preflight ------------------------------------------------------

command -v cargo >/dev/null 2>&1 || fail \
  "Rust toolchain not found. Install it first: https://rustup.rs (then re-run this script)."

command -v curl >/dev/null 2>&1 || fail "curl is required but not found."

if ! command -v llama-server >/dev/null 2>&1; then
  fail "llama-server (llama.cpp) not found — it runs the local model.
        macOS/Linux:  brew install llama.cpp
        other:        https://github.com/ggml-org/llama.cpp/blob/master/docs/install.md
        Then re-run this script."
fi

mkdir -p "$STATE_DIR"

# ---------- local model sidecar -------------------------------------------

if curl -fsS "http://127.0.0.1:${MODEL_PORT}/health" >/dev/null 2>&1; then
  say "llama-server already running on :${MODEL_PORT} — reusing it."
else
  say "starting llama-server on :${MODEL_PORT} (first run downloads the model, ~0.4 GB)…"
  nohup llama-server -hf "$MODEL" --host 127.0.0.1 --port "$MODEL_PORT" -c 4096 \
    > "$STATE_DIR/llama-server.log" 2>&1 &
  echo $! > "$STATE_DIR/llama-server.pid"
  for _ in $(seq 1 600); do
    curl -fsS "http://127.0.0.1:${MODEL_PORT}/health" >/dev/null 2>&1 && break
    sleep 1
  done
  curl -fsS "http://127.0.0.1:${MODEL_PORT}/health" >/dev/null 2>&1 || fail \
    "llama-server did not become healthy within 10 minutes.
        Check the log: $STATE_DIR/llama-server.log (slow network? disk full?)"
  say "model ready."
fi

# ---------- web UI (embedded into the daemon by the api crate) --------------

if [ ! -d "$REPO_ROOT/apps/web-ui/dist" ]; then
  command -v pnpm >/dev/null 2>&1 || fail "pnpm (with Node 22) is required to build the web UI,
        which the daemon embeds (apps/web-ui/dist is missing).
        Install Node 22 and pnpm (https://pnpm.io/installation), then re-run this script."
  say "building the web UI (log: $STATE_DIR/ui-build.log)…"
  { pnpm --dir "$REPO_ROOT/apps/web-ui" install --frozen-lockfile \
      && pnpm --dir "$REPO_ROOT/apps/web-ui" build; } \
    > "$STATE_DIR/ui-build.log" 2>&1 || fail \
    "web UI build failed — see $STATE_DIR/ui-build.log (needs Node 22 and pnpm)"
fi

# ---------- build ----------------------------------------------------------

say "building Kelter (first build takes a few minutes)…"
cargo build --workspace --manifest-path "$REPO_ROOT/Cargo.toml" \
  > "$STATE_DIR/build.log" 2>&1 || fail \
  "build failed — see $STATE_DIR/build.log"

# ---------- config + daemon ------------------------------------------------

CONFIG="$STATE_DIR/config.toml"
if [ ! -f "$CONFIG" ]; then
  TOKEN=$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')
  cat > "$CONFIG" <<EOF
[server]
bind = "127.0.0.1:${PORT}"
token = "${TOKEN}"

[policy]
engine = "local:qwen2.5-0.5b-instruct"

[kernel]
max_steps = 8
reflection = true

[trust]
allow_unsandboxed_subprocess = ["wordstats"]

[tools]
workspace = "${STATE_DIR}/work"
extension_dirs = ["${REPO_ROOT}/extensions"]
EOF
else
  TOKEN=$(sed -n 's/^token = "\(.*\)"/\1/p' "$CONFIG")
fi

if curl -fsS "$BASE/healthz" >/dev/null 2>&1; then
  say "a daemon is already listening on :${PORT} — reusing it."
else
  say "starting the Kelter daemon on :${PORT}…"
  nohup "$REPO_ROOT/target/debug/kelter" --config "$CONFIG" \
    > "$STATE_DIR/daemon.log" 2>&1 &
  echo $! > "$STATE_DIR/daemon.pid"
  for _ in $(seq 1 50); do
    curl -fsS "$BASE/healthz" >/dev/null 2>&1 && break
    sleep 0.3
  done
  curl -fsS "$BASE/healthz" >/dev/null 2>&1 || fail \
    "daemon did not come up — see $STATE_DIR/daemon.log"
fi

# ---------- first run ------------------------------------------------------

TASK="Read the file ${REPO_ROOT}/examples/sample.md and save it as raw.md. Then extract the structure of raw.md into structure.json. Then write a markdown report from structure.json to report.md. Then finish."

say "submitting the demo task…"
RUN_ID=$(curl -fsS -X POST "$BASE/api/v1/runs" \
  -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' \
  -d "{\"task\": \"${TASK}\"}" | sed -E 's/.*"run_id":"([^"]+)".*/\1/')

STATUS=""
for _ in $(seq 1 90); do
  BODY=$(curl -fsS "$BASE/api/v1/runs/$RUN_ID" -H "Authorization: Bearer ${TOKEN}")
  STATUS=$(printf '%s' "$BODY" | sed -E 's/.*"status":"([a-z]+)".*/\1/')
  { [ "$STATUS" = "completed" ] || [ "$STATUS" = "failed" ]; } && break
  sleep 2
done

if [ "$STATUS" != "completed" ]; then
  fail "the demo run did not complete (status: ${STATUS:-unknown}).
        Inspect: curl -H \"Authorization: Bearer \$TOKEN\" $BASE/api/v1/runs/$RUN_ID
        Daemon log: $STATE_DIR/daemon.log"
fi

ELAPSED=$((SECONDS - START_TS))
say "first run completed in ${ELAPSED}s total. Report: $STATE_DIR/work/report.md"
echo
printf '%s\n' "  UI:    ${BASE}/#token=${TOKEN}"
printf '%s\n' "  API:   ${BASE}/api/v1  (Authorization: Bearer ${TOKEN})"
printf '%s\n' "  stop:  kill \$(cat $STATE_DIR/daemon.pid) \$(cat $STATE_DIR/llama-server.pid 2>/dev/null)"
echo

if [ "$NO_OPEN" = 0 ] && command -v open >/dev/null 2>&1; then
  open "${BASE}/#token=${TOKEN}"
fi
say "done."
