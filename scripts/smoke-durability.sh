#!/usr/bin/env bash
# Durability money shot, reproducibly:
#   A) runs + journal + memory survive a full daemon restart;
#   B) a run killed AFTER a side effect fired (but before its receipt)
#      resumes to completion with the effect exactly once (receipt-gate +
#      idempotency key);
#   C) a run killed BEFORE the side effect fired resumes and produces the
#      effect exactly once.
#
#   scripts/smoke-durability.sh [evidence-dir]
#
# Needs llama-server on :8080 (the resumed run still has to decide `finish`).
# Crash injection uses the documented KELTER_CRASH_POINT test hook.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="${1:-}"
PORT=7541
BASE="http://127.0.0.1:${PORT}"
TOKEN="smoke-dur-$$"

log() { printf '[smoke-durability] %s\n' "$*"; }
die() { printf '[smoke-durability] FAIL: %s\n' "$*" >&2; exit 1; }

command -v sqlite3 >/dev/null || die "sqlite3 CLI required for DB assertions"
curl -fsS http://127.0.0.1:8080/health >/dev/null 2>&1 \
  || die "llama-server is not running on :8080 — start it first:
       llama-server -hf Qwen/Qwen2.5-0.5B-Instruct-GGUF:q4_k_m --port 8080 -c 4096"

log "building workspace"
cargo build --workspace --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null

# ---------------------------------------------------------------- helpers --

RUN_DIR=""
DAEMON_PID=""

write_config() { # $1 = dir
  cat > "$1/config.toml" <<EOF
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
path = "$1/kelter.db"

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

start_daemon() { # $1 = dir, $2 = logfile, [$3 = crash point]
  ( cd "$1" && \
    KELTER_CRASH_POINT="${3:-}" RUST_LOG=info \
    exec "$REPO_ROOT/target/debug/kelter" --config config.toml \
    > "$1/$2" 2>&1 ) &
  DAEMON_PID=$!
  for _ in $(seq 1 60); do
    curl -fsS "$BASE/healthz" >/dev/null 2>&1 && return 0
    kill -0 "$DAEMON_PID" 2>/dev/null || return 1
    sleep 0.3
  done
  return 1
}

stop_daemon() { kill "$DAEMON_PID" 2>/dev/null || true; wait "$DAEMON_PID" 2>/dev/null || true; }
trap stop_daemon EXIT

submit() { # $1 = task → run id on stdout
  curl -fsS -X POST "$BASE/api/v1/runs" \
    -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' \
    -d "{\"task\": \"$1\"}" | sed -E 's/.*"run_id":"([^"]+)".*/\1/'
}

wait_status() { # $1 = run id, $2 = max polls → status on stdout
  local status=""
  for _ in $(seq 1 "$2"); do
    status=$(curl -fsS "$BASE/api/v1/runs/$1" -H "Authorization: Bearer ${TOKEN}" \
      | sed -E 's/.*"status":"([a-z]+)".*/\1/') || true
    { [ "$status" = "completed" ] || [ "$status" = "failed" ]; } && break
    sleep 2
  done
  printf '%s' "$status"
}

LEDGER_TASK="Append the entry 'durable-hello' to the ledger using append_ledger. Then finish."

# ------------------------------------------------- A: restart survival -----

log "=== A: restart survival ==="
RUN_DIR="$(mktemp -d)"
write_config "$RUN_DIR"
start_daemon "$RUN_DIR" daemon-a1.log || die "daemon A did not start"
RUN_A=$(submit "$LEDGER_TASK")
STATUS=$(wait_status "$RUN_A" 45)
[ "$STATUS" = "completed" ] || die "A: baseline run did not complete ($STATUS)"
log "A: run $RUN_A completed; killing daemon"
stop_daemon

EPISODES=$(sqlite3 "$RUN_DIR/kelter.db" 'SELECT COUNT(*) FROM episodes;')
[ "$EPISODES" -ge 1 ] || die "A: no reflection episode persisted"

start_daemon "$RUN_DIR" daemon-a2.log || die "daemon A did not restart"
BODY=$(curl -fsS "$BASE/api/v1/runs/$RUN_A" -H "Authorization: Bearer ${TOKEN}")
printf '%s' "$BODY" | grep -q '"status":"completed"' \
  || die "A: run did not survive the restart"
JOURNAL=$(sqlite3 "$RUN_DIR/kelter.db" "SELECT COUNT(*) FROM journal WHERE run_id='$RUN_A';")
log "A: PASS — run, journal ($JOURNAL entries) and memory ($EPISODES episode/s) survived a full restart"
stop_daemon
[ -n "$EVIDENCE_DIR" ] && { mkdir -p "$EVIDENCE_DIR"; sed "s#${REPO_ROOT}#<repo>#g;s#${RUN_DIR}#<run>#g" "$RUN_DIR/daemon-a2.log" > "$EVIDENCE_DIR/durability-restart.log"; }

# ------------------------- B: kill AFTER the side effect, before receipt ---

log "=== B: crash after side effect (the hard window) ==="
RUN_DIR="$(mktemp -d)"
write_config "$RUN_DIR"
start_daemon "$RUN_DIR" daemon-b1.log "after_execute:append_ledger" \
  || die "daemon B did not start"
RUN_B=$(submit "$LEDGER_TASK")
# The daemon must kill itself at the injected crash point.
for _ in $(seq 1 60); do
  kill -0 "$DAEMON_PID" 2>/dev/null || break
  sleep 1
done
kill -0 "$DAEMON_PID" 2>/dev/null && die "B: daemon did not crash at the injection point"
LINES=$(wc -l < "$RUN_DIR/work/ledger.txt" | tr -d ' ')
[ "$LINES" = "1" ] || die "B: expected 1 ledger line at crash time, got $LINES"
log "B: daemon crashed after the append; ledger has 1 line, no receipt journaled"

start_daemon "$RUN_DIR" daemon-b2.log || die "daemon B did not restart"
STATUS=$(wait_status "$RUN_B" 45)
[ "$STATUS" = "completed" ] || die "B: resumed run did not complete ($STATUS)"
LINES=$(wc -l < "$RUN_DIR/work/ledger.txt" | tr -d ' ')
[ "$LINES" = "1" ] || die "B: DUPLICATED side effect — ledger has $LINES lines"
grep -q "model NOT queried" "$RUN_DIR/daemon-b2.log" \
  || die "B: no evidence of journal replay in the log"
grep -q "journaled idempotency key" "$RUN_DIR/daemon-b2.log" \
  || die "B: no evidence of keyed re-dispatch in the log"
curl -fsS "$BASE/api/v1/runs/$RUN_B" -H "Authorization: Bearer ${TOKEN}" \
  | grep -q '"deduped":true' || die "B: tool did not report the dedupe"
log "B: PASS — resumed to completion, side effect exactly once (tool deduped on the journaled key)"
stop_daemon
[ -n "$EVIDENCE_DIR" ] && { sed "s#${REPO_ROOT}#<repo>#g;s#${RUN_DIR}#<run>#g" "$RUN_DIR/daemon-b2.log" > "$EVIDENCE_DIR/durability-resume-after-effect.log"; curl -s "$BASE/api/v1/runs/$RUN_B" -H "Authorization: Bearer ${TOKEN}" | sed "s#${REPO_ROOT}#<repo>#g" > "$EVIDENCE_DIR/durability-resume-run.json" || true; }

# ------------------------ C: kill BEFORE the side effect (after intent) ----

log "=== C: crash before the side effect ==="
RUN_DIR="$(mktemp -d)"
write_config "$RUN_DIR"
start_daemon "$RUN_DIR" daemon-c1.log "after_intent:append_ledger" \
  || die "daemon C did not start"
RUN_C=$(submit "$LEDGER_TASK")
for _ in $(seq 1 60); do
  kill -0 "$DAEMON_PID" 2>/dev/null || break
  sleep 1
done
kill -0 "$DAEMON_PID" 2>/dev/null && die "C: daemon did not crash at the injection point"
[ ! -f "$RUN_DIR/work/ledger.txt" ] \
  || die "C: ledger should not exist yet at crash time"
log "C: daemon crashed after intent, before the append; ledger empty"

start_daemon "$RUN_DIR" daemon-c2.log || die "daemon C did not restart"
STATUS=$(wait_status "$RUN_C" 45)
[ "$STATUS" = "completed" ] || die "C: resumed run did not complete ($STATUS)"
LINES=$(wc -l < "$RUN_DIR/work/ledger.txt" | tr -d ' ')
[ "$LINES" = "1" ] || die "C: expected exactly 1 ledger line after resume, got $LINES"
log "C: PASS — effect fired exactly once, on the resume leg"
stop_daemon
[ -n "$EVIDENCE_DIR" ] && sed "s#${REPO_ROOT}#<repo>#g;s#${RUN_DIR}#<run>#g" "$RUN_DIR/daemon-c2.log" > "$EVIDENCE_DIR/durability-resume-before-effect.log"

log "PASS: all three durability scenarios green"
