//! PyO3 bindings for the drevo embedded graph database.
//!
//! Phase 16 task `00115`. The contract this crate implements lives in
//! `audit/RFC-python-api.md`; see in particular:
//!
//! * §3 — naming and type system (`snake_case` methods, frozen `pyclass`
//!   dataclass-like wrappers, `uuid::Uuid` ↔ Python `uuid.UUID`),
//! * §4 — sync-only surface with GIL released on every storage I/O via
//!   [`pyo3::Python::allow_threads`],
//! * §5 — typed exception hierarchy rooted at `drevo.DrevoError` plus
//!   the variant-by-variant `DrevoError → PyErr` mapping in
//!   [`errors::map_err`],
//! * §6 — list-returning surface (iterators land alongside the pure-Python
//!   `drevo.rag` layer in task `00117`),
//! * §7 — batch APIs (`create_nodes` / `create_edges`) are deferred —
//!   they require a transactional batch entry on the Rust side and are
//!   tracked as a separate follow-up under Phase 16.
//!
//! Module layout:
//!
//! * [`errors`] — `create_exception!` hierarchy + `map_err`.
//! * [`types`] — frozen `#[pyclass]` wrappers for `Node`, `Edge`,
//!   `NewNode`, `NewEdge`, `NodePatch`, `EdgePatch`, `ScoredNode`,
//!   `SubGraph`, `CompactReport`, plus the `Direction` `IntEnum`.
//! * [`handle`] — the `Drevo` handle (`open`, `close`, CRUD, traversal,
//!   FTS), every storage method wrapped in `py.allow_threads(...)`.

#![warn(missing_docs)]

mod errors;
mod handle;
mod types;

use pyo3::prelude::*;

/// PyO3 module entry point.
///
/// Registered under the runtime module name `_drevo` per the
/// `[lib] name = "_drevo"` declaration in `Cargo.toml`. The user-facing
/// import is `import drevo`; the pure-Python `drevo/__init__.py`
/// (task `00116`) re-exports symbols from this module.
#[pymodule]
fn _drevo(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Exception hierarchy (§5.1 of the RFC).
    errors::register(py, m)?;

    // Plain-data types (§3.2 + §3.3).
    m.add_class::<types::Direction>()?;
    m.add_class::<types::Node>()?;
    m.add_class::<types::Edge>()?;
    m.add_class::<types::NewNode>()?;
    m.add_class::<types::NewEdge>()?;
    m.add_class::<types::NodePatch>()?;
    m.add_class::<types::EdgePatch>()?;
    m.add_class::<types::ScoredNode>()?;
    m.add_class::<types::SubGraph>()?;
    m.add_class::<types::CompactReport>()?;
    m.add_class::<types::ImportReport>()?;
    m.add_class::<types::BloatReport>()?;

    // The handle.
    m.add_class::<handle::Drevo>()?;

    // Version string — surfaced to Python as `drevo.__version__`
    // (re-exported by `drevo/__init__.py` in task `00116`).
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;

    Ok(())
}
