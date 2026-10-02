//! Explicit transactions — `Drevo.begin()` / `Drevo.transaction()` (#554).
//!
//! A [`Transaction`] wraps one registered native transaction
//! (`NativeService::begin_tx` / `execute_in_tx` / `commit_tx` /
//! `rollback_tx`): its Cypher statements read their own writes, nobody else
//! sees them until `commit()`, and `rollback()` discards them. Commit is one
//! fsynced WAL batch and optimistic — a graph that changed since `begin()`
//! raises the retryable [`TransactionConflict`]. A statement that fails
//! inside the transaction rolls it back (Neo4j semantics: the working copy
//! may already hold part of that statement's writes), and any use of a
//! closed transaction raises [`TransactionError`].
//!
//! The transaction holds only a [`Weak`] reference to the database, upgraded
//! for the duration of each call: `Drevo.close()` waits for every strong
//! reference to go away, so a live transaction object must never keep the
//! handle open. Once the database is closed the transaction raises
//! `RuntimeError`, like the handle itself.

use std::sync::{Arc, Mutex, Weak};

use drevo::native::{CommitError, NativeTxId, PrepareError, ResolveError};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use crate::cypher::{map_cypher_err, params_from_py, result_to_py, CypherResult};
use crate::errors::{
    panic_to_pyerr, ConstraintViolation, PreparedTransactionError, StorageError,
    TransactionConflict, TransactionError, UnknownGidError,
};
use crate::native_backend::NativeBackend;

/// Map a failed commit onto the Python exception hierarchy.
fn map_commit_err(e: CommitError) -> PyErr {
    match e {
        CommitError::Conflict => TransactionConflict::new_err(e.to_string()),
        CommitError::Constraint(v) => ConstraintViolation::new_err((v.message.clone(), v.kind)),
        CommitError::Io(msg) => StorageError::new_err(msg),
        // Writes are paused while a two-phase-commit transaction is prepared
        // (#556); retryable once it is resolved, like an optimistic conflict.
        e @ CommitError::PreparedPending(_) => TransactionConflict::new_err(e.to_string()),
    }
}

/// Map a failed prepare (#556) onto the Python exception hierarchy.
fn map_prepare_err(e: PrepareError) -> PyErr {
    match e {
        PrepareError::Conflict | PrepareError::PreparedPending(_) => {
            TransactionConflict::new_err(e.to_string())
        }
        PrepareError::Constraint(v) => ConstraintViolation::new_err((v.message.clone(), v.kind)),
        PrepareError::DuplicateGid(_) => PreparedTransactionError::new_err(e.to_string()),
        PrepareError::UnknownTransaction => TransactionError::new_err("transaction is closed"),
        PrepareError::Io(msg) => StorageError::new_err(msg),
    }
}

/// Map a failed two-phase-commit resolution (#556) onto Python.
pub(crate) fn map_resolve_err(e: ResolveError) -> PyErr {
    match e {
        ResolveError::UnknownGid(_) => UnknownGidError::new_err(e.to_string()),
        ResolveError::Io(msg) => StorageError::new_err(msg),
    }
}

/// A prepared, unresolved two-phase-commit transaction, as listed by
/// `Drevo.list_prepared()` (#556).
#[pyclass(frozen, name = "PreparedTransaction", eq)]
#[derive(PartialEq)]
pub struct PreparedTransaction {
    gid: String,
    prepared_at_ms: i64,
    op_count: usize,
}

impl PreparedTransaction {
    pub(crate) fn new(info: drevo::native::PreparedInfo) -> Self {
        Self {
            gid: info.gid,
            prepared_at_ms: info.prepared_at_ms,
            op_count: info.op_count,
        }
    }
}

#[pymethods]
impl PreparedTransaction {
    /// The coordinator's global transaction id.
    #[getter]
    fn gid(&self) -> &str {
        &self.gid
    }

    /// When it was prepared (Unix ms).
    #[getter]
    fn prepared_at_ms(&self) -> i64 {
        self.prepared_at_ms
    }

    /// Number of write operations in its write set.
    #[getter]
    fn op_count(&self) -> usize {
        self.op_count
    }

    fn __repr__(&self) -> String {
        format!(
            "PreparedTransaction(gid={:?}, op_count={})",
            self.gid, self.op_count
        )
    }
}

fn guarded<T>(f: impl FnOnce() -> PyResult<T>) -> PyResult<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .unwrap_or_else(|p| Err(panic_to_pyerr(p)))
}

/// An explicit transaction; see the module docs.
#[pyclass(name = "Transaction")]
pub struct Transaction {
    db: Weak<NativeBackend>,
    /// `None` once committed, rolled back, or aborted by a failed statement.
    id: Mutex<Option<NativeTxId>>,
}

impl Transaction {
    /// Begin a transaction on `db`.
    pub(crate) fn begin(db: &Arc<NativeBackend>) -> Self {
        Self {
            db: Arc::downgrade(db),
            id: Mutex::new(Some(db.begin_tx())),
        }
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, Option<NativeTxId>> {
        self.id.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn db(&self) -> PyResult<Arc<NativeBackend>> {
        self.db.upgrade().ok_or_else(|| {
            PyRuntimeError::new_err(
                "Drevo handle is closed; create a fresh instance with Drevo.open(...)",
            )
        })
    }

    fn current(&self) -> PyResult<NativeTxId> {
        (*self.slot()).ok_or_else(|| TransactionError::new_err("transaction is closed"))
    }

    /// Close the transaction, returning its id (an error if already closed).
    fn take(&self) -> PyResult<NativeTxId> {
        self.slot()
            .take()
            .ok_or_else(|| TransactionError::new_err("transaction is closed"))
    }

    /// Discard the transaction if it is still open (best effort).
    fn abort(&self) {
        if let Some(id) = self.slot().take() {
            if let Some(db) = self.db.upgrade() {
                db.rollback_tx(id);
            }
        }
    }
}

#[pymethods]
impl Transaction {
    /// Run one Cypher statement inside the transaction. A failure rolls the
    /// whole transaction back and closes it.
    #[pyo3(signature = (query, params=None))]
    fn execute(
        &self,
        py: Python<'_>,
        query: &str,
        params: Option<Bound<'_, PyAny>>,
    ) -> PyResult<CypherResult> {
        guarded(|| {
            let params = params_from_py(params.as_ref())?;
            let db = self.db()?;
            let id = self.current()?;
            match py.allow_threads(|| db.execute_cypher_in_tx(id, query, params)) {
                Ok(result) => result_to_py(py, &result),
                Err(failure) => {
                    self.abort();
                    Err(map_cypher_err(failure))
                }
            }
        })
    }

    /// Commit: apply every write atomically (one fsynced WAL batch). Raises
    /// `TransactionConflict` if the graph changed since `begin()`.
    fn commit(&self, py: Python<'_>) -> PyResult<()> {
        guarded(|| {
            let db = self.db()?;
            let id = self.take()?;
            py.allow_threads(|| db.commit_tx(id))
                .map_err(map_commit_err)
        })
    }

    /// Two-phase commit, phase one (#556): validate the transaction and record
    /// it durably as prepared under `gid`, closing it. Finish it with
    /// `Drevo.commit_prepared(gid)` or `Drevo.rollback_prepared(gid)` — from
    /// any handle, also after a restart. While it is prepared, every other
    /// write raises `TransactionConflict`.
    fn prepare(&self, py: Python<'_>, gid: &str) -> PyResult<()> {
        guarded(|| {
            let db = self.db()?;
            let id = self.take()?;
            py.allow_threads(|| db.prepare_tx(id, gid))
                .map_err(map_prepare_err)
        })
    }

    /// Roll back: discard every write.
    fn rollback(&self) -> PyResult<()> {
        guarded(|| {
            let id = self.take()?;
            if let Some(db) = self.db.upgrade() {
                db.rollback_tx(id);
            }
            Ok(())
        })
    }

    /// Whether the transaction has been committed, rolled back or aborted.
    #[getter]
    fn closed(&self) -> bool {
        self.slot().is_none()
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// Commit on a clean exit, roll back on an exception (which then
    /// propagates). A transaction already closed inside the block is left
    /// alone.
    #[pyo3(signature = (exc_type, _exc_value=None, _traceback=None))]
    fn __exit__(
        &self,
        py: Python<'_>,
        exc_type: Option<Bound<'_, PyAny>>,
        _exc_value: Option<Bound<'_, PyAny>>,
        _traceback: Option<Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        if self.closed() {
            return Ok(false);
        }
        match exc_type {
            Some(t) if !t.is_none() => self.rollback()?,
            _ => self.commit(py)?,
        }
        Ok(false)
    }

    fn __repr__(&self) -> String {
        format!("Transaction(closed={})", self.closed())
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        // An abandoned transaction must not leak its registered slot.
        self.abort();
    }
}
