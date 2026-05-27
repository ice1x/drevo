//! The `Drevo` handle — the Python-side entry point to the database.
//!
//! Implements RFC §4 (sync surface with GIL released on every storage
//! I/O) and §5.4 (panic-catch wrapper so a Rust panic never crosses the
//! FFI boundary as an abort signal).
//!
//! ## Concurrency model
//!
//! The Rust `drevo::db::Drevo` is `Send + Sync`. The Python wrapper
//! stores it inside `Mutex<Option<Arc<...>>>`:
//!
//! 1. The `Mutex` is held **only** to take a fresh `Arc<Drevo>` clone
//!    out of the `Option` (or to take/replace the inner during
//!    `close()` / `compact()`).
//! 2. Every storage call drops the mutex guard BEFORE entering
//!    `py.allow_threads(...)`, so the GIL release inside the closure
//!    never happens while a Rust mutex is held — that combination is
//!    a classic deadlock pattern (thread A holds the mutex with the
//!    GIL released; thread B acquires the GIL and blocks on the
//!    mutex; thread A's I/O completes but cannot reacquire the GIL
//!    because B is holding it).
//! 3. Concurrent reads + writes from N Python threads on the same
//!    handle therefore run side-by-side at the Python layer — the
//!    underlying redb backend handles its own write serialisation
//!    via an internal `RwLock`. The Python-side mutex is short-lived
//!    (release-on-clone) and never spans an `allow_threads` window.
//!
//! `close()` and `compact()` are the only methods that need exclusive
//! ownership of the inner `Drevo`. They `take()` the `Arc` out of the
//! slot to forbid new operations, then wait (via `Arc::try_unwrap` in
//! a 1 ms-sleep loop, all under `allow_threads`) for the last
//! in-flight clone to drop — at that point they own the inner
//! `Drevo` by value (close) or by `&mut` (compact). `compact` puts
//! the inner back when finished.
//!
//! ## GIL release
//!
//! Every method that touches the backend wraps the Rust call in
//! [`Python::allow_threads`]. Property getters (`Node.id`, etc.) and
//! pure constructors (`Direction::Out`) do **not** release the GIL — the
//! allow_threads round-trip would dominate sub-microsecond accesses.
//!
//! ## Panic safety
//!
//! Each `#[pymethods]` body that calls into `drevo` is wrapped in
//! [`std::panic::catch_unwind`] via the local [`guarded`] helper. A
//! caught panic is rendered as `drevo.PanicError` rather than letting
//! the unwind cross the C ABI (which is undefined behaviour). Mirrors
//! the C-FFI discipline audited in `audit/AUDIT-ffi.md`.

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pyo3::exceptions::{PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use drevo::db as ddb;
use drevo::model as dmodel;

use crate::errors::{map_err, panic_to_pyerr};
use crate::types;

/// Python-side `Drevo` handle. See module-level docs for the
/// concurrency model and GIL-release contract.
#[pyclass(name = "Drevo")]
pub struct Drevo {
    inner: Mutex<Option<Arc<ddb::Drevo>>>,
}

impl Drevo {
    /// Open an in-memory backend — used by both `open_in_memory` and as
    /// the fallback path for unit tests.
    fn from_inner(inner: ddb::Drevo) -> Self {
        Self {
            inner: Mutex::new(Some(Arc::new(inner))),
        }
    }
}

// ── Internal helpers ───────────────────────────────────────────────────

/// Run a closure under `catch_unwind`, mapping any panic to
/// `drevo.PanicError`.
fn guarded<F, T>(f: F) -> PyResult<T>
where
    F: FnOnce() -> PyResult<T>,
{
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => Err(panic_to_pyerr(payload)),
    }
}

/// Borrow a fresh `Arc<ddb::Drevo>` clone out of the slot.
///
/// The mutex guard exists for just long enough to clone the inner
/// `Arc` (cheap atomic refcount bump) and is dropped before this
/// helper returns. Every caller then runs its storage I/O against
/// the cloned `Arc` under `py.allow_threads(...)` — at which point
/// no Rust mutex is held, so the GIL release cannot deadlock against
/// another thread waiting on the mutex (the bug this layout fixes).
///
/// Returns `RuntimeError` if `close()` has already been called.
fn borrow_db(slot: &Mutex<Option<Arc<ddb::Drevo>>>) -> PyResult<Arc<ddb::Drevo>> {
    let guard = slot
        .lock()
        .map_err(|_| PyRuntimeError::new_err("drevo handle mutex poisoned"))?;
    match guard.as_ref() {
        Some(arc) => Ok(Arc::clone(arc)),
        None => Err(PyRuntimeError::new_err(
            "Drevo handle is closed; create a fresh instance with Drevo.open(...)",
        )),
    }
}

/// Spin-wait for the last `Arc<ddb::Drevo>` clone to drop so we can
/// reclaim exclusive ownership of the inner. Used by `close()` and
/// `compact()` (the only methods that need `self` / `&mut self` on
/// `ddb::Drevo`). The caller invokes this under `py.allow_threads`
/// so the wait does not hold the GIL.
///
/// In practice the loop fires at most once because all storage
/// methods complete in microseconds (the redb backend's serialisation
/// of writes dominates). 1 ms is short enough to be invisible on the
/// fast path but long enough to avoid pegging a CPU.
fn into_owned(mut arc: Arc<ddb::Drevo>) -> ddb::Drevo {
    loop {
        arc = match Arc::try_unwrap(arc) {
            Ok(inner) => return inner,
            Err(stale) => {
                std::thread::sleep(Duration::from_millis(1));
                stale
            }
        };
    }
}

/// Compatibility wrapper preserving the previous `with_db` signature.
///
/// Internally calls [`borrow_db`] — the mutex is released as soon as
/// the inner `Arc<Drevo>` clone is taken, BEFORE the closure `f` runs.
/// Every storage method below therefore enters
/// `py.allow_threads(...)` with no Rust mutex held, defusing the
/// GIL+Mutex deadlock pattern. The closure receives `&ddb::Drevo`
/// (deref of the local `Arc`), so the storage-method bodies do not
/// need to change.
fn with_db<R, F>(slot: &Mutex<Option<Arc<ddb::Drevo>>>, f: F) -> PyResult<R>
where
    F: FnOnce(&ddb::Drevo) -> PyResult<R>,
{
    let arc = borrow_db(slot)?;
    f(&arc)
}

// ── Drevo methods ──────────────────────────────────────────────────────

#[pymethods]
impl Drevo {
    /// Open a disk-backed database at `path`.
    ///
    /// `path` may be a `str`, `bytes`, or `os.PathLike`. The redb
    /// backend creates the file if it does not exist and acquires an
    /// exclusive file lock — opening the same path from a second
    /// process raises `drevo.LockedError`.
    #[classmethod]
    fn open(
        _cls: &Bound<'_, pyo3::types::PyType>,
        py: Python<'_>,
        path: PyObject,
    ) -> PyResult<Self> {
        let path: PathBuf = extract_path(py, path)?;
        guarded(|| {
            let db = py
                .allow_threads(|| ddb::Drevo::open(&path))
                .map_err(map_err)?;
            Ok(Self::from_inner(db))
        })
    }

    /// Open an ephemeral in-memory database — backing store is the
    /// process-local `MemoryBackend`. All data is lost when the handle
    /// is dropped.
    #[classmethod]
    fn open_in_memory(_cls: &Bound<'_, pyo3::types::PyType>, py: Python<'_>) -> PyResult<Self> {
        guarded(|| {
            let db = py
                .allow_threads(ddb::Drevo::open_in_memory)
                .map_err(map_err)?;
            Ok(Self::from_inner(db))
        })
    }

    /// Flush pending writes and release the file lock. After `close()`
    /// every subsequent method raises `RuntimeError`.
    ///
    /// Concurrency: takes the `Arc<Drevo>` out of the slot under the
    /// mutex (so no NEW operations can start), drops the guard, then
    /// waits under `py.allow_threads(...)` for any IN-FLIGHT operations
    /// to drop their cloned `Arc` — at that point `Arc::try_unwrap`
    /// succeeds and the inner `Drevo` is consumed by value to flush.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        guarded(|| {
            let taken = {
                let mut guard = self
                    .inner
                    .lock()
                    .map_err(|_| PyRuntimeError::new_err("drevo handle mutex poisoned"))?;
                guard.take()
            };
            match taken {
                Some(arc) => py
                    .allow_threads(move || into_owned(arc).close())
                    .map_err(map_err),
                None => Ok(()),
            }
        })
    }

    /// `__enter__` — returns the handle so `with Drevo.open(...) as d:`
    /// binds `d`.
    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// `__exit__` — close the handle. Exceptions raised inside the
    /// `with` body propagate; the close still runs.
    #[pyo3(signature = (_exc_type=None, _exc_value=None, _traceback=None))]
    fn __exit__(
        &self,
        py: Python<'_>,
        _exc_type: Option<PyObject>,
        _exc_value: Option<PyObject>,
        _traceback: Option<PyObject>,
    ) -> PyResult<bool> {
        self.close(py)?;
        // Returning `False` re-raises any in-flight exception, which is
        // the standard `with`-block contract.
        Ok(false)
    }

    /// Reclaim unused storage + checkpoint allocator state.
    /// Returns a `CompactReport` with byte counts and the new
    /// allocator next-id values.
    ///
    /// Concurrency: needs `&mut ddb::Drevo` (redb's compactor requires
    /// an exclusive write transaction). Takes the `Arc<Drevo>` out of
    /// the slot, waits under `py.allow_threads` for the last in-flight
    /// clone to drop (`Arc::try_unwrap` succeeds), runs the compaction,
    /// then re-wraps the inner in a fresh `Arc` and puts it back in
    /// the slot — so subsequent storage calls see a live handle again.
    /// On error the inner is still restored so the handle stays usable.
    fn compact(&self, py: Python<'_>) -> PyResult<types::CompactReport> {
        guarded(|| {
            let arc = {
                let mut guard = self
                    .inner
                    .lock()
                    .map_err(|_| PyRuntimeError::new_err("drevo handle mutex poisoned"))?;
                guard.take().ok_or_else(|| {
                    PyRuntimeError::new_err(
                        "Drevo handle is closed; create a fresh instance with Drevo.open(...)",
                    )
                })?
            };
            let (inner, result) = py.allow_threads(move || {
                let mut inner = into_owned(arc);
                let report = inner.compact();
                (inner, report)
            });
            // Always put the inner back so the handle remains usable
            // even if compaction returned an error.
            {
                let mut guard = self
                    .inner
                    .lock()
                    .map_err(|_| PyRuntimeError::new_err("drevo handle mutex poisoned"))?;
                *guard = Some(Arc::new(inner));
            }
            let report = result.map_err(map_err)?;
            Ok(types::CompactReport::new(report))
        })
    }

    /// Lightweight liveness probe — returns `None` (Python `None`) on
    /// success, raises `drevo.StorageError` if the backend is
    /// unreachable.
    fn health_check(&self, py: Python<'_>) -> PyResult<()> {
        guarded(|| {
            with_db(&self.inner, |db| {
                py.allow_threads(|| db.health_check()).map_err(map_err)
            })
        })
    }

    // ── Node CRUD ──────────────────────────────────────────────────

    fn create_node(&self, py: Python<'_>, new_node: types::NewNode) -> PyResult<types::Node> {
        guarded(|| {
            with_db(&self.inner, |db| {
                let node = py
                    .allow_threads(|| db.create_node(new_node.inner.clone()))
                    .map_err(map_err)?;
                Ok(types::Node::new(node))
            })
        })
    }

    fn get_node(&self, py: Python<'_>, id: u64) -> PyResult<Option<types::Node>> {
        guarded(|| {
            with_db(&self.inner, |db| {
                let res = py.allow_threads(|| db.get_node(id)).map_err(map_err)?;
                Ok(res.map(types::Node::new))
            })
        })
    }

    fn get_node_by_uuid(
        &self,
        py: Python<'_>,
        uuid: &Bound<'_, PyBytes>,
    ) -> PyResult<Option<types::Node>> {
        let bytes = uuid.as_bytes();
        let arr: [u8; 16] = bytes
            .try_into()
            .map_err(|_| PyTypeError::new_err("uuid must be exactly 16 bytes"))?;
        guarded(|| {
            with_db(&self.inner, |db| {
                let res = py
                    .allow_threads(|| db.get_node_by_uuid(&arr))
                    .map_err(map_err)?;
                Ok(res.map(types::Node::new))
            })
        })
    }

    fn get_node_by_title(&self, py: Python<'_>, title: &str) -> PyResult<Option<types::Node>> {
        guarded(|| {
            with_db(&self.inner, |db| {
                let res = py
                    .allow_threads(|| db.get_node_by_title(title))
                    .map_err(map_err)?;
                Ok(res.map(types::Node::new))
            })
        })
    }

    fn update_node(
        &self,
        py: Python<'_>,
        id: u64,
        patch: types::NodePatch,
    ) -> PyResult<types::Node> {
        guarded(|| {
            with_db(&self.inner, |db| {
                let node = py
                    .allow_threads(|| db.update_node(id, patch.inner.clone()))
                    .map_err(map_err)?;
                Ok(types::Node::new(node))
            })
        })
    }

    fn delete_node(&self, py: Python<'_>, id: u64) -> PyResult<()> {
        guarded(|| {
            with_db(&self.inner, |db| {
                py.allow_threads(|| db.delete_node(id)).map_err(map_err)
            })
        })
    }

    // ── Edge CRUD ──────────────────────────────────────────────────

    fn create_edge(&self, py: Python<'_>, new_edge: types::NewEdge) -> PyResult<types::Edge> {
        guarded(|| {
            with_db(&self.inner, |db| {
                let edge = py
                    .allow_threads(|| db.create_edge(new_edge.inner.clone()))
                    .map_err(map_err)?;
                Ok(types::Edge::new(edge))
            })
        })
    }

    fn get_edge(&self, py: Python<'_>, id: u64) -> PyResult<Option<types::Edge>> {
        guarded(|| {
            with_db(&self.inner, |db| {
                let res = py.allow_threads(|| db.get_edge(id)).map_err(map_err)?;
                Ok(res.map(types::Edge::new))
            })
        })
    }

    fn get_edge_by_uuid(
        &self,
        py: Python<'_>,
        uuid: &Bound<'_, PyBytes>,
    ) -> PyResult<Option<types::Edge>> {
        let bytes = uuid.as_bytes();
        let arr: [u8; 16] = bytes
            .try_into()
            .map_err(|_| PyTypeError::new_err("uuid must be exactly 16 bytes"))?;
        guarded(|| {
            with_db(&self.inner, |db| {
                let res = py
                    .allow_threads(|| db.get_edge_by_uuid(&arr))
                    .map_err(map_err)?;
                Ok(res.map(types::Edge::new))
            })
        })
    }

    fn update_edge(
        &self,
        py: Python<'_>,
        id: u64,
        patch: types::EdgePatch,
    ) -> PyResult<types::Edge> {
        guarded(|| {
            with_db(&self.inner, |db| {
                let edge = py
                    .allow_threads(|| db.update_edge(id, patch.inner.clone()))
                    .map_err(map_err)?;
                Ok(types::Edge::new(edge))
            })
        })
    }

    fn delete_edge(&self, py: Python<'_>, id: u64) -> PyResult<()> {
        guarded(|| {
            with_db(&self.inner, |db| {
                py.allow_threads(|| db.delete_edge(id)).map_err(map_err)
            })
        })
    }

    /// Return every edge incident on `node_id` in the given direction.
    fn edges_of(
        &self,
        py: Python<'_>,
        node_id: u64,
        direction: types::Direction,
    ) -> PyResult<Vec<types::Edge>> {
        guarded(|| {
            with_db(&self.inner, |db| {
                let dir: dmodel::Direction = direction.into();
                let edges = py
                    .allow_threads(|| db.edges_of(node_id, dir))
                    .map_err(map_err)?;
                Ok(edges.into_iter().map(types::Edge::new).collect())
            })
        })
    }

    // ── Index queries ──────────────────────────────────────────────

    fn list_nodes_by_kind(
        &self,
        py: Python<'_>,
        kind: &str,
        limit: usize,
        offset: usize,
    ) -> PyResult<Vec<types::Node>> {
        guarded(|| {
            with_db(&self.inner, |db| {
                let nodes = py
                    .allow_threads(|| db.list_nodes_by_kind(kind, limit, offset))
                    .map_err(map_err)?;
                Ok(nodes.into_iter().map(types::Node::new).collect())
            })
        })
    }

    fn list_edges_by_kind(
        &self,
        py: Python<'_>,
        kind: &str,
        limit: usize,
        offset: usize,
    ) -> PyResult<Vec<types::Edge>> {
        guarded(|| {
            with_db(&self.inner, |db| {
                let edges = py
                    .allow_threads(|| db.list_edges_by_kind(kind, limit, offset))
                    .map_err(map_err)?;
                Ok(edges.into_iter().map(types::Edge::new).collect())
            })
        })
    }

    fn list_recent(&self, py: Python<'_>, limit: usize) -> PyResult<Vec<types::Node>> {
        guarded(|| {
            with_db(&self.inner, |db| {
                let nodes = py
                    .allow_threads(|| db.list_recent(limit))
                    .map_err(map_err)?;
                Ok(nodes.into_iter().map(types::Node::new).collect())
            })
        })
    }

    // ── Traversal ──────────────────────────────────────────────────

    #[pyo3(signature = (start_id, max_depth, direction, edge_kind = None))]
    fn bfs(
        &self,
        py: Python<'_>,
        start_id: u64,
        max_depth: u8,
        direction: types::Direction,
        edge_kind: Option<&str>,
    ) -> PyResult<Vec<types::Node>> {
        guarded(|| {
            with_db(&self.inner, |db| {
                let dir: dmodel::Direction = direction.into();
                let nodes = py
                    .allow_threads(|| db.bfs(start_id, max_depth, dir, edge_kind))
                    .map_err(map_err)?;
                Ok(nodes.into_iter().map(types::Node::new).collect())
            })
        })
    }

    #[pyo3(signature = (start_id, max_depth, direction, edge_kind = None))]
    fn dfs(
        &self,
        py: Python<'_>,
        start_id: u64,
        max_depth: u8,
        direction: types::Direction,
        edge_kind: Option<&str>,
    ) -> PyResult<Vec<types::Node>> {
        guarded(|| {
            with_db(&self.inner, |db| {
                let dir: dmodel::Direction = direction.into();
                let nodes = py
                    .allow_threads(|| db.dfs(start_id, max_depth, dir, edge_kind))
                    .map_err(map_err)?;
                Ok(nodes.into_iter().map(types::Node::new).collect())
            })
        })
    }

    #[pyo3(signature = (from_id, to_id, edge_kind = None))]
    fn shortest_path(
        &self,
        py: Python<'_>,
        from_id: u64,
        to_id: u64,
        edge_kind: Option<&str>,
    ) -> PyResult<Option<Vec<u64>>> {
        guarded(|| {
            with_db(&self.inner, |db| {
                py.allow_threads(|| db.shortest_path_filtered(from_id, to_id, edge_kind))
                    .map_err(map_err)
            })
        })
    }

    #[pyo3(signature = (root, depth, edge_kind = None))]
    fn subgraph(
        &self,
        py: Python<'_>,
        root: u64,
        depth: u8,
        edge_kind: Option<&str>,
    ) -> PyResult<types::SubGraph> {
        guarded(|| {
            with_db(&self.inner, |db| {
                let sg = py
                    .allow_threads(|| db.subgraph_filtered(root, depth, edge_kind))
                    .map_err(map_err)?;
                Ok(types::SubGraph::new(sg))
            })
        })
    }

    #[pyo3(signature = (node_id, direction, edge_kind = None))]
    fn neighbors(
        &self,
        py: Python<'_>,
        node_id: u64,
        direction: types::Direction,
        edge_kind: Option<&str>,
    ) -> PyResult<Vec<types::Node>> {
        guarded(|| {
            with_db(&self.inner, |db| {
                let dir: dmodel::Direction = direction.into();
                let nodes = py
                    .allow_threads(|| db.neighbors(node_id, dir, edge_kind))
                    .map_err(map_err)?;
                Ok(nodes.into_iter().map(types::Node::new).collect())
            })
        })
    }

    // ── Full-text search ───────────────────────────────────────────

    fn search_fts(
        &self,
        py: Python<'_>,
        query: &str,
        limit: usize,
    ) -> PyResult<Vec<types::ScoredNode>> {
        guarded(|| {
            with_db(&self.inner, |db| {
                let results = py
                    .allow_threads(|| db.search_fts(query, limit))
                    .map_err(map_err)?;
                Ok(results.into_iter().map(types::ScoredNode::new).collect())
            })
        })
    }
}

/// Extract a filesystem path from a Python `str | bytes | os.PathLike`.
/// PyO3 0.23 has [`pyo3::types::PyAnyMethods::extract`] specialised for
/// `PathBuf`, which understands `__fspath__` automatically.
fn extract_path(_py: Python<'_>, obj: PyObject) -> PyResult<PathBuf> {
    Python::with_gil(|py| {
        let bound = obj.bind(py);
        bound.extract::<PathBuf>().map_err(|e| {
            PyTypeError::new_err(format!("path must be str, bytes, or os.PathLike: {e}"))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use drevo::model::{NewEdge, NewNode, Properties};

    /// Sanity-check the wrapper at the Rust level — open an in-memory
    /// db, create a node, fetch it back, close. This exercises the
    /// `Arc<Mutex<Option<...>>>` slot life-cycle without needing a
    /// live Python interpreter.
    #[test]
    fn smoke_round_trip_in_memory() {
        let db = ddb::Drevo::open_in_memory().expect("open_in_memory");
        let node = db
            .create_node(NewNode {
                kind: "note".into(),
                title: "hello".into(),
                body: "world".into(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .expect("create_node");
        assert_eq!(node.id, 1);

        let fetched = db.get_node(1).expect("get_node").expect("some");
        assert_eq!(fetched.title, "hello");

        let edge_target = db
            .create_node(NewNode {
                kind: "note".into(),
                title: "target".into(),
                body: String::new(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .expect("create target node");

        let edge = db
            .create_edge(NewEdge {
                from_id: node.id,
                to_id: edge_target.id,
                kind: "links_to".into(),
                weight: 1.0,
                properties: Properties::default(),
            })
            .expect("create_edge");
        assert_eq!(edge.from_id, 1);
        assert_eq!(edge.to_id, 2);

        db.close().expect("close");
    }
}
