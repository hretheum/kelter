#!/usr/bin/env bash
# Package the Kelter desktop shell (Phase 8, item G).
#
#   scripts/package-desktop.sh [--universal] [--bundles app,dmg]
#
# Produces an **UNSIGNED** macOS bundle (.app + .dmg) by default. Signing and
# notarization are DELIBERATELY NOT done here — they require an Apple Developer
# ID certificate (a paid, human-held credential). See docs/packaging.md for the
# documented human signing/notarization steps.
#
# Windows (.msi/nsis) and Linux (.deb/AppImage) bundles build from their own
# host OSes (or CI) with the same tauri.conf.json targets.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TAURI_DIR="$REPO_ROOT/apps/desktop/src-tauri"

UNIVERSAL=0
BUNDLES="app,dmg"
while [ $# -gt 0 ]; do
  case "$1" in
    --universal) UNIVERSAL=1 ;;
    --bundles) BUNDLES="$2"; shift ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
  shift
done

# Resolve a Tauri CLI (cargo-tauri or the npm package), else explain.
if cargo tauri --version >/dev/null 2>&1; then
  TAURI=(cargo tauri)
elif [ -x "$REPO_ROOT/apps/desktop/node_modules/.bin/tauri" ]; then
  TAURI=("$REPO_ROOT/apps/desktop/node_modules/.bin/tauri")
else
  echo "Tauri CLI not found. Install one of:" >&2
  echo "  cargo install tauri-cli --version '^2'" >&2
  echo "  (or) npm i -D @tauri-apps/cli   # in apps/desktop" >&2
  exit 3
fi

# Universal (arm64 + x86_64) needs both Rust targets.
TARGET_ARGS=()
if [ "$UNIVERSAL" = 1 ]; then
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null 2>&1 || true
  TARGET_ARGS=(--target universal-apple-darwin)
fi

# IMPORTANT: no signing. tauri.conf.json leaves macOS.signingIdentity = null,
# so the build is unsigned. Do NOT pass APPLE_SIGNING_IDENTITY here — signing
# is a documented human step (docs/packaging.md).
echo "[package] building UNSIGNED desktop bundle(s): ${BUNDLES} ${TARGET_ARGS[*]:-}"
( cd "$TAURI_DIR" && "${TAURI[@]}" build --bundles "${BUNDLES}" ${TARGET_ARGS[@]+"${TARGET_ARGS[@]}"} )

echo "[package] done. Artifacts under $TAURI_DIR/target/**/release/bundle/"
echo "[package] These are UNSIGNED. To ship: sign + notarize with a Developer"
echo "[package] ID cert (human step) — see docs/packaging.md."
