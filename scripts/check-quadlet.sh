#!/usr/bin/env bash
# Drift check: does the unit running on a host equal the canonical repo unit
# plus that deployment's drop-ins? (decision D19 — one canonical unit,
# deployment specifics only in Quadlet drop-ins.)
#
# Renders BOTH sides with the same podman quadlet generator (`quadlet -dryrun
# -user`) — the host's own binary, so a podman version difference between a
# workstation and the host can never show up as drift:
#   repo side: deploy/kelter.container + <dropins>/*.conf, staged into a temp
#              dir and rendered with QUADLET_UNIT_DIRS pointing at it;
#   host side: what systemd actually uses — the default user search path
#              (~/.config/containers/systemd, …).
# The generated kelter.service of each side is normalized (comments,
# SourcePath= and the raw [X-Container] echo dropped; the `podman run` argv
# split into flag/value pairs and sorted, so flag ORDER does not matter, every
# flag and value does) and diffed. Exit 0 = equivalent, 1 = drift (diff
# printed), 2 = could not check.
#
#   scripts/check-quadlet.sh                      # on the host itself (K12)
#   scripts/check-quadlet.sh --ssh core@k12       # from a workstation
#   scripts/check-quadlet.sh --host-dir DIR       # host side = units in DIR
#   scripts/check-quadlet.sh --render-only        # print the repo side, no compare
#
# Options: --dropins DIR (default deploy/k12/kelter.container.d; "none" for
# the bare canonical unit), --quadlet BIN (default: the usual libexec paths).
set -euo pipefail
# macOS tar (bsdtar) adds AppleDouble ._* files with xattrs to archives; quadlet on
# the host then fails to parse ._kelter.container. Never send them.
export COPYFILE_DISABLE=1


REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
UNIT="$REPO_ROOT/deploy/kelter.container"
DROPINS="$REPO_ROOT/deploy/k12/kelter.container.d"
SSH_TARGET=""
HOST_DIR=""
RENDER_ONLY=0
QUADLET_BIN="${QUADLET_BIN:-}"

log()  { printf '[check-quadlet] %s\n' "$*" >&2; }
die()  { printf '[check-quadlet] ERROR: %s\n' "$*" >&2; exit 2; }

while [ $# -gt 0 ]; do
  case "$1" in
    --ssh)         SSH_TARGET="${2:?--ssh needs user@host}"; shift 2 ;;
    --host-dir)    HOST_DIR="${2:?--host-dir needs a directory}"; shift 2 ;;
    --dropins)     DROPINS="${2:?--dropins needs a directory or none}"; shift 2 ;;
    --quadlet)     QUADLET_BIN="${2:?--quadlet needs a path}"; shift 2 ;;
    --render-only) RENDER_ONLY=1; shift ;;
    -h|--help)     sed -n '2,25p' "$0"; exit 0 ;;
    *)             die "unknown argument: $1" ;;
  esac
done

[ -f "$UNIT" ] || die "missing $UNIT"
if [ "$DROPINS" != "none" ]; then
  [ -d "$DROPINS" ] || die "missing drop-in directory $DROPINS"
fi

# Stage the repo side: kelter.container + kelter.container.d/*.conf.
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
SHIP="$STAGE/ship"
mkdir -p "$SHIP/repo/kelter.container.d"
cp "$UNIT" "$SHIP/repo/kelter.container"
if [ "$DROPINS" != "none" ]; then
  for f in "$DROPINS"/*.conf; do
    [ -e "$f" ] && cp "$f" "$SHIP/repo/kelter.container.d/"
  done
fi

# Runs on the rendering machine (locally, or on the host over ssh). $1 = repo
# stage dir, $2 = host unit dir ("" = default search path, "-" = skip).
# Prints the two raw dry-run outputs between markers. Shipped next to the
# staged unit, so the ssh path needs no nested shell quoting.
cat > "$SHIP/render.sh" <<'RENDER'
set -u
q="${QUADLET_BIN:-}"
if [ -z "$q" ]; then
  for c in /usr/libexec/podman/quadlet /usr/lib/podman/quadlet; do
    [ -x "$c" ] && q="$c" && break
  done
fi
[ -n "$q" ] && [ -x "$q" ] || { echo "@@NOQUADLET@@"; exit 0; }
echo "@@VERSION@@ $("$q" -version 2>/dev/null || echo unknown)"
echo "@@REPO@@"
QUADLET_UNIT_DIRS="$1" "$q" -dryrun -user 2>&1 || echo "@@FAILED@@"
if [ "$2" != "-" ]; then
  echo "@@HOST@@"
  if [ -n "$2" ]; then
    QUADLET_UNIT_DIRS="$2" "$q" -dryrun -user 2>&1 || echo "@@FAILED@@"
  else
    "$q" -dryrun -user 2>&1 || echo "@@FAILED@@"
  fi
fi
echo "@@END@@"
RENDER

host_arg="$HOST_DIR"
[ "$RENDER_ONLY" = 1 ] && host_arg="-"

if [ -n "$SSH_TARGET" ]; then
  log "rendering on $SSH_TARGET with its own quadlet"
  # Ship render.sh + the staged repo side as one tar stream; render there.
  # (--host-dir with --ssh is a path ON the host; no ~ expansion — use $HOME.)
  # shellcheck disable=SC2029  # $host_arg is meant to expand client-side
  RAW="$(tar --no-xattrs -C "$SHIP" -cf - . | ssh "$SSH_TARGET" \
    "d=\$(mktemp -d) && tar -C \"\$d\" -xf - && sh \"\$d/render.sh\" \"\$d/repo\" \"$host_arg\"; rm -rf \"\$d\"")" \
    || die "ssh to $SSH_TARGET failed"
else
  RAW="$(QUADLET_BIN="$QUADLET_BIN" sh "$SHIP/render.sh" "$SHIP/repo" "$host_arg")"
fi

case "$RAW" in
  *@@NOQUADLET@@*) die "podman's quadlet generator not found (pass --quadlet BIN)" ;;
esac
log "quadlet $(printf '%s\n' "$RAW" | sed -n 's/^@@VERSION@@ //p')"

# section NAME: the raw dry-run output between @@NAME@@ and the next marker.
section() {
  printf '%s\n' "$RAW" | awk -v want="@@$1@@" '
    /^@@[A-Z]+@@/ { on = ($1 == want); next }
    on { print }'
}

# extract_service: only the ---kelter.service--- block of a dry-run output
# (the host renders every unit it has — llama-swap and friends too).
extract_service() {
  awk '
    /^---.*---$/ { on = ($0 == "---kelter.service---"); next }
    on { print }'
}

# normalize: drop comments/blank lines, SourcePath= (temp vs real path) and
# the [X-Container] echo of the raw keys; split ExecStart'"'"'s `podman run`
# argv (quadlet escapes embedded spaces as \x20, so whitespace splits safely)
# into "flag value" units; prefix every line with its section; sort.
normalize() {
  awk '
    /^[[:space:]]*#/ || /^[[:space:]]*$/ { next }
    /^\[.*\]$/ { sec = $0; next }
    sec == "[X-Container]" { next }
    /^SourcePath=/ { next }
    sec == "[Service]" && /^ExecStart=.* run / {
      sub(/^ExecStart=/, "")
      n = split($0, t, /[[:space:]]+/)
      i = 1
      while (i <= n && t[i] != "run") { cmd = cmd t[i] " "; i++ }
      print sec " ExecStart: " cmd "run"
      for (i++; i <= n; i++) {
        if (t[i] ~ /^-/ && t[i] !~ /=/ && i < n && t[i+1] !~ /^-/) {
          print sec " ExecStart: " t[i] " " t[i+1]; i++
        } else {
          print sec " ExecStart: " t[i]
        }
      }
      next
    }
    { print sec " " $0 }' | LC_ALL=C sort
}

REPO_OUT="$(section REPO)"
case "$REPO_OUT" in *@@FAILED@@*|"") printf '%s\n' "$REPO_OUT" >&2; die "quadlet rejected the repo unit + drop-ins" ;; esac
REPO_SVC="$(printf '%s\n' "$REPO_OUT" | extract_service)"
[ -n "$REPO_SVC" ] || { printf '%s\n' "$REPO_OUT" >&2; die "repo side generated no kelter.service"; }
# The generator's own diagnostics are the lines it prefixes with its name (the
# unit's comments are echoed into the output too — never grep those).
if printf '%s\n' "$REPO_OUT" | grep '^quadlet-generator' | grep -qv 'Loading source'; then
  printf '%s\n' "$REPO_OUT" >&2
  die "quadlet reported a problem with the repo unit + drop-ins"
fi

if [ "$RENDER_ONLY" = 1 ]; then
  printf '%s\n' "$REPO_SVC" | normalize
  exit 0
fi

HOST_OUT="$(section HOST)"
HOST_SVC="$(printf '%s\n' "$HOST_OUT" | extract_service)"
[ -n "$HOST_SVC" ] || { printf '%s\n' "$HOST_OUT" >&2; die "host side has no kelter.service — is the unit installed?"; }

A="$STAGE/repo.norm"; B="$STAGE/host.norm"
printf '%s\n' "$REPO_SVC" | normalize > "$A"
printf '%s\n' "$HOST_SVC" | normalize > "$B"

if diff -u --label "repo (deploy/kelter.container + $(basename "$(dirname "$DROPINS")")/$(basename "$DROPINS"))" \
          --label "host (${SSH_TARGET:-${HOST_DIR:-this host}})" "$A" "$B"; then
  log "OK: host unit is equivalent to the repo unit + drop-ins ($(wc -l < "$A") normalized lines)"
  exit 0
fi
log "DRIFT: the host unit differs from the repo unit + drop-ins (diff above; - repo, + host)"
exit 1
