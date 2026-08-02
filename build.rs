use std::process::Command;

fn main() {
    emit_version();

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
///     (e.g. a source tarball); keeps the build infallible.
fn emit_version() {
    // Re-run when the override changes, and when git HEAD/tags move so a
    // dev build reflects a freshly cut tag without a manual `cargo clean`.
    println!("cargo:rerun-if-env-changed=DREVO_VERSION");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/tags");

    let version = version_from_env()
        .or_else(version_from_git)
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    println!("cargo:rustc-env=DREVO_VERSION={version}");
}

/// The explicit `DREVO_VERSION` build-arg / env override, if non-empty.
fn version_from_env() -> Option<String> {
    match std::env::var("DREVO_VERSION") {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
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
