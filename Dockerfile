# Production multi-stage build.

# === Builder ===
# Pin the builder to bookworm so its glibc matches the bookworm-slim runtime
# below. A bare `-slim` tag floats to newer Debian, producing binaries that
# need a newer glibc than the runtime image carries.
FROM rust:1.97-slim-bookworm AS builder

RUN apt-get update && apt-get install -y pkg-config && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# ── Layer 1: cache dependency compilation ────────────────────────────────────
# Manifests only: this layer is reused as long as Cargo.toml/lock are
# unchanged, even when sources change.
COPY Cargo.toml Cargo.lock ./
COPY bin/usernames-indexer/Cargo.toml bin/usernames-indexer/
RUN mkdir -p bin/usernames-indexer/src \
    && echo 'fn main() {}' > bin/usernames-indexer/src/main.rs \
    && touch bin/usernames-indexer/src/lib.rs \
    && cargo build --release -p usernames-indexer \
    && rm -rf bin/usernames-indexer/src

# ── Layer 2: the real build ──────────────────────────────────────────────────
# The migrations are embedded at compile time (sqlx::migrate!), so they are
# part of the source set, not a runtime asset.
COPY bin/usernames-indexer/src bin/usernames-indexer/src
COPY bin/usernames-indexer/migrations bin/usernames-indexer/migrations
RUN touch bin/usernames-indexer/src/main.rs bin/usernames-indexer/src/lib.rs \
    && cargo build --release -p usernames-indexer

# === Runtime ===
FROM debian:bookworm-slim

# rustls (postgres TLS and HTTPS RPC alike) reads the system trust store.
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 indexer

COPY --from=builder /app/target/release/usernames-indexer /usr/local/bin/usernames-indexer

USER indexer
# Inside a container the API must bind the container interface, not loopback.
ENV LISTEN_ADDR=0.0.0.0:8080
EXPOSE 8080
# Liveness: GET /health. Readiness for resolution: GET /v1/status (503s from
# the resolve endpoints until the first window lands are by design).
ENTRYPOINT ["/usr/local/bin/usernames-indexer"]
