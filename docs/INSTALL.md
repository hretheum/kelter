# Installing Kelter

Kelter is a single binary, `kelter`, with the web UI embedded. Two ways to get
it. Kelter is **not** published on crates.io, so `cargo install kelter` does
not work (by design: install from the GitHub Release or from git).

## 1. Prebuilt binary (GitHub Releases)

Grab the archive for your platform from the
[Releases page](https://github.com/hretheum/kelter/releases):

| Platform | Archive |
| --- | --- |
| macOS, Apple Silicon | `kelter-<version>-aarch64-apple-darwin.tar.gz` |
| macOS, Intel | `kelter-<version>-x86_64-apple-darwin.tar.gz` |
| Linux x86_64 (glibc 2.35+) | `kelter-<version>-x86_64-unknown-linux-gnu.tar.gz` |
| Linux arm64 (glibc 2.35+) | `kelter-<version>-aarch64-unknown-linux-gnu.tar.gz` |

Windows binaries are not built yet (the code uses Unix-only APIs).

Verify and unpack (example for macOS Apple Silicon):

```sh
V=v0.1.0   # the release you downloaded
curl -LO https://github.com/hretheum/kelter/releases/download/$V/kelter-$V-aarch64-apple-darwin.tar.gz
curl -LO https://github.com/hretheum/kelter/releases/download/$V/SHA256SUMS.txt
shasum -a 256 -c SHA256SUMS.txt --ignore-missing    # Linux: sha256sum -c --ignore-missing
tar -xzf kelter-$V-aarch64-apple-darwin.tar.gz
./kelter-$V-aarch64-apple-darwin/kelter --config config.toml
```

The archive also holds `README.md` and both licence files. Start from
[`config.example.toml`](../config.example.toml). The binaries are not signed or
notarised, so on macOS a downloaded file may need
`xattr -d com.apple.quarantine kelter` before the first run.

## 2. From source with cargo

Needs a recent stable Rust (CI pins 1.99.0) and, for the embedded UI, Node 22 +
pnpm (`corepack enable` is enough).

```sh
cargo install --git https://github.com/hretheum/kelter daemon
```

The package is called `daemon`; the binary it installs is `kelter`. On a fresh
checkout the web UI is not built yet, so `crates/api/build.rs` runs
`pnpm install --frozen-lockfile && pnpm build` in `apps/web-ui` for you. Without
pnpm the build stops early with instructions. Expect roughly 6-7 minutes on a
recent laptop (a clean machine, UI build included).

From a clone you can build the UI yourself first (this is what CI does):

```sh
pnpm --dir apps/web-ui install --frozen-lockfile && pnpm --dir apps/web-ui build
cargo build --release -p daemon      # target/release/kelter
```

To pin a version: `cargo install --git https://github.com/hretheum/kelter --tag v0.1.0 daemon`.
