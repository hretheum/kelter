#!/usr/bin/env bash
# Fetch a Claude Code result branch from K12 into a local repo (on the Mac).
#   deploy/claude-code/pobierz.sh <zadanie|kelter/zadanie> [repo-dir] [sha256]
# The result is a git bundle the runner wrote to ~/cc-runner/out on K12: a
# file, not a repo, so nothing on either host runs git inside anything the
# session could have written. Pass the `bundle.sha256` from the tool result
# to verify it.
set -euo pipefail
ID="${1#kelter/}"
REPO="${2:-.}"
WANT="${3:-}"
HOST="${K12_SSH:-k12}"
[[ "$ID" =~ ^[0-9a-f]{12}$ ]] || { echo "zadanie must be 12 hex characters" >&2; exit 2; }
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
scp -q "$HOST:cc-runner/out/$ID.bundle" "$TMP/"
if [ -n "$WANT" ]; then
  GOT="$(shasum -a 256 "$TMP/$ID.bundle" | cut -d' ' -f1)"
  [ "$GOT" = "$WANT" ] || { echo "sha256 mismatch: $GOT != $WANT" >&2; exit 1; }
fi
git -C "$REPO" bundle verify -q "$TMP/$ID.bundle"
git -C "$REPO" fetch -q "$TMP/$ID.bundle" "refs/heads/kelter/$ID:refs/heads/kelter/$ID"
echo "kelter/$ID → $(git -C "$REPO" rev-parse --short "kelter/$ID") in $REPO"
git -C "$REPO" log --oneline "main..kelter/$ID"
