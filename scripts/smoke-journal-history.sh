#!/usr/bin/env bash
# ADR 0036 against the real daemon: the run history (GET /runs/{id}/events)
# carries refusals and resumes.
#   R) a run whose model proposes a schema-invalid action and then a
#      duplicate → two `rejected` entries, each carrying the refused proposal;
#   K) a run killed right after its side effect (KELTER_CRASH_POINT) and
#      restarted → one `resumed` entry naming the NEW process's session and
#      the replayed step; the side effect happened exactly once.
#
#   scripts/smoke-journal-history.sh [evidence-dir]
#
# Real `kelter` binary, real SQLite, real ledger MCP server. The model is a
# scripted OpenAI-compatible stub (scripts/fake-llm-scripted.py) — the point
# is the journal, not model quality, and a scripted model makes the refusals
# deterministic. Needs python3 and curl.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="${1:-}"
PORT=7591
LLM_PORT=18091
BASE="http://127.0.0.1:${PORT}"
TOKEN="smoke-journal-$$"

log() { printf '[smoke-journal-history] %s\n' "$*"; }
die() { printf '[smoke-journal-history] FAIL: %s\n' "$*" >&2; exit 1; }

log "building daemon + ledger server"
cargo build -p daemon -p mcp --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null

RUN_DIR="$(mktemp -d)"
mkdir -p "$RUN_DIR/work"
( cd "$RUN_DIR" && exec python3 "$REPO_ROOT/scripts/fake-llm-scripted.py" "$LLM_PORT" ) &
LLM_PID=$!
DAEMON_PID=""
cleanup() { kill "$DAEMON_PID" "$LLM_PID" 2>/dev/null || true; }
trap cleanup EXIT

cat > "$RUN_DIR/config.toml" <<CFG
[server]
bind = "127.0.0.1:${PORT}"
token = "${TOKEN}"

[policy]
engine = "local:scripted-stub"
base_url = "http://127.0.0.1:${LLM_PORT}"

[kernel]
max_steps = 4

[storage]
backend = "sqlite"
path = "${RUN_DIR}/kelter.db"

[tools]
workspace = "${RUN_DIR}/work"
bundled_tools = false
extension_dirs = []

[[tools.servers]]
name = "ledger"
command = "${REPO_ROOT}/target/debug/ledger-server"
args = ["--workspace", "${RUN_DIR}/work"]
CFG

start_daemon() { # $1 = logfile, [$2 = crash point]
  ( cd "$RUN_DIR" && KELTER_CRASH_POINT="${2:-}" RUST_LOG=info \
    exec "$REPO_ROOT/target/debug/kelter" --config config.toml > "$RUN_DIR/$1" 2>&1 ) &
  DAEMON_PID=$!
  for _ in $(seq 1 60); do
    curl -fsS "$BASE/healthz" >/dev/null 2>&1 && return 0
    kill -0 "$DAEMON_PID" 2>/dev/null || return 1
    sleep 0.3
  done
  return 1
}
stop_daemon() { kill "$DAEMON_PID" 2>/dev/null || true; wait "$DAEMON_PID" 2>/dev/null || true; }
submit() {
  curl -fsS -X POST "$BASE/api/v1/runs" -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' -d "{\"task\": \"$1\"}" \
    | sed -E 's/.*"run_id":"([^"]+)".*/\1/'
}
wait_status() {
  local s=""
  for _ in $(seq 1 60); do
    s=$(curl -fsS "$BASE/api/v1/runs/$1" -H "Authorization: Bearer ${TOKEN}" \
      | sed -E 's/.*"status":"([a-z]+)".*/\1/') || true
    case "$s" in completed|failed|incomplete) break ;; esac
    sleep 0.5
  done
  printf '%s' "$s"
}
history() { curl -fsS "$BASE/api/v1/runs/$1/events" -H "Authorization: Bearer ${TOKEN}"; }
session_of() { grep -h "execution session" "$RUN_DIR/$1" | sed -E 's/\x1b\[[0-9;]*m//g; s/.*session=([0-9a-f-]+).*/\1/'; }

# ------------------------------------------------------ R: refusals -------
log "=== R: a run with refusals ==="
start_daemon d-r.log || die "daemon did not start"
RUN_R=$(submit "Append journal-hello to the ledger, then finish.")
[ "$(wait_status "$RUN_R")" = "completed" ] || die "R: run did not complete"
history "$RUN_R" > "$RUN_DIR/history-r.json"
python3 - "$RUN_DIR/history-r.json" <<'PY' || die "R: history assertions failed"
import json, sys
j = json.load(open(sys.argv[1]))["journal"]
rej = [e for e in j if e["kind"] == "rejected"]
assert [(e["step"], e["source"], e["subject"]) for e in rej] == \
    [(0, "schema", "proposal"), (1, "duplicate", "proposal")], rej
assert rej[0]["arguments"] == {"entry": 5}, rej[0]
assert rej[1]["arguments"] == {"entry": "journal-hello"}, rej[1]
kinds = [e["kind"] for e in j]
assert kinds.index("rejected") < kinds.index("decision"), kinds
assert "resumed" not in kinds, "a first execution is not a resume"
PY
log "R: PASS — 2 refusals in the history, each with the refused proposal"
stop_daemon

# ------------------------------------- K: killed after the side effect ----
log "=== K: killed after the side effect, then restarted ==="
start_daemon d-k1.log "after_execute:append_ledger" || die "daemon did not start"
RUN_K=$(submit "Append durable-hello to the ledger, then finish.")
for _ in $(seq 1 60); do kill -0 "$DAEMON_PID" 2>/dev/null || break; sleep 0.5; done
kill -0 "$DAEMON_PID" 2>/dev/null && die "K: daemon did not crash at the injection point"
[ "$(grep -c durable-hello "$RUN_DIR/work/ledger.txt")" = "1" ] || die "K: expected 1 line at crash"
start_daemon d-k2.log || die "daemon did not restart"
[ "$(wait_status "$RUN_K")" = "completed" ] || die "K: resumed run did not complete"
[ "$(grep -c durable-hello "$RUN_DIR/work/ledger.txt")" = "1" ] || die "K: side effect duplicated"
history "$RUN_K" > "$RUN_DIR/history-k.json"
python3 - "$RUN_DIR/history-k.json" "$(session_of d-k1.log)" "$(session_of d-k2.log)" <<'PY' \
  || die "K: history assertions failed"
import json, sys
j = json.load(open(sys.argv[1]))["journal"]
crashed, restarted = sys.argv[2], sys.argv[3]
assert crashed and restarted and crashed != restarted, (crashed, restarted)
res = [e for e in j if e["kind"] == "resumed"]
assert len(res) == 1, res
r = res[0]
assert (r["at_step"], r["cause"], r["replayed_steps"]) == (0, "interrupted", [0]), r
assert r["session"] == restarted, (r["session"], restarted)
receipts = [e for e in j if e["kind"] == "receipt" and e["step"] == 0]
assert len(receipts) == 1 and receipts[0]["result"].get("deduped") is True, receipts
PY
log "K: PASS — one resume entry, session of the restarted process, step 0 replayed, effect once"
stop_daemon

if [ -n "$EVIDENCE_DIR" ]; then
  mkdir -p "$EVIDENCE_DIR"
  cp "$RUN_DIR/history-r.json" "$EVIDENCE_DIR/journal-history-rejections.json"
  cp "$RUN_DIR/history-k.json" "$EVIDENCE_DIR/journal-history-resume.json"
fi
log "PASS: refusals and resumes are in the run history"
