# Web-UI build stage (design/06 §5): the api crate embeds apps/web-ui/dist
# via RustEmbed at compile time, so the frontend must be built first.
FROM node:22-bookworm-slim AS webui
WORKDIR /src/apps/web-ui
RUN corepack enable
COPY apps/web-ui/package.json apps/web-ui/pnpm-lock.yaml apps/web-ui/pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile
COPY apps/web-ui/ ./
RUN pnpm build

# ---------------------------------------------------------------------------
# Rust build, split with cargo-chef so DEPENDENCIES are their own cached layer.
#
# Previously this was a single `COPY crates ./crates && cargo build`, which
# meant one edited line in kernel-core invalidated the layer and recompiled all
# ~415 dependency crates — wasmtime and cranelift included. cargo-chef derives
# a dependency-only "recipe" from the manifests, so the expensive layer is
# keyed on Cargo.toml/Cargo.lock alone. A source-only change now recompiles
# just this workspace's crates.
# ---------------------------------------------------------------------------
FROM rust:1.95-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /src

# Planner: needs the real sources, but only to emit recipe.json.
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
# The one expensive layer. Its only inputs are the manifests, via recipe.json.
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
# Only now the real sources. Everything below re-runs on a source change;
# everything above is reused.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY --from=webui /src/apps/web-ui/dist ./apps/web-ui/dist
RUN cargo build --release -p daemon -p mcp

# Runtime stage
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -m -d /data kelter
COPY --from=builder /src/target/release/kelter /usr/local/bin/kelter
COPY --from=builder /src/target/release/tools-server /usr/local/bin/tools-server
# Crate `mcp` deklaruje CZTERY binarki, nie dwie. Bez memory-server demon nie startuje
# w ogole ("failed to start the builtin memory extension"), a bez code-server nie ma
# narzedzi do agentic codingu. Zweryfikowane na K12 2026-07-22.
COPY --from=builder /src/target/release/ledger-server /usr/local/bin/ledger-server
COPY --from=builder /src/target/release/code-server /usr/local/bin/code-server
COPY --from=builder /src/target/release/memory-server /usr/local/bin/memory-server
USER kelter
WORKDIR /data
EXPOSE 7466
HEALTHCHECK --interval=30s --timeout=3s CMD curl -fsS http://127.0.0.1:7466/healthz || exit 1
ENTRYPOINT ["kelter"]
CMD ["--config", "/data/config.toml"]
