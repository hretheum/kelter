#!/usr/bin/env bash
# Store retention money-shot (Phase 9, ADR 0017): the SQLite store no longer
# grows unbounded.
#   - N+extra TERMINAL runs → `prune` (max_runs=N) → exactly N remain;
#   - an in-flight (running) run is NEVER pruned;
#   - the pruned runs' journal rows are gone with them (consistent);
#   - durability/exactly-once is untouched (smoke-durability run separately).
#
#   scripts/smoke-retention.sh [evidence-dir]
#
# Needs llama-server on :8080 (runs use the local model) + sqlite3.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="${1:-}"
PORT=7651
BASE="http://127.0.0.1:${PORT}"
TOKEN="smoke-ret-$$"
BIN="$REPO_ROOT/target/debug/kelter"

log() { printf '[smoke-retention] %s\n' "$*"; }
die() { printf '[smoke-retention] FAIL: %s\n' "$*" >&2; exit 1; }

command -v sqlite3 >/dev/null || die "sqlite3 required"
curl -fsS http://127.0.0.1:8080/health >/dev/null 2>&1 || die "llama-server not on :8080"

log "building workspace"
cargo build --workspace --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null

WORK="$(mktemp -d)"
DB="$WORK/kelter.db"
DPID=""
cleanup() { kill -9 "${DPID:-0}" 2>/dev/null || true; }
trap cleanup EXIT

# Retention: keep the newest 2 terminal runs. prune_on_start=false so we
# control WHEN pruning happens (the `prune` subcommand).
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
path = "$DB"
[retention]
prune_on_start = false
max_runs = 2
[tools]
workspace = "$WORK/work"
bundled_tools = false
extension_dirs = []
[[tools.servers]]
name = "ledger"
command = "${REPO_ROOT}/target/debug/ledger-server"
args = ["--workspace", "$WORK/work"]
EOF

( cd "$WORK" && RUST_LOG=info "$BIN" --config config.toml > "$WORK/daemon.log" 2>&1 ) &
DPID=$!
for _ in $(seq 1 60); do curl -fsS "$BASE/healthz" >/dev/null 2>&1 && break; sleep 0.5; done
curl -fsS "$BASE/healthz" >/dev/null 2>&1 || { cat "$WORK/daemon.log"; die "daemon did not start"; }
# Redaction check: the token value must NOT be in the daemon log (only a masked
# fingerprint) — a real assertion, not a claim.
grep -q "$TOKEN" "$WORK/daemon.log" && die "REDACTION: token value leaked into the log"
grep -q "bearer token active (fingerprint" "$WORK/daemon.log" || die "no masked token fingerprint logged"
log "redaction: token value absent from logs; only a masked fingerprint present"

# ---- create 4 terminal runs -------------------------------------------------
log "=== submitting 4 tasks (→ 4 terminal runs) ==="
for i in 1 2 3 4; do
  RID=$(curl -fsS -X POST "$BASE/api/v1/runs" -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -d "{\"task\":\"Append the entry ret-$i to the ledger using append_ledger. Then finish.\"}" \
    | sed -E 's/.*"run_id":"([^"]+)".*/\1/')
  for _ in $(seq 1 30); do
    S=$(curl -fsS "$BASE/api/v1/runs/$RID" -H "Authorization: Bearer ${TOKEN}" | sed -E 's/.*"status":"([a-z]+)".*/\1/')
    { [ "$S" = completed ] || [ "$S" = failed ]; } && break; sleep 1
  done
done
kill -9 "$DPID" 2>/dev/null || true; DPID=""

# ---- inject an in-flight (running) run directly into the store --------------
INFLIGHT="11111111-1111-1111-1111-111111111111"
sqlite3 "$DB" "INSERT INTO runs (id, status, created_at, data) VALUES ('$INFLIGHT', 'running', 9999999999, '{\"in_flight\":true}');"
sqlite3 "$DB" "INSERT INTO journal (run_id, entry) VALUES ('$INFLIGHT', '{\"kind\":\"intent\"}');"

TERMINAL_BEFORE=$(sqlite3 "$DB" "SELECT COUNT(*) FROM runs WHERE status IN ('completed','failed');")
log "before prune: $TERMINAL_BEFORE terminal runs + 1 in-flight; total $(sqlite3 "$DB" 'SELECT COUNT(*) FROM runs;')"
[ "$TERMINAL_BEFORE" -ge 3 ] || die "expected >=3 terminal runs to make the prune meaningful"

# ---- prune (subcommand): keep newest 2 terminal -----------------------------
log "=== prune (keep newest 2 terminal) ==="
REPORT=$(cd "$WORK" && "$BIN" --config config.toml prune)
log "  prune report: $REPORT"
PRUNED=$(printf '%s' "$REPORT" | python3 -c 'import json,sys;print(json.load(sys.stdin)["runs_pruned"])')
[ "$PRUNED" -eq "$((TERMINAL_BEFORE-2))" ] || die "expected $((TERMINAL_BEFORE-2)) pruned, got $PRUNED"

TERMINAL_AFTER=$(sqlite3 "$DB" "SELECT COUNT(*) FROM runs WHERE status IN ('completed','failed');")
[ "$TERMINAL_AFTER" -eq 2 ] || die "expected exactly 2 terminal runs after prune, got $TERMINAL_AFTER"

# ---- the in-flight run is UNTOUCHED (the safety invariant) -------------------
STILL=$(sqlite3 "$DB" "SELECT COUNT(*) FROM runs WHERE id='$INFLIGHT' AND status='running';")
[ "$STILL" = "1" ] || die "IN-FLIGHT run was pruned — durability invariant broken!"
IJ=$(sqlite3 "$DB" "SELECT COUNT(*) FROM journal WHERE run_id='$INFLIGHT';")
[ "$IJ" = "1" ] || die "in-flight run's journal was touched"
log "  after prune: 2 terminal remain; the in-flight run + its journal UNTOUCHED"

# ---- pruned runs' journal is gone with them (consistency) -------------------
ORPHAN=$(sqlite3 "$DB" "SELECT COUNT(*) FROM journal j WHERE NOT EXISTS (SELECT 1 FROM runs r WHERE r.id=j.run_id);")
[ "$ORPHAN" = "0" ] || die "orphaned journal rows after prune ($ORPHAN) — inconsistent"
log "  no orphaned journal rows (run + markers pruned together)"

if [ -n "$EVIDENCE_DIR" ]; then
  mkdir -p "$EVIDENCE_DIR"
  scrub() { sed "s#${REPO_ROOT}#<repo>#g;s#${WORK}#<work>#g;s#${TOKEN}#<token>#g"; }
  {
    echo "=== store retention (Phase 9) ==="
    echo "before: $TERMINAL_BEFORE terminal + 1 in-flight"
    echo "prune report: $REPORT"
    echo "after: 2 terminal remain; in-flight untouched ($STILL); orphan journal rows: $ORPHAN"
    echo "--- redaction: token value absent from daemon log; masked fingerprint present ---"
    grep "bearer token active (fingerprint" "$WORK/daemon.log" | scrub | head -1
  } | scrub > "$EVIDENCE_DIR/retention-prune.log"
fi

log "PASS: prune keeps N terminal, NEVER prunes in-flight, journal consistent; token redacted in logs"
