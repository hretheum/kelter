# Web-UI build stage (design/06 §5): the api crate embeds apps/web-ui/dist
# via RustEmbed at compile time, so the frontend must be built first.
FROM node:22-bookworm-slim AS webui
WORKDIR /src/apps/web-ui
RUN corepack enable
COPY apps/web-ui/package.json apps/web-ui/pnpm-lock.yaml apps/web-ui/pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile
COPY apps/web-ui/ ./
RUN pnpm build

# Rust build stage
FROM rust:1.95-bookworm AS builder
WORKDIR /src
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
USER kelter
WORKDIR /data
EXPOSE 7466
HEALTHCHECK --interval=30s --timeout=3s CMD curl -fsS http://127.0.0.1:7466/healthz || exit 1
ENTRYPOINT ["kelter"]
CMD ["--config", "/data/config.toml"]
