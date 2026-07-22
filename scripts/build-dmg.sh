#!/usr/bin/env bash
# Buduje UNSIGNED .dmg dla Kelter — od web-ui po gotowy, zamontowany obraz, jedna komenda.
#
#   scripts/build-dmg.sh [--universal] [--out KATALOG]
#
# DLACZEGO osobny skrypt zamiast package-desktop.sh: krok .dmg w Tauri
# (bundle_dmg.sh) uklada okno DMG AppleScriptem sterujacym Finderem — wymaga
# uprawnien Automation i regularnie sypie ("failed to run bundle_dmg.sh").
# Tu .app budujemy przez package-desktop.sh (--bundles app pomija bundle_dmg.sh),
# a .dmg skladamy przez hdiutil: plain, drag-to-install, bez AppleScriptu.
#
# Wynik: UNSIGNED — przy otwarciu prawy-klik -> Otworz -> Otworz (Gatekeeper).
# Podpis/notaryzacja to osobny krok ludzki (Developer ID cert, docs/packaging.md).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# cargo/tauri/pnpm w PATH (na wypadek nieinteraktywnego shella)
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"

UNIVERSAL=0
OUT_DIR="$REPO_ROOT/dist"
while [ $# -gt 0 ]; do
  case "$1" in
    --universal) UNIVERSAL=1 ;;
    --out) OUT_DIR="$2"; shift ;;
    *) echo "nieznany argument: $1" >&2; exit 2 ;;
  esac
  shift
done

echo "==> [1/3] web-ui (RustEmbedowany do daemona przy kompilacji; Tauri nie ma beforeBuildCommand, wiec budujemy sami)"
[ -d apps/web-ui/node_modules ] || pnpm -C apps/web-ui install
pnpm -C apps/web-ui build

echo "==> [2/3] unsigned .app (package-desktop.sh --bundles app: pomija bundle_dmg.sh)"
if [ "$UNIVERSAL" = 1 ]; then
  ./scripts/package-desktop.sh --universal --bundles app
else
  ./scripts/package-desktop.sh --bundles app
fi

echo "==> [3/3] .dmg przez hdiutil (bez AppleScriptu)"
if [ "$UNIVERSAL" = 1 ]; then
  APP_DIR="$REPO_ROOT/apps/desktop/src-tauri/target/universal-apple-darwin/release/bundle/macos"
  ARCH=universal
else
  APP_DIR="$REPO_ROOT/apps/desktop/src-tauri/target/release/bundle/macos"
  case "$(uname -m)" in arm64) ARCH=aarch64 ;; x86_64) ARCH=x86_64 ;; *) ARCH="$(uname -m)" ;; esac
fi
APP="$(find "$APP_DIR" -maxdepth 1 -name '*.app' 2>/dev/null | head -1)"
[ -n "$APP" ] || { echo "BLAD: nie znaleziono .app w $APP_DIR — build .app padl?" >&2; exit 1; }

VERSION="$(grep -m1 '"version"' "$REPO_ROOT/apps/desktop/src-tauri/tauri.conf.json" | sed -E 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/')"
DMG="$OUT_DIR/Kelter_${VERSION:-0.0.0}_${ARCH}-unsigned.dmg"
mkdir -p "$OUT_DIR"

# zdejmij zawieszony mount o tej samej nazwie (inaczej hdiutil czasem odmawia,
# a nizej mielibysmy /Volumes/Kelter 1 zamiast /Volumes/Kelter)
[ -d "/Volumes/Kelter" ] && hdiutil detach "/Volumes/Kelter" >/dev/null 2>&1 || true

STAGING="$(mktemp -d)"
trap 'rm -rf "$STAGING"' EXIT
cp -R "$APP" "$STAGING/"
ln -s /Applications "$STAGING/Applications"   # drag-to-install: appke przeciagasz na skrot Applications
rm -f "$DMG"
hdiutil create -volname "Kelter" -srcfolder "$STAGING" -ov -format UDZO "$DMG" >/dev/null

echo
echo "OK gotowe: $DMG"
ls -lh "$DMG"

# Zamontuj od razu i otworz okno w Finderze (bez dwuklikania).
echo
echo "==> montuje $DMG"
MOUNT_OUT="$(hdiutil attach "$DMG" -nobrowse 2>/dev/null || hdiutil attach "$DMG")"
# mount point = ostatnia kolumna (tab-separated) linii z /Volumes/; awk -F tab
# radzi sobie tez z nazwami ze spacja. Fallback: deterministyczny /Volumes/Kelter
# (volname), bo ewentualny wczesniejszy mount zdjelismy wyzej.
MOUNT_POINT="$(printf '%s\n' "$MOUNT_OUT" | awk -F'\t' '/\/Volumes\//{print $NF; exit}')"
[ -n "${MOUNT_POINT:-}" ] && [ -d "$MOUNT_POINT" ] || MOUNT_POINT="/Volumes/Kelter"

if [ -d "$MOUNT_POINT" ]; then
  open "$MOUNT_POINT"
  echo "OK zamontowane: $MOUNT_POINT (okno otwarte w Finderze)"
  echo "   po instalacji odmontuj: hdiutil detach \"$MOUNT_POINT\""
else
  echo "UWAGA: nie udalo sie zamontowac — otworz recznie: open \"$DMG\""
fi
