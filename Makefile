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

.PHONY: audit fmt clippy clippy-wasm test doc dead-deps coverage msrv-check help

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
