//! Unit tests for the version resolver shared with `build.rs` (issue #274).
//!
//! The resolver lives in `version_resolve.rs` at the repo root and is pulled
//! into `build.rs` via `include!`; we `include!` the same file here so the exact
//! logic that emits `DREVO_VERSION` is under test — in particular that a
//! git-less, arg-less build reports `0.0.0-unknown` instead of a silent,
//! release-looking `0.0.0`.

include!("../version_resolve.rs");

#[test]
fn explicit_env_override_wins() {
    // The release image's `--build-arg DREVO_VERSION=0.1.0` path.
    assert_eq!(
        resolve_version(Some("0.1.0".into()), Some("0.0.4-2-gabc".into()), "0.0.0"),
        "0.1.0"
    );
}

#[test]
fn blank_env_falls_through_to_git() {
    // An empty build-arg must not win over a real git describe.
    assert_eq!(
        resolve_version(Some("   ".into()), Some("0.0.4-2-gabc".into()), "0.0.0"),
        "0.0.4-2-gabc"
    );
}

#[test]
fn no_env_no_git_placeholder_is_marked_unknown() {
    // The bug in #274: plain `docker build` (no arg, no `.git`) → the 0.0.0
    // Cargo placeholder → must surface as `0.0.0-unknown`, not a silent 0.0.0.
    assert_eq!(resolve_version(None, None, "0.0.0"), "0.0.0-unknown");
    assert_eq!(
        resolve_version(Some("".into()), None, "0.0.0"),
        "0.0.0-unknown"
    );
}

#[test]
fn no_env_no_git_real_cargo_version_is_kept_verbatim() {
    // If Cargo.toml ever carried a real version, don't mangle it.
    assert_eq!(resolve_version(None, None, "1.4.2"), "1.4.2");
}

#[test]
fn env_is_trimmed() {
    assert_eq!(resolve_version(Some("  v9 \n".into()), None, "0.0.0"), "v9");
}
