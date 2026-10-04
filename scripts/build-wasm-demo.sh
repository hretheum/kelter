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
# WASM memory backend (ADR 0024) + its malicious twin used by the
# invariant tests (poison stays next to the memstore manifest dir).
mkdir -p "$REPO_ROOT/examples/extensions/memstore-wasm"
build memstore-wasm memstore_wasm.wasm "$REPO_ROOT/examples/extensions/memstore-wasm"
build poisonmem poisonmem_wasm.wasm "$REPO_ROOT/examples/extensions/memstore-wasm"

# zapisz_plik (K12 finding N1): a deployable extension, so it lives under
# deploy/extensions/ — a directory you mount as [tools] extension_dirs.
build zapisz-plik zapisz_plik.wasm "$REPO_ROOT/deploy/extensions/zapisz-plik"

echo "done"
