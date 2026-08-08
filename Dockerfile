# Stage 1: Builder — compile the server binary in a full Rust toolchain.
# Pinned to 1.88: the workspace's `time` dependency raised its MSRV to 1.88,
# so the previous rust:1.85 base failed the release build (task 00163).
FROM rust:1.88-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock* build.rs version_resolve.rs cbindgen.toml ./
COPY src/ src/
# Cargo parses every `[[bench]]` / `[[bin]]` / `[[test]]` declaration in
# Cargo.toml even when we ask it to build a single bin — the file must
# exist or the manifest fails to parse with:
#   error: can't find `<name>` bench at `benches/<name>.rs`
# Copying the bench sources costs ~tens of KB and is exercised purely
# by manifest parsing (we never run `cargo bench` in this stage).
# Locked by `tests/dockerfile_tests.rs::dockerfile_copies_every_cargo_manifest_target_dir`.
COPY benches/ benches/
# Same rationale for the `[[example]]` targets (the #241 load-harness:
# load_harness / churn_compact / http_load). Their `required-features` gates
# *compiling* them, not the manifest path check, so cargo still needs the
# sources present to parse Cargo.toml — otherwise the release build fails with
#   error: can't find `churn_compact` example at `examples/churn_compact.rs`
# We never `cargo build --example` here; this COPY is purely for parsing.
COPY examples/ examples/
# Phase 16 task 00115 promoted the repo to a Cargo workspace with
# `drevo-py` as the second member. Cargo refuses to load *any* workspace
# manifest if a declared member's Cargo.toml is missing — even when the
# target being built (`--bin drevo-server`) does not depend on the
# member. Without this COPY, `cargo build` fails inside the container
# with:
#   error: failed to load manifest for workspace member `/build/drevo-py`
# The `drevo-py` source itself is never compiled in this stage —
# `cargo build --bin drevo-server` filters the dep graph to the server
# binary, so pyo3 / pythonize do not enter the build. The COPY is
# strictly for manifest parsing.
# Locked by `tests/dockerfile_tests.rs::dockerfile_copies_every_workspace_member_dir`.
COPY drevo-py/ drevo-py/

# The embedded Web UI (Phase 15 task 00092) is compiled INTO the binary via
# `include_str!("../static/web/…")` in src/web_ui.rs, so these assets must be in
# the build context or the release compile fails with "couldn't read
# static/web/styles.css" (task 00163).
COPY static/ static/

# Build only the server binary in release mode.
# cbindgen feature is excluded — no C header needed in the container.
#
# The compiled Cargo feature set is a build ARG. The deploy image ships the
# FULL server, INCLUDING `embeddings-proxy`, so the OpenAI-compatible
# `/v1/embeddings` endpoint (issue #217) is available out of the box — there is
# no point shipping a deploy image without its functionality. It stays
# runtime-gated (answers 503 until `DREVO_EMBEDDINGS_UPSTREAM` is configured), so
# nothing is forced on and the "opt-in at runtime" contract holds. The extra
# HTTP client uses pure-Rust `rustls` on the `ring` provider (no aws-lc-rs / no
# extra system deps). Override to a lean build via:
#   docker build --build-arg CARGO_FEATURES="http,redb-backend" .
# (The Cargo library default stays dependency-free; this opinionated default is
# the deploy image only.) Locked by
# `tests/dockerfile_tests.rs::dockerfile_features_are_build_arg_overridable`.
ARG CARGO_FEATURES="http,redb-backend,embeddings-proxy"
# The version the built server reports (`/`, `/status`, Bolt `server` agent,
# metrics). `.git` is excluded from the build context, so `build.rs` cannot run
# `git describe` here and would fall back to `CARGO_PKG_VERSION` (0.0.0, since the
# release flow keeps the version in the git tag, not Cargo.toml). `scripts/release.sh`
# passes `--build-arg DREVO_VERSION=<next>` so the deployed image reports the real
# released version. Locked by
# `tests/dockerfile_tests.rs::dockerfile_threads_the_version_build_arg`.
ARG DREVO_VERSION=""
ENV DREVO_VERSION=${DREVO_VERSION}
RUN cargo build --release --bin drevo-server \
        --no-default-features --features "${CARGO_FEATURES}"

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
# Enable the Neo4j-compatible Bolt listener by default in the container
# (task 00163) so Neo4j drivers / Cypher tools can connect. It shares the same
# single drevo-server process + redb handle as the HTTP API.
ENV DREVO_BOLT_PORT=7687

EXPOSE 8080 7687

# Use exec form so the binary receives SIGTERM directly from Docker
ENTRYPOINT ["drevo-server"]
