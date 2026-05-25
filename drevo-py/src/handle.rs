//! The `Drevo` handle — the Python-side entry point to the database.
//!
//! Implements RFC §4 (sync surface with GIL released on every storage
//! I/O) and §5.4 (panic-catch wrapper so a Rust panic never crosses the
//! FFI boundary as an abort signal).
//!
//! ## Concurrency model
//!
//! The Rust `drevo::db::Drevo` is `Send + Sync`. The Python wrapper
//! stores it inside `Arc<Mutex<Option<...>>>` for two reasons:
//!
//! 1. `Drevo::close` consumes `self` by value; we have to be able to
//!    `take()` the inner handle once during `close()` / `__exit__`.
//! 2. `Drevo::compact` requires `&mut self`. The `Mutex` provides the
//!    exclusive access for the duration of the compaction.
//!
//! Read paths (`get_node`, `bfs`, `search_fts`, …) lock the mutex only
//! to grab a `&Drevo` reference — they never block writers because the
//! underlying redb backend serialises writes internally via its own
//! `RwLock`. The Python-side mutex is therefore short-lived
//! (release-on-borrow) and does not serialise reads in practice.
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
    inner: Arc<Mutex<Option<ddb::Drevo>>>,
}

impl Drevo {
    /// Open an in-memory backend — used by both `open_in_memory` and as
    /// the fallback path for unit tests.
    fn from_inner(inner: ddb::Drevo) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Some(inner))),
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

/// Lock the inner handle and run `f` against the live `&ddb::Drevo`.
/// Returns `RuntimeError` if `close()` has already been called.
fn with_db<R, F>(slot: &Arc<Mutex<Option<ddb::Drevo>>>, f: F) -> PyResult<R>
where
    F: FnOnce(&ddb::Drevo) -> PyResult<R>,
{
    let guard = slot
        .lock()
        .map_err(|_| PyRuntimeError::new_err("drevo handle mutex poisoned"))?;
    match guard.as_ref() {
        Some(db) => f(db),
        None => Err(PyRuntimeError::new_err(
            "Drevo handle is closed; create a fresh instance with Drevo.open(...)",
        )),
    }
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
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        guarded(|| {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| PyRuntimeError::new_err("drevo handle mutex poisoned"))?;
            match guard.take() {
                Some(db) => {
                    py.allow_threads(|| db.close()).map_err(map_err)?;
                    Ok(())
                }
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
    fn compact(&self, py: Python<'_>) -> PyResult<types::CompactReport> {
        guarded(|| {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| PyRuntimeError::new_err("drevo handle mutex poisoned"))?;
            let db = guard.as_mut().ok_or_else(|| {
                PyRuntimeError::new_err(
                    "Drevo handle is closed; create a fresh instance with Drevo.open(...)",
                )
            })?;
            let report = py.allow_threads(|| db.compact()).map_err(map_err)?;
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
