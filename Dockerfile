# Stage 1: Builder — compile the server binary in a full Rust toolchain
FROM rust:1.85-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock* build.rs cbindgen.toml ./
COPY src/ src/

# Build only the server binary in release mode.
# cbindgen feature is excluded — no C header needed in the container.
RUN cargo build --release --bin drevo-server \
        --no-default-features --features "http,redb-backend"

# Stage 2: Runtime — minimal Debian image with just the binary
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        wget \
    && rm -rf /var/lib/apt/lists/*

# Create a non-root user for the server process
RUN groupadd --system drevo && \
    useradd --system --gid drevo --create-home drevo

# Data directory for the redb database file
RUN mkdir -p /data && chown drevo:drevo /data
VOLUME ["/data"]

COPY --from=builder /build/target/release/drevo-server /usr/local/bin/drevo-server

USER drevo

ENV DREVO_HOST=0.0.0.0
ENV DREVO_PORT=8080
ENV DREVO_DATA_DIR=/data

EXPOSE 8080

# Use exec form so the binary receives SIGTERM directly from Docker
ENTRYPOINT ["drevo-server"]
