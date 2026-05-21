//! Phase 9 task `00060` — Rustdoc for all public APIs.
//!
//! Locks in the doc-coverage gate end-to-end:
//!
//! - **Zero rustdoc warnings under `-D warnings`** — running `cargo doc
//!   --no-deps --all-features` with `RUSTDOCFLAGS="-D warnings"` must
//!   succeed. This catches both `missing_docs` violations (already
//!   activated in `src/lib.rs`) **and** the lints rustdoc emits for
//!   broken intra-doc links (`rustdoc::broken_intra_doc_links`),
//!   private-item links from public docs (`rustdoc::private_intra_doc_links`),
//!   and redundant explicit link targets
//!   (`rustdoc::redundant_explicit_links`). Without this gate the CI is
//!   silent about a public surface that points at items rustdoc cannot
//!   resolve — the public docs render with the literal `[\`Drevo\`]`
//!   text instead of a link, which is exactly the failure mode task
//!   `00060` is supposed to prevent (`drevo-rust` §"Code Style" —
//!   _"Doc-comments on every `pub` item"_).
//!
//! - **The `RUSTDOCFLAGS` gate is wired into the Makefile** — the
//!   `audit` matrix needs `make doc` to fail on any rustdoc warning so
//!   the rule can be enforced in one local command (`drevo-tdd`
//!   §"CI Gates").
//!
//! - **The CI workflow runs the same gate** — otherwise a contributor
//!   who runs `cargo doc` without the env var sees a green local build
//!   but ships a regression. The CI doc step must export
//!   `RUSTDOCFLAGS=-D warnings` before calling `cargo doc`.
//!
//! These three checks together mean the doc-coverage gate cannot drift:
//! the warning floor is asserted by `cargo doc` itself (Phase 9 task
//! `00060`), and the gate's *wiring* is asserted by this test file so a
//! regression in either layer fails CI.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// Behavioural gate: cargo doc emits zero warnings under -D warnings.
// ---------------------------------------------------------------------------

/// Run `cargo doc --no-deps --all-features` under `RUSTDOCFLAGS="-D
/// warnings"` and assert it exits cleanly. This is the load-bearing
/// invariant of Phase 9 task `00060`: every public item must carry a
/// rustdoc comment, every intra-doc link must resolve to a public item,
/// and there must be no redundant link targets.
///
/// Marked `#[ignore]` because it spawns a nested `cargo` build (~30 s on
/// a cold cache) and would otherwise dominate the unit-test runtime. CI
/// runs `cargo test -- --include-ignored` (or the dedicated `make doc`
/// target) for the full audit pass.
#[test]
#[ignore = "spawns cargo doc; run via `make doc` or `cargo test -- --include-ignored`"]
fn cargo_doc_emits_no_warnings() {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .current_dir(repo_root())
        .env("RUSTDOCFLAGS", "-D warnings")
        .args([
            "doc",
            "--no-deps",
            "--all-features",
            "--manifest-path",
            "Cargo.toml",
        ])
        .output()
        .unwrap_or_else(|e| panic!("spawn cargo doc: {e}"));
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!(
            "`cargo doc --no-deps --all-features` failed under `RUSTDOCFLAGS=-D warnings` \
             — this is the load-bearing gate for Phase 9 task `00060` (rustdoc for all \
             public APIs). Either a `pub` item is missing a rustdoc, or an intra-doc \
             link does not resolve. Stderr:\n{stderr}"
        );
    }
}

// ---------------------------------------------------------------------------
// Wiring gates: the gate above is only useful if the Makefile + CI run it.
// ---------------------------------------------------------------------------

#[test]
fn makefile_doc_target_uses_rustdocflags_deny_warnings() {
    let makefile = read("Makefile");
    let has_deny = makefile.contains("RUSTDOCFLAGS")
        && (makefile.contains("-D warnings") || makefile.contains("-Dwarnings"));
    assert!(
        has_deny,
        "Makefile must export `RUSTDOCFLAGS=-D warnings` for the `doc` target so \
         `make audit` fails on any rustdoc warning. This is the load-bearing \
         enforcement of Phase 9 task `00060` (drevo-rust §\"Code Style\"). Current \
         Makefile:\n{makefile}"
    );
}

#[test]
fn ci_workflow_runs_cargo_doc_with_deny_warnings() {
    let ci = read(".github/workflows/ci.yml");
    let normalised: String = ci.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        normalised.contains("cargo doc"),
        ".github/workflows/ci.yml must run `cargo doc` so the rustdoc gate is \
         enforced on every PR (drevo-tdd §\"CI Gates\"). Current workflow:\n{ci}"
    );
    let has_rustdocflags =
        ci.contains("RUSTDOCFLAGS") && (ci.contains("-D warnings") || ci.contains("-Dwarnings"));
    assert!(
        has_rustdocflags,
        ".github/workflows/ci.yml must export `RUSTDOCFLAGS=-D warnings` (env on \
         the doc step or workflow-level) so a regression in any intra-doc link or \
         a missing public-item doc fails the build. Current workflow:\n{ci}"
    );
}

// ---------------------------------------------------------------------------
// Source-level guards: catch the most common regression families without
// having to spawn `cargo doc`. These run on every `cargo test`.
// ---------------------------------------------------------------------------

/// Scan every `.rs` file under `src/` for the doc patterns that rustdoc
/// would reject at the next `cargo doc` run. Cheap belt-and-braces
/// check that makes the unit-test matrix point straight at the file
/// that drifted, without waiting for the (slow, ignored)
/// `cargo_doc_emits_no_warnings` job.
///
/// Constants that are referenced from `pub` doc-comments must themselves
/// be declared `pub` — otherwise rustdoc emits
/// `private_intra_doc_links` warnings that fail under `-D warnings`. We
/// assert this at source level so the regression surfaces in `cargo
/// test`, not in `cargo doc`.
#[test]
fn doc_referenced_api_constants_are_public() {
    let body = read("src/api.rs");
    let must_be_pub = [
        "DEFAULT_LIST_LIMIT",
        "MAX_LIST_LIMIT",
        "DEFAULT_NEIGHBORS_DEPTH",
        "DEFAULT_SUBGRAPH_DEPTH",
        "DEFAULT_SEARCH_LIMIT",
        "MAX_SEARCH_LIMIT",
    ];
    let mut missing = Vec::new();
    for needle in must_be_pub {
        let declares_pub = body
            .lines()
            .any(|l| l.trim_start().starts_with(&format!("pub const {needle}")));
        if !declares_pub {
            missing.push(needle.to_string());
        }
    }
    assert!(
        missing.is_empty(),
        "src/api.rs declares the following constants without `pub` even though \
         `pub` doc-comments link to them. rustdoc emits \
         `private_intra_doc_links` warnings under `-D warnings`. Make each one \
         `pub const`: {missing:?}"
    );
}

/// Module-level rustdoc (`//!`) renders against the *module* scope, not
/// the crate root, so `[\`Drevo\`]` does not resolve from `src/api.rs`
/// or `src/dump.rs`. Either qualify the link as
/// `[\`crate::Drevo\`]` / `[\`Drevo\`](crate::Drevo)` or drop the link
/// to a plain code span. This guard catches the regression in `cargo
/// test` rather than `cargo doc`.
#[test]
fn module_level_docs_qualify_top_level_symbols() {
    // (source file, exact substring). The substring is the *bare* link
    // form rustdoc cannot resolve from a non-root module. Adding the
    // file's own module scope avoids false positives in `lib.rs`, where
    // the symbols are in scope.
    let banned_pairs: &[(&str, &str)] = &[
        ("src/api.rs", "[`Drevo`]"),
        ("src/api.rs", "[`Drevo::"),
        ("src/api.rs", "[`ApiState`]"),
        ("src/dump.rs", "[`Drevo`]"),
        ("src/dump.rs", "[`Drevo::"),
        ("src/dump.rs", "[`DumpError`]"),
        ("src/dump.rs", "[`DumpError::"),
        ("src/dump.rs", "[`DrevoError`]"),
        ("src/dump.rs", "[`DrevoError::"),
        ("src/dump.rs", "[`Properties`]"),
        ("src/dump.rs", "[`ImportReport::"),
        ("src/dump.rs", "[`edges_skipped`]"),
        ("src/dump.rs", "[`FORMAT_V1`]"),
        ("src/ffi.rs", "[`Drevo`]"),
        ("src/ffi.rs", "[`DrevoHandle`]"),
        ("src/ffi.rs", "[`drevo_open`]"),
        ("src/ffi.rs", "[`drevo_open_in_memory`]"),
        ("src/ffi.rs", "[`drevo_close`]"),
        ("src/ffi.rs", "[`drevo_last_error`]"),
        ("src/ffi.rs", "[`drevo_free_string`]"),
        ("src/wasm.rs", "[`Drevo`]"),
        ("src/wasm.rs", "[`WasmDrevo`]"),
        ("src/traversal.rs", "[`Drevo`]"),
        ("src/server.rs", "[`Config`]"),
        ("src/server.rs", "[`Config::from_env`]"),
        ("src/server.rs", "[`ConfigError`]"),
        ("src/server.rs", "[`run`]"),
        ("src/server.rs", "[`shutdown_signal`]"),
        ("src/server.rs", "[`crate::bin::server`]"),
        ("src/server.rs", "[`tests/`]"),
        // `[`audit/AUDIT-fts.md`](https://...)` — proper markdown URL link,
        // not an intra-doc link — is accepted; the regression we guard
        // against is the bare `[`audit/AUDIT-fts.md`]` intra-doc form.
        // Detected by looking for the backtick-closing `]` *without* a
        // following `(`. Matched as `]` not followed by `(` below.
        ("src/db.rs", "[`bfs`]"),
        ("src/model.rs", "[`Drevo::search_fts`]"),
    ];
    let mut hits = Vec::new();
    for (rel, needle) in banned_pairs {
        let body = read(rel);
        for (idx, line) in body.lines().enumerate() {
            // Only module-level docs (`//!`) are checked: `///` doc-comments
            // sit next to items inside a module that may have brought the
            // symbol into scope via `use`, but `//!` lives at the module
            // header where the only in-scope items are the module's own
            // declarations. Bare `[`Drevo`]` from a `//!` therefore fails
            // to resolve while `///` inside the same file may resolve fine.
            let trimmed = line.trim_start();
            if !trimmed.starts_with("//!") {
                continue;
            }
            // Only flag intra-doc links — i.e. `[`X`]` *not* followed by
            // `(`. A regular markdown URL like `[`X`](https://…)` is fine.
            let Some(pos) = line.find(needle) else {
                continue;
            };
            let suffix_start = pos + needle.len();
            let after = line.get(suffix_start..suffix_start + 1).unwrap_or("");
            if after == "(" {
                continue;
            }
            hits.push(format!("{rel}:{}: {}", idx + 1, line.trim()));
        }
    }
    assert!(
        hits.is_empty(),
        "Module-level rustdoc (`//!`) must qualify top-level symbols with \
         `crate::` (e.g. `[\\`crate::Drevo\\`]`) — bare `[\\`Drevo\\`]` does \
         not resolve from a sub-module's header and rustdoc rejects it \
         under `-D warnings`. Offending lines:\n{}",
        hits.join("\n")
    );
}

/// `dump.rs` carried a redundant explicit link target — `[\`Properties\`](crate::model::Properties)`
/// where the link text equals the target's last segment. rustdoc emits
/// `rustdoc::redundant_explicit_links` for those and they fail under
/// `-D warnings`. Keep them out of the source so the regression cannot
/// silently reappear.
#[test]
fn dump_rs_has_no_redundant_explicit_link_targets() {
    let body = read("src/dump.rs");
    let bad = body
        .lines()
        .enumerate()
        .filter(|(_, l)| l.contains("[`Properties`](crate::model::Properties)"))
        .map(|(i, l)| format!("src/dump.rs:{}: {}", i + 1, l.trim()))
        .collect::<Vec<_>>();
    assert!(
        bad.is_empty(),
        "`[\\`Properties\\`](crate::model::Properties)` is a redundant explicit \
         link target — rustdoc emits `redundant_explicit_links` and `-D warnings` \
         turns it into a build break. Replace with `[\\`crate::model::Properties\\`]`. \
         Offending lines:\n{}",
        bad.join("\n")
    );
}
