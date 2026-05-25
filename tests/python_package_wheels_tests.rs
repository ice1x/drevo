//! Phase 16 task `00116` — text-level scaffolding tests for the
//! `drevo-py` Python package skeleton + wheel build configuration.
//!
//! Same pattern as `python_api_scaffolding_tests.rs` for `00115`: these
//! tests do **not** invoke `maturin`, `pip`, `cibuildwheel`, or any
//! Python interpreter (the existing CI runners do not provision Python —
//! the dedicated Python CI matrix is task `00122`). Instead they lock
//! the *layout* and *contract* of every file the task ships by grepping
//! the files on disk:
//!
//! 1. `drevo-py/pyproject.toml` — PEP 621 metadata, `maturin` build
//!    backend, project name `drevo-py`, ABI3 target ≥ 3.10, license,
//!    classifiers, `[tool.maturin]` pointing at the right module name
//!    and the `python/` source dir.
//! 2. `drevo-py/python/drevo/__init__.py` — pure-Python shim that
//!    imports `_drevo`, re-exports the public surface, and wraps the
//!    raw 16-byte `bytes` UUIDs returned by `_drevo` as `uuid.UUID`
//!    (RFC §3.2 default + §12.2 amendment).
//! 3. `drevo-py/python/drevo/errors.py` — `InvalidWeightError(ValueError)`
//!    subclass so `except drevo.InvalidWeightError:` works after the
//!    shim lands (RFC §5.3 + §12.3 amendment).
//! 4. `drevo-py/python/drevo/__init__.pyi` — hand-authored type stubs
//!    matching the RFC §3.3 `Drevo` surface, so `mypy --strict` sees
//!    typed signatures.
//! 5. `drevo-py/python/drevo/py.typed` — PEP 561 marker (empty file).
//! 6. `drevo-py/LICENSE` — dual MIT / Apache-2.0 (matches the
//!    `license = "MIT OR Apache-2.0"` in `drevo-py/Cargo.toml`).
//! 7. `drevo-py/CHANGELOG.md` — Keep-a-Changelog format with the
//!    initial `0.1.0` entry covering `00115` + `00116`.
//! 8. `drevo-py/README.md` — already shipped by `00115`; this task
//!    extends it with `pip install drevo-py` quick-start once the
//!    wheel build is in place.
//! 9. `.github/workflows/python-wheels.yml` — `cibuildwheel` matrix
//!    for cp310/cp311/cp312/cp313 × {linux, macos, windows}, plus
//!    `twine check dist/*`. Triggered manually + on every PR via
//!    `workflow_dispatch` and the existing PR `pull_request` event,
//!    but kept **off** branch-protection gates until task `00122`
//!    promotes it.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

// ── 1. pyproject.toml ──────────────────────────────────────────────────

#[test]
fn pyproject_toml_declares_maturin_build_backend() {
    let toml = read(&repo_root().join("drevo-py").join("pyproject.toml"));
    assert!(
        toml.contains("[build-system]"),
        "drevo-py/pyproject.toml must declare [build-system] — without it \
         `pip install .` cannot select a backend"
    );
    assert!(
        toml.contains("build-backend = \"maturin\""),
        "drevo-py/pyproject.toml must set build-backend = \"maturin\" — \
         RFC §2 wheel layout requires the PyO3-aware build backend"
    );
    assert!(
        toml.contains("requires") && toml.contains("maturin"),
        "drevo-py/pyproject.toml [build-system].requires must include maturin"
    );
}

#[test]
fn pyproject_toml_declares_pep621_metadata() {
    let toml = read(&repo_root().join("drevo-py").join("pyproject.toml"));
    assert!(
        toml.contains("[project]"),
        "drevo-py/pyproject.toml must declare a PEP 621 [project] table"
    );
    assert!(
        toml.contains("name = \"drevo-py\""),
        "[project].name must be \"drevo-py\" (matches Cargo crate name)"
    );
    // Version source: dynamic via maturin reading Cargo.toml — either a
    // literal `version = "0.1.0"` OR `dynamic = ["version"]` is acceptable.
    assert!(
        toml.contains("version = \"0.1.0\"") || toml.contains("dynamic"),
        "[project] must declare a version, either statically (\"0.1.0\") \
         or dynamically via maturin"
    );
    assert!(
        toml.contains("description"),
        "[project].description must be set so PyPI shows a one-liner"
    );
    assert!(
        toml.contains("requires-python") && toml.contains(">=3.10"),
        "[project].requires-python must be \">=3.10\" — matches the \
         abi3-py310 feature in drevo-py/Cargo.toml"
    );
    assert!(
        toml.contains("license"),
        "[project].license must be set (PyPI requires this metadata)"
    );
    assert!(
        toml.contains("classifiers"),
        "[project].classifiers must enumerate Python versions and the OS \
         compat matrix so PyPI search surfaces the package correctly"
    );
}

#[test]
fn pyproject_toml_configures_maturin_python_layout() {
    let toml = read(&repo_root().join("drevo-py").join("pyproject.toml"));
    assert!(
        toml.contains("[tool.maturin]"),
        "drevo-py/pyproject.toml must have a [tool.maturin] section \
         to point maturin at the python/ source layout"
    );
    // `python-source = "python"` puts the pure-Python `drevo/` package
    // under drevo-py/python/drevo/, which is what RFC §2 wheel layout
    // mandates (cdylib + .py files merged into one wheel).
    assert!(
        toml.contains("python-source = \"python\""),
        "[tool.maturin].python-source must be \"python\" so maturin \
         picks up drevo-py/python/drevo/*.py into the wheel"
    );
    // The module name passed to maturin is the *Python package* name,
    // i.e. the directory under python/. Must match `import drevo`.
    assert!(
        toml.contains("module-name = \"drevo._drevo\""),
        "[tool.maturin].module-name must be \"drevo._drevo\" — the dotted \
         path the cdylib is installed at inside the wheel. RFC §2: end \
         users `import drevo`; `_drevo` is the private extension module."
    );
    // Features forwarded to cargo when maturin builds the wheel.
    // `extension-module` is mandatory (no libpython linkage); the wheel
    // dynamically resolves libpython at import time.
    assert!(
        toml.contains("features") && toml.contains("extension-module"),
        "[tool.maturin].features must include \"pyo3/extension-module\" \
         so the wheel builds against the abi3 surface without linking \
         libpython"
    );
}

// ── 2. python/drevo/__init__.py shim ───────────────────────────────────

#[test]
fn drevo_package_init_imports_native_extension() {
    let init = read(
        &repo_root()
            .join("drevo-py")
            .join("python")
            .join("drevo")
            .join("__init__.py"),
    );
    assert!(
        init.contains("from . import _drevo") || init.contains("from ._drevo import"),
        "drevo/__init__.py must import the `_drevo` native extension \
         module (RFC §2 wheel layout — `_drevo` is the cdylib)"
    );
    // Re-exports: the user-facing import is `from drevo import Drevo`
    // — so `Drevo` (and the headline classes) must appear in __all__
    // or as a direct re-export.
    for symbol in [
        "Drevo",
        "Node",
        "Edge",
        "NewNode",
        "NewEdge",
        "NodePatch",
        "EdgePatch",
        "Direction",
        "ScoredNode",
        "SubGraph",
        "CompactReport",
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
        "InvalidWeightError",
    ] {
        assert!(
            init.contains(symbol),
            "drevo/__init__.py must re-export `{symbol}` so \
             `from drevo import {symbol}` works at the top level"
        );
    }
}

#[test]
fn drevo_package_init_wraps_uuid_bytes_as_uuid_uuid() {
    let init = read(
        &repo_root()
            .join("drevo-py")
            .join("python")
            .join("drevo")
            .join("__init__.py"),
    );
    // RFC §12.2 amendment: the PyO3 layer returns 16-byte `bytes`; this
    // shim layer wraps them as `uuid.UUID`. The shim therefore MUST
    // `import uuid` and replace the `_drevo.Node.uuid` / `_drevo.Edge.uuid`
    // descriptors (or wrap them in a thin Python subclass) so callers
    // see `uuid.UUID` instances.
    assert!(
        init.contains("import uuid"),
        "drevo/__init__.py must `import uuid` to convert 16-byte \
         `bytes` UUIDs from `_drevo` into native `uuid.UUID` objects \
         (RFC §12.2 amendment)"
    );
    // The wrapping has to actually use the imported module — accept
    // either `uuid.UUID(bytes=...)` (canonical constructor) or
    // `uuid.UUID(int=...)` as evidence of an actual conversion.
    assert!(
        init.contains("uuid.UUID"),
        "drevo/__init__.py must actually construct `uuid.UUID(...)` \
         instances — `import uuid` without usage is not enough"
    );
}

#[test]
fn drevo_package_init_exposes_version() {
    let init = read(
        &repo_root()
            .join("drevo-py")
            .join("python")
            .join("drevo")
            .join("__init__.py"),
    );
    // `_drevo` exports `__version__` from the Rust side (set in
    // `drevo-py/src/lib.rs` to `env!("CARGO_PKG_VERSION")`). The shim
    // re-exports it so `drevo.__version__` works.
    assert!(
        init.contains("__version__"),
        "drevo/__init__.py must re-export `__version__` from `_drevo` \
         so `drevo.__version__` resolves at the top level"
    );
}

// ── 3. python/drevo/errors.py ──────────────────────────────────────────

#[test]
fn errors_py_declares_invalid_weight_error_subclass_of_value_error() {
    let errors = read(
        &repo_root()
            .join("drevo-py")
            .join("python")
            .join("drevo")
            .join("errors.py"),
    );
    // RFC §5.3 + §12.3: `InvalidWeightError` extends `ValueError` because
    // the Rust mapper raises `PyValueError` for `DrevoError::InvalidWeight`.
    // The subclass exists so callers can `except drevo.InvalidWeightError:`
    // without forcing the PyO3 layer to import-and-raise the subclass
    // (which would couple the cdylib to the pure-Python module).
    assert!(
        errors.contains("class InvalidWeightError(ValueError)"),
        "drevo/errors.py must declare `class InvalidWeightError(ValueError):` \
         — RFC §5.3 + §12.3 amendment. The pure-Python subclass closes \
         the gap left by `00115`."
    );
}

// ── 4. python/drevo/__init__.pyi type stubs ────────────────────────────

#[test]
fn type_stubs_declare_public_surface() {
    let stub = read(
        &repo_root()
            .join("drevo-py")
            .join("python")
            .join("drevo")
            .join("__init__.pyi"),
    );
    // Every class in the public surface must appear in the stub so
    // `mypy --strict drevo/` can resolve `drevo.Drevo`, `drevo.Node`, …
    // The stub also locks the *signatures* a user sees, separate from
    // whatever the PyO3 introspection would invent at runtime.
    for class in [
        "class Drevo",
        "class Node",
        "class Edge",
        "class NewNode",
        "class NewEdge",
        "class NodePatch",
        "class EdgePatch",
        "class Direction",
        "class ScoredNode",
        "class SubGraph",
        "class CompactReport",
    ] {
        assert!(
            stub.contains(class),
            "drevo/__init__.pyi must declare `{class}:` so mypy --strict \
             resolves the user-facing surface"
        );
    }
    // Every method on `Drevo` listed in RFC §3.3.
    for method in [
        "def open(",
        "def open_in_memory(",
        "def close(",
        "def __enter__(",
        "def __exit__(",
        "def compact(",
        "def health_check(",
        "def create_node(",
        "def get_node(",
        "def get_node_by_uuid(",
        "def get_node_by_title(",
        "def update_node(",
        "def delete_node(",
        "def create_edge(",
        "def get_edge(",
        "def get_edge_by_uuid(",
        "def update_edge(",
        "def delete_edge(",
        "def edges_of(",
        "def list_nodes_by_kind(",
        "def list_edges_by_kind(",
        "def list_recent(",
        "def bfs(",
        "def dfs(",
        "def shortest_path(",
        "def subgraph(",
        "def neighbors(",
        "def search_fts(",
    ] {
        assert!(
            stub.contains(method),
            "drevo/__init__.pyi must declare `{method}...` on `Drevo` \
             — RFC §3.3 type-stub block lists this method"
        );
    }
    // Exception hierarchy — must be importable from `drevo` per shim
    // re-exports.
    for exc in [
        "class DrevoError",
        "class NotFoundError",
        "class NodeNotFoundError",
        "class EdgeNotFoundError",
        "class ConflictError",
        "class DuplicateTitleError",
        "class StorageError",
        "class SerializationError",
        "class LockedError",
        "class PanicError",
        "class InvalidWeightError",
    ] {
        assert!(
            stub.contains(exc),
            "drevo/__init__.pyi must declare `{exc}` so static type \
             checkers see the exception hierarchy"
        );
    }
}

// ── 5. PEP 561 marker ──────────────────────────────────────────────────

#[test]
fn pep561_marker_exists_and_is_empty() {
    let marker = repo_root()
        .join("drevo-py")
        .join("python")
        .join("drevo")
        .join("py.typed");
    assert!(
        marker.exists(),
        "drevo-py/python/drevo/py.typed must exist — PEP 561 marker \
         tells mypy / pyright to read the inline / .pyi type info"
    );
    // PEP 561 specifies the marker is an empty file. Some projects put
    // a `partial\n` line in it to mark partial typing; we ship full
    // stubs, so the file must be empty (or near-empty).
    let body = fs::read(&marker).unwrap();
    assert!(
        body.len() <= 16,
        "drevo-py/python/drevo/py.typed should be empty (PEP 561) — \
         found {} bytes",
        body.len()
    );
}

// ── 6. LICENSE ─────────────────────────────────────────────────────────

#[test]
fn license_file_documents_dual_mit_apache_license() {
    let license = read(&repo_root().join("drevo-py").join("LICENSE"));
    // drevo-py/Cargo.toml declares `license = "MIT OR Apache-2.0"` —
    // the LICENSE file must surface both license names so a downstream
    // consumer scanning `pip show drevo-py` or `cargo about` resolves
    // the SPDX identifiers correctly.
    assert!(
        license.contains("MIT") && license.contains("Apache"),
        "drevo-py/LICENSE must mention both MIT and Apache-2.0 — the \
         Cargo manifest declares `MIT OR Apache-2.0` and PyPI scrapes \
         the LICENSE file for the license text"
    );
}

// ── 7. CHANGELOG.md ────────────────────────────────────────────────────

#[test]
fn changelog_documents_initial_release_and_tasks() {
    let changelog = read(&repo_root().join("drevo-py").join("CHANGELOG.md"));
    assert!(
        changelog.contains("Keep a Changelog") || changelog.contains("keepachangelog"),
        "drevo-py/CHANGELOG.md should cite the Keep-a-Changelog format \
         so the layout is unambiguous"
    );
    assert!(
        changelog.contains("0.1.0"),
        "drevo-py/CHANGELOG.md must record the initial 0.1.0 release"
    );
    assert!(
        changelog.contains("00115"),
        "drevo-py/CHANGELOG.md must cite task 00115 — first release shipped \
         the PyO3 core surface"
    );
    assert!(
        changelog.contains("00116"),
        "drevo-py/CHANGELOG.md must cite task 00116 — first release ships \
         the package skeleton + wheel build"
    );
}

// ── 8. README quick-start mentions pip install ─────────────────────────

#[test]
fn drevo_py_readme_documents_pip_install() {
    let readme = read(&repo_root().join("drevo-py").join("README.md"));
    assert!(
        readme.contains("pip install"),
        "drevo-py/README.md must show `pip install drevo-py` or \
         `pip install .` so a Python user lands on a usable command \
         after `00116` ships the wheel skeleton"
    );
    assert!(
        readme.contains("maturin"),
        "drevo-py/README.md must mention maturin — RFC §2 build backend"
    );
}

// ── 9. cibuildwheel workflow ───────────────────────────────────────────

#[test]
fn cibuildwheel_workflow_exists_with_full_matrix() {
    let wf = read(
        &repo_root()
            .join(".github")
            .join("workflows")
            .join("python-wheels.yml"),
    );
    // cibuildwheel is the de-facto cross-platform wheel builder; the
    // pypa/cibuildwheel action is the standard incantation.
    assert!(
        wf.contains("cibuildwheel") || wf.contains("pypa/cibuildwheel"),
        ".github/workflows/python-wheels.yml must invoke `cibuildwheel` \
         (RFC §2 / Phase 16 cross-cutting acceptance criteria)"
    );
    // RFC §2 / README cross-cutting acceptance criteria — every minor
    // version cp310..cp313 must build a wheel.
    for cpy in ["cp310", "cp311", "cp312", "cp313"] {
        assert!(
            wf.contains(cpy),
            "python-wheels.yml CIBW_BUILD must enumerate `{cpy}` — RFC \
             §2 wheel matrix"
        );
    }
    // OS matrix: linux, macos, windows — at least the runner labels
    // must appear so the matrix actually fans out.
    for os in ["ubuntu", "macos", "windows"] {
        assert!(
            wf.contains(os),
            "python-wheels.yml runner matrix must include `{os}-latest` \
             — RFC §2 / Phase 16 cross-cutting acceptance criteria"
        );
    }
    // `twine check dist/*` validates wheel metadata before any future
    // release task uploads — locks the contract today.
    assert!(
        wf.contains("twine check"),
        "python-wheels.yml must run `twine check dist/*` so wheel \
         metadata is validated even before publishing"
    );
}

#[test]
fn cibuildwheel_workflow_does_not_publish_to_pypi() {
    let wf = read(
        &repo_root()
            .join(".github")
            .join("workflows")
            .join("python-wheels.yml"),
    );
    // Task 00116 acceptance criterion: "No publishing yet — the wheel
    // build is exercised in CI on every PR; publishing to PyPI lands
    // later as a separate release task." Catching `twine upload` or
    // `pypa/gh-action-pypi-publish` here prevents a future contributor
    // from accidentally turning the CI workflow into a publish workflow.
    assert!(
        !wf.contains("twine upload") && !wf.contains("pypi-publish"),
        ".github/workflows/python-wheels.yml MUST NOT publish to PyPI \
         — task 00116 builds wheels only; publishing is a separate \
         release task"
    );
}
