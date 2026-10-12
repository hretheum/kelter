#!/usr/bin/env bash
# ADR 0040 against the real daemon: the run's reflection episode follows
# verification (ADR 0037), through GET /api/v1/runs/{id}/episodes.
#   U) a `completed` run WITHOUT a contract → its episode is `unverified`
#      (v1, provenance reflection) — not a success.
#   H) a human confirms (POST /runs/{id}/confirmation) → v2 `success`
#      (provenance human); revoke (DELETE) → v3 `unverified`; all three
#      versions kept, retrieval sees one episode.
#   C) a run WITH a contract that holds → `success` at once (v1,
#      reflection); confirming it is refused (409) and the episode does not
#      move.
#   M) what the MODEL saw (the stub logs every request): a run of the same
#      task while U is unverified gets no episode in its prompt; after the
#      confirmation it gets `outcome: Success`; after the revoke, none again
#      (maintainer's decision: unverified episodes never enter model context).
#
#   scripts/smoke-unverified-episodes.sh [evidence-dir]
#
# Real `kelter` binary, real SQLite (schema v4), real ledger MCP server; the
# model is the scripted stub (scripts/fake-llm-scripted.py). Needs python3
# and curl.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="${1:-}"
PORT=7594
LLM_PORT=18094
BASE="http://127.0.0.1:${PORT}"
TOKEN="smoke-episodes-$$"

log() { printf '[smoke-unverified-episodes] %s\n' "$*"; }
die() { printf '[smoke-unverified-episodes] FAIL: %s\n' "$*" >&2; exit 1; }

log "building daemon + ledger server"
cargo build -p daemon -p mcp --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null

RUN_DIR="$(mktemp -d)"
mkdir -p "$RUN_DIR/work"
cat > "$RUN_DIR/queue.json" <<'Q'
[
  {"action": "append_ledger", "arguments": {"entry": "Dear Alice, the report is attached."}},
  {"action": "finish", "arguments": {"summary": "E-mail to Alice sent."}},
  {"action": "append_ledger", "arguments": {"entry": "probe 1"}},
  {"action": "finish", "arguments": {"summary": "probe 1"}},
  {"action": "append_ledger", "arguments": {"entry": "probe 2"}},
  {"action": "finish", "arguments": {"summary": "probe 2"}},
  {"action": "append_ledger", "arguments": {"entry": "probe 3"}},
  {"action": "finish", "arguments": {"summary": "probe 3"}},
  {"action": "append_ledger", "arguments": {"entry": "Q3 ledger entry."}},
  {"action": "finish", "arguments": {"summary": "Ledger updated."}}
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
# expect <file> <version> <outcome> <provenance> <n-versions>
expect() {
  python3 - "$@" <<'PY'
import json, sys
f, ver, outcome, prov, n = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], int(sys.argv[5])
b = json.load(open(f))
assert b["history_supported"] is True, b
assert len(b["current"]) == 1, b["current"]
c = b["current"][0]
assert (c["version"], c["outcome"], c["provenance"]) == (ver, outcome, prov), c
assert len(b["versions"]) == n, b["versions"]
assert len({v["id"] for v in b["versions"]}) == 1, "one episode id across versions"
PY
}

TASK_U="Send an e-mail to Alice with the report."
# probe <name>: run TASK_U again (no contract) and save the FIRST request the
# model received in that run — the prompt that carries the memory context.
probe() {
  local before n id
  before=$(wc -l < "$RUN_DIR/requests.log" 2>/dev/null || echo 0)
  id=$(post /api/v1/runs "{\"task\": \"$TASK_U\"}" | field '["run_id"]')
  [ "$(wait_status "$id")" = "completed" ] || die "M: probe run $1 did not complete"
  n=$((before + 1))
  sed -n "${n}p" "$RUN_DIR/requests.log" > "$RUN_DIR/prompt-$1.json"
  grep -q "Task: $TASK_U" "$RUN_DIR/prompt-$1.json" || die "M: prompt-$1 is not the probe's"
}

# ---------------------------------------------------- U: unverified ---------
log "=== U: a completed run without a contract ==="
RUN_U=$(post /api/v1/runs "{\"task\": \"$TASK_U\"}" | field '["run_id"]')
[ "$(wait_status "$RUN_U")" = "completed" ] || die "U: run did not complete"
[ "$(get "/api/v1/runs/$RUN_U" | field '["verification"]')" = "none" ] || die "U: not unverified"
get "/api/v1/runs/$RUN_U/episodes" > "$RUN_DIR/episodes-1-unverified.json"
expect "$RUN_DIR/episodes-1-unverified.json" 1 unverified reflection 1 || die "U: episode state"
log "U: PASS — episode v1 unverified (reflection)"
probe 1-unverified
! grep -q "| outcome:" "$RUN_DIR/prompt-1-unverified.json" \
  || die "M: an unverified episode reached the model: $(cat "$RUN_DIR/prompt-1-unverified.json")"
log "M: PASS — while unverified, the model's prompt holds no episode"

# ----------------------------------------------- H: confirm / revoke --------
log "=== H: confirm → success v2; revoke → unverified v3 ==="
C=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/v1/runs/$RUN_U/episodes")
[ "$C" = "401" ] || die "H: anonymous episodes read should be 401, got $C"
post "/api/v1/runs/$RUN_U/confirmation" > /dev/null
get "/api/v1/runs/$RUN_U/episodes" > "$RUN_DIR/episodes-2-confirmed.json"
expect "$RUN_DIR/episodes-2-confirmed.json" 2 success human 2 || die "H: confirmed state"
probe 2-confirmed
grep -q "task: $TASK_U | outcome: Success" "$RUN_DIR/prompt-2-confirmed.json" \
  || die "M: the confirmed episode did not reach the model: $(cat "$RUN_DIR/prompt-2-confirmed.json")"
[ "$(grep -o '| outcome:' "$RUN_DIR/prompt-2-confirmed.json" | wc -l)" = "1" ] \
  || die "M: only the confirmed episode may reach the model (probe 1's is unverified)"
log "M: PASS — after confirmation the model sees exactly 'outcome: Success'"
C=$(code DELETE "/api/v1/runs/$RUN_U/confirmation")
[ "$C" = "200" ] || die "H: revoke returned $C"
get "/api/v1/runs/$RUN_U/episodes" > "$RUN_DIR/episodes-3-revoked.json"
expect "$RUN_DIR/episodes-3-revoked.json" 3 unverified human 3 || die "H: revoked state"
python3 - "$RUN_DIR/episodes-3-revoked.json" <<'PY' || die "H: history"
import json, sys
b = json.load(open(sys.argv[1]))
assert [v["outcome"] for v in b["versions"]] == ["unverified", "success", "unverified"], b
PY
grep -q "episode revised" "$RUN_DIR/daemon.log" || die "H: no 'episode revised' log line"
probe 3-revoked
! grep -q "| outcome:" "$RUN_DIR/prompt-3-revoked.json" \
  || die "M: a revoked (unverified) episode reached the model"
log "M: PASS — after the revoke the model's prompt holds no episode again"
log "H: PASS — v2 success (human), v3 unverified (human), history kept"

# ------------------------------------------------------ C: contract ---------
log "=== C: a run whose contract holds ==="
RUN_C=$(post /api/v1/runs '{"task": "Append the Q3 entry to the ledger.",
  "success_contract": {"require": [{"kind": "action_succeeded", "action": "append_ledger"}]}}' \
  | field '["run_id"]')
[ "$(wait_status "$RUN_C")" = "completed" ] || die "C: run did not complete"
[ "$(get "/api/v1/runs/$RUN_C" | field '["verification"]')" = "contract" ] || die "C: not verified"
get "/api/v1/runs/$RUN_C/episodes" > "$RUN_DIR/episodes-contract.json"
expect "$RUN_DIR/episodes-contract.json" 1 success reflection 1 || die "C: episode state"
C=$(code POST "/api/v1/runs/$RUN_C/confirmation")
[ "$C" = "409" ] || die "C: confirming a verified run should be 409, got $C"
get "/api/v1/runs/$RUN_C/episodes" > "$RUN_DIR/episodes-contract-after.json"
expect "$RUN_DIR/episodes-contract-after.json" 1 success reflection 1 || die "C: episode moved"
log "C: PASS — episode v1 success (reflection) at once; confirm 409, unchanged"

if [ -n "$EVIDENCE_DIR" ]; then
  mkdir -p "$EVIDENCE_DIR"
  cp "$RUN_DIR"/episodes-*.json "$RUN_DIR"/prompt-*.json "$EVIDENCE_DIR/"
fi
log "PASS: unverified episode kept out of model context until a human confirms; contract-verified is success at once"
