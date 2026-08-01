//! CI invariant: the committed cbindgen header (`drevo.h`) must stay in
//! sync with the FFI surface.
//!
//! `build.rs` writes `drevo.h` into `CARGO_MANIFEST_DIR` (the repo root)
//! on every build with the default `cbindgen` feature, so any `cargo
//! check`/`build` regenerates the tracked header in place. When someone
//! changes the FFI surface (or the crate version) without re-committing
//! the regenerated header, the committed `drevo.h` silently drifts: CI
//! keeps regenerating it locally but never compares it against git, so
//! the drift stays invisible until `scripts/release.sh` refuses to run
//! on the now-dirty tree (`working tree is dirty`). That is exactly the
//! blocker PR #230 had to clean up by hand.
//!
//! This test pins the durable fix: the `Check` job in
//! `.github/workflows/ci.yml` MUST, right after building, assert the
//! regenerated `drevo.h` matches the committed one via
//! `git diff --exit-code -- drevo.h`. A stale header then fails the PR
//! that introduced the drift — on the feature branch, never on `main`.
//!
//! Pure-text test: no `act`, no Docker, no GitHub API. Mirrors the
//! pattern of `tests/ci_self_hosted_runner_tests.rs`.

use std::fs;
use std::path::{Path, PathBuf};

fn ci_yml_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".github")
        .join("workflows")
        .join("ci.yml")
}

fn ci_yml() -> String {
    let path = ci_yml_path();
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

/// The gate step must both regenerate and diff the header. We assert on
/// the `git diff --exit-code` guard naming `drevo.h`, which is the load-
/// bearing line: it is what turns a stale header into a red check.
#[test]
fn ci_check_job_asserts_drevo_h_is_in_sync() {
    let body = ci_yml();

    // Normalise whitespace so a reflow of the run-script line can't
    // silently drop the guard.
    let has_diff_guard = body
        .lines()
        .any(|line| line.contains("git diff --exit-code") && line.contains("drevo.h"));

    assert!(
        has_diff_guard,
        "`.github/workflows/ci.yml` must contain a step that runs \
         `git diff --exit-code -- drevo.h` after building, so a stale \
         committed cbindgen header fails the PR that introduced the \
         drift instead of silently blocking `scripts/release.sh` later. \
         See tests/cbindgen_header_sync_ci_tests.rs for the rationale."
    );
}
