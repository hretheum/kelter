#!/usr/bin/env bash
# Governed, durable outbound A2A smoke (Phase 4b):
#   1) a remote A2A agent registers via manifest (provides=["agent"]) and
#      the kernel exposes delegate_to_toy — no subprocess spawned;
#   2) a real model run delegates a sub-task; the answer comes back as a
#      validated observation journaled with the agent's identity;
#   3) MONEY SHOT: kill Kelter mid-poll (after the remote task was created +
#      journaled); resume RECONNECTS to the task by id (GetTask), no second
#      delegation;
#   4) governance: an agent manifest without `network` is refused at startup.
#
#   scripts/smoke-a2a.sh [evidence-dir]
#
# Needs: python3 >= 3.10 (toy agent deps auto-installed); llama-server :8080.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="${1:-}"
PORT=7571
AGENT_PORT=7590
BASE="http://127.0.0.1:${PORT}"
TOKEN="smoke-a2a-$$"

log() { printf '[smoke-a2a] %s\n' "$*"; }
die() { printf '[smoke-a2a] FAIL: %s\n' "$*" >&2; exit 1; }

curl -fsS http://127.0.0.1:8080/health >/dev/null 2>&1 \
  || die "llama-server is not running on :8080"

log "building workspace"
cargo build --workspace --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null

# ---- toy A2A agent (delegation target), slow enough to kill mid-poll -----
AGENT_DIR="$REPO_ROOT/examples/a2a-agent"
chmod +x "$AGENT_DIR/run.sh"
log "starting toy A2A agent on :${AGENT_PORT} (first run installs starlette/uvicorn)"
( "$AGENT_DIR/run.sh" --port "$AGENT_PORT" --slow 6 > "$AGENT_DIR/agent.log" 2>&1 ) &
AGENT_PID=$!
trap 'kill "$AGENT_PID" "${DAEMON_PID:-}" 2>/dev/null || true' EXIT
for _ in $(seq 1 120); do
  curl -fsS "http://127.0.0.1:${AGENT_PORT}/.well-known/agent-card.json" >/dev/null 2>&1 && break
  kill -0 "$AGENT_PID" 2>/dev/null || die "toy agent died (see $AGENT_DIR/agent.log)"
  sleep 0.5
done
log "toy agent card resolvable"

RUN_DIR="$(mktemp -d)"
write_config() {
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
extension_dirs = ["${REPO_ROOT}/examples/extensions/toy-agent"]

[[tools.servers]]
name = "ledger"
command = "${REPO_ROOT}/target/debug/ledger-server"
args = ["--workspace", "$RUN_DIR/work"]
EOF
}
write_config

DAEMON_PID=""
start_daemon() { # [$1 crash point]
  ( cd "$RUN_DIR" && KELTER_CRASH_POINT="${1:-}" RUST_LOG=info \
    exec "$REPO_ROOT/target/debug/kelter" --config config.toml \
    > "$RUN_DIR/daemon-$(date +%s%N).log" 2>&1 ) &
  DAEMON_PID=$!
  for _ in $(seq 1 60); do
    curl -fsS "$BASE/healthz" >/dev/null 2>&1 && return 0
    kill -0 "$DAEMON_PID" 2>/dev/null || return 1
    sleep 0.3
  done
  return 1
}
latest_log() { ls -t "$RUN_DIR"/daemon-*.log | head -1; }

DELEGATE_TASK="Delegate the sub-task 'summarize kelter' to the remote agent using delegate_to_toy. Then finish."

# ---- 1 + 2: registration + end-to-end delegation ------------------------
log "=== delegation end-to-end ==="
start_daemon || die "daemon did not start"
grep -q "a2a: remote agent 'toy' registered" "$(latest_log)" \
  || die "toy agent was not registered from its manifest"

RUN=$(curl -fsS -X POST "$BASE/api/v1/runs" -H "Authorization: Bearer ${TOKEN}" \
  -H 'Content-Type: application/json' -d "{\"task\": \"${DELEGATE_TASK}\"}" \
  | sed -E 's/.*"run_id":"([^"]+)".*/\1/')
STATUS=""
for _ in $(seq 1 60); do
  BODY=$(curl -fsS "$BASE/api/v1/runs/$RUN" -H "Authorization: Bearer ${TOKEN}")
  STATUS=$(printf '%s' "$BODY" | sed -E 's/.*"status":"([a-z]+)".*/\1/')
  { [ "$STATUS" = "completed" ] || [ "$STATUS" = "failed" ]; } && break
  sleep 2
done
[ "$STATUS" = "completed" ] || die "delegation run did not complete ($STATUS): $BODY"
printf '%s' "$BODY" | grep -q '"action":"delegate_to_toy"' \
  || die "run did not use delegate_to_toy"
printf '%s' "$BODY" | grep -q 'SUMMARIZE KELTER' \
  || die "remote agent's answer did not come back into the run"
printf '%s' "$BODY" | python3 -c 'import json,sys; r=json.load(sys.stdin); au=r["agent_usage"]; assert "toy" in au and au["toy"]["delegations"]>=1, au; print("  per-agent usage:", json.dumps(au))'
log "delegation completed; remote answer validated + journaled with agent identity"
kill "$DAEMON_PID" 2>/dev/null || true; wait "$DAEMON_PID" 2>/dev/null || true

# ---- 3: MONEY SHOT — kill after remote task created, resume reconnects ---
log "=== durable A2A: kill after remote task created, resume reconnects ==="
rm -f "$RUN_DIR/kelter.db"*
start_daemon "after_a2a_create:delegate_to_toy" || die "daemon did not start"
RUN2=$(curl -fsS -X POST "$BASE/api/v1/runs" -H "Authorization: Bearer ${TOKEN}" \
  -H 'Content-Type: application/json' -d "{\"task\": \"${DELEGATE_TASK}\"}" \
  | sed -E 's/.*"run_id":"([^"]+)".*/\1/')
# The daemon self-aborts at the crash point (right after journaling the id).
for _ in $(seq 1 60); do kill -0 "$DAEMON_PID" 2>/dev/null || break; sleep 1; done
kill -0 "$DAEMON_PID" 2>/dev/null && die "daemon did not crash at after_a2a_create"
CRASH_LOG="$(latest_log)"
grep -q "remote task created and journaled" "$CRASH_LOG" \
  || die "no evidence the remote task was created + journaled before the crash"
TASK_ID=$(python3 -c "
import json,sqlite3,glob
db=glob.glob('$RUN_DIR/kelter.db')[0]
rows=sqlite3.connect(db).execute('select entry from journal').fetchall()
for (e,) in rows:
    j=json.loads(e)
    if j.get('kind')=='note' and j.get('key')=='a2a_task_id':
        print(j['value']); break
")
[ -n "$TASK_ID" ] || die "task id was not journaled"
log "crashed after creating+journaling remote task id=$TASK_ID"

start_daemon || die "daemon did not restart"
STATUS=""
for _ in $(seq 1 60); do
  BODY=$(curl -fsS "$BASE/api/v1/runs/$RUN2" -H "Authorization: Bearer ${TOKEN}")
  STATUS=$(printf '%s' "$BODY" | sed -E 's/.*"status":"([a-z]+)".*/\1/')
  { [ "$STATUS" = "completed" ] || [ "$STATUS" = "failed" ]; } && break
  sleep 2
done
RESUME_LOG="$(latest_log)"
[ "$STATUS" = "completed" ] || die "resumed A2A run did not complete ($STATUS)"
grep -q "reconnecting to existing remote task from the journal — NOT re-delegating" "$RESUME_LOG" \
  || die "resume did not reconnect to the journaled task (money shot failed)"
# The toy agent counts SendMessage internally per task id; the resumed run
# must reference the SAME task id, proving no re-delegation.
printf '%s' "$BODY" | grep -q "$TASK_ID" \
  || die "resumed run used a different task id — double delegation!"
log "PASS money shot: resumed by reconnecting to task $TASK_ID — zero re-delegation"
kill "$DAEMON_PID" 2>/dev/null || true; wait "$DAEMON_PID" 2>/dev/null || true

# ---- 4: governance — agent without `network` refused at startup ---------
log "=== governance: agent manifest without network is refused ==="
BAD_DIR="$(mktemp -d)"; mkdir -p "$BAD_DIR/exts/nonet"
cat > "$BAD_DIR/exts/nonet/kelter-extension.toml" <<EOF
[extension]
name = "nonet"
provides = ["agent"]

[agent]
card_url = "http://127.0.0.1:${AGENT_PORT}"
EOF
sed "s#extension_dirs = .*#extension_dirs = [\"$BAD_DIR/exts\"]#" "$RUN_DIR/config.toml" > "$BAD_DIR/config.toml"
set +e
( cd "$BAD_DIR" && "$REPO_ROOT/target/debug/kelter" --config config.toml > "$BAD_DIR/daemon.log" 2>&1 )
RC=$?
set -e
[ "$RC" -ne 0 ] || die "daemon should have refused an agent without network permission"
grep -q "network" "$BAD_DIR/daemon.log" \
  || die "expected a network-permission refusal, got: $(tail -2 "$BAD_DIR/daemon.log")"
log "startup refused: $(grep -o 'agent extension .nonet.*' "$BAD_DIR/daemon.log" | head -1 | cut -c1-80)…"

if [ -n "$EVIDENCE_DIR" ]; then
  mkdir -p "$EVIDENCE_DIR"
  scrub() { sed "s#${REPO_ROOT}#<repo>#g;s#${RUN_DIR}#<run>#g"; }
  printf '%s' "$BODY" | scrub > "$EVIDENCE_DIR/a2a-resume-run.json"
  grep -E 'a2a:|reconnect|registered' "$CRASH_LOG" "$RESUME_LOG" | scrub > "$EVIDENCE_DIR/a2a-durable.log"
  scrub < "$BAD_DIR/daemon.log" > "$EVIDENCE_DIR/a2a-network-refused.log"
fi

log "PASS: governed durable outbound A2A — delegate, validate, reconnect-not-re-delegate, network-enforced"
