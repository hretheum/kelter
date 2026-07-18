#!/usr/bin/env bash
# Build the demo WASM modules from source and copy them next to their
# manifests. The compiled .wasm files are checked in (small, deterministic),
# so this is only needed after editing the guest source.
#
#   scripts/build-wasm-demo.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="wasm32-unknown-unknown"

rustup target list --installed | grep -q "$TARGET" \
  || { echo "installing rust target $TARGET"; rustup target add "$TARGET"; }

build() { # $1 crate dir, $2 wasm name, $3 dest dir
  ( cd "$REPO_ROOT/examples/wasm-tools/$1" && cargo build --release --target "$TARGET" >/dev/null )
  cp "$REPO_ROOT/examples/wasm-tools/$1/target/$TARGET/release/$2" "$3/"
  echo "built $1 -> $3/$2 ($(wc -c < "$3/$2" | tr -d ' ') bytes)"
}

build wordcount wordcount_wasm.wasm "$REPO_ROOT/examples/extensions/wordcount-wasm"
# netpeek is the sandbox-probe module used by the smoke (kept next to the
# wordcount manifest dir for the smoke to point at).
build netpeek netpeek_wasm.wasm "$REPO_ROOT/examples/extensions/wordcount-wasm"

echo "done"
