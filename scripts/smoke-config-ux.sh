#!/usr/bin/env bash
# Config-UX smoke (Phase 8.5): the model is chosen/configured at runtime via the
# API — no hand-editing config.toml.
#   - GET /config/model shows the current model;
#   - POST /config/model swaps it live (oracle rebuilt) AND persists an overlay;
#   - a daemon restart keeps the UI-chosen model (restart survival);
#   - POST /config/model/test does a REAL health check (ok live + fail on a bad
#     port);
#   - anonymous config access is refused (401);
#   - the API key is stored in a 0600, gitignored secret file — never in
#     config.toml, never in the logs.
#
#   scripts/smoke-config-ux.sh [evidence-dir]
#
# Needs llama-server on :8080 (the swapped model runs against it).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="${1:-}"
PORT=7631
BASE="http://127.0.0.1:${PORT}"
TOKEN="smoke-cfg-$$"
BIN="$REPO_ROOT/target/debug/kelter"

log() { printf '[smoke-config] %s\n' "$*"; }
die() { printf '[smoke-config] FAIL: %s\n' "$*" >&2; exit 1; }

curl -fsS http://127.0.0.1:8080/health >/dev/null 2>&1 || die "llama-server not on :8080"

log "building workspace"
cargo build --workspace --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null

WORK="$(mktemp -d)"
DPID=""
cleanup() { kill -9 "${DPID:-0}" 2>/dev/null || true; }
trap cleanup EXIT

cat > "$WORK/config.toml" <<EOF
[server]
bind = "127.0.0.1:${PORT}"
token = "${TOKEN}"
[policy]
engine = "local:qwen2.5-0.5b-instruct"
[kernel]
max_steps = 3
[storage]
backend = "sqlite"
path = "$WORK/kelter.db"
[tools]
workspace = "$WORK/work"
bundled_tools = false
extension_dirs = []

[[tools.servers]]
name = "ledger"
command = "${REPO_ROOT}/target/debug/ledger-server"
args = ["--workspace", "$WORK/work"]
EOF

start() { # logfile
  ( cd "$WORK" && RUST_LOG=info "$BIN" --config config.toml > "$WORK/$1" 2>&1 ) &
  DPID=$!
  for _ in $(seq 1 60); do curl -fsS "$BASE/healthz" >/dev/null 2>&1 && return 0; sleep 0.5; done
  cat "$WORK/$1"; return 1
}
stop() { kill -9 "$DPID" 2>/dev/null || true; wait "$DPID" 2>/dev/null || true; }
authget() { curl -fsS "$BASE$1" -H "Authorization: Bearer ${TOKEN}"; }
authpost() { curl -fsS -X POST "$BASE$1" -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' -d "$2"; }

# ---- 1: GET current config + auth ----------------------------------------
log "=== 1: GET config + auth ==="
start daemon1.log || die "daemon did not start"
CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/v1/config/model")
[ "$CODE" = "401" ] || die "anonymous config access not refused (got $CODE)"
ENGINE=$(authget /api/v1/config/model | python3 -c 'import json,sys;print(json.load(sys.stdin)["engine"])')
[ "$ENGINE" = "local:qwen2.5-0.5b-instruct" ] || die "unexpected initial engine: $ENGINE"
log "  GET config → $ENGINE; anonymous → 401"

# ---- 2: Test connection (real, both ways) --------------------------------
log "=== 2: Test connection (real health check) ==="
OK=$(authpost /api/v1/config/model/test '{"provider":"local","base_url":"http://127.0.0.1:8080"}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["ok"])')
[ "$OK" = "True" ] || die "live health check should succeed"
BAD=$(authpost /api/v1/config/model/test '{"provider":"local","base_url":"http://127.0.0.1:59999"}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["ok"])')
[ "$BAD" = "False" ] || die "health check on a dead port should fail"
log "  test :8080 → ok=true; test :59999 → ok=false (real, not faked)"

# ---- 3: SET model → live swap + persist ----------------------------------
log "=== 3: set model → live apply + persist ==="
authpost /api/v1/config/model '{"engine":"local:swapped-model","base_url":"http://127.0.0.1:8080"}' >/dev/null
NEW=$(authget /api/v1/config/model | python3 -c 'import json,sys;print(json.load(sys.stdin)["engine"])')
[ "$NEW" = "local:swapped-model" ] || die "engine did not update (got $NEW)"
[ -f "$WORK/model-override.toml" ] || die "override was not persisted"
grep -q 'swapped-model' "$WORK/model-override.toml" || die "override file missing the new engine"
# The live swap applies: a new run's policy reflects the swapped oracle
# (llama-server ignores the model name, so the run still completes).
RUN=$(authpost /api/v1/runs '{"task":"Say hi and finish."}' | sed -E 's/.*"run_id":"([^"]+)".*/\1/')
for _ in $(seq 1 30); do
  P=$(authget "/api/v1/runs/$RUN" | python3 -c 'import json,sys;print(json.load(sys.stdin)["policy"])')
  [ -n "$P" ] && break; sleep 1
done
[ "$P" = "local:swapped-model" ] || die "swapped oracle not applied to a live run (policy=$P)"
log "  set → engine=local:swapped-model, override persisted, applied to a live run"

# ---- 4: restart survival -------------------------------------------------
log "=== 4: restart survival ==="
stop
start daemon2.log || die "daemon did not restart"
AFTER=$(authget /api/v1/config/model | python3 -c 'import json,sys;print(json.load(sys.stdin)["engine"])')
[ "$AFTER" = "local:swapped-model" ] || die "UI-chosen model did not survive restart (got $AFTER)"
grep -q "applying persisted model overlay" "$WORK/daemon2.log" || die "no evidence the overlay was applied on restart"
log "  after restart → engine=$AFTER (overlay applied, no config.toml edit)"

# ---- 5: key handling (secure, no leak) -----------------------------------
log "=== 5: key handling — 0600 secret file, never in tree/logs ==="
DUMMY="sk-or-DUMMY-smoke-do-not-use-0000000000"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/v1/config/model/key" \
  -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' -d "{\"api_key\":\"$DUMMY\"}")
[ "$CODE" = "204" ] || die "set-key did not return 204 (got $CODE)"
KEYFILE="$WORK/kelter-secret-openrouter.key"
[ -f "$KEYFILE" ] || die "secret file was not written"
PERMS=$(stat -f '%Lp' "$KEYFILE" 2>/dev/null || stat -c '%a' "$KEYFILE")
[ "$PERMS" = "600" ] || die "secret file perms are $PERMS, expected 600"
grep -q "$DUMMY" "$KEYFILE" || die "secret file does not contain the key"
# The key must NOT appear in config.toml or ANY daemon log.
grep -q "$DUMMY" "$WORK/config.toml" && die "KEY LEAKED into config.toml"
grep -rq "$DUMMY" "$WORK"/daemon*.log && die "KEY LEAKED into a daemon log"
# GET config never returns the key value, only api_key_set.
authget /api/v1/config/model | grep -q "$DUMMY" && die "KEY LEAKED via the config API"
log "  key stored 0600; NOT in config.toml, NOT in logs, NOT in the API response"
stop

if [ -n "$EVIDENCE_DIR" ]; then
  mkdir -p "$EVIDENCE_DIR"
  scrub() { sed "s#${REPO_ROOT}#<repo>#g;s#${WORK}#<work>#g;s#${TOKEN}#<token>#g"; }
  {
    echo "=== set→persist→restart survival (Phase 8.5) ==="
    echo "initial engine:  local:qwen2.5-0.5b-instruct"
    echo "after set:       local:swapped-model  (applied to a live run; override persisted)"
    echo "after restart:   $AFTER  (overlay re-applied, no config.toml edit)"
    echo "--- persisted overlay (model-override.toml) ---"
    cat "$WORK/model-override.toml"
    echo "--- restart log line ---"
    grep "applying persisted model overlay" "$WORK/daemon2.log"
    echo "=== health check (real) ==="
    echo "test :8080 → ok=true ; test :59999 → ok=false"
    echo "=== key handling ==="
    echo "secret file perms: $PERMS (0600); key absent from config.toml, logs, and the API"
  } | scrub > "$EVIDENCE_DIR/config-ux-set-persist-restart.log"
fi

log "PASS: runtime-mutable model config — get/set/persist/apply/restart-survival, real health check, bearer-gated, key stored securely (no leak)"
