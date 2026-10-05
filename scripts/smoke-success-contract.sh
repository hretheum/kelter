#!/usr/bin/env bash
# ADR 0037 against the real daemon: the success contract and the learning
# gate, on the deployment case that motivated it — "send an e-mail" done by
# writing to a file instead.
#   C) WITH a contract requiring `send_email`, a model that only appends to
#      the ledger → `incomplete`, `verification: contract`, the reason names
#      the unmet condition; never `completed`.
#   U) WITHOUT a contract, the same behaviour → `completed`,
#      `verification: none`; `POST /learn/distill` skips it.
#   H) a human confirms it (POST /runs/{id}/confirmation) → journaled with
#      the principal; distill now learns from it. Revoking (DELETE) forgets
#      that fact and the run leaves learning again.
#   V) a contract naming an action the daemon does not offer → 422 at
#      creation, no run.
#
#   scripts/smoke-success-contract.sh [evidence-dir]
#
# Real `kelter` binary, real SQLite, real ledger MCP server, plus a tiny MCP
# stub that offers `send_email` (scripts/mcp-mail-stub.py) so the contract is
# valid. The model is the scripted stub (scripts/fake-llm-scripted.py); it
# never calls send_email. Needs python3 and curl.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="${1:-}"
PORT=7593
LLM_PORT=18093
BASE="http://127.0.0.1:${PORT}"
TOKEN="smoke-contract-$$"

log() { printf '[smoke-success-contract] %s\n' "$*"; }
die() { printf '[smoke-success-contract] FAIL: %s\n' "$*" >&2; exit 1; }

log "building daemon + ledger server"
cargo build -p daemon -p mcp --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null

RUN_DIR="$(mktemp -d)"
mkdir -p "$RUN_DIR/work"
cat > "$RUN_DIR/queue.json" <<'Q'
[
  {"action": "append_ledger", "arguments": {"entry": "Dear Bob, the report is attached."}},
  {"action": "finish", "arguments": {"summary": "E-mail to Bob sent."}},
  {"action": "append_ledger", "arguments": {"entry": "Dear Alice, the report is attached."}},
  {"action": "finish", "arguments": {"summary": "E-mail to Alice sent."}}
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

[[tools.servers]]
name = "mail"
command = "python3"
args = ["${REPO_ROOT}/scripts/mcp-mail-stub.py"]
CFG

( cd "$RUN_DIR" && RUST_LOG=info exec "$REPO_ROOT/target/debug/kelter" --config config.toml \
    > "$RUN_DIR/daemon.log" 2>&1 ) &
DAEMON_PID=$!
for _ in $(seq 1 60); do
  curl -fsS "$BASE/healthz" >/dev/null 2>&1 && break
  kill -0 "$DAEMON_PID" 2>/dev/null || { cat "$RUN_DIR/daemon.log"; die "daemon exited"; }
  sleep 0.3
done
curl -fsS "$BASE/healthz" >/dev/null 2>&1 || die "daemon did not start"
grep -q "self-learning: governed distillation enabled" "$RUN_DIR/daemon.log" \
  || die "self-learning was not enabled"

auth=(-H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json')
get() { curl -fsS "$BASE$1" "${auth[@]}"; }
post() { curl -fsS -X POST "$BASE$1" "${auth[@]}" -d "${2:-{\}}"; }
code() { curl -s -o "$RUN_DIR/last-body" -w '%{http_code}' -X "$1" "$BASE$2" "${auth[@]}" ${3:+-d "$3"}; }
field() { python3 -c "import json,sys; print(json.load(sys.stdin)$1)"; }
wait_status() {
  local s=""
  for _ in $(seq 1 60); do
    s=$(get "/api/v1/runs/$1" | field '["status"]') || true
    case "$s" in completed|failed|incomplete) break ;; esac
    sleep 0.5
  done
  printf '%s' "$s"
}

get /api/v1/tools | field '["tools"]' | grep -q "send_email" || die "send_email not offered"

# --------------------------------------------------- V: validation ---------
log "=== V: a contract naming an action the daemon does not offer ==="
C=$(code POST /api/v1/runs \
  '{"task": "fax Bob", "success_contract": {"require": [{"kind": "action_succeeded", "action": "send_fax"}]}}')
[ "$C" = "422" ] || die "V: expected 422, got $C"
grep -q "unknown action \`send_fax\`" "$RUN_DIR/last-body" || die "V: reason: $(cat "$RUN_DIR/last-body")"
[ "$(get /api/v1/runs | field '.__len__()')" = "0" ] || die "V: a run was created"
log "V: PASS — 422 ($(cat "$RUN_DIR/last-body")), no run"

# ------------------------------------------------------ C: contract ---------
log "=== C: 'send an e-mail' with a send_email contract, model writes instead ==="
RUN_C=$(post /api/v1/runs '{"task": "Send an e-mail to Bob with the report.",
  "success_contract": {"require": [{"kind": "action_succeeded", "action": "send_email"}]}}' \
  | field '["run_id"]')
[ "$(wait_status "$RUN_C")" = "incomplete" ] || die "C: run is not incomplete"
get "/api/v1/runs/$RUN_C" > "$RUN_DIR/run-contract-unmet.json"
get "/api/v1/runs/$RUN_C/events" > "$RUN_DIR/history-contract-unmet.json"
python3 - "$RUN_DIR/run-contract-unmet.json" "$RUN_DIR/history-contract-unmet.json" <<'PY' \
  || die "C: assertions failed"
import json, sys
run = json.load(open(sys.argv[1])); hist = json.load(open(sys.argv[2]))
assert run["status"] == "incomplete", run["status"]
assert any(s["action"] == "append_ledger" and s["ok"] for s in run["steps"]), run["steps"]
assert run["verification"] == "contract", run["verification"]
assert "success contract not met" in run["error"] and "send_email" in run["error"], run["error"]
assert run["contract_report"]["satisfied"] is False, run["contract_report"]
assert run["output"]["summary"] == "E-mail to Bob sent.", run["output"]
t = [e for e in hist["journal"] if e["kind"] == "terminal"]
assert len(t) == 1 and t[0]["status"] == "incomplete" and t[0]["verification"] == "contract", t
assert hist["verification"] == "contract", hist
PY
[ ! -e "$RUN_DIR/sent.log" ] || die "C: send_email was actually called"
log "C: PASS — incomplete: $(field '["error"]' < "$RUN_DIR/run-contract-unmet.json")"

# ---------------------------------------------------- U: unverified ---------
log "=== U: the same behaviour without a contract ==="
RUN_U=$(post /api/v1/runs '{"task": "Send an e-mail to Alice with the report."}' | field '["run_id"]')
[ "$(wait_status "$RUN_U")" = "completed" ] || die "U: run did not complete"
get "/api/v1/runs/$RUN_U" > "$RUN_DIR/run-unverified.json"
[ "$(field '["verification"]' < "$RUN_DIR/run-unverified.json")" = "none" ] || die "U: not unverified"
post /api/v1/learn/distill '{"limit": 10}' > "$RUN_DIR/distill-before.json"
python3 - "$RUN_DIR/distill-before.json" "$RUN_U" "$RUN_C" <<'PY' || die "U: distill took it"
import json, sys
r = json.load(open(sys.argv[1]))
assert sys.argv[2] not in r["source_run_ids"] and sys.argv[3] not in r["source_run_ids"], r
assert r["facts_created"] == 0 and r["plans_created"] == 0, r
PY
log "U: PASS — completed, verification none, distill skipped it"

# --------------------------------------------- H: human confirmation --------
log "=== H: a human confirms, distill learns; revoke forgets ==="
C=$(code POST "/api/v1/runs/$RUN_C/confirmation")
[ "$C" = "409" ] || die "H: confirming an incomplete run should be 409, got $C"
C=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/v1/runs/$RUN_U/confirmation")
[ "$C" = "401" ] || die "H: anonymous confirmation should be 401, got $C"
post "/api/v1/runs/$RUN_U/confirmation" > "$RUN_DIR/confirm.json"
[ "$(field '["run"]["confirmation"]["principal"]' < "$RUN_DIR/confirm.json")" = "default" ] \
  || die "H: confirmation has no principal"
post /api/v1/learn/distill '{"limit": 10}' > "$RUN_DIR/distill-after.json"
python3 - "$RUN_DIR/distill-after.json" "$RUN_U" <<'PY' || die "H: distill did not take it"
import json, sys
r = json.load(open(sys.argv[1]))
assert r["source_run_ids"] == [sys.argv[2]] and r["facts_created"] == 1, r
PY
get "/api/v1/runs/$RUN_U" > "$RUN_DIR/run-confirmed.json"
C=$(code DELETE "/api/v1/runs/$RUN_U/confirmation")
[ "$C" = "200" ] || die "H: revoke returned $C"
cp "$RUN_DIR/last-body" "$RUN_DIR/revoke.json"
get "/api/v1/runs/$RUN_U/events" > "$RUN_DIR/history-confirmed-revoked.json"
get /api/v1/learn > "$RUN_DIR/learn-after-revoke.json"
python3 - "$RUN_DIR/revoke.json" "$RUN_DIR/history-confirmed-revoked.json" \
  "$RUN_DIR/learn-after-revoke.json" "$RUN_U" <<'PY' || die "H: revoke assertions failed"
import json, sys
rev = json.load(open(sys.argv[1])); hist = json.load(open(sys.argv[2]))
learned = json.load(open(sys.argv[3])); run_u = sys.argv[4]
assert rev["run"]["confirmation"] is None and len(rev["forgotten_learned_ids"]) == 1, rev
d = [(e["decision"], e["principal"]) for e in hist["journal"]
     if e["kind"] == "success_confirmation_decided"]
assert d == [("confirm", "default"), ("revoke", "default")], d
assert not any(run_u in f["provenance"]["source_run_ids"] for f in learned), learned
PY
post /api/v1/learn/distill '{"limit": 10}' | python3 -c \
  'import json,sys; r=json.load(sys.stdin); assert r["facts_created"]==0, r' \
  || die "H: a revoked run was learned again"
log "H: PASS — confirm (principal default) → learned; revoke → forgotten, not relearned"

if [ -n "$EVIDENCE_DIR" ]; then
  mkdir -p "$EVIDENCE_DIR"
  cp "$RUN_DIR"/run-*.json "$RUN_DIR"/history-*.json "$RUN_DIR"/distill-*.json \
     "$RUN_DIR"/confirm.json "$RUN_DIR"/revoke.json "$EVIDENCE_DIR/"
fi
log "PASS: contract enforced, unverified success kept out of learning until confirmed"
