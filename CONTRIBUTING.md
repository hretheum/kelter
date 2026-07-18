# Contributing

Thanks for your interest in contributing to agentkernel.

## Prerequisites

- A stable Rust toolchain (install via [rustup](https://rustup.rs/)). The project targets the
  current stable release; nightly is not required.

## Building and testing

From the workspace root:

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt
```

Run `cargo fmt` before committing so formatting stays consistent across the workspace. `cargo
clippy --workspace -- -D warnings` must pass with no warnings.

## Pull requests

Before opening a PR, please make sure:

- New behavior is covered by tests, and existing tests still pass (`cargo test --workspace`).
- The code is clippy-clean and formatted (`cargo clippy --workspace -- -D warnings`, `cargo fmt`).
- The change is scoped and the PR description explains the motivation and approach.

Small, focused PRs are easier to review than large ones. If you're planning a substantial change,
consider opening an issue first to discuss the approach.

## Licensing

This project is dual-licensed under MIT OR Apache-2.0.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
