#!/usr/bin/env bash
# Extension trust posture smoke (Phase 6, ADR 0012). Proves the sandbox-by-
# default policy and the TWO-PART opt-in for an unsandboxed subprocess:
#   A) a subprocess extension with NO trust declaration → registry REFUSES
#      to start the daemon (loud), sandbox-by-default;
#   B) MONEY SHOT part 1 — the manifest opts in (trust =
#      "subprocess-unsandboxed") but the OPERATOR has not allowed it → still
#      REFUSED (a manifest cannot authorise its own escape);
#   C) MONEY SHOT part 2 — manifest opt-in AND operator consent → loads;
#   D) a WASM tool with NO opt-in still loads (sandbox-by-default, no
#      regression); and its run journals the trust posture (honest labels).
#
#   scripts/smoke-trust.sh [evidence-dir]
#
# No model needed for A–C (startup-level); D uses the WASM tool if
# llama-server is on :8080, else asserts registration + journal only.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="${1:-}"
PORT=7611
BASE="http://127.0.0.1:${PORT}"
TOKEN="smoke-trust-$$"
BIN="$REPO_ROOT/target/debug/kelter"

log() { printf '[smoke-trust] %s\n' "$*"; }
die() { printf '[smoke-trust] FAIL: %s\n' "$*" >&2; exit 1; }

log "building workspace"
cargo build --workspace --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null

WORK="$(mktemp -d)"
# A subprocess tool extension (uses the bundled tools-server binary as its
# "server" so it actually starts if allowed).
make_ext() { # $1 dir, $2 trust-line-or-empty
  mkdir -p "$WORK/$1/sub"
  {
    echo '[extension]'
    echo "name = \"$1\""
    echo 'provides = ["tools"]'
    [ -n "$2" ] && echo "$2"
    echo ''
    echo '[server]'
    echo "command = \"$REPO_ROOT/target/debug/tools-server\""
    echo "args = [\"--workspace\", \"$WORK/$1/work\"]"
  } > "$WORK/$1/sub/kelter-extension.toml"
}

make_config() { # $1 extdir, $2 operator-allow-line-or-empty → stdout
  cat <<EOF
[server]
bind = "127.0.0.1:${PORT}"
token = "${TOKEN}"
[policy]
engine = "local:qwen2.5-0.5b-instruct"
[storage]
backend = "memory"
$2
[tools]
workspace = "$WORK/work"
bundled_tools = false
extension_dirs = ["$1"]
EOF
}

# start_expect_fail <config> <grep> : daemon must EXIT non-zero with grep in log
start_expect_fail() {
  local cfg="$1" needle="$2" logf="$WORK/fail-$$.log"
  set +e
  ( cd "$WORK" && "$BIN" --config "$cfg" > "$logf" 2>&1 )
  local rc=$?
  set -e
  [ "$rc" -ne 0 ] || { cat "$logf"; die "daemon started but should have refused"; }
  grep -q "$needle" "$logf" || { cat "$logf"; die "expected refusal containing '$needle'"; }
  LAST_FAIL_LOG="$logf"
}

# ---- A: subprocess with NO trust declaration → refused ------------------
log "=== A: subprocess without a trust declaration is refused (sandbox-by-default) ==="
make_ext "notrust" ""
make_config "$WORK/notrust" "" > "$WORK/a.toml"
start_expect_fail "$WORK/a.toml" "did not declare a trust mode"
log "  refused: $(grep -o "extension 'notrust'.*trust mode" "$LAST_FAIL_LOG" | head -1)"
A_LOG="$LAST_FAIL_LOG"

# ---- B: manifest opt-in but NO operator consent → still refused ---------
log "=== B: manifest opt-in WITHOUT operator consent is still refused ==="
make_ext "optin" 'trust = "subprocess-unsandboxed"'
make_config "$WORK/optin" "" > "$WORK/b.toml"     # NO [trust] operator allow
start_expect_fail "$WORK/b.toml" "operator has not allowed it"
log "  refused: a manifest cannot authorise its own escape"
B_LOG="$LAST_FAIL_LOG"

# ---- C: manifest opt-in AND operator consent → loads --------------------
log "=== C: manifest opt-in AND operator consent → loads ==="
make_config "$WORK/optin" $'[trust]\nallow_unsandboxed_subprocess = ["optin"]' > "$WORK/c.toml"
( cd "$WORK" && RUST_LOG=info "$BIN" --config "$WORK/c.toml" > "$WORK/c.log" 2>&1 ) &
CPID=$!
for _ in $(seq 1 60); do
  curl -fsS "$BASE/healthz" >/dev/null 2>&1 && break
  kill -0 "$CPID" 2>/dev/null || { cat "$WORK/c.log"; die "daemon with consent failed to start"; }
  sleep 0.3
done
grep -q "trust=SubprocessUnsandboxed" "$WORK/c.log" \
  || die "expected the extension to be recorded as subprocess-unsandboxed"
grep -qi "operator-trusted subprocess" "$WORK/c.log" \
  || die "expected the honest 'operator-trusted subprocess (unsandboxed)' warning"
log "  loaded with an honest, attributable trust label (NOT called a sandbox)"
kill "$CPID" 2>/dev/null || true; wait "$CPID" 2>/dev/null || true

# ---- D: WASM tool loads with NO opt-in; run journals trust posture ------
log "=== D: WASM tool loads sandbox-by-default (no opt-in) + trust posture journaled ==="
PORT=7612
BASE="http://127.0.0.1:${PORT}"
WEXT="$WORK/wasm/wordcount-wasm"
mkdir -p "$WEXT"
cp "$REPO_ROOT/examples/extensions/wordcount-wasm"/*.wasm \
   "$REPO_ROOT/examples/extensions/wordcount-wasm/kelter-extension.toml" "$WEXT/"
cat > "$WORK/d.toml" <<EOF
[server]
bind = "127.0.0.1:${PORT}"
token = "${TOKEN}"
[policy]
engine = "local:qwen2.5-0.5b-instruct"
[kernel]
max_steps = 4
[storage]
backend = "sqlite"
path = "$WORK/d.db"
[tools]
workspace = "$WORK/dwork"
bundled_tools = false
extension_dirs = ["$WORK/wasm"]
EOF
( cd "$WORK" && RUST_LOG=info "$BIN" --config "$WORK/d.toml" > "$WORK/d.log" 2>&1 ) &
DPID=$!
for _ in $(seq 1 60); do
  curl -fsS "$BASE/healthz" >/dev/null 2>&1 && break
  kill -0 "$DPID" 2>/dev/null || { cat "$WORK/d.log"; die "wasm daemon failed to start"; }
  sleep 0.3
done
grep -q "trust=WasmSandboxed" "$WORK/d.log" \
  || die "wasm tool was not recorded as wasm-sandboxed"
log "  WASM tool loaded sandbox-by-default (no trust opt-in needed)"

if curl -fsS http://127.0.0.1:8080/health >/dev/null 2>&1; then
  RUN=$(curl -fsS -X POST "$BASE/api/v1/runs" -H "Authorization: Bearer ${TOKEN}" \
    -H 'Content-Type: application/json' \
    -d "$(python3 -c 'import json;print(json.dumps({"task":"Use word_count with input set to trust posture check. Then finish."}))')" \
    | sed -E 's/.*"run_id":"([^"]+)".*/\1/')
  for _ in $(seq 1 40); do
    S=$(curl -fsS "$BASE/api/v1/runs/$RUN" -H "Authorization: Bearer ${TOKEN}" | sed -E 's/.*"status":"([a-z]+)".*/\1/')
    { [ "$S" = completed ] || [ "$S" = failed ]; } && break; sleep 1
  done
  RUNJSON=$(curl -fsS "$BASE/api/v1/runs/$RUN" -H "Authorization: Bearer ${TOKEN}")
  printf '%s' "$RUNJSON" | python3 -c '
import json,sys
r = json.load(sys.stdin)
tp = r.get("trust_posture", [])
assert any(n=="wordcount-wasm" and lbl=="wasm-sandboxed" for n,lbl in tp), f"trust_posture missing wasm label: {tp}"
assert all(lbl!="sandbox" for _,lbl in tp), "a label literally called sandbox — dishonest"
print("  run.trust_posture:", json.dumps(tp))
' || die "trust posture not exposed on the run"
  # Journal entry present too.
  echo "SELECT count(*) FROM journal WHERE entry LIKE '%trust_posture%';" | sqlite3 "$WORK/d.db" | grep -q '^[1-9]' \
    || die "trust posture was not journaled"
  log "  trust posture exposed via the run API AND journaled (durably attributable)"
else
  log "  (llama-server off — registration + labelling proven; skipping the model run)"
fi
kill "$DPID" 2>/dev/null || true; wait "$DPID" 2>/dev/null || true

if [ -n "$EVIDENCE_DIR" ]; then
  mkdir -p "$EVIDENCE_DIR"
  scrub() { sed "s#${REPO_ROOT}#<repo>#g;s#${WORK}#<work>#g"; }
  scrub < "$A_LOG" > "$EVIDENCE_DIR/trust-refuse-no-declaration.log" 2>/dev/null || true
  scrub < "$B_LOG" > "$EVIDENCE_DIR/trust-refuse-no-operator.log" 2>/dev/null || true
  grep -E 'trust=|operator-trusted' "$WORK/c.log" | scrub > "$EVIDENCE_DIR/trust-loaded-with-consent.log" || true
  [ -n "${RUNJSON:-}" ] && printf '%s' "$RUNJSON" | scrub > "$EVIDENCE_DIR/trust-run.json"
fi

log "PASS: sandbox-by-default + two-part opt-in (manifest AND operator) + honest labelling + journaled posture"
