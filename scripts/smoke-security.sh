#!/usr/bin/env bash
# Security-surface smoke (Phase 11, ADR 0018):
#   - TLS opt-in: the daemon serves HTTPS with a config cert (curl over TLS);
#   - DEFAULT flow unchanged: the single bearer token still works (over HTTPS);
#   - SSRF-fix: the config health-check refuses an off-allowlist target (e.g. a
#     cloud-metadata IP) and accepts a preset — bearer-gated;
#   - per-caller principals: two tokens → distinct per-run attribution in the
#     journal (default vs a named principal);
#   - secrets (token/cert/key) are NOT in the tree or the logs.
# WS-auth (no token → refused) is proven deterministically in the api tests
# (tokio-tungstenite); noted here.
#
#   scripts/smoke-security.sh [evidence-dir]
#
# Needs: openssl (self-signed demo cert) + sqlite3. llama-server optional.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="${1:-}"
PORT=7661
BASE="https://127.0.0.1:${PORT}"
TOKEN="smoke-sec-default-$$"
ALICE="smoke-sec-alice-$$"
BIN="$REPO_ROOT/target/debug/kelter"

log() { printf '[smoke-security] %s\n' "$*"; }
die() { printf '[smoke-security] FAIL: %s\n' "$*" >&2; exit 1; }

command -v openssl >/dev/null || die "openssl required (demo cert)"
command -v sqlite3 >/dev/null || die "sqlite3 required"

log "building workspace"
cargo build --workspace --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null

WORK="$(mktemp -d)"
DB="$WORK/kelter.db"
DPID=""
cleanup() { kill -9 "${DPID:-0}" 2>/dev/null || true; }
trap cleanup EXIT

# ---- self-signed demo cert (localhost) — NEVER committed --------------------
openssl req -x509 -newkey rsa:2048 \
  -keyout "$WORK/key.pem" -out "$WORK/cert.pem" -days 1 -nodes \
  -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" >/dev/null 2>&1 \
  || die "openssl cert generation failed"
chmod 600 "$WORK/key.pem"

cat > "$WORK/config.toml" <<EOF
[server]
bind = "127.0.0.1:${PORT}"
token = "${TOKEN}"
[policy]
engine = "local:qwen2.5-0.5b-instruct"
[kernel]
max_steps = 2
[storage]
backend = "sqlite"
path = "$DB"
[tls]
cert = "$WORK/cert.pem"
key = "$WORK/key.pem"
[auth]
[[auth.principals]]
name = "alice"
token_env = "KELTER_TOKEN_ALICE"
[tools]
workspace = "$WORK/work"
bundled_tools = false
extension_dirs = []

[[tools.servers]]
name = "ledger"
command = "${REPO_ROOT}/target/debug/ledger-server"
args = ["--workspace", "$WORK/work"]
EOF

( cd "$WORK" && KELTER_TOKEN_ALICE="$ALICE" RUST_LOG=info "$BIN" --config config.toml > "$WORK/daemon.log" 2>&1 ) &
DPID=$!
for _ in $(seq 1 60); do curl -k -fsS "$BASE/healthz" >/dev/null 2>&1 && break; sleep 0.5; done
curl -k -fsS "$BASE/healthz" >/dev/null 2>&1 || { cat "$WORK/daemon.log"; die "HTTPS daemon did not start"; }

# ---- 1: TLS + default flow unchanged ---------------------------------------
log "=== 1: TLS opt-in + default bearer flow ==="
grep -q "serving over HTTPS (rustls)" "$WORK/daemon.log" || die "daemon did not enable TLS"
# Plain HTTP to a TLS port fails; HTTPS works. Default token still authorises.
CODE=$(curl -k -s -o /dev/null -w '%{http_code}' "$BASE/api/v1/runs" -H "Authorization: Bearer ${TOKEN}")
[ "$CODE" = "200" ] || die "default bearer flow broken over HTTPS (got $CODE)"
# Anonymous still refused.
CODE=$(curl -k -s -o /dev/null -w '%{http_code}' "$BASE/api/v1/runs")
[ "$CODE" = "401" ] || die "anonymous not refused (got $CODE)"
log "  HTTPS live; default token → 200; anonymous → 401 (default flow unchanged)"

# ---- 2: SSRF-fix on the config health-check --------------------------------
log "=== 2: SSRF-fix (off-allowlist target refused) ==="
META=$(curl -k -fsS -X POST "$BASE/api/v1/config/model/test" -H "Authorization: Bearer ${TOKEN}" \
  -H 'Content-Type: application/json' -d '{"provider":"local","base_url":"http://169.254.169.254"}' \
  | python3 -c 'import json,sys;d=json.load(sys.stdin);print(d["ok"], "allowlist" in d["detail"])')
[ "$META" = "False True" ] || die "metadata IP was not refused by the allowlist ($META)"
# A preset origin passes the allowlist (fetch may fail, but not an allowlist refusal).
PRESET=$(curl -k -fsS -X POST "$BASE/api/v1/config/model/test" -H "Authorization: Bearer ${TOKEN}" \
  -H 'Content-Type: application/json' -d '{"provider":"local","base_url":"http://127.0.0.1:8080"}' \
  | python3 -c 'import json,sys;print("allowlist" in json.load(sys.stdin)["detail"])')
[ "$PRESET" = "False" ] || die "preset origin was wrongly refused by the allowlist"
# Anonymous health-check → refused entirely.
CODE=$(curl -k -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/v1/config/model/test" \
  -H 'Content-Type: application/json' -d '{"provider":"local","base_url":"http://127.0.0.1:8080"}')
[ "$CODE" = "401" ] || die "anonymous health-check not refused (got $CODE)"
log "  metadata IP → refused (allowlist); preset → allowed; anonymous → 401"

# ---- 3: per-caller principals — distinct attribution -----------------------
log "=== 3: per-caller principals ==="
mk() { curl -k -fsS -X POST "$BASE/api/v1/runs" -H "Authorization: Bearer $1" \
  -H 'Content-Type: application/json' -d '{"task":"say hi and finish"}' \
  | sed -E 's/.*"run_id":"([^"]+)".*/\1/'; }
RA=$(mk "$ALICE"); RD=$(mk "$TOKEN")
sleep 1
PA=$(sqlite3 "$DB" "SELECT entry FROM journal WHERE run_id='$RA' AND entry LIKE '%\"key\":\"principal\"%';")
PD=$(sqlite3 "$DB" "SELECT entry FROM journal WHERE run_id='$RD' AND entry LIKE '%\"key\":\"principal\"%';")
echo "$PA" | grep -q '"principal":"alice"' || die "alice's run not attributed to 'alice' ($PA)"
echo "$PD" | grep -q '"principal":"default"' || die "default run not attributed to 'default' ($PD)"
log "  two tokens → distinct per-run principals in the journal (alice / default)"

# ---- 4: secrets not in the tree or the logs --------------------------------
log "=== 4: secrets not in tree/logs ==="
grep -q "$TOKEN" "$WORK/daemon.log" && die "SECRET: default token leaked into the log"
grep -q "$ALICE" "$WORK/daemon.log" && die "SECRET: alice token leaked into the log"
grep -q "BEGIN" "$WORK/daemon.log" && die "SECRET: a key/cert PEM leaked into the log"
# The cert/key live only in the temp workdir, never the repo; .gitignore covers PEM.
( cd "$REPO_ROOT" && git check-ignore -q x.pem ) || die ".gitignore does not cover *.pem"
log "  tokens + PEM absent from logs; *.pem gitignored"

if [ -n "$EVIDENCE_DIR" ]; then
  mkdir -p "$EVIDENCE_DIR"
  scrub() { sed "s#${REPO_ROOT}#<repo>#g;s#${WORK}#<work>#g;s#${TOKEN}#<token>#g;s#${ALICE}#<alice-token>#g"; }
  {
    echo "=== Phase 11 security surface ==="
    echo "TLS: $(grep 'serving over HTTPS' "$WORK/daemon.log" | head -1)"
    echo "default flow: bearer→200, anon→401 (unchanged)"
    echo "SSRF: metadata IP refused (allowlist); preset allowed; anon→401"
    echo "principals: alice run → $(echo "$PA" | python3 -c 'import json,sys;print(json.load(sys.stdin)["value"]["principal"])' 2>/dev/null || echo alice)"
    echo "            default run → $(echo "$PD" | python3 -c 'import json,sys;print(json.load(sys.stdin)["value"]["principal"])' 2>/dev/null || echo default)"
    echo "secrets: tokens + PEM absent from logs; *.pem gitignored"
  } | scrub > "$EVIDENCE_DIR/security-surface.log"
fi

log "PASS: TLS opt-in + default-flow unchanged + SSRF-fix + per-caller principals + no secret leak"
