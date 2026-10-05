#!/usr/bin/env bash
# Governed self-learning smoke (Phase 10, ADR 0016), end-to-end through the
# daemon:
#   - a completed run is DISTILLED into a learned fact with provenance
#     (ai_extracted, human_validated=false, source run id);
#   - GET /learn lists it (inspectability);
#   - a later similar run RETRIEVES it as context ("learned: injected…" in the
#     log) yet still validates/completes (the floor is unchanged);
#   - FORGET the batch → the layer is empty and a later run no longer sees it
#     (complete reversal, no residue);
#   - anonymous access is refused (401).
#
# The poisoned-fact→still-validated crux is proven deterministically in
# crates/kernel-core/tests/learned_test.rs; this smoke shows the live path.
#
#   scripts/smoke-selflearn.sh [evidence-dir]
#
# Needs llama-server on :8080 (runs use the local model).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="${1:-}"
PORT=7641
BASE="http://127.0.0.1:${PORT}"
TOKEN="smoke-learn-$$"
BIN="$REPO_ROOT/target/debug/kelter"

log() { printf '[smoke-selflearn] %s\n' "$*"; }
die() { printf '[smoke-selflearn] FAIL: %s\n' "$*" >&2; exit 1; }

command -v python3 >/dev/null || die "python3 required"
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
max_steps = 4
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

( cd "$WORK" && RUST_LOG=info "$BIN" --config config.toml > "$WORK/daemon.log" 2>&1 ) &
DPID=$!
for _ in $(seq 1 60); do curl -fsS "$BASE/healthz" >/dev/null 2>&1 && break; sleep 0.5; done
curl -fsS "$BASE/healthz" >/dev/null 2>&1 || { cat "$WORK/daemon.log"; die "daemon did not start"; }
grep -q "self-learning: governed distillation enabled" "$WORK/daemon.log" \
  || die "self-learning was not enabled"

authpost() { local body="${2:-}"; [ -z "$body" ] && body='{}'; curl -fsS -X POST "$BASE$1" -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' -d "$body"; }
authget() { curl -fsS "$BASE$1" -H "Authorization: Bearer ${TOKEN}"; }
authdel() { curl -fsS -X DELETE "$BASE$1" -H "Authorization: Bearer ${TOKEN}"; }
submit() { authpost /api/v1/runs "{\"task\":\"$1\"}" | sed -E 's/.*"run_id":"([^"]+)".*/\1/'; }
wait_done() { for _ in $(seq 1 40); do
  S=$(authget "/api/v1/runs/$1" | sed -E 's/.*"status":"([a-z]+)".*/\1/')
  { [ "$S" = completed ] || [ "$S" = failed ]; } && { echo "$S"; return; }; sleep 2; done; echo timeout; }

# ---- 0: auth ----
CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/v1/learn")
[ "$CODE" = "401" ] || die "anonymous /learn not refused (got $CODE)"
log "anonymous /learn → 401"

# ---- 1: a completed run to learn from ----
log "=== 1: run a task, then distil ==="
R1=$(submit "Append the entry learn-src to the ledger using append_ledger. Then finish.")
[ "$(wait_done "$R1")" = completed ] || die "seed run did not complete"

# ADR 0037: a run without a success contract is unverified and learnable only
# once a human confirms it.
authpost "/api/v1/runs/$R1/confirmation" >/dev/null || die "could not confirm the seed run"
REP=$(authpost /api/v1/learn/distill '{"limit":10}')
BATCH=$(printf '%s' "$REP" | python3 -c 'import json,sys;print(json.load(sys.stdin)["batch_id"])')
CNT=$(printf '%s' "$REP" | python3 -c 'import json,sys;print(json.load(sys.stdin)["facts_created"])')
[ "$CNT" -ge 1 ] || die "distillation produced no facts"
log "  distilled $CNT fact(s), batch $BATCH"

# ---- 2: provenance + inspectability ----
log "=== 2: provenance + inspectability ==="
LIST=$(authget /api/v1/learn)
printf '%s' "$LIST" | R1="$R1" python3 -c '
import json,os,sys
facts=json.load(sys.stdin); src=os.environ["R1"]
assert facts, "no learned facts listed"
p=facts[0]["provenance"]
assert p["level"]=="ai_extracted", p
assert p["human_validated"] is False, p
assert src in p["source_run_ids"], (src, p)
print("  learned[0]: level=%s human_validated=%s source=%s" % (p["level"], p["human_validated"], p["source_run_ids"][0][:8]))
' || die "learned fact missing/!provenance"

# ---- 3: learned enters as context on a later run (floor intact) ----
log "=== 3: a later similar run retrieves the learned fact as context ==="
BEFORE=$(grep -c "learned: injected distilled facts as context" "$WORK/daemon.log" || true)
R2=$(submit "Append the entry learn-src to the ledger using append_ledger. Then finish.")
[ "$(wait_done "$R2")" = completed ] || die "second run did not complete (validation floor should be intact)"
AFTER=$(grep -c "learned: injected distilled facts as context" "$WORK/daemon.log" || true)
[ "$AFTER" -gt "$BEFORE" ] || die "learned fact was NOT injected as context on the similar run"
log "  learned injected as context (+$((AFTER-BEFORE))) AND the run still validated + completed"

# ---- 4: reversibility — forget the batch, no residue ----
log "=== 4: forget the batch → complete reversal, no residue ==="
authdel "/api/v1/learn/batch/$BATCH" >/dev/null
REMAIN=$(authget /api/v1/learn | python3 -c 'import json,sys;print(len(json.load(sys.stdin)))')
[ "$REMAIN" = "0" ] || die "learned layer not empty after forget ($REMAIN remain)"
BEFORE=$(grep -c "learned: injected distilled facts as context" "$WORK/daemon.log" || true)
R3=$(submit "Append the entry learn-src to the ledger using append_ledger. Then finish.")
[ "$(wait_done "$R3")" = completed ] || die "post-forget run did not complete"
AFTER=$(grep -c "learned: injected distilled facts as context" "$WORK/daemon.log" || true)
[ "$AFTER" = "$BEFORE" ] || die "RESIDUE: learned still injected after forget"
log "  after forget: list empty AND no learned context on a later run (zero residue)"

if [ -n "$EVIDENCE_DIR" ]; then
  mkdir -p "$EVIDENCE_DIR"
  scrub() { sed "s#${REPO_ROOT}#<repo>#g;s#${WORK}#<work>#g;s#${TOKEN}#<token>#g"; }
  {
    echo "=== governed self-learning (Phase 10) ==="
    echo "distilled: $CNT fact(s), batch $BATCH from run ${R1}"
    echo "--- learned list (provenance) ---"
    authget /api/v1/learn | python3 -m json.tool
    echo "--- learned injected as context (log) ---"
    grep "learned: injected distilled facts as context" "$WORK/daemon.log" | head -3
    echo "--- forget → no residue ---"
    echo "list after forget-batch: empty; later run shows no learned injection"
  } 2>/dev/null | scrub > "$EVIDENCE_DIR/selflearn-run.log" || true
fi

log "PASS: distil→provenance→inspect→learned-as-context (floor intact)→forget→zero-residue; anonymous refused"
