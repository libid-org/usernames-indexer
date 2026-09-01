# Production multi-stage build, two runtime images from one compile.
#
# Pick one with `--target`:
#   docker build --target indexer -t usernames-indexer .
#   docker build --target api     -t usernames-api     .
#
# There is deliberately no image carrying both. A single entrypoint would have
# to default to one of them, and a deployment that pulled it expecting the
# other would run a container that looks healthy while doing half the job.

# === Builder ===
# Pin the builder to bookworm so its glibc matches the bookworm-slim runtime
# below. A bare `-slim` tag floats to newer Debian, producing binaries that
# need a newer glibc than the runtime image carries.
FROM rust:1.97-slim-bookworm AS builder

RUN apt-get update && apt-get install -y pkg-config && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# ── Layer 1: cache dependency compilation ────────────────────────────────────
# Manifests only: this layer is reused as long as the manifests and the lock
# are unchanged, even when sources change.
COPY Cargo.toml Cargo.lock ./
COPY crates/usernames-core/Cargo.toml crates/usernames-core/
COPY bin/usernames-indexer/Cargo.toml bin/usernames-indexer/
COPY bin/usernames-api/Cargo.toml bin/usernames-api/
RUN mkdir -p crates/usernames-core/src bin/usernames-indexer/src bin/usernames-api/src \
    && touch crates/usernames-core/src/lib.rs \
    && for b in usernames-indexer usernames-api; do \
         echo 'fn main() {}' > "bin/$b/src/main.rs"; \
         touch "bin/$b/src/lib.rs"; \
       done \
    && cargo build --release --workspace \
    && rm -rf crates/usernames-core/src bin/usernames-indexer/src bin/usernames-api/src

# ── Layer 2: the real build ──────────────────────────────────────────────────
# The migrations are embedded at compile time (sqlx::migrate!), so they are
# part of the source set, not a runtime asset, and they belong to the core
# crate that calls the macro.
COPY crates/usernames-core/src crates/usernames-core/src
COPY crates/usernames-core/migrations crates/usernames-core/migrations
COPY bin/usernames-indexer/src bin/usernames-indexer/src
COPY bin/usernames-api/src bin/usernames-api/src
RUN find crates bin -name '*.rs' -exec touch {} + \
    && cargo build --release --workspace

# === Runtime base ===
FROM debian:bookworm-slim AS runtime
# rustls (postgres TLS and HTTPS RPC alike) reads the system trust store.
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 usernames
USER usernames

# === The write half ===
# Indexes and serves nothing, so it exposes no port. A stalled indexer exits
# rather than idling behind a healthy-looking endpoint; the supervisor
# restarts it.
FROM runtime AS indexer
COPY --from=builder /app/target/release/usernames-indexer /usr/local/bin/usernames-indexer
ENTRYPOINT ["/usr/local/bin/usernames-indexer"]

# === The read half ===
# Reads and indexes nothing: no RPC_URL, no writer lease, no migration.
FROM runtime AS api
COPY --from=builder /app/target/release/usernames-api /usr/local/bin/usernames-api
# Inside a container the API must bind the container interface, not loopback.
ENV LISTEN_ADDR=0.0.0.0:8080
EXPOSE 8080
# Liveness: GET /health. Readiness for resolution: GET /v1/status (503s from
# the resolve endpoints until the first window lands are by design).
ENTRYPOINT ["/usr/local/bin/usernames-api"]
