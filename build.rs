use std::process::Command;

// Pure resolver shared with `tests/version_resolve_tests.rs` (see the file).
include!("version_resolve.rs");

fn main() {
    emit_version();
    emit_git_sha();
    emit_build_date();

    #[cfg(feature = "cbindgen")]
    {
        use std::env;
        use std::path::PathBuf;

        let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
        let out_dir = PathBuf::from(&crate_dir);

        cbindgen::Builder::new()
            .with_crate(&crate_dir)
            .with_config(cbindgen::Config::from_file("cbindgen.toml").unwrap())
            .generate()
            .expect("Unable to generate C bindings")
            .write_to_file(out_dir.join("drevo.h"));
    }
}

/// Resolve the version string the server reports (`/`, `/status`, the Bolt
/// `server` agent, metrics) and expose it to the crate as the compile-time
/// `DREVO_VERSION` env var, read via `env!("DREVO_VERSION")`.
///
/// The version is decoupled from `Cargo.toml`'s `version` field on purpose:
/// the release flow (`scripts/release.sh`) treats the `vX.Y.Z` git tag as the
/// single source of truth and never bumps `Cargo.toml`, so `CARGO_PKG_VERSION`
/// stays `0.0.0` and would otherwise be what the running server reported.
///
/// Resolution order (first that yields a value wins):
///  1. `DREVO_VERSION` build env — how the release image gets a correct version
///     even though `.git` is excluded from the Docker build context
///     (`scripts/release.sh` passes `--build-arg DREVO_VERSION=<next>`).
///  2. `git describe --tags` on the surrounding checkout — a correct version for
///     native/dev builds straight from a git clone (e.g. `0.0.4`, or
///     `0.0.4-3-g<sha>` a few commits past the tag).
///  3. `CARGO_PKG_VERSION` — last-resort fallback for a git-less, arg-less build
///     (e.g. a source tarball or a plain `docker build`); keeps the build
///     infallible. Because the release flow never bumps `Cargo.toml`, this is
///     the placeholder `0.0.0`, which [`resolve_version`] reports as
///     `0.0.0-unknown` so a versionless build is visibly flagged rather than
///     masquerading as a real `0.0.0` release.
fn emit_version() {
    // Re-run when the override changes, and when git HEAD/tags move so a
    // dev build reflects a freshly cut tag without a manual `cargo clean`.
    println!("cargo:rerun-if-env-changed=DREVO_VERSION");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/tags");

    // Resolution + the `0.0.0 → 0.0.0-unknown` marker live in the shared,
    // unit-tested `resolve_version` (see `version_resolve.rs`).
    let version = resolve_version(
        std::env::var("DREVO_VERSION").ok(),
        version_from_git(),
        env!("CARGO_PKG_VERSION"),
    );

    println!("cargo:rustc-env=DREVO_VERSION={version}");
}

/// Expose the short git SHA the binary was built from as the optional
/// compile-time `DREVO_GIT_SHA` env, read via `option_env!` in `lib.rs` and
/// surfaced by `CALL drevo.info()` (issue #303).
///
/// Like the version, a release image cannot `git` (its Docker context excludes
/// `.git`), so a `DREVO_GIT_SHA` build-arg takes precedence; a native/dev build
/// falls back to `git rev-parse`. When neither is available the env is left
/// unset — `option_env!` then yields `None` — so the build stays infallible.
fn emit_git_sha() {
    println!("cargo:rerun-if-env-changed=DREVO_GIT_SHA");
    let sha = std::env::var("DREVO_GIT_SHA")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(git_short_sha);
    if let Some(sha) = sha {
        println!("cargo:rustc-env=DREVO_GIT_SHA={}", sha.trim());
    }
}

/// `git rev-parse --short HEAD`. `None` when git is unavailable or fails.
fn git_short_sha() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?;
    let sha = sha.trim();
    if sha.is_empty() {
        None
    } else {
        Some(sha.to_string())
    }
}

/// Expose an ISO-8601 build timestamp as the optional compile-time
/// `DREVO_BUILD_DATE` env (surfaced by `CALL drevo.info()`, issue #303).
///
/// Populated only when the build supplies it — the release flow passes a
/// `DREVO_BUILD_DATE` build-arg. build.rs deliberately does not invent a
/// timestamp: doing so would make every build non-reproducible and pull in a
/// date-formatting dependency. When unset, `option_env!` yields `None`.
fn emit_build_date() {
    println!("cargo:rerun-if-env-changed=DREVO_BUILD_DATE");
    if let Ok(date) = std::env::var("DREVO_BUILD_DATE") {
        let date = date.trim();
        if !date.is_empty() {
            println!("cargo:rustc-env=DREVO_BUILD_DATE={date}");
        }
    }
}

/// `git describe --tags --always --dirty`, with any leading `v` stripped so
/// `v0.0.4` reports as `0.0.4`. `None` when git is unavailable (e.g. inside the
/// Docker build, which excludes `.git`) or the command fails for any reason.
fn version_from_git() -> Option<String> {
    let out = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let desc = String::from_utf8(out.stdout).ok()?;
    let desc = desc.trim();
    if desc.is_empty() {
        return None;
    }
    Some(desc.strip_prefix('v').unwrap_or(desc).to_string())
}
