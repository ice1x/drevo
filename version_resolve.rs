// Pure version-resolution logic, `include!`d by both `build.rs` (to emit the
// `DREVO_VERSION` the server reports) and `tests/version_resolve_tests.rs` (to
// test it). It is deliberately NOT a crate module: `build.rs` cannot depend on
// the crate, and the crate must not ship build-time logic, so the one shared
// source of truth is this file, pulled into each via `include!`. Regular `//`
// comments (not `//!`) because `include!` splices this mid-file.

/// Resolve the reported version from the three sources, first non-empty wins:
///
///  1. `env_version` — the explicit `DREVO_VERSION` build-arg / env override
///     (how the release image injects the real version; `.git` is excluded from
///     the Docker build context so this is the only path there).
///  2. `git_version` — `git describe` on a dev checkout.
///  3. `cargo_version` — `CARGO_PKG_VERSION`, the last-resort fallback.
///
/// The wrinkle: the release flow keeps the git tag as the version source of
/// truth and never bumps `Cargo.toml`, so `CARGO_PKG_VERSION` is the placeholder
/// `0.0.0`. If we fall all the way through to it, the value is not a real
/// version — it just means "this image was built without a version" (no
/// build-arg AND no `.git`, e.g. a plain `docker build`). Reporting a bare
/// `0.0.0` there is misleading (it looks like a real release); mark it
/// `0.0.0-unknown` so the misconfiguration is visible in `GET /` / the UI
/// instead of silently masquerading as a release.
fn resolve_version(
    env_version: Option<String>,
    git_version: Option<String>,
    cargo_version: &str,
) -> String {
    let non_empty = |s: String| {
        let t = s.trim().to_string();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    };
    env_version
        .and_then(non_empty)
        .or_else(|| git_version.and_then(non_empty))
        .unwrap_or_else(|| {
            let cargo = cargo_version.trim();
            if cargo == "0.0.0" {
                // Placeholder, not a real version → flag it clearly.
                "0.0.0-unknown".to_string()
            } else {
                cargo.to_string()
            }
        })
}
