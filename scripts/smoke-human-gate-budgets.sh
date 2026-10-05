#!/usr/bin/env bash
# ADR 0038 (human gate) + ADR 0039 (budgets) against the real daemon.
#   A) `append_ledger = "ask"`: the run pauses awaiting_action_approval, the
#      ledger untouched; the daemon is RESTARTED — the run is re-surfaced, not
#      executed; a human allows → the line is appended EXACTLY once, the
#      decision journaled with the principal.
#   D) `write_report = "deny"`: refused, journaled (`rejected`, source
#      `action_policy`), the model told; no report file.
#   T) a per-run token budget: stopped BEFORE the model call that would cross
#      it (the call is never made), `failed` with `budget_stop`.
#   W) a per-run wall-clock budget: a slow model call eats it; the action it
#      proposed is refused before it runs.
#
#   scripts/smoke-human-gate-budgets.sh [evidence-dir]
#
# Real `kelter` binary, real SQLite, the bundled reference tool server. The
# model is the scripted stub (scripts/fake-llm-scripted.py) with per-reply
# `_usage`/`_delay`. Needs python3 and curl.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="${1:-}"
PORT=7595
LLM_PORT=18095
BASE="http://127.0.0.1:${PORT}"
TOKEN="smoke-gate-$$"

log() { printf '[smoke-human-gate-budgets] %s\n' "$*"; }
die() { printf '[smoke-human-gate-budgets] FAIL: %s\n' "$*" >&2; exit 1; }

log "building daemon + bundled tool server"
cargo build -p daemon -p mcp --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null

RUN_DIR="$(mktemp -d)"
mkdir -p "$RUN_DIR/work"
cat > "$RUN_DIR/queue.json" <<'Q'
[
  {"action": "append_ledger", "arguments": {"entry": "approved-entry"}},
  {"action": "finish", "arguments": {"summary": "appended after approval"}},
  {"action": "write_report", "arguments": {"source": "x.json", "output": "report.md", "format": "markdown"}},
  {"action": "finish", "arguments": {"summary": "gave up on the report"}},
  {"action": "extract_structure", "arguments": {"source": "none.json", "save_as": "t.json"}, "_usage": [400, 0]},
  {"action": "extract_structure", "arguments": {"source": "none.json", "save_as": "slow.json"}, "_delay": 2.5}
]
Q
( cd "$RUN_DIR" && FAKE_LLM_QUEUE="$RUN_DIR/queue.json" \
    exec python3 "$REPO_ROOT/scripts/fake-llm-scripted.py" "$LLM_PORT" ) &
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
reflection = false

[storage]
backend = "sqlite"
path = "${RUN_DIR}/kelter.db"

[tools]
workspace = "${RUN_DIR}/work"
bundled_tools = true
extension_dirs = []

[approval]
ask_timeout_secs = 600
[approval.actions]
append_ledger = "ask"
write_report = "deny"

[budget]
prompt_overhead_tokens = 0
CFG

start_daemon() { # $1 = logfile
  ( cd "$RUN_DIR" && RUST_LOG=info \
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

auth=(-H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json')
get() { curl -fsS "$BASE$1" "${auth[@]}"; }
post() { curl -fsS -X POST "$BASE$1" "${auth[@]}" -d "${2:-{\}}"; }
field() { python3 -c "import json,sys; print(json.load(sys.stdin)$1)"; }
wait_status() { # $1 = run id, $2.. = statuses to wait for
  local id="$1"; shift; local s=""
  for _ in $(seq 1 80); do
    s=$(get "/api/v1/runs/$id" | field '["status"]') || true
    for want in "$@"; do [ "$s" = "$want" ] && { printf '%s' "$s"; return 0; }; done
    sleep 0.25
  done
  printf '%s' "$s"
}
ledger_count() { grep -c "$1" "$RUN_DIR/work/ledger.txt" 2>/dev/null || true; }
calls() { wc -l < "$RUN_DIR/calls.log" | tr -d ' '; }

start_daemon daemon-1.log || { cat "$RUN_DIR/daemon-1.log"; die "daemon did not start"; }
grep -q "approval: default=Allow entries=2" "$RUN_DIR/daemon-1.log" || die "gate not logged"
grep -q "approval: action-approval timeout sweeper active (timeout=600s)" "$RUN_DIR/daemon-1.log" \
  || die "sweeper not started"

# ------------------------------------------------ A: ask → restart → allow
log "=== A: ask pauses; the pause survives a restart; allow executes once ==="
RUN_A=$(post /api/v1/runs '{"task": "append approved-entry to the ledger"}' | field '["run_id"]')
[ "$(wait_status "$RUN_A" awaiting_action_approval)" = "awaiting_action_approval" ] \
  || die "A: run did not pause"
get "/api/v1/runs/$RUN_A" > "$RUN_DIR/run-awaiting-approval.json"
get /api/v1/approvals > "$RUN_DIR/approvals-queue.json"
[ "$(ledger_count approved-entry)" = "0" ] || [ "$(ledger_count approved-entry)" = "" ] \
  || die "A: executed before a decision"
[ "$(field '[0]["kind"]' < "$RUN_DIR/approvals-queue.json")" = "action_approval" ] \
  || die "A: not in the queue"
CALLS_BEFORE=$(calls)

log "A: restarting the daemon while the run waits"
stop_daemon
start_daemon daemon-2.log || { cat "$RUN_DIR/daemon-2.log"; die "daemon did not restart"; }
grep -q "resume: still awaiting a human action approval" "$RUN_DIR/daemon-2.log" \
  || die "A: the restart did not re-surface the pause"
[ "$(get "/api/v1/runs/$RUN_A" | field '["status"]')" = "awaiting_action_approval" ] \
  || die "A: not paused after restart"
[ "$(calls)" = "$CALLS_BEFORE" ] || die "A: the restart called the model"
[ "$(ledger_count approved-entry)" = "0" ] || [ "$(ledger_count approved-entry)" = "" ] \
  || die "A: the restart executed the action"

C=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/v1/runs/$RUN_A/action-approval" \
  -H 'Content-Type: application/json' -d '{"action": "append_ledger", "decision": "allow"}')
[ "$C" = "401" ] || die "A: anonymous decision should be 401, got $C"
post "/api/v1/runs/$RUN_A/action-approval" '{"action": "append_ledger", "decision": "allow"}' \
  > "$RUN_DIR/decide-allow.json"
[ "$(wait_status "$RUN_A" completed failed incomplete)" = "completed" ] || die "A: did not complete"
get "/api/v1/runs/$RUN_A" > "$RUN_DIR/run-approved.json"
get "/api/v1/runs/$RUN_A/events" > "$RUN_DIR/history-approved.json"
[ "$(ledger_count approved-entry)" = "1" ] || die "A: ledger has $(ledger_count approved-entry) lines"
python3 - "$RUN_DIR/history-approved.json" <<'PY' || die "A: journal assertions failed"
import json, sys
h = json.load(open(sys.argv[1]))
kinds = [e["kind"] for e in h["journal"]]
assert kinds.count("action_approval_requested") == 1, kinds
d = [e for e in h["journal"] if e["kind"] == "action_approval_decided"]
assert len(d) == 1 and d[0]["decision"] == "allow" and d[0]["principal"] == "default", d
assert [e["step"] for e in h["journal"] if e["kind"] == "intent"] == [0], kinds
r = [e for e in h["journal"] if e["kind"] == "resumed"]
assert any(x["cause"] == "pause_resolved" and x["replayed_steps"] == [0] for x in r), r
PY
log "A: PASS — paused, survived a restart untouched, allowed by 'default', appended once"

# ------------------------------------------------------------ D: deny
log "=== D: deny policy refuses and tells the model ==="
RUN_D=$(post /api/v1/runs '{"task": "write the report"}' | field '["run_id"]')
S=$(wait_status "$RUN_D" completed failed incomplete)
get "/api/v1/runs/$RUN_D/events" > "$RUN_DIR/history-denied.json"
get "/api/v1/runs/$RUN_D" > "$RUN_DIR/run-denied.json"
[ "$S" = "incomplete" ] || die "D: expected incomplete (nothing effective ran), got $S"
[ ! -e "$RUN_DIR/work/report.md" ] || die "D: the denied report was written"
python3 - "$RUN_DIR/history-denied.json" <<'PY' || die "D: journal assertions failed"
import json, sys
h = json.load(open(sys.argv[1]))
r = [e for e in h["journal"] if e["kind"] == "rejected"]
assert len(r) == 1 and r[0]["source"] == "action_policy" and r[0]["action"] == "write_report", r
assert r[0]["subject"] == "proposal" and "denies 'write_report'" in r[0]["reason"], r
assert not any(e["kind"] == "decision" and e["action"] == "write_report" for e in h["journal"])
PY
log "D: PASS — refused (action_policy), journaled, never executed"

# ------------------------------------------------------- T: token budget
log "=== T: token budget stops before the call that would cross it ==="
# Calibrate: a 1-token budget is refused before the first call; its
# `requested` is the adapter's worst case for that call.
RUN_CAL=$(post /api/v1/runs '{"task": "token run", "budget": {"max_tokens": 1}}' | field '["run_id"]')
[ "$(wait_status "$RUN_CAL" failed)" = "failed" ] || die "T: calibration run did not stop"
BOUND=$(get "/api/v1/runs/$RUN_CAL" | field '["budget_stop"]["requested"]' | cut -d. -f1)
# The real run: call 1 (worst case BOUND) fits; it reports 400 tokens, so
# call 2 — whose prompt is at least as long — would need 400 + ≥BOUND > MAX.
MAX=$((BOUND + 300))
log "T: first-call worst case = $BOUND tokens → max_tokens = $MAX"
CALLS_BEFORE=$(calls)
RUN_T=$(post /api/v1/runs "{\"task\": \"token run\", \"budget\": {\"max_tokens\": $MAX}}" \
  | field '["run_id"]')
[ "$(wait_status "$RUN_T" failed completed incomplete)" = "failed" ] || die "T: did not stop"
get "/api/v1/runs/$RUN_T" > "$RUN_DIR/run-token-budget.json"
get "/api/v1/runs/$RUN_T/events" > "$RUN_DIR/history-token-budget.json"
[ "$(calls)" = "$((CALLS_BEFORE + 1))" ] || die "T: the refused call was made ($(calls) calls)"
python3 - "$RUN_DIR/run-token-budget.json" "$RUN_DIR/history-token-budget.json" "$MAX" <<'PY' \
  || die "T: assertions failed"
import json, sys
run = json.load(open(sys.argv[1])); h = json.load(open(sys.argv[2])); mx = int(sys.argv[3])
s = run["budget_stop"]
assert s["limit"] == "tokens" and s["checkpoint"] == "model_call", s
assert s["used"] <= mx < s["used"] + s["requested"], s
assert run["usage"]["prompt_tokens"] + run["usage"]["completion_tokens"] <= mx, run["usage"]
assert "token budget exceeded before a model call" in run["error"], run["error"]
r = [e for e in h["journal"] if e["kind"] == "rejected" and e["source"] == "budget"]
assert len(r) == 1 and r[0]["subject"] == "none", r
assert h["budget_stop"] == s, h["budget_stop"]
PY
log "T: PASS — $(field '["error"]' < "$RUN_DIR/run-token-budget.json")"

# -------------------------------------------------------- W: wall clock
log "=== W: wall-clock budget refuses the action a slow call proposed ==="
RUN_W=$(post /api/v1/runs '{"task": "slow run", "budget": {"max_wall_secs": 2}}' | field '["run_id"]')
[ "$(wait_status "$RUN_W" failed completed incomplete awaiting_action_approval)" = "failed" ] \
  || die "W: did not stop"
get "/api/v1/runs/$RUN_W" > "$RUN_DIR/run-wall-budget.json"
get "/api/v1/runs/$RUN_W/events" > "$RUN_DIR/history-wall-budget.json"
python3 - "$RUN_DIR/run-wall-budget.json" "$RUN_DIR/history-wall-budget.json" <<'PY' \
  || die "W: assertions failed"
import json, sys
run = json.load(open(sys.argv[1])); h = json.load(open(sys.argv[2]))
s = run["budget_stop"]
assert s["limit"] == "wall_time" and s["checkpoint"] == "action", s
assert s["max"] == 2 and s["used"] >= 2, s
# The proposal was accepted, the action never dispatched: a Decision, no Intent.
kinds = [e["kind"] for e in h["journal"]]
assert "decision" in kinds and "intent" not in kinds, kinds
r = [e for e in h["journal"] if e["kind"] == "rejected" and e["source"] == "budget"]
assert len(r) == 1 and r[0]["subject"] == "decision" and r[0]["action"] == "extract_structure", r
PY
log "W: PASS — $(field '["error"]' < "$RUN_DIR/run-wall-budget.json")"

if [ -n "$EVIDENCE_DIR" ]; then
  mkdir -p "$EVIDENCE_DIR"
  cp "$RUN_DIR"/run-*.json "$RUN_DIR"/history-*.json "$RUN_DIR"/approvals-queue.json \
     "$RUN_DIR"/decide-allow.json "$RUN_DIR/config.toml" "$EVIDENCE_DIR/"
fi
log "PASS: ask survives a restart and executes once; deny refuses; budgets stop before crossing"
