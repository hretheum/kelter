#!/usr/bin/env bash
# Plan mode, live governance leg (Phase 4c).
#
# What this proves LIVE with a small local model:
#   - the model proposes a plan (schema-constrained via GBNF — it emits a
#     structurally valid plan envelope);
#   - the kernel VALIDATES the plan and REJECTS an invalid one (a
#     self-referential dependency = a cycle) BEFORE executing any node —
#     nothing runs, no side effect.
#
# The durable-execution money shot (multi-node plan → crash → resume replays
# the journaled plan WITHOUT re-querying the model and does NOT re-run
# completed nodes, exactly-once side effects) is proven DETERMINISTICALLY in
# crates/kernel-core/tests/plan_test.rs — that harness is authoritative and
# stronger than a flaky live 0.5B run. See docs/BUILD-EVIDENCE.md §4c: live
# plan *proposal* on a 0.5B is slow/unreliable (constrained generation of a
# structured plan stresses the small model), which is exactly why the
# kernel-side validation gate — exercised here — is the guarantee.
#
#   scripts/smoke-plan.sh [evidence-dir]
#
# Needs llama-server on :8080.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="${1:-}"
PORT=7581
BASE="http://127.0.0.1:${PORT}"
TOKEN="smoke-plan-$$"

log() { printf '[smoke-plan] %s\n' "$*"; }
die() { printf '[smoke-plan] FAIL: %s\n' "$*" >&2; exit 1; }

curl -fsS http://127.0.0.1:8080/health >/dev/null 2>&1 \
  || die "llama-server is not running on :8080"

log "building workspace"
cargo build --workspace --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null

RUN_DIR="$(mktemp -d)"
cat > "$RUN_DIR/config.toml" <<EOF
[server]
bind = "127.0.0.1:${PORT}"
token = "${TOKEN}"

[policy]
engine = "local:qwen2.5-0.5b-instruct"

[kernel]
mode = "plan"
max_steps = 12

[storage]
backend = "sqlite"
path = "$RUN_DIR/kelter.db"

[tools]
workspace = "$RUN_DIR/work"
bundled_tools = false
extension_dirs = []

[[tools.servers]]
name = "ledger"
command = "${REPO_ROOT}/target/debug/ledger-server"
args = ["--workspace", "$RUN_DIR/work"]
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

# This task reliably drives the 0.5B to name the node after its action and
# make it depend on itself — a self-cycle the kernel must reject.
TASK="Make a plan with node append (tool, append_ledger, entry plan-hello) then review."
BODY=$(python3 -c 'import json,sys; print(json.dumps({"task": sys.argv[1]}))' "$TASK")

log "=== a real model proposes a plan; the kernel validates it ==="
REJECTED=0
COMPLETED=0
for i in $(seq 1 6); do
  rm -f "$RUN_DIR/work/ledger.txt"
  RUN=$(curl -fsS -X POST "$BASE/api/v1/runs" -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' -d "$BODY" | sed -E 's/.*"run_id":"([^"]+)".*/\1/')
  STATUS=""
  for _ in $(seq 1 40); do
    STATUS=$(curl -fsS "$BASE/api/v1/runs/$RUN" -H "Authorization: Bearer ${TOKEN}" \
      | sed -E 's/.*"status":"([a-z]+)".*/\1/')
    { [ "$STATUS" = "completed" ] || [ "$STATUS" = "failed" ]; } && break
    sleep 1
  done
  ERR=$(curl -fsS "$BASE/api/v1/runs/$RUN" -H "Authorization: Bearer ${TOKEN}" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin).get("error") or "")')
  log "  try $i: $STATUS ${ERR:+— $ERR}"
  if printf '%s' "$ERR" | grep -q "plan rejected"; then
    REJECTED=$((REJECTED + 1))
    # An invalid plan is rejected BEFORE execution: no side effect.
    [ ! -f "$RUN_DIR/work/ledger.txt" ] \
      || die "an invalid plan produced a side effect — validation is not a pre-execution gate!"
  fi
  [ "$STATUS" = "completed" ] && COMPLETED=$((COMPLETED + 1))
done

[ "$REJECTED" -ge 1 ] \
  || die "expected at least one plan to be rejected by validation, saw none in 6 tries"
log "PASS: $REJECTED/6 invalid plans rejected BEFORE any node executed (governance gate live)"
if [ "$COMPLETED" -ge 1 ]; then
  log "  ($COMPLETED/6 valid plans also executed to completion)"
fi

if [ -n "$EVIDENCE_DIR" ]; then
  mkdir -p "$EVIDENCE_DIR"
  grep -E 'plan rejected|plan_node|plan completed' "$RUN_DIR/daemon.log" \
    | sed "s#${REPO_ROOT}#<repo>#g;s#${RUN_DIR}#<run>#g" > "$EVIDENCE_DIR/plan-validation.log" || true
fi

log "PASS: plan mode — model proposes, kernel validates and rejects invalid plans pre-execution"
log "NOTE: durable plan execution + crash-resume money shot is proven in"
log "      crates/kernel-core/tests/plan_test.rs (deterministic; see BUILD-EVIDENCE §4c)"
