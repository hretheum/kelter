#!/usr/bin/env bash
# Quadlet validation harness for deploy/kelter.container + deploy/k12 drop-ins.
#
# Two layers:
#  1. static checks (any host, no podman): sections, required keys, every
#     [Container] key a known Quadlet key (podman 6.1 list), and the D19
#     invariants — the canonical unit sets no list key a drop-in could never
#     take back (Network=, EnvironmentFile=), publishes on loopback only, logs
#     to journald; each drop-in only touches [Container]/[Unit]/[Service] keys
#     that are deployment-specific.
#  2. real validation, where podman's quadlet generator is available (on the
#     target host, or the binary CI builds from a pinned podman tag):
#     `quadlet -dryrun -user` of the bare unit and of unit + K12 drop-ins, and
#     a self-test of scripts/check-quadlet.sh (identical → 0, drift → 1).
# On hosts without quadlet, layer 2 is SKIPPED and the script says so — it
# does not pretend to have validated. Set QUADLET_BIN to force a binary (CI
# does; a QUADLET_BIN that is not executable is a failure, not a skip).
#
#   scripts/validate-quadlet.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
UNIT="$REPO_ROOT/deploy/kelter.container"
K12_DROPINS="$REPO_ROOT/deploy/k12/kelter.container.d"
CHECK="$REPO_ROOT/scripts/check-quadlet.sh"

log()  { printf '[quadlet] %s\n' "$*"; }
fail() { printf '[quadlet] FAIL: %s\n' "$*" >&2; exit 1; }

[ -f "$UNIT" ] || fail "missing $UNIT"
[ -d "$K12_DROPINS" ] || fail "missing $K12_DROPINS"

# ---- static lint (any host) ----------------------------------------------

# Every key Quadlet accepts in [Container] (podman v6.1.3,
# pkg/systemd/quadlet/quadlet.go). An unknown key makes the generator refuse
# the whole unit, so catching it here is worth the list going stale slowly.
CONTAINER_KEYS=" AddCapability AddDevice AddHost Annotation AppArmor AutoUpdate
CgroupsMode ContainerName ContainersConfModule DNS DNSOption DNSSearch
DropCapability Environment EnvironmentFile EnvironmentHost Entrypoint Exec
ExposeHostPort GIDMap GlobalArgs Group GroupAdd HealthCmd HealthInterval
HealthOnFailure HealthLogDestination HealthMaxLogCount HealthMaxLogSize
HealthRetries HealthStartPeriod HealthStartupCmd HealthStartupInterval
HealthStartupRetries HealthStartupSuccess HealthStartupTimeout HealthTimeout
HostName HttpProxy IP IP6 Image ImageVolume Label LogDriver LogOpt Mask Memory
Mount Network NetworkAlias NoNewPrivileges Notify PidsLimit Pod PodmanArgs
PublishPort Pull ReadOnly ReadOnlyTmpfs ReloadCmd ReloadSignal RemapGid RemapUid
RemapUidSize RemapUsers Retry RetryDelay Rootfs RunInit SeccompProfile Secret
SecurityLabelDisable SecurityLabelFileType SecurityLabelLevel
SecurityLabelNested SecurityLabelType ServiceName ShmSize StopSignal
StartWithPod StopTimeout SubGIDMap SubUIDMap Sysctl Timezone Tmpfs UIDMap
Ulimit Unmask User UserNS VolatileTmp Volume WorkingDir "
CONTAINER_KEYS="$(printf '%s' "$CONTAINER_KEYS" | tr '\n' ' ')"

# keys FILE SECTION → the keys set in that section, one per line.
keys() {
  awk -v want="[$2]" '
    /^[[:space:]]*[#;]/ || /^[[:space:]]*$/ { next }
    /^\[.*\]$/ { sec = $0; next }
    sec == want { k = $0; sub(/=.*/, "", k); gsub(/[[:space:]]/, "", k); print k }' "$1"
}
# value FILE SECTION KEY → every value of KEY in SECTION.
values() {
  awk -v want="[$2]" -v key="$3" '
    /^\[.*\]$/ { sec = $0; next }
    sec == want && index($0, key "=") == 1 { print substr($0, length(key) + 2) }' "$1"
}
check_container_keys() {
  local f="$1" k
  for k in $(keys "$f" Container); do
    case "$CONTAINER_KEYS" in
      *" $k "*) ;;
      *) fail "$(basename "$f"): unknown [Container] key '$k'" ;;
    esac
  done
}

log "static checks on $(basename "$UNIT"):"
for section in "\[Unit\]" "\[Container\]" "\[Service\]" "\[Install\]"; do
  grep -q "^${section}$" "$UNIT" || fail "missing ${section} section"
done
grep -q '^Image=' "$UNIT" || fail "missing Image= key"
grep -q '^Exec=' "$UNIT" || fail "missing Exec= key"
# Keys that exist in docker-compose but are invalid in Quadlet units:
if grep -qE '^(command|ports|volumes):' "$UNIT"; then
  fail "compose-style keys found — this must be a systemd unit, not YAML"
fi
check_container_keys "$UNIT"
# D19: list keys accumulate across drop-ins, so a default here could never be
# removed by a deployment — the canonical unit must not set them.
for k in Network EnvironmentFile; do
  [ -z "$(values "$UNIT" Container "$k")" ] || fail "canonical unit sets $k= (deployment-specific → drop-in)"
done
for p in $(values "$UNIT" Container PublishPort); do
  case "$p" in
    127.0.0.1:*|\[::1\]:*) ;;
    *) fail "canonical unit publishes '$p' — loopback only (127.0.0.1:…)" ;;
  esac
done
[ "$(values "$UNIT" Container LogDriver)" = journald ] || fail "canonical unit must log to journald (docs/observability.md)"
[ "$(values "$UNIT" Unit StartLimitIntervalSec)" = 0 ] || fail "canonical unit must set StartLimitIntervalSec=0"
values "$UNIT" Container Volume | grep -q '^kelter-state:/data/state' || fail "canonical unit must mount kelter-state at /data/state"

log "static checks on $(basename "$K12_DROPINS")/:"
n=0
for f in "$K12_DROPINS"/*.conf; do
  [ -e "$f" ] || continue
  n=$((n + 1))
  check_container_keys "$f"
  # Only [Container] belongs in these drop-ins today; anything else would be a
  # behaviour K12 has and the canonical unit does not — exactly the drift D19
  # removes. Widen deliberately if a real deployment need appears.
  awk '/^\[.*\]$/ && $0 != "[Container]" { exit 1 }' "$f" \
    || fail "$(basename "$f"): only a [Container] section is expected in a K12 drop-in"
done
[ "$n" -gt 0 ] || fail "no *.conf drop-ins in $K12_DROPINS"
img="$(cat "$K12_DROPINS"/*.conf | awk '/^\[.*\]$/ { s = $0; next } s == "[Container]" && /^Image=/ { v = substr($0, 7) } END { print v }')"
case "$img" in
  *:sha-[0-9a-f]*) ;;
  *) fail "K12 image must be pinned by commit SHA (…:sha-<commit>), got '$img'" ;;
esac
log "  static checks OK"

# ---- real validation (quadlet hosts / CI) --------------------------------

if [ -n "${QUADLET_BIN:-}" ]; then
  [ -x "$QUADLET_BIN" ] || fail "QUADLET_BIN=$QUADLET_BIN is not executable"
else
  for candidate in /usr/libexec/podman/quadlet /usr/lib/podman/quadlet; do
    [ -x "$candidate" ] && QUADLET_BIN="$candidate" && break
  done
fi

if [ -z "${QUADLET_BIN:-}" ]; then
  log "podman's quadlet generator NOT available on this host — real validation SKIPPED."
  log "Run on the target podman host (this is the human step):"
  log "  scripts/validate-quadlet.sh"
  log "  scripts/check-quadlet.sh     # installed unit vs repo + deploy/k12 drop-ins"
  exit 0
fi

log "quadlet $("$QUADLET_BIN" -version 2>/dev/null || echo '?') at $QUADLET_BIN"
log "dry-run: canonical unit alone"
"$CHECK" --quadlet "$QUADLET_BIN" --dropins none --render-only >/dev/null \
  || fail "quadlet -dryrun rejected deploy/kelter.container"
log "dry-run: canonical unit + K12 drop-ins"
"$CHECK" --quadlet "$QUADLET_BIN" --render-only | grep 'ExecStart: ' | sed 's/^/[quadlet]   /'
"$CHECK" --quadlet "$QUADLET_BIN" --render-only | grep -q 'ExecStart: --network llm-net' \
  || fail "K12 drop-ins did not reach the generated podman run (no --network llm-net)"

# Self-test of the drift comparator: an exact install must compare equal, a
# changed drop-in must be reported as drift (exit 1, not 0 and not 2).
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/same" "$TMP/drift"
cp "$UNIT" "$TMP/same/"; cp -r "$K12_DROPINS" "$TMP/same/"
cp "$UNIT" "$TMP/drift/"; cp -r "$K12_DROPINS" "$TMP/drift/"
printf '[Container]\nPublishPort=0.0.0.0:7467:7466\n' > "$TMP/drift/kelter.container.d/99-drift.conf"
log "self-test: check-quadlet on an exact install → expect 0"
"$CHECK" --quadlet "$QUADLET_BIN" --host-dir "$TMP/same" >/dev/null 2>&1 \
  || fail "check-quadlet reported drift on an identical install"
log "self-test: check-quadlet on a drifted install → expect 1"
set +e
"$CHECK" --quadlet "$QUADLET_BIN" --host-dir "$TMP/drift" >/dev/null 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "check-quadlet returned $rc on a drifted install (want 1)"
log "PASS: unit + drop-ins generate a valid systemd unit; drift check works"
