#!/usr/bin/env bash
# A2A INBOUND smoke (Phase 7, ADR 0013): Kelter AS an A2A server.
#   1) Agent Card resolves at /.well-known/agent-card.json and declares the
#      bearer auth scheme (public discovery);
#   2) AUTH: an anonymous task call is refused (401); a bearer call is accepted
#      and the caller is attributed in the run's journal;
#   3) message/send creates a durable run, tasks/get polls it to completion,
#      tasks/cancel cancels an in-flight task;
#   4) DURABLE INBOUND (money shot): a crash mid inbound-task resumes on
#      restart, tasks/get by the same id still works, side effect exactly once;
#   5) INTEROP loop Kelter->Kelter: instance A delegates (4b outbound) to
#      instance B's inbound endpoint, authenticated; B runs it governed+durable;
#   6) CONFORMANCE: a generic A2A client (urllib, no SDK) resolves the card and
#      does message/send -> proof this is real A2A, not a private protocol.
#
#   scripts/smoke-a2a-inbound.sh [evidence-dir]
#
# Model-driven legs (task completion, the loop) need llama-server on :8080;
# without it, the card/auth/plumbing legs still run and the rest is skipped.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="${1:-}"
PORT_B=7601     # inbound server under test
PORT_BD=7603    # inbound server for the durable-crash leg
PORT_A=7602     # client instance for the Kelter<->Kelter loop
TOKEN_B="smoke-in-b-$$"
TOKEN_A="smoke-in-a-$$"
BIN="$REPO_ROOT/target/debug/kelter"

log() { printf '[smoke-a2a-inbound] %s\n' "$*"; }
die() { printf '[smoke-a2a-inbound] FAIL: %s\n' "$*" >&2; exit 1; }

command -v sqlite3 >/dev/null || die "sqlite3 CLI required for journal assertions"
command -v python3 >/dev/null || die "python3 required for the generic A2A client"

MODEL_UP=0
curl -fsS http://127.0.0.1:8080/health >/dev/null 2>&1 && MODEL_UP=1
[ "$MODEL_UP" = 1 ] || log "NOTE: llama-server not on :8080 — model-driven legs will be skipped"

log "building workspace"
cargo build --workspace --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null

WORK="$(mktemp -d)"
declare -a PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

# ---- helpers -------------------------------------------------------------

# write_inbound_config <dir> <port> <token> <db>
write_inbound_config() {
  cat > "$1/config.toml" <<EOF
[server]
bind = "127.0.0.1:$2"
token = "$3"
[policy]
engine = "local:qwen2.5-0.5b-instruct"
[kernel]
max_steps = 4
[storage]
backend = "sqlite"
path = "$4"
[tools]
workspace = "$1/work"
bundled_tools = false
extension_dirs = []

[[tools.servers]]
name = "ledger"
command = "${REPO_ROOT}/target/debug/ledger-server"
args = ["--workspace", "$1/work"]
EOF
}

# start_daemon <dir> <port> <logfile> [crash_point] -> sets LAST_PID
start_daemon() {
  local dir="$1" port="$2" logf="$3" crash="${4:-}"
  ( cd "$dir" && KELTER_CRASH_POINT="$crash" RUST_LOG=info \
    exec "$BIN" --config config.toml > "$dir/$logf" 2>&1 ) &
  LAST_PID=$!
  PIDS+=("$LAST_PID")
  for _ in $(seq 1 60); do
    curl -fsS "http://127.0.0.1:${port}/healthz" >/dev/null 2>&1 && return 0
    kill -0 "$LAST_PID" 2>/dev/null || return 1
    sleep 0.3
  done
  return 1
}

# a2a_rpc <port> <token> <method> <params-json> [extra-curl-args...]
a2a_rpc() {
  local port="$1" token="$2" method="$3" params="$4"; shift 4
  curl -fsS "$@" -X POST "http://127.0.0.1:${port}/a2a/rpc" \
    -H "Authorization: Bearer ${token}" -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":\"s\",\"method\":\"${method}\",\"params\":${params}}"
}

get_state() { # <port> <token> <task_id>
  a2a_rpc "$1" "$2" "tasks/get" "{\"id\":\"$3\"}" \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)["result"]["status"]["state"])'
}

# ---- 1 + 2: Agent Card + auth -------------------------------------------

log "=== 1/2: Agent Card (public discovery) + auth ==="
mkdir -p "$WORK/b/work"
write_inbound_config "$WORK/b" "$PORT_B" "$TOKEN_B" "$WORK/b/kelter.db"
start_daemon "$WORK/b" "$PORT_B" "b.log" || { cat "$WORK/b/b.log"; die "inbound daemon B did not start"; }
B_PID=$LAST_PID

CARD=$(curl -fsS "http://127.0.0.1:${PORT_B}/.well-known/agent-card.json")
printf '%s' "$CARD" > "$WORK/agent-card.json"
python3 - "$WORK/agent-card.json" <<'PY' || die "Agent Card malformed / missing auth declaration"
import json,sys
c=json.load(open(sys.argv[1]))
assert c["name"]=="Kelter", c.get("name")
assert c["securitySchemes"]["bearer"]["scheme"]=="bearer", "no bearer scheme"
assert c["security"][0]["bearer"]==[], "bearer not globally required"
ifaces=c.get("interfaces") or c.get("supportedInterfaces")
url=next(i["url"] for i in ifaces if i["protocolBinding"]=="JSONRPC")
assert url.endswith("/a2a/rpc"), url
print("  card ok: name=%s scheme=%s endpoint=%s" % (
    c["name"], c["securitySchemes"]["bearer"]["scheme"], url))
PY
log "  Agent Card resolves at the well-known path and declares bearer auth"

# Anonymous task call must be refused (the security surface).
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:${PORT_B}/a2a/rpc" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"SendMessage","params":{"message":{"parts":[{"text":"hi"}]}}}')
[ "$CODE" = "401" ] || die "anonymous message/send was not refused (got HTTP $CODE)"
log "  anonymous message/send refused (HTTP 401) — no open task server"

if [ "$MODEL_UP" != 1 ]; then
  log "PARTIAL PASS (no model): card + auth-refusal proven; skipping run-completion legs"
  [ -n "$EVIDENCE_DIR" ] && { mkdir -p "$EVIDENCE_DIR"; cp "$WORK/agent-card.json" "$EVIDENCE_DIR/a2a-inbound-agent-card.json"; }
  exit 0
fi

# ---- 3: message/send -> run, tasks/get, attribution ----------------------

log "=== 3: message/send creates a durable run; tasks/get; caller attributed ==="
SEND=$(a2a_rpc "$PORT_B" "$TOKEN_B" "message/send" \
  '{"message":{"messageId":"m1","role":"ROLE_USER","parts":[{"text":"Reply with the word READY and finish."}]}}' \
  -H "A2A-Caller: smoke-tester")
TASK_ID=$(printf '%s' "$SEND" | python3 -c 'import json,sys;print(json.load(sys.stdin)["result"]["task"]["id"])')
[ -n "$TASK_ID" ] || die "message/send did not return a task id"
log "  message/send -> task ${TASK_ID}"

STATE=""
for _ in $(seq 1 60); do
  STATE=$(get_state "$PORT_B" "$TOKEN_B" "$TASK_ID")
  { [ "$STATE" = "TASK_STATE_COMPLETED" ] || [ "$STATE" = "TASK_STATE_FAILED" ]; } && break
  sleep 1
done
[ "$STATE" = "TASK_STATE_COMPLETED" ] || die "inbound task did not complete (state=$STATE)"
# Caller attribution is durable in the run's journal.
sqlite3 "$WORK/b/kelter.db" \
  "SELECT entry FROM journal WHERE run_id='${TASK_ID}' AND entry LIKE '%a2a_inbound%';" \
  | grep -q 'smoke-tester' || die "caller not attributed in the journal"
log "  tasks/get -> COMPLETED; caller 'smoke-tester' attributed in the run journal"

# ---- 4: tasks/cancel -----------------------------------------------------

log "=== 4: tasks/cancel an in-flight task ==="
CANCEL_STATE=""
for attempt in 1 2 3; do
  CSEND=$(a2a_rpc "$PORT_B" "$TOKEN_B" "message/send" \
    '{"message":{"parts":[{"text":"Append the entry cancel-me to the ledger using append_ledger. Then finish."}]}}')
  CID=$(printf '%s' "$CSEND" | python3 -c 'import json,sys;print(json.load(sys.stdin)["result"]["task"]["id"])')
  # Cancel immediately, while it is still submitted/working.
  a2a_rpc "$PORT_B" "$TOKEN_B" "tasks/cancel" "{\"id\":\"$CID\"}" >/dev/null
  sleep 1
  CANCEL_STATE=$(get_state "$PORT_B" "$TOKEN_B" "$CID")
  [ "$CANCEL_STATE" = "TASK_STATE_CANCELED" ] && break
  log "  (attempt $attempt raced to $CANCEL_STATE, retrying)"
done
[ "$CANCEL_STATE" = "TASK_STATE_CANCELED" ] || die "tasks/cancel did not cancel the task (state=$CANCEL_STATE)"
log "  tasks/cancel -> TASK_STATE_CANCELED (durable)"

# ---- 5: DURABLE INBOUND money shot (crash + reconnect) -------------------

log "=== 5: durable inbound — crash mid-task, tasks/get survives, effect once ==="
mkdir -p "$WORK/bd/work"
write_inbound_config "$WORK/bd" "$PORT_BD" "$TOKEN_B" "$WORK/bd/kelter.db"
start_daemon "$WORK/bd" "$PORT_BD" "bd1.log" "after_execute:append_ledger" \
  || { cat "$WORK/bd/bd1.log"; die "durable inbound daemon did not start"; }
BD_PID=$LAST_PID
DSEND=$(a2a_rpc "$PORT_BD" "$TOKEN_B" "message/send" \
  '{"message":{"parts":[{"text":"Append the entry durable-inbound to the ledger using append_ledger. Then finish."}]}}')
DID=$(printf '%s' "$DSEND" | python3 -c 'import json,sys;print(json.load(sys.stdin)["result"]["task"]["id"])')
log "  inbound task ${DID} submitted; waiting for the injected crash"
for _ in $(seq 1 60); do kill -0 "$BD_PID" 2>/dev/null || break; sleep 1; done
kill -0 "$BD_PID" 2>/dev/null && die "daemon did not crash at the injection point"
LINES=$(wc -l < "$WORK/bd/work/ledger.txt" | tr -d ' ')
[ "$LINES" = "1" ] || die "expected 1 ledger line at crash time, got $LINES"
log "  server crashed after the append (ledger has 1 line, no receipt yet)"

# Restart against the same db: the run auto-resumes; tasks/get keeps working.
start_daemon "$WORK/bd" "$PORT_BD" "bd2.log" \
  || { cat "$WORK/bd/bd2.log"; die "durable inbound daemon did not restart"; }
BD_PID=$LAST_PID
STATE=""
for _ in $(seq 1 60); do
  STATE=$(get_state "$PORT_BD" "$TOKEN_B" "$DID")
  { [ "$STATE" = "TASK_STATE_COMPLETED" ] || [ "$STATE" = "TASK_STATE_FAILED" ]; } && break
  sleep 1
done
[ "$STATE" = "TASK_STATE_COMPLETED" ] || die "resumed inbound task did not complete (state=$STATE)"
LINES=$(wc -l < "$WORK/bd/work/ledger.txt" | tr -d ' ')
[ "$LINES" = "1" ] || die "DUPLICATED side effect after resume — ledger has $LINES lines"
grep -q "model NOT queried" "$WORK/bd/bd2.log" || die "no evidence of journal replay on resume"
log "  restart resumed the run; SAME task id still resolves; side effect exactly once"

# ---- 6: INTEROP loop Kelter -> Kelter ------------------------------------

log "=== 6: interop loop — instance A delegates to instance B (inbound), authed ==="
mkdir -p "$WORK/a/ext/kelter_b" "$WORK/a/work"
cat > "$WORK/a/ext/kelter_b/kelter-extension.toml" <<EOF
[extension]
name = "kelter_b"
description = "Another Kelter instance, reached over inbound A2A (ADR 0013)"
provides = ["agent"]

[permissions]
network = true

[agent]
card_url = "http://127.0.0.1:${PORT_B}"
token_env = "KELTER_B_TOKEN"
EOF
cat > "$WORK/a/config.toml" <<EOF
[server]
bind = "127.0.0.1:${PORT_A}"
token = "${TOKEN_A}"
[policy]
engine = "local:qwen2.5-0.5b-instruct"
[kernel]
max_steps = 4
[storage]
backend = "memory"
[tools]
workspace = "$WORK/a/work"
bundled_tools = false
extension_dirs = ["$WORK/a/ext"]
EOF
( cd "$WORK/a" && KELTER_B_TOKEN="$TOKEN_B" RUST_LOG=info \
  exec "$BIN" --config config.toml > "$WORK/a/a.log" 2>&1 ) &
A_PID=$!
PIDS+=("$A_PID")
for _ in $(seq 1 60); do curl -fsS "http://127.0.0.1:${PORT_A}/healthz" >/dev/null 2>&1 && break; sleep 0.3; done
curl -fsS "http://127.0.0.1:${PORT_A}/healthz" >/dev/null 2>&1 || { cat "$WORK/a/a.log"; die "client instance A did not start"; }

# A run whose only sensible move is to delegate to kelter_b, then finish.
A_RUN=$(curl -fsS -X POST "http://127.0.0.1:${PORT_A}/api/v1/runs" \
  -H "Authorization: Bearer ${TOKEN_A}" -H 'Content-Type: application/json' \
  -d '{"task":"Delegate this to kelter_b: Reply with the word DELEGATED and finish. Then finish."}' \
  | sed -E 's/.*"run_id":"([^"]+)".*/\1/')
log "  instance A run ${A_RUN} submitted (should call delegate_to_kelter_b)"
A_STATE=""
for _ in $(seq 1 90); do
  A_STATE=$(curl -fsS "http://127.0.0.1:${PORT_A}/api/v1/runs/${A_RUN}" \
    -H "Authorization: Bearer ${TOKEN_A}" | sed -E 's/.*"status":"([a-z]+)".*/\1/')
  { [ "$A_STATE" = "completed" ] || [ "$A_STATE" = "failed" ]; } && break
  sleep 1
done
# Hard gate: B received an AUTHENTICATED delegation from Kelter's outbound
# client and ran it as a governed durable run (proves the inbound loop + auth).
sqlite3 "$WORK/b/kelter.db" \
  "SELECT entry FROM journal WHERE entry LIKE '%a2a_inbound%';" \
  | grep -q 'kelter-a2a-client' \
  || { cat "$WORK/a/a.log"; die "instance B did not receive an authenticated delegation from A"; }
log "  instance B received an authenticated delegation from A's outbound client (attributed)"
# Strong goal: A's run completed via a delegate step returning B's answer.
A_BODY=$(curl -fsS "http://127.0.0.1:${PORT_A}/api/v1/runs/${A_RUN}" -H "Authorization: Bearer ${TOKEN_A}")
if printf '%s' "$A_BODY" | grep -q '"action":"delegate_to_kelter_b"'; then
  log "  loop money-shot: A delegated to B over A2A and got the answer back (A state=$A_STATE)"
else
  log "  NOTE: A's 0.5B did not emit the delegate step this run (state=$A_STATE) — loop connectivity+auth still proven B-side above"
fi
printf '%s' "$A_BODY" > "$WORK/loop-a-run.json"

# ---- 7: CONFORMANCE — generic A2A client ---------------------------------

log "=== 7: conformance — a generic A2A client (urllib, no SDK) ==="
GEN_LOG="$WORK/generic-client.log"
# v1.0 canonical method name (SendMessage); the endpoint also accepts message/send.
python3 "$REPO_ROOT/examples/a2a-generic-client.py" \
  --base "http://127.0.0.1:${PORT_B}" --token "$TOKEN_B" \
  --text "hello from a generic A2A client" --method SendMessage \
  | tee "$GEN_LOG"
grep -q 'card.name: Kelter' "$GEN_LOG" || die "generic client did not resolve the Agent Card"
grep -q 'final.state: TASK_STATE_COMPLETED' "$GEN_LOG" \
  || die "generic client did not drive a task to completion"
log "  generic client resolved the card and completed a task over JSON-RPC — real A2A"

# ---- evidence ------------------------------------------------------------

if [ -n "$EVIDENCE_DIR" ]; then
  mkdir -p "$EVIDENCE_DIR"
  scrub() { sed "s#${REPO_ROOT}#<repo>#g;s#${WORK}#<work>#g;s#${TOKEN_B}#<token-b>#g;s#${TOKEN_A}#<token-a>#g"; }
  cp "$WORK/agent-card.json" "$EVIDENCE_DIR/a2a-inbound-agent-card.json"
  scrub < "$WORK/bd/bd2.log" | grep -E 'resume|model NOT queried|journaled idempotency|status' \
    > "$EVIDENCE_DIR/a2a-inbound-durable-resume.log" || true
  scrub < "$GEN_LOG" > "$EVIDENCE_DIR/a2a-inbound-generic-client.log" || true
  scrub < "$WORK/loop-a-run.json" > "$EVIDENCE_DIR/a2a-inbound-loop-run.json" || true
  { echo "task=$TASK_ID caller-attribution:";
    sqlite3 "$WORK/b/kelter.db" "SELECT entry FROM journal WHERE entry LIKE '%a2a_inbound%';" | scrub;
  } > "$EVIDENCE_DIR/a2a-inbound-attribution.log" || true
fi

log "PASS: A2A inbound — card+auth, message/send->durable run, tasks/get+cancel, durable-crash reconnect, Kelter<->Kelter loop, generic-client conformance"
