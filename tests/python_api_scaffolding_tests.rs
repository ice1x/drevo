//! Phase 16 task `00115` — text-level scaffolding tests for the
//! `drevo-py` workspace member.
//!
//! These tests do **not** compile the PyO3 crate (the workspace's
//! existing CI doesn't ship a Python interpreter — that's task `00122`).
//! Instead they lock the *layout* and *contract* of the new crate by
//! grepping the files on disk:
//!
//! 1. The repo root `Cargo.toml` declares a `[workspace]` with
//!    `members = [".", "drevo-py"]` and `default-members = ["."]` — so
//!    a future contributor cannot accidentally pull `drevo-py` into the
//!    default build path (which would break CI on the first push that
//!    runs `cargo check` without Python available).
//!
//! 2. `drevo-py/Cargo.toml` declares the crate name + cdylib + the
//!    `pyo3` extension-module / abi3 features required by RFC §3.
//!
//! 3. `drevo-py/src/lib.rs` carries the `#[pymodule]` entry point named
//!    `_drevo` (per RFC §2 "Wheel layout").
//!
//! 4. Every module the RFC contract requires (`errors`, `types`,
//!    `handle`) exists on disk and has the rustdoc preamble citing
//!    `audit/RFC-python-api.md`.
//!
//! 5. The error module declares every Python exception listed in RFC
//!    §5.1 (`DrevoError`, `NotFoundError`, `NodeNotFoundError`,
//!    `EdgeNotFoundError`, `ConflictError`, `DuplicateTitleError`,
//!    `StorageError`, `SerializationError`, `LockedError`, `PanicError`).
//!
//! 6. Every `DrevoError` variant from `src/error.rs` has a matching arm
//!    in the Python error-mapping table. This is a "structural" check —
//!    the test reads both files and asserts each Rust variant appears
//!    in `drevo-py/src/errors.rs`.
//!
//! 7. The handle module exposes the public methods enumerated in RFC
//!    §3.3 (the `Drevo` class stub).
//!
//! Failure modes:
//!   * Renaming the `drevo-py` directory without updating this test
//!     fails CI with a clear "directory not found".
//!   * Adding a new `DrevoError` variant without mapping it on the
//!     Python side fails CI with the variant name in the message.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

// ── 1. Workspace declaration ───────────────────────────────────────────

#[test]
fn root_cargo_toml_declares_workspace_with_drevo_py_member() {
    let root_toml = read(&repo_root().join("Cargo.toml"));
    assert!(
        root_toml.contains("[workspace]"),
        "root Cargo.toml must declare a [workspace] section for Phase 16 \
         task 00115 — drevo-py is its second member"
    );
    assert!(
        root_toml.contains("\"drevo-py\""),
        "root Cargo.toml [workspace.members] must include \"drevo-py\""
    );
}

#[test]
fn root_cargo_toml_keeps_drevo_py_out_of_default_members() {
    let root_toml = read(&repo_root().join("Cargo.toml"));
    assert!(
        root_toml.contains("default-members"),
        "root Cargo.toml must declare default-members so CI's plain `cargo \
         check` does not try to compile drevo-py (which needs Python on PATH)"
    );
    // The default-members line MUST include "." and MUST NOT include drevo-py.
    let line = root_toml
        .lines()
        .find(|l| l.trim_start().starts_with("default-members"))
        .expect("default-members line not found");
    assert!(
        line.contains("\".\"") || line.contains("\"./\""),
        "default-members must include the root crate (\".\"): got {line:?}"
    );
    assert!(
        !line.contains("\"drevo-py\""),
        "default-members MUST NOT include drevo-py — CI runners do not have \
         Python on PATH; got {line:?}"
    );
}

// ── 2. drevo-py/Cargo.toml ─────────────────────────────────────────────

#[test]
fn drevo_py_cargo_toml_declares_correct_metadata() {
    let toml = read(&repo_root().join("drevo-py").join("Cargo.toml"));
    assert!(
        toml.contains("name = \"drevo-py\""),
        "drevo-py/Cargo.toml must declare `name = \"drevo-py\"`"
    );
    assert!(
        toml.contains("name = \"_drevo\""),
        "drevo-py [lib].name must be `_drevo` — the runtime extension \
         module name imported by the pure-Python shim per RFC §2 \"Wheel \
         layout\""
    );
    assert!(
        toml.contains("cdylib"),
        "drevo-py/Cargo.toml [lib].crate-type must include `cdylib` so \
         PyO3 produces a shared library importable by CPython"
    );
    assert!(
        toml.contains("pyo3"),
        "drevo-py/Cargo.toml must depend on the `pyo3` crate"
    );
    assert!(
        toml.contains("extension-module"),
        "drevo-py must enable the pyo3 `extension-module` feature so the \
         wheel uses dynamic libpython resolution (RFC §3)"
    );
    assert!(
        toml.contains("abi3-py310"),
        "drevo-py must enable `abi3-py310` so a single wheel runs on \
         every CPython ≥ 3.10 (matches Phase 16 CI matrix)"
    );
    assert!(
        toml.contains("pythonize"),
        "drevo-py must depend on `pythonize` so `Node.properties` round-trip \
         between Python dict and serde_json::Value (RFC §3.2)"
    );
}

// ── 3. PyO3 entry point ────────────────────────────────────────────────

#[test]
fn drevo_py_lib_rs_has_pymodule_named_underscore_drevo() {
    let lib = read(&repo_root().join("drevo-py").join("src").join("lib.rs"));
    assert!(
        lib.contains("#[pymodule]"),
        "drevo-py/src/lib.rs must carry a #[pymodule] entry point"
    );
    // The function name is what becomes the Python module name — must
    // be `_drevo` to match the [lib] declaration above.
    assert!(
        lib.contains("fn _drevo("),
        "drevo-py/src/lib.rs's #[pymodule] function must be named \
         `_drevo` (matches [lib].name in drevo-py/Cargo.toml)"
    );
}

// ── 4. Required modules exist + cite the RFC ───────────────────────────

#[test]
fn drevo_py_required_modules_exist_and_cite_rfc() {
    for module in ["errors", "types", "handle"] {
        let path = repo_root()
            .join("drevo-py")
            .join("src")
            .join(format!("{module}.rs"));
        assert!(
            path.exists(),
            "drevo-py/src/{module}.rs must exist — required by RFC §2"
        );
        let body = read(&path);
        assert!(
            body.contains("RFC")
                || body.contains("audit/RFC-python-api.md")
                || body.contains("RFC-python-api"),
            "drevo-py/src/{module}.rs must cite audit/RFC-python-api.md \
             in its rustdoc preamble — every Phase 16 task references the \
             RFC as its contract"
        );
    }
}

// ── 5. Exception hierarchy ─────────────────────────────────────────────

#[test]
fn errors_module_declares_full_exception_hierarchy() {
    let errors = read(&repo_root().join("drevo-py").join("src").join("errors.rs"));
    for class in [
        "DrevoError",
        "NotFoundError",
        "NodeNotFoundError",
        "EdgeNotFoundError",
        "ConflictError",
        "DuplicateTitleError",
        "StorageError",
        "SerializationError",
        "LockedError",
        "PanicError",
    ] {
        assert!(
            errors.contains(class),
            "drevo-py/src/errors.rs must declare {class} (RFC §5.1)"
        );
    }
}

// ── 6. Every DrevoError variant has a Python arm ───────────────────────

#[test]
fn every_rust_drevo_error_variant_has_a_python_arm() {
    let rust_err = read(&repo_root().join("src").join("error.rs"));
    let py_err = read(&repo_root().join("drevo-py").join("src").join("errors.rs"));

    // Extract every `Variant` line from the DrevoError enum.
    let variants: Vec<&str> = rust_err
        .lines()
        .filter_map(|l| {
            let trimmed = l.trim_start();
            // Match `VariantName(` or `VariantName,` patterns inside the
            // `pub enum DrevoError { ... }` block.
            if trimmed.starts_with("///")
                || trimmed.starts_with("//")
                || trimmed.starts_with("#[")
                || trimmed.is_empty()
            {
                return None;
            }
            // A variant line looks like `NodeNotFound(u64),` or
            // `Locked,`. Bail on anything that doesn't look like an
            // identifier start.
            let first = trimmed.split(|c: char| !c.is_ascii_alphanumeric()).next()?;
            if first.is_empty() || !first.chars().next()?.is_ascii_uppercase() {
                return None;
            }
            // Heuristic: variant identifiers are at most one PascalCase word
            // and the line continues with `(` or `,`.
            let after = trimmed[first.len()..].trim_start();
            if after.starts_with('(') || after.starts_with(',') {
                Some(first)
            } else {
                None
            }
        })
        .collect();

    assert!(
        !variants.is_empty(),
        "could not parse any DrevoError variants from src/error.rs — \
         either the file moved or the heuristic in this test needs an update"
    );

    for variant in &variants {
        // The Python error mapper uses `D::Variant(...)` patterns —
        // search for the variant name with the `D::` prefix to avoid
        // matching unrelated identifiers.
        let needle = format!("D::{variant}");
        assert!(
            py_err.contains(&needle),
            "DrevoError::{variant} has no arm in drevo-py/src/errors.rs \
             map_err — RFC §5.2 requires every Rust variant to map to a \
             Python exception. Add a `D::{variant} => ...` arm there."
        );
    }
}

// ── 7. Public methods on the Drevo handle ──────────────────────────────

#[test]
fn handle_exposes_rfc_required_methods() {
    let handle = read(&repo_root().join("drevo-py").join("src").join("handle.rs"));
    let required = [
        "fn open(",
        "fn open_in_memory(",
        "fn close(",
        "fn __enter__(",
        "fn __exit__(",
        "fn compact(",
        "fn health_check(",
        "fn create_node(",
        "fn get_node(",
        "fn get_node_by_uuid(",
        "fn get_node_by_title(",
        "fn update_node(",
        "fn delete_node(",
        "fn create_edge(",
        "fn get_edge(",
        "fn get_edge_by_uuid(",
        "fn update_edge(",
        "fn delete_edge(",
        "fn edges_of(",
        "fn list_nodes_by_kind(",
        "fn list_edges_by_kind(",
        "fn list_recent(",
        "fn bfs(",
        "fn dfs(",
        "fn shortest_path(",
        "fn subgraph(",
        "fn neighbors(",
        "fn search_fts(",
    ];
    for method in required {
        assert!(
            handle.contains(method),
            "drevo-py/src/handle.rs is missing `{method}` — RFC §3.3 \
             type-stub block lists this method as part of the `Drevo` \
             class contract."
        );
    }
}

// ── 8. GIL release contract ────────────────────────────────────────────

#[test]
fn handle_releases_gil_on_storage_io() {
    let handle = read(&repo_root().join("drevo-py").join("src").join("handle.rs"));
    // RFC §4.2: "Every storage I/O call wraps the Rust body in
    // py.allow_threads(|| {...})". Count occurrences as a cheap proxy —
    // we expect at least one per method that performs storage I/O.
    // Today there are ~25 such methods.
    let count = handle.matches("allow_threads").count();
    assert!(
        count >= 20,
        "drevo-py/src/handle.rs has only {count} `allow_threads` calls — \
         RFC §4.2 requires the GIL be released on every storage I/O. \
         Audit the file and add the missing `py.allow_threads(...)` \
         wrappers."
    );
}

// ── 9. Panic-catch contract ────────────────────────────────────────────

#[test]
fn handle_wraps_methods_in_catch_unwind() {
    let handle = read(&repo_root().join("drevo-py").join("src").join("handle.rs"));
    assert!(
        handle.contains("catch_unwind"),
        "drevo-py/src/handle.rs must wrap method bodies in \
         std::panic::catch_unwind — RFC §5.4 forbids a Rust panic from \
         crossing the FFI boundary as an abort signal. See `guarded()` \
         in handle.rs for the canonical pattern."
    );
    assert!(
        handle.contains("panic_to_pyerr"),
        "drevo-py/src/handle.rs must convert caught panics via \
         errors::panic_to_pyerr so they surface as `drevo.PanicError` \
         (RFC §5.2 table — bottom row, \"panic across FFI\")"
    );
}
