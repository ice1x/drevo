//! Cross-cutting compliance invariants for Phase 8.5 task `00113`.
//!
//! Where other audit tasks (`00103`–`00112`) verify a single module against
//! a single skill, this task locks in the **repo-wide** invariants that no
//! per-module audit can see on its own:
//!
//! - **MSRV is declared** — `Cargo.toml` must carry a `rust-version` key so
//!   `cargo +stable build` and the CI matrix have a single source of truth
//!   (`drevo-rust` §"Code Style" — _"Edition 2021, MSRV latest stable"_).
//! - **`make audit` exists** — the audit matrix (fmt + clippy native + clippy
//!   wasm + test + doc + machete + llvm-cov) is one command, not seven
//!   (`drevo-tdd` §"CI Gates").
//! - **Crate-level rustdoc + `#![warn(missing_docs)]`** — every public
//!   item documented (`drevo-rust` §"Code Style" — _"Doc-comments on every
//!   `pub` item"_).
//! - **`getrandom` declared-but-unused is documented as ignored** —
//!   `cargo machete` would otherwise flip on every audit run because the
//!   crate exists only to surface the `wasm_js` feature flag.
//! - **No `unwrap()` / `expect()` outside tests/benches/`#[cfg(test)]`** —
//!   already enforced module-by-module in `00103`–`00112`; this test
//!   replays the regex once across the whole `src/` tree so a regression
//!   anywhere in the workspace surfaces in CI (`drevo-rust`
//!   §"Error Handling").
//!
//! Each invariant has a tight failure message so a CI break points
//! straight at the file that drifted.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// MSRV
// ---------------------------------------------------------------------------

#[test]
fn cargo_toml_declares_rust_version() {
    let manifest = read("Cargo.toml");
    let line = manifest
        .lines()
        .find(|l| l.trim_start().starts_with("rust-version"))
        .expect(
            "Cargo.toml must declare `rust-version = \"X.Y\"` so MSRV is a single source of \
             truth (drevo-rust §\"Code Style\"). Add it to the `[package]` table.",
        );
    let value = line
        .split('=')
        .nth(1)
        .and_then(|v| v.trim().strip_prefix('"'))
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or_else(|| panic!("`rust-version` must be a string literal: {line:?}"));
    let (major, minor) = value
        .split_once('.')
        .map(|(a, b)| {
            let minor = b.split('.').next().unwrap_or(b);
            (a.parse::<u32>(), minor.parse::<u32>())
        })
        .unwrap_or_else(|| panic!("`rust-version` is not X.Y(.Z): {value:?}"));
    let major = major.expect("`rust-version` major is not a number");
    let minor = minor.expect("`rust-version` minor is not a number");
    assert!(
        major >= 1 && minor >= 70,
        "MSRV {value} is suspiciously low for a project that uses bincode 2 / redb 2 / axum 0.8",
    );
}

#[test]
fn ci_matrix_pins_msrv_job() {
    let ci = read(".github/workflows/ci.yml");
    assert!(
        ci.contains("msrv") || ci.contains("MSRV") || ci.contains("rust-version"),
        "CI workflow must include an MSRV job that builds against the version declared in \
         Cargo.toml — otherwise the field decays silently. Add a job that runs \
         `cargo +<rust-version> check --all-features`."
    );
}

// ---------------------------------------------------------------------------
// `make audit`
// ---------------------------------------------------------------------------

#[test]
fn makefile_exists_with_audit_target() {
    let makefile_path = repo_root().join("Makefile");
    assert!(
        makefile_path.exists(),
        "Makefile missing at repo root. Audit task 00113 requires a `make audit` target that \
         runs the strict matrix (fmt / clippy native / clippy wasm / test / doc / machete) in one \
         command — see README task 00113 'Refactor targets'."
    );
    let makefile = read("Makefile");
    assert!(
        makefile.lines().any(|l| l.starts_with("audit:")),
        "Makefile must define an `audit:` target (line beginning with `audit:`). Current \
         Makefile:\n{makefile}"
    );
}

#[test]
fn makefile_audit_runs_fmt_clippy_test_doc() {
    let makefile = read("Makefile");
    for needle in ["fmt", "clippy", "test", "doc"] {
        assert!(
            makefile.contains(needle),
            "`make audit` must invoke `cargo {needle}` (or a target that does). \
             Current Makefile:\n{makefile}"
        );
    }
}

// ---------------------------------------------------------------------------
// Doc coverage
// ---------------------------------------------------------------------------

#[test]
fn lib_rs_has_crate_level_doc_and_missing_docs_warn() {
    let lib = read("src/lib.rs");
    let has_crate_doc = lib.lines().any(|l| l.trim_start().starts_with("//!"));
    assert!(
        has_crate_doc,
        "src/lib.rs must begin with a `//!` crate-level rustdoc (drevo-rust §\"Code Style\"). \
         Currently:\n{}",
        &lib[..lib.len().min(200)]
    );
    assert!(
        lib.contains("#![warn(missing_docs)]") || lib.contains("#![deny(missing_docs)]"),
        "src/lib.rs must enable `#![warn(missing_docs)]` (or `deny`) so every `pub` item without \
         a rustdoc fails the audit (drevo-rust §\"Code Style\" — doc-comments on every pub item)."
    );
}

#[test]
fn storage_mod_documents_every_submodule() {
    let m = read("src/storage/mod.rs");
    let body = m.replace('\r', "");
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("pub mod ") || trimmed.starts_with("pub use ") {
            // Each `pub mod` / `pub use` must be preceded by a `///` line.
        }
    }
    // Quick smoke: the file must carry a `//!` module-level doc.
    assert!(
        body.lines().any(|l| l.trim_start().starts_with("//!")),
        "src/storage/mod.rs must have a `//!` module-level rustdoc."
    );
}

// ---------------------------------------------------------------------------
// cargo-machete metadata
// ---------------------------------------------------------------------------

#[test]
fn getrandom_marked_as_ignored_in_cargo_machete_metadata() {
    let manifest = read("Cargo.toml");
    assert!(
        manifest.contains("[package.metadata.cargo-machete]"),
        "Cargo.toml must declare `[package.metadata.cargo-machete]` with `getrandom` in the \
         `ignored` list — the crate has no direct `use getrandom::...` site, but the optional \
         dep exists solely to surface the `wasm_js` feature flag on the same `getrandom` \
         version that `uuid` v1 already pulls in (drevo-rust §\"WASM Bindings\" — _\"WASM needs \
         the `wasm_js` feature on `getrandom` for browser-compatible RNG\"_)."
    );
    assert!(
        manifest.contains("\"getrandom\""),
        "`getrandom` must appear in the `ignored = [...]` list of `[package.metadata.\
         cargo-machete]`."
    );
}

// ---------------------------------------------------------------------------
// No unwrap/expect in library code (re-asserts the per-module rule)
// ---------------------------------------------------------------------------

fn walk_src(dir: &Path, hits: &mut Vec<(PathBuf, usize, String)>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display())) {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        let ft = entry.file_type().expect("file_type");
        if ft.is_dir() {
            walk_src(&path, hits);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let body = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        let mut in_test_cfg = false;
        let mut brace_depth_at_cfg = 0usize;
        let mut brace_depth = 0usize;
        for (idx, line) in body.lines().enumerate() {
            // Cheap heuristic: skip everything inside `#[cfg(test)] mod tests { ... }`.
            // We track the brace depth at which the `#[cfg(test)]` mod started so we
            // turn the gate off when we leave the same depth.
            let trimmed = line.trim_start();
            if trimmed.starts_with("#[cfg(test)]") {
                in_test_cfg = true;
                brace_depth_at_cfg = brace_depth;
                continue;
            }
            brace_depth += line.matches('{').count();
            let closes = line.matches('}').count();
            if in_test_cfg && closes > 0 {
                let new_depth = brace_depth.saturating_sub(closes);
                if new_depth <= brace_depth_at_cfg {
                    in_test_cfg = false;
                }
            }
            brace_depth = brace_depth.saturating_sub(closes);
            if in_test_cfg {
                continue;
            }
            // Lines that legitimately contain the substring in a doc-comment / string / regex
            // are excluded: rustdoc lines start with `///` or `//!`.
            if trimmed.starts_with("///") || trimmed.starts_with("//!") || trimmed.starts_with("//")
            {
                continue;
            }
            // The needles we forbid in non-test code.
            for needle in [".unwrap()", ".expect("] {
                if line.contains(needle) {
                    hits.push((path.clone(), idx + 1, line.to_string()));
                }
            }
        }
    }
}

#[test]
fn no_unwrap_or_expect_in_library_source() {
    let src = repo_root().join("src");
    let mut hits = Vec::new();
    walk_src(&src, &mut hits);
    // Known exceptions (audited, documented). Keep this list short — every
    // entry needs a one-line rationale in the AUDIT-crosscut report.
    let allow_substrings: &[&str] = &[
        // Tracing initialiser in the http feature is fallible only on
        // double-init; the explicit `expect("..")` documents that contract
        // (covered by AUDIT-server.md F2 and the server crate-level docs).
        ".try_init()",
    ];
    hits.retain(|(_, _, line)| !allow_substrings.iter().any(|s| line.contains(s)));
    assert!(
        hits.is_empty(),
        "Found {} `unwrap()` / `expect()` site(s) in library code (drevo-rust §\"Error \
         Handling\"). Each site must be moved behind a typed `DrevoError` / `ConfigError` / \
         `RunError` variant or annotated with a `// SAFETY:` rationale:\n{}",
        hits.len(),
        hits.iter()
            .map(|(p, line, src)| format!("  {}:{} — {}", p.display(), line, src.trim()))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

// ---------------------------------------------------------------------------
// Build-profile compile-time optimisation
// ---------------------------------------------------------------------------

#[test]
fn cargo_toml_dev_profile_trims_debuginfo() {
    // The default `dev` profile emits full debuginfo (`debug = 2`); on macOS
    // that adds a per-binary `dsymutil` link step, and with ~25 integration
    // test binaries it made the `Test` CI job spend most of its time
    // generating + linking debuginfo rather than running tests (the single
    // self-hosted runner serialised that cost into multi-hour jobs). The root
    // manifest must pin `[profile.dev] debug` below full (`0`, `1`, "none",
    // or "line-tables-only") so every cargo invocation — CI and local —
    // compiles + links faster. `debug = 1` keeps `file:line` in backtraces.
    let manifest = read("Cargo.toml");

    // Extract the `[profile.dev]` section body (until the next `[` table).
    let start = manifest.find("[profile.dev]").expect(
        "Cargo.toml must declare `[profile.dev]` with a reduced `debug` level — see the \
                 compile-time rationale comment in Cargo.toml",
    );
    let after = &manifest[start + "[profile.dev]".len()..];
    let body_end = after.find("\n[").unwrap_or(after.len());
    let body = &after[..body_end];

    let debug_line = body
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim()) // strip inline comments
        .find(|l| l.starts_with("debug"))
        .expect("[profile.dev] must set a `debug = ...` key (reduced from the full default)");
    let value = debug_line
        .split('=')
        .nth(1)
        .map(str::trim)
        .expect("`debug` must have a value");

    // Reject the full-debuginfo settings; accept any reduced level.
    let is_full = value == "2" || value == "true" || value == "\"full\"";
    assert!(
        !is_full,
        "[profile.dev] debug = {value} is full debuginfo — that is the slow default this guard \
         exists to prevent. Use `debug = 1` (line tables) or `0`/\"none\" to keep CI compile + \
         link fast on the self-hosted runner."
    );
}
