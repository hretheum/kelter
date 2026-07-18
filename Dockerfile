# Build stage
FROM rust:1.95-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY apps/web-ui ./apps/web-ui
RUN cargo build --release -p daemon -p mcp

# Runtime stage
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -m -d /data agentkernel
COPY --from=builder /src/target/release/agentkernel /usr/local/bin/agentkernel
COPY --from=builder /src/target/release/tools-server /usr/local/bin/tools-server
USER agentkernel
WORKDIR /data
EXPOSE 7466
HEALTHCHECK --interval=30s --timeout=3s CMD curl -fsS http://127.0.0.1:7466/healthz || exit 1
ENTRYPOINT ["agentkernel"]
CMD ["--config", "/data/config.toml"]
