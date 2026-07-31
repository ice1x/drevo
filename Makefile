# drevo — developer-facing audit / CI matrix.
#
# Phase 8.5 audit task `00113` cross-cutting refactor target. Lives in the
# repo root so a fresh checkout can run the full quality matrix with a single
# command — and so `tests/crosscut_audit_tests.rs::makefile_*` can lock it in.
#
#   make audit       # full audit matrix (fmt + clippy native + clippy wasm
#                    # + test + doc + dead deps + coverage summary)
#   make fmt         # cargo fmt --check
#   make clippy      # cargo clippy --all-targets --all-features -D warnings
#   make clippy-wasm # WASM target clippy, --no-default-features --features wasm
#   make test        # cargo test (1216-test baseline as of 2026-05-19)
#   make doc         # cargo doc --no-deps with -D missing_docs
#   make dead-deps   # cargo machete (requires cargo-machete)
#   make coverage    # cargo llvm-cov --summary-only (requires cargo-llvm-cov)
#   make msrv-check  # cargo +<rust-version> check --all-features (requires
#                    # the toolchain declared in Cargo.toml `rust-version`)
#
# Tools used:
#   * cargo machete  — install with `cargo install cargo-machete --locked`
#   * cargo llvm-cov — install with `cargo install cargo-llvm-cov --locked`
#                     and `rustup component add llvm-tools-preview`
# A missing tool is downgraded to a warning so the rest of `make audit`
# keeps running — CI installs all three.

CARGO ?= cargo
WASM_TARGET := wasm32-unknown-unknown
MSRV := $(shell awk -F'"' '/^rust-version/ {print $$2}' Cargo.toml)

IMAGE ?= ghcr.io/ice1x/drevo

.PHONY: audit fmt clippy clippy-wasm test doc dead-deps coverage msrv-check help \
        image release release-patch release-major release-image next-version

help:
	@echo "drevo audit matrix — see Makefile header for full list."
	@echo "  make audit       full matrix (default goal of CI mirror)"
	@echo "  make fmt         cargo fmt --check"
	@echo "  make clippy      cargo clippy --all-targets --all-features -D warnings"
	@echo "  make clippy-wasm cargo clippy --target $(WASM_TARGET) --no-default-features --features wasm"
	@echo "  make test        cargo test"
	@echo "  make doc         cargo doc --no-deps with -D missing_docs"
	@echo "  make dead-deps   cargo machete"
	@echo "  make coverage    cargo llvm-cov --summary-only"
	@echo "  make msrv-check  cargo +$(MSRV) check --all-features"
	@echo ""
	@echo "  make image       build the container image locally ($(IMAGE):dev)"
	@echo "  make next-version   print the next MINOR version (dry run)"
	@echo "  make release     bump MINOR, tag vX.Y.Z, push -> CI publishes to ghcr.io"
	@echo "  make release-patch / release-major   bump PATCH / MAJOR instead"
	@echo "  make release-image  bump + docker build + push to the DEPLOY registry"
	@echo "                      (Docker Hub ice1x/drevo; override with DREVO_IMAGE) + tag"

audit: fmt clippy clippy-wasm test doc dead-deps coverage
	@echo ""
	@echo "✓ drevo audit matrix complete."

fmt:
	$(CARGO) fmt --all -- --check

clippy:
	$(CARGO) clippy --all-targets --all-features -- -D warnings

clippy-wasm:
	$(CARGO) clippy --target $(WASM_TARGET) --no-default-features --features wasm -- -D warnings

test:
	$(CARGO) test --all-targets

doc:
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc --no-deps --all-features

dead-deps:
	@if command -v cargo-machete >/dev/null 2>&1; then \
		$(CARGO) machete; \
	else \
		echo "WARN: cargo-machete not installed; \`cargo install cargo-machete --locked\`"; \
	fi

coverage:
	@if command -v cargo-llvm-cov >/dev/null 2>&1; then \
		$(CARGO) llvm-cov --all-features --summary-only; \
	else \
		echo "WARN: cargo-llvm-cov not installed; \`cargo install cargo-llvm-cov --locked\` and \`rustup component add llvm-tools-preview\`"; \
	fi

msrv-check:
	@if [ -z "$(MSRV)" ]; then \
		echo "ERROR: Cargo.toml does not declare a `rust-version` (audit 00113)"; \
		exit 1; \
	fi
	$(CARGO) +$(MSRV) check --all-features

# ── Container image / release ───────────────────────────────────────────
# Two registries are in play:
#   * `make release` cuts a `vX.Y.Z` tag; Docker Publish CI builds it and
#     pushes to ghcr.io/ice1x/drevo (see .github/workflows/docker-publish.yml).
#   * the compose / run-drevo deploy pulls `ice1x/drevo` from **Docker Hub**.
# `make release-image` closes that gap: it increments the version, builds the
# image locally, and pushes it to the deploy registry (Docker Hub by default;
# override with DREVO_IMAGE) in ONE command, then also cuts the git tag.
# `make image` remains a quick local smoke build (no push).

image:
	docker build -t $(IMAGE):dev .

next-version:
	@scripts/release.sh next minor

release:
	@scripts/release.sh minor

release-patch:
	@scripts/release.sh patch

release-major:
	@scripts/release.sh major

# One command: bump version -> build the image -> push it to the deploy
# registry -> git tag. This is what actually ships a new *deployed* image.
release-image:
	@scripts/release.sh image minor
