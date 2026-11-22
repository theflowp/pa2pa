FROM rust:1-slim AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates/pa2pa-crypto crates/pa2pa-crypto
COPY crates/pa2pa-proto crates/pa2pa-proto
COPY crates/pa2pa-core crates/pa2pa-core
COPY crates/pa2pa-relay crates/pa2pa-relay
COPY crates/pa2pa-mcp crates/pa2pa-mcp
COPY crates/pa2pa-daemon crates/pa2pa-daemon
COPY crates/pa2pa-cli crates/pa2pa-cli

# Build only the headless `pa2pa` binary (excluding desktop GUI `pa2pa-pill`)
RUN sed -i '/"crates\/pa2pa-pill",/d' Cargo.toml \
    && cargo build --release --bin pa2pa

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/pa2pa /usr/local/bin/pa2pa

VOLUME ["/data"]
ENV PA2PA_RELAY_LISTEN=0.0.0.0:8787
ENV PA2PA_RELAY_DB=/data/pa2pa-relay.db
ENV PA2PA_RELAY_TTL_HOURS=168

EXPOSE 8787
ENTRYPOINT ["/usr/local/bin/pa2pa", "relay"]
