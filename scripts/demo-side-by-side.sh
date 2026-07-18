#!/usr/bin/env bash
# The Kelter money shot, reproducibly: the SAME multi-step agent task run
# twice — once on a small local model (llama.cpp + GBNF constrained
# decoding), once on a frontier model via OpenRouter — with the engine
# selected by ONE config line.
#
#   scripts/demo-side-by-side.sh [evidence-dir]
#
# Local leg needs llama-server on :8080. Frontier leg runs when
# OPENROUTER_API_KEY is set (model override: KELTER_DEMO_FRONTIER_MODEL,
# default anthropic/claude-sonnet-5); otherwise it is reported as skipped.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="${1:-}"
FRONTIER_MODEL="${KELTER_DEMO_FRONTIER_MODEL:-anthropic/claude-sonnet-5}"

say() { printf '\033[1m[demo]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[demo]\033[0m %s\n' "$*" >&2; exit 1; }

command -v cargo >/dev/null || die "cargo not found"
say "building…"
cargo build --workspace --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null

TASK="Read the file ${REPO_ROOT}/examples/sample.md and save it as raw.md. Then extract the structure of raw.md into structure.json. Then write a markdown report from structure.json to report.md. Then finish."

# run_leg <label> <engine> <port>  → writes $RUN_DIR/<label>.json, prints table
run_leg() {
  local label="$1" engine="$2" port="$3"
  local run_dir base token status body t0 t1
  run_dir="$(mktemp -d)"
  base="http://127.0.0.1:${port}"
  token="demo-${label}-$$"

  cat > "$run_dir/config.toml" <<EOF
[server]
bind = "127.0.0.1:${port}"
token = "${token}"

[policy]
engine = "${engine}"

[kernel]
max_steps = 8
reflection = true

[tools]
workspace = "${run_dir}/work"
extension_dirs = ["${REPO_ROOT}/extensions"]
EOF

  (cd "$run_dir" && exec "$REPO_ROOT/target/debug/kelter" --config config.toml \
    > "$run_dir/daemon.log" 2>&1) &
  local pid=$!
  for _ in $(seq 1 50); do
    curl -fsS "$base/healthz" >/dev/null 2>&1 && break; sleep 0.2
  done
  curl -fsS "$base/healthz" >/dev/null || { kill $pid 2>/dev/null; die "daemon for $label did not start (see $run_dir/daemon.log)"; }

  t0=$(date +%s)
  local run_id
  run_id=$(curl -fsS -X POST "$base/api/v1/runs" \
    -H "Authorization: Bearer ${token}" -H 'Content-Type: application/json' \
    -d "{\"task\": \"${TASK}\"}" | sed -E 's/.*"run_id":"([^"]+)".*/\1/')

  status=""
  for _ in $(seq 1 90); do
    body=$(curl -fsS "$base/api/v1/runs/$run_id" -H "Authorization: Bearer ${token}")
    status=$(printf '%s' "$body" | sed -E 's/.*"status":"([a-z]+)".*/\1/')
    { [ "$status" = "completed" ] || [ "$status" = "failed" ]; } && break
    sleep 2
  done
  t1=$(date +%s)

  kill $pid 2>/dev/null || true
  printf '%s' "$body" > "$run_dir/run.json"

  echo
  say "── $label  (engine = \"$engine\")  →  $status in $((t1 - t0))s"
  python3 - "$run_dir/run.json" <<'EOF'
import json, sys
r = json.load(open(sys.argv[1]))
for s in r["steps"]:
    print(f'   step {s["index"]}: {s["action"]} {json.dumps(s["arguments"])} ok={s["ok"]}')
print("   output:", json.dumps(r.get("output")))
EOF

  [ "$status" = "completed" ] || die "$label leg failed (see $run_dir)"

  if [ -n "$EVIDENCE_DIR" ]; then
    mkdir -p "$EVIDENCE_DIR"
    sed "s#${REPO_ROOT}#<repo>#g" "$run_dir/run.json" > "$EVIDENCE_DIR/demo-${label}-run.json"
  fi
}

say "the one-line swap (this is the whole demo):"
diff <(grep '^engine' "$REPO_ROOT/examples/config-local.toml") \
     <(grep '^engine' "$REPO_ROOT/examples/config-openrouter.toml") || true

curl -fsS http://127.0.0.1:8080/health >/dev/null 2>&1 \
  || die "llama-server is not running on :8080 — start it first:
       llama-server -hf Qwen/Qwen2.5-0.5B-Instruct-GGUF:q4_k_m --port 8080 -c 4096"

run_leg "local" "local:qwen2.5-0.5b-instruct" 7521

if [ -n "${OPENROUTER_API_KEY:-}" ]; then
  run_leg "frontier" "openrouter:${FRONTIER_MODEL}" 7522
else
  echo
  say "── frontier leg SKIPPED: OPENROUTER_API_KEY is not set."
  say "   To run it: OPENROUTER_API_KEY=... $0"
  if [ -n "$EVIDENCE_DIR" ]; then
    mkdir -p "$EVIDENCE_DIR"
    printf 'skipped: OPENROUTER_API_KEY not present (%s)\n' \
      "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$EVIDENCE_DIR/demo-frontier-SKIPPED.log"
  fi
fi

echo
say "same kernel, same tools, same task — the engine is one config line."
