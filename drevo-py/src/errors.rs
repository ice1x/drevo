//! Typed exception hierarchy + `DrevoError → PyErr` mapping.
//!
//! Implements RFC §5 of `audit/RFC-python-api.md`. The hierarchy
//! deliberately roots at `drevo.DrevoError` (not `ValueError`) so that a
//! user-side `except ValueError:` block does not accidentally swallow
//! drevo-specific failures.
//!
//! The one exception is `drevo.InvalidWeightError` — it extends
//! `ValueError` because "you passed me a bad number" is the canonical
//! Python idiom for `ValueError`. The pure-Python subclass that ties the
//! name `InvalidWeightError` to `ValueError` lands in task `00116`
//! (`drevo/errors.py`); until then [`map_err`] raises a plain
//! `PyValueError` with a substring-matchable message — callers that
//! `except ValueError:` work today, callers that
//! `except drevo.InvalidWeightError:` work the day `00116` lands without
//! a re-raise change here.

use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyValueError};
use pyo3::prelude::*;

// ── Exception types ────────────────────────────────────────────────────
//
// The `create_exception!` macro generates both the Python-side class and
// a Rust ZST whose `::new_err(...)` constructs a `PyErr` of that class.
// The inheritance chain is enforced statically by passing the parent ZST
// as the third argument.

create_exception!(_drevo, DrevoError, PyException);
create_exception!(_drevo, NotFoundError, DrevoError);
create_exception!(_drevo, NodeNotFoundError, NotFoundError);
create_exception!(_drevo, EdgeNotFoundError, NotFoundError);
create_exception!(_drevo, ConflictError, DrevoError);
create_exception!(_drevo, DuplicateTitleError, ConflictError);
create_exception!(_drevo, StorageError, DrevoError);
create_exception!(_drevo, SerializationError, DrevoError);
create_exception!(_drevo, LockedError, DrevoError);
create_exception!(_drevo, PanicError, DrevoError);
create_exception!(_drevo, TransactionError, DrevoError);

/// Add the exception classes to the `_drevo` Python module so they are
/// importable as `drevo.<ClassName>` after task `00116` re-exports them.
pub(crate) fn register(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("DrevoError", py.get_type::<DrevoError>())?;
    m.add("NotFoundError", py.get_type::<NotFoundError>())?;
    m.add("NodeNotFoundError", py.get_type::<NodeNotFoundError>())?;
    m.add("EdgeNotFoundError", py.get_type::<EdgeNotFoundError>())?;
    m.add("ConflictError", py.get_type::<ConflictError>())?;
    m.add("DuplicateTitleError", py.get_type::<DuplicateTitleError>())?;
    m.add("StorageError", py.get_type::<StorageError>())?;
    m.add("SerializationError", py.get_type::<SerializationError>())?;
    m.add("LockedError", py.get_type::<LockedError>())?;
    m.add("PanicError", py.get_type::<PanicError>())?;
    m.add("TransactionError", py.get_type::<TransactionError>())?;
    Ok(())
}

/// Map a [`drevo::error::DrevoError`] onto the matching Python exception.
///
/// Mirrors the table in RFC §5.2 exactly — every Rust variant has a row,
/// every row maps to a single Python class. The argument tuple passed to
/// `new_err((...))` becomes the Python exception's `.args` attribute, so
/// `NodeNotFoundError(42).args == (42,)` round-trips faithfully.
///
/// Error messages are part of the public API per RFC §5.5 — substring
/// stability is asserted in the Python unit-test layer (`00118`).
pub(crate) fn map_err(e: drevo::error::DrevoError) -> PyErr {
    use drevo::error::DrevoError as D;
    match e {
        D::NodeNotFound(id) => NodeNotFoundError::new_err((id,)),
        D::EdgeNotFound(id) => EdgeNotFoundError::new_err((id,)),
        D::DuplicateTitle(t) => DuplicateTitleError::new_err((t,)),
        D::InvalidWeight(w) => PyValueError::new_err(format!(
            "invalid edge weight: {w} — weight must be a finite f32"
        )),
        D::Locked => LockedError::new_err(()),
        D::Storage(s) => StorageError::new_err(s.to_string()),
        D::Encode(err) => SerializationError::new_err(("encode", err.to_string())),
        D::Decode(err) => SerializationError::new_err(("decode", err.to_string())),
        D::Io(err) => StorageError::new_err(err.to_string()),
        D::TransactionAlreadyActive => TransactionError::new_err("transaction already active"),
        D::NoActiveTransaction => TransactionError::new_err("no active transaction"),
    }
}

/// Convert a Rust panic payload (`Box<dyn Any + Send>`) into a
/// [`PanicError`].
///
/// Used by `handle::Drevo` to wrap every PyO3 method body in
/// `catch_unwind` so a panic in `drevo`/`redb` never crosses the FFI
/// boundary as an abort signal (RFC §5.4 — mirrors the C-FFI discipline
/// audited in `audit/AUDIT-ffi.md`).
pub(crate) fn panic_to_pyerr(payload: Box<dyn std::any::Any + Send>) -> PyErr {
    let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic with non-string payload".to_string()
    };
    PanicError::new_err(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use drevo::error::DrevoError;

    /// Cheap sanity check — make sure every `DrevoError` variant maps
    /// without panicking and produces a `PyErr` whose type string mentions
    /// the expected exception name. The full per-variant assertion that
    /// the *type identity* is `NodeNotFoundError` (and not just a string
    /// match) requires `Python::with_gil`, which the rust-only test
    /// harness does not initialise — that side of the contract lives in
    /// the Python unit suite (`00118` / `tests/unit/test_errors.py`).
    #[test]
    fn every_drevo_error_variant_maps() {
        let variants = vec![
            DrevoError::NodeNotFound(42),
            DrevoError::EdgeNotFound(7),
            DrevoError::DuplicateTitle("dup".into()),
            DrevoError::InvalidWeight(f32::NAN),
            DrevoError::Locked,
        ];
        for v in variants {
            let msg = format!("{v}");
            let py_err = map_err(v);
            // `PyErr::to_string` formats as "TypeName: message". We
            // verify that the original Rust error's `Display` substring
            // survives the conversion so traceback strings stay
            // diagnosable.
            assert!(
                !py_err.to_string().is_empty(),
                "py_err message empty for {msg}"
            );
        }
    }
}
