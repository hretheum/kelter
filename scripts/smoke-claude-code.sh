#!/usr/bin/env bash
# The `claude-code` extension (roadmap KRW.1) against the real daemon:
# daemon → extension subprocess (claude-code-bridge, stdio MCP) → runner
# (HTTP) → `claude` (a fake that commits a file) in a git worktree.
#   S) sonnet path: claude_code_run → sprawdz passes → contract met →
#      `completed` (verification: contract); the branch holds the commit.
#   F) the check fails → the contract is unmet → `incomplete`, never
#      `completed`, even though Claude Code itself reported success.
#   O) claude_code_opus under `ask`: the run pauses BEFORE the runner is
#      called; allow → it runs once, on Opus.
#
#   scripts/smoke-claude-code.sh [evidence-dir]
#
# Needs node 22, python3, git, curl. The model is the scripted stub.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="${1:-}"
PORT=7596
LLM_PORT=18096
RUNNER_PORT=18790
BASE="http://127.0.0.1:${PORT}"
TOKEN="smoke-cc-$$"
RUNNER_TOKEN="runner-token-$$-0123456789abcdef0123456789"

log() { printf '[smoke-claude-code] %s\n' "$*"; }
die() { printf '[smoke-claude-code] FAIL: %s\n' "$*" >&2; exit 1; }

log "building daemon + bridge"
cargo build -p daemon -p mcp --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null
cargo build --manifest-path "$REPO_ROOT/deploy/claude-code/bridge/Cargo.toml" >/dev/null

RUN_DIR="$(mktemp -d)"
mkdir -p "$RUN_DIR/work" "$RUN_DIR/ext" "$RUN_DIR/runner"
export GIT_AUTHOR_NAME=smoke GIT_AUTHOR_EMAIL=smoke@localhost GIT_COMMITTER_NAME=smoke GIT_COMMITTER_EMAIL=smoke@localhost

# A repo the runner knows, as a bare mirror (what K12 holds).
git init -q -b main "$RUN_DIR/src"
echo demo > "$RUN_DIR/src/README"
git -C "$RUN_DIR/src" add . && git -C "$RUN_DIR/src" commit -q -m init
git clone -q --bare "$RUN_DIR/src" "$RUN_DIR/runner/repos/demo.git"

cat > "$RUN_DIR/runner/config.json" <<J
{ "repos_dir": "$RUN_DIR/runner/repos", "work_dir": "$RUN_DIR/runner/work",
  "state_dir": "$RUN_DIR/runner/state", "out_dir": "$RUN_DIR/runner/out",
  "worker": null, "repos": { "demo": {} },
  "checks": { "plik-jest": { "repo": "demo", "cmd": ["test", "-f", "wynik.txt"] },
              "zawsze-zle": { "repo": "demo", "cmd": ["sh", "-c", "echo boom; exit 2"] } } }
J
printf '%s\n' "$RUNNER_TOKEN" > "$RUN_DIR/runner/runner-token"
printf 'oauth-smoke\n' > "$RUN_DIR/runner/claude-oauth-token"
chmod +x "$REPO_ROOT/deploy/claude-code/runner/test/fake-claude.sh"
FAKE_LOG="$RUN_DIR/claude-calls.log" RUNNER_PASS_ENV=FAKE_LOG RUNNER_CONFIG_DIR="$RUN_DIR/runner" \
  PORT="$RUNNER_PORT" RUNNER_CLAUDE_BIN="$REPO_ROOT/deploy/claude-code/runner/test/fake-claude.sh" \
  node "$REPO_ROOT/deploy/claude-code/runner/server.mjs" > "$RUN_DIR/runner.log" 2>&1 &
RUNNER_PID=$!

# The extension exactly as shipped, pointed at the local runner.
EXT="$RUN_DIR/ext/claude-code"
mkdir -p "$EXT"
cp "$REPO_ROOT/deploy/extensions/claude-code/kelter-extension.toml" "$EXT/"
cp "$REPO_ROOT/deploy/claude-code/bridge/target/debug/claude-code-bridge" "$EXT/"
python3 - "$REPO_ROOT/deploy/extensions/claude-code/bridge.json" "$EXT/bridge.json" "$RUNNER_PORT" <<'PY'
import json, sys
b = json.load(open(sys.argv[1])); b["base_url"] = f"http://127.0.0.1:{sys.argv[3]}"
json.dump(b, open(sys.argv[2], "w"))
PY
printf '%s\n' "$RUNNER_TOKEN" > "$EXT/runner-token"

BR='kelter/[0-9a-f]{12}'
cat > "$RUN_DIR/queue.json" <<Q
[
  {"action": "claude_code_run", "arguments": {"repo": "demo", "model": "sonnet", "zlecenie": "dodaj wynik.txt"}},
  {"action": "sprawdz", "arguments": {"sprawdzenie": "plik-jest", "galaz": "@BR@"}, "_subst": {"@BR@": "$BR"}},
  {"action": "finish", "arguments": {"summary": "zrobione i sprawdzone"}},
  {"action": "claude_code_run", "arguments": {"repo": "demo", "model": "haiku", "zlecenie": "dodaj wynik.txt"}},
  {"action": "sprawdz", "arguments": {"sprawdzenie": "zawsze-zle", "galaz": "@BR@"}, "_subst": {"@BR@": "$BR"}},
  {"action": "finish", "arguments": {"summary": "twierdzę, że zrobione"}},
  {"action": "claude_code_opus", "arguments": {"repo": "demo", "zlecenie": "duże zadanie"}},
  {"action": "finish", "arguments": {"summary": "opus skończył"}}
]
Q
( cd "$RUN_DIR" && FAKE_LLM_QUEUE="$RUN_DIR/queue.json" \
    exec python3 "$REPO_ROOT/scripts/fake-llm-scripted.py" "$LLM_PORT" > "$RUN_DIR/llm.log" 2>&1 ) &
LLM_PID=$!
DAEMON_PID=""
cleanup() { kill "$DAEMON_PID" "$LLM_PID" "$RUNNER_PID" 2>/dev/null || true; }
trap cleanup EXIT

cat > "$RUN_DIR/config.toml" <<CFG
[server]
bind = "127.0.0.1:${PORT}"
token = "${TOKEN}"

[policy]
engine = "local:scripted-stub"
base_url = "http://127.0.0.1:${LLM_PORT}"

[kernel]
max_steps = 6
reflection = false

[storage]
backend = "sqlite"
path = "${RUN_DIR}/kelter.db"

[tools]
workspace = "${RUN_DIR}/work"
bundled_tools = false
extension_dirs = ["${RUN_DIR}/ext"]

[trust]
allow_unsandboxed_subprocess = ["claude-code"]

[approval]
ask_timeout_secs = 600
[approval.actions]
claude_code_opus = "ask"
CFG

( cd "$RUN_DIR" && RUST_LOG=info exec "$REPO_ROOT/target/debug/kelter" --config config.toml > "$RUN_DIR/daemon.log" 2>&1 ) &
DAEMON_PID=$!
for _ in $(seq 1 60); do curl -fsS "$BASE/healthz" >/dev/null 2>&1 && break; sleep 0.3; done
curl -fsS "$BASE/healthz" >/dev/null || { cat "$RUN_DIR/daemon.log"; die "daemon did not start"; }

auth=(-H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json')
get() { curl -fsS "$BASE$1" "${auth[@]}"; }
post() { curl -fsS -X POST "$BASE$1" "${auth[@]}" -d "${2:-{\}}"; }
field() { python3 -c "import json,sys; print(json.load(sys.stdin)$1)"; }
wait_status() {
  local id="$1"; shift; local s=""
  for _ in $(seq 1 120); do
    s=$(get "/api/v1/runs/$id" | field '["status"]') || true
    for want in "$@"; do [ "$s" = "$want" ] && { printf '%s' "$s"; return 0; }; done
    sleep 0.25
  done
  printf '%s' "$s"
}
claude_calls() { wc -l < "$RUN_DIR/claude-calls.log" 2>/dev/null | tr -d ' ' || echo 0; }

TOOLS=$(get /api/v1/tools)
for t in claude_code_run claude_code_opus sprawdz; do
  grep -q "\"$t\"" <<<"$TOOLS" || { cat "$RUN_DIR/daemon.log"; die "tool $t not registered"; }
done
CONTRACT='"success_contract": {"require": [{"kind": "action_succeeded", "action": "sprawdz"}]}'

log "=== S: sonnet → check passes → completed, verified by contract ==="
RUN_S=$(post /api/v1/runs "{\"task\": \"dodaj wynik.txt w demo\", $CONTRACT}" | field '["run_id"]')
S=$(wait_status "$RUN_S" completed failed incomplete)
get "/api/v1/runs/$RUN_S" > "$RUN_DIR/run-sonnet.json"
get "/api/v1/runs/$RUN_S/events" > "$RUN_DIR/history-sonnet.json"
[ "$S" = "completed" ] || { cat "$RUN_DIR/run-sonnet.json"; cat "$RUN_DIR/llm.log"; die "S: expected completed, got $S"; }
[ "$(field '["verification"]' < "$RUN_DIR/run-sonnet.json")" = "contract" ] || die "S: not contract-verified"
BRANCH_S=$(grep -oE 'kelter/[0-9a-f]{12}' "$RUN_DIR/history-sonnet.json" | head -1)
ID_S=${BRANCH_S#kelter/}
git -C "$RUN_DIR/src" fetch -q "$RUN_DIR/runner/out/$ID_S.bundle" "refs/heads/$BRANCH_S:refs/heads/$BRANCH_S" \
  || die "S: no result bundle for $BRANCH_S"
git -C "$RUN_DIR/src" show "$BRANCH_S:wynik.txt" >/dev/null || die "S: branch $BRANCH_S has no wynik.txt"
git -C "$RUN_DIR/runner/repos/demo.git" rev-parse -q --verify "$BRANCH_S" >/dev/null \
  && die "S: the base repo was written to"
grep -q -- "--model sonnet" "$RUN_DIR/claude-calls.log" || die "S: claude not called on sonnet"
[ "$(claude_calls)" = "1" ] || die "S: claude called $(claude_calls) times"
python3 - "$RUN_DIR/history-sonnet.json" <<'PY' || die "S: journal assertions failed"
import json, sys
h = json.load(open(sys.argv[1]))
acts = [e["action"] for e in h["journal"] if e["kind"] == "decision"]
assert acts[:2] == ["claude_code_run", "sprawdz"], acts
intents = [e for e in h["journal"] if e["kind"] == "intent"]
assert intents and intents[0].get("idempotency_key"), intents  # mutating: keyed by the kernel
PY
log "S: PASS — $BRANCH_S in a bundle (base repo untouched), check passed, completed (contract)"

log "=== F: claude says success, the check fails → incomplete ==="
RUN_F=$(post /api/v1/runs "{\"task\": \"dodaj wynik.txt, haiku\", $CONTRACT}" | field '["run_id"]')
F=$(wait_status "$RUN_F" completed failed incomplete)
get "/api/v1/runs/$RUN_F" > "$RUN_DIR/run-check-failed.json"
[ "$F" = "incomplete" ] || { cat "$RUN_DIR/run-check-failed.json"; die "F: expected incomplete, got $F"; }
grep -q "sprawdz" "$RUN_DIR/run-check-failed.json" || die "F: unmet condition not named"
log "F: PASS — $(field '["error"]' < "$RUN_DIR/run-check-failed.json")"

log "=== O: opus pauses for a human before the runner is called ==="
BEFORE=$(claude_calls)
RUN_O=$(post /api/v1/runs '{"task": "duże zadanie na opusie"}' | field '["run_id"]')
[ "$(wait_status "$RUN_O" awaiting_action_approval)" = "awaiting_action_approval" ] || die "O: did not pause"
sleep 1
[ "$(claude_calls)" = "$BEFORE" ] || die "O: runner called before a decision"
get /api/v1/approvals > "$RUN_DIR/approvals-queue.json"
post "/api/v1/runs/$RUN_O/action-approval" '{"action": "claude_code_opus", "decision": "allow"}' > /dev/null
[ "$(wait_status "$RUN_O" completed failed incomplete)" = "completed" ] || die "O: did not complete after allow"
[ "$(claude_calls)" = "$((BEFORE + 1))" ] || die "O: claude called $(claude_calls) times"
tail -1 "$RUN_DIR/claude-calls.log" | grep -q -- "--model opus" || die "O: not run on opus"
log "O: PASS — paused, allowed, ran once on opus"

if [ -n "$EVIDENCE_DIR" ]; then
  mkdir -p "$EVIDENCE_DIR"
  cp "$RUN_DIR"/run-*.json "$RUN_DIR"/history-*.json "$RUN_DIR/approvals-queue.json" \
     "$RUN_DIR/claude-calls.log" "$RUN_DIR/config.toml" "$EVIDENCE_DIR/"
fi
log "PASS: sonnet verified by contract; failed check → incomplete; opus asks first"
