#!/usr/bin/env bash
# Build the static (musl) claude-code-bridge for the Kelter container and put
# it into the extension directory. Run on K12 (amd64) from a checkout:
#   deploy/claude-code/build-bridge.sh
# Uses podman (or docker) with the Rust image at CI's pin; no host toolchain.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ENGINE="$(command -v podman || command -v docker)"
"$ENGINE" run --rm -v "$ROOT/deploy/claude-code/bridge:/src:Z" -w /src \
  -e CARGO_TARGET_DIR=/src/target-musl docker.io/library/rust:1.99-alpine \
  cargo build --release --locked
install -m 0755 "$ROOT/deploy/claude-code/bridge/target-musl/release/claude-code-bridge" \
  "$ROOT/deploy/extensions/claude-code/claude-code-bridge"
file "$ROOT/deploy/extensions/claude-code/claude-code-bridge" 2>/dev/null || true
echo "built deploy/extensions/claude-code/claude-code-bridge"
