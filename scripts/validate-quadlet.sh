#!/usr/bin/env bash
# Quadlet validation harness for deploy/kelter.container.
#
# Real validation requires podman's quadlet generator — present on the
# target host (e.g. the homelab box), NOT on a Mac dev machine. On hosts
# without podman this script runs the static checks and prints the exact
# commands to run on the real host; it does not pretend to have validated.
#
#   scripts/validate-quadlet.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
UNIT="$REPO_ROOT/deploy/kelter.container"

log()  { printf '[quadlet] %s\n' "$*"; }
fail() { printf '[quadlet] FAIL: %s\n' "$*" >&2; exit 1; }

[ -f "$UNIT" ] || fail "missing $UNIT"

# ---- static lint (any host) ----------------------------------------------

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
log "  static checks OK (sections + required keys present)"

# ---- real validation (podman hosts only) ---------------------------------

QUADLET_BIN=""
for candidate in /usr/libexec/podman/quadlet /usr/lib/podman/quadlet; do
  [ -x "$candidate" ] && QUADLET_BIN="$candidate" && break
done

if [ -z "$QUADLET_BIN" ] && ! command -v podman >/dev/null 2>&1; then
  log "podman/quadlet NOT available on this host — real validation SKIPPED."
  log "Run on the target podman host (this is the human step):"
  log "  mkdir -p ~/.config/containers/systemd"
  log "  cp deploy/kelter.container ~/.config/containers/systemd/"
  log "  /usr/libexec/podman/quadlet -dryrun -user   # must print a generated unit, no errors"
  log "  systemctl --user daemon-reload && systemctl --user start kelter"
  exit 0
fi

if [ -n "$QUADLET_BIN" ]; then
  TMP="$(mktemp -d)"
  cp "$UNIT" "$TMP/"
  log "running quadlet -dryrun…"
  if QUADLET_UNIT_DIRS="$TMP" "$QUADLET_BIN" -dryrun -user; then
    log "PASS: quadlet generated a valid systemd unit"
  else
    fail "quadlet -dryrun rejected the unit"
  fi
else
  log "podman present but quadlet binary not found at the known paths —"
  log "run: podman system service --help / check your distro's quadlet path."
fi
