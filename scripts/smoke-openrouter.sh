#!/usr/bin/env bash
# Smoke test, frontier leg: the SAME wedge task through the openrouter
# adapter. The only config difference vs smoke-local.sh is the
# [policy] engine line — that is the point.
#
# Requires OPENROUTER_API_KEY in the environment. Without it this script
# reports SKIPPED and exits 0 (the adapter itself is covered by unit tests
# on mocked HTTP: crates/policy/tests/openrouter_mock_test.rs).
#
# Usage: scripts/smoke-openrouter.sh [evidence-dir] [model]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="${1:-}"
MODEL="${2:-anthropic/claude-sonnet-5}"
PORT=7512
TOKEN="smoke-or-$(date +%s)"
RUN_DIR="$(mktemp -d)"
BASE="http://127.0.0.1:${PORT}"

log() { printf '[smoke-openrouter] %s\n' "$*"; }
die() { printf '[smoke-openrouter] FAIL: %s\n' "$*" >&2; exit 1; }

if [ -z "${OPENROUTER_API_KEY:-}" ]; then
  log "SKIPPED: OPENROUTER_API_KEY is not set."
  log "To run the live frontier leg: OPENROUTER_API_KEY=... $0"
  [ -n "$EVIDENCE_DIR" ] && {
    mkdir -p "$EVIDENCE_DIR"
    printf 'skipped: OPENROUTER_API_KEY not present in this environment (%s)\n' \
      "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$EVIDENCE_DIR/smoke-openrouter-SKIPPED.log"
  }
  exit 0
fi

log "building workspace"
cargo build --workspace --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null

cat > "$RUN_DIR/config.toml" <<EOF
[server]
bind = "127.0.0.1:${PORT}"
token = "${TOKEN}"

[policy]
engine = "openrouter:${MODEL}"

[kernel]
max_steps = 8
reflection = true

[tools]
workspace = "${RUN_DIR}/work"
EOF

log "config diff vs local leg (the one-line swap):"
diff <(grep '^engine' "$REPO_ROOT/examples/config-local.toml") \
     <(grep '^engine' "$RUN_DIR/config.toml") || true

log "starting daemon on :${PORT}"
(cd "$RUN_DIR" && "$REPO_ROOT/target/debug/agentkernel" --config config.toml \
  > "$RUN_DIR/daemon.log" 2>&1) &
DAEMON_PID=$!
trap 'kill "$DAEMON_PID" 2>/dev/null || true' EXIT

for _ in $(seq 1 50); do
  curl -fsS "$BASE/healthz" >/dev/null 2>&1 && break
  sleep 0.2
done
curl -fsS "$BASE/healthz" >/dev/null || die "daemon did not come up"

TASK="Read the file ${REPO_ROOT}/examples/sample.md and save it as raw.md. Then extract the structure of raw.md into structure.json. Then write a markdown report from structure.json to report.md. Then finish."

log "submitting wedge task (model: ${MODEL})"
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
log "final status: $STATUS"
python3 - "$RUN_DIR/run.json" <<'EOF'
import json, sys
r = json.load(open(sys.argv[1]))
for s in r["steps"]:
    print(f'  step {s["index"]}: {s["action"]} {json.dumps(s["arguments"])} ok={s["ok"]}')
print("  output:", json.dumps(r.get("output")))
EOF

[ "$STATUS" = "completed" ] || die "run did not complete (see $RUN_DIR)"
[ -f "$RUN_DIR/work/report.md" ] || die "report.md was not produced"

if [ -n "$EVIDENCE_DIR" ]; then
  mkdir -p "$EVIDENCE_DIR"
  # Sanitize machine-specific paths out of committed evidence.
  sed "s#${REPO_ROOT}#<repo>#g" "$RUN_DIR/run.json" \
    > "$EVIDENCE_DIR/smoke-openrouter-run.json"
  sed "s#${REPO_ROOT}#<repo>#g" "$RUN_DIR/work/report.md" \
    > "$EVIDENCE_DIR/smoke-openrouter-report.md"
  log "evidence copied to $EVIDENCE_DIR"
fi

log "PASS: frontier leg completed end-to-end"
