//! Frozen `#[pyclass]` wrappers for the public data types.
//!
//! Implements RFC §3.2 — each Rust type from `drevo::model` gets a
//! mirror class on the Python side. Per the RFC §3.2 "Plain-data
//! classes" line, every wrapper uses `#[pyclass(frozen)]` so instances
//! are hashable, `__eq__`-able, and `__repr__`-able by default — mutation
//! goes through the `*Patch` types, exactly mirroring the Rust API.
//!
//! UUIDs cross the boundary as 16-byte `bytes`. The pure-Python shim
//! `drevo/__init__.py` (task `00116`) is responsible for converting to
//! and from `uuid.UUID` so the PyO3 layer stays free of `uuid`-module
//! Python imports on the hot path (RFC §3.2 Q-1).
//!
//! `Node.properties` / `Edge.properties` are round-tripped through
//! [`pythonize`] so they appear on the Python side as native
//! `dict[str, object]` — JSON-serialisable, mypy-friendly, no opaque
//! handles. The conversion direction is symmetric: Python `dict[str,
//! Any]` → `serde_json::Value` on input, `serde_json::Value` → Python
//! object on output.

use drevo::model;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use pythonize::{depythonize, pythonize};

// ── Direction ──────────────────────────────────────────────────────────
//
// `Direction` is exposed to Python as `drevo.Direction` with attributes
// `OUT`, `IN`, `BOTH`. PyO3 0.23's `#[pyclass(eq, eq_int)]` gives a
// fieldless enum with `__int__` + `__eq__` against ints, which is what
// the RFC §3.2 row "Direction enum → drevo.Direction (IntEnum)" demands
// without forcing us to write a separate `pyo3::IntoPy` block. The
// Rust-side variants stay CamelCase (`Out`, `In`, `Both`) so `clippy`
// is happy; `#[pyo3(name = "...")]` re-exports them as the screaming-
// snake aliases used in Python code.

/// Direction of edge traversal — exported to Python as
/// `drevo.Direction.OUT` / `drevo.Direction.IN` / `drevo.Direction.BOTH`.
#[pyclass(eq, eq_int, name = "Direction")]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Follow outgoing edges (`from_id == node`).
    #[pyo3(name = "OUT")]
    Out = 0,
    /// Follow incoming edges (`to_id == node`).
    #[pyo3(name = "IN")]
    In = 1,
    /// Follow edges in both directions, deduplicating self-loops.
    #[pyo3(name = "BOTH")]
    Both = 2,
}

impl From<Direction> for model::Direction {
    fn from(d: Direction) -> Self {
        match d {
            Direction::Out => model::Direction::Outgoing,
            Direction::In => model::Direction::Incoming,
            Direction::Both => model::Direction::Both,
        }
    }
}

// ── Properties helpers ─────────────────────────────────────────────────

/// Convert a `drevo` Properties map to a Python `dict[str, Any]`.
///
/// `pythonize` walks the `serde_json::Value` tree once and produces a
/// fresh `dict` keyed by string. Returns an empty dict when the input
/// map is empty, never `None` — Python users expect attribute access on
/// every node to succeed.
pub(crate) fn props_to_py<'py>(
    py: Python<'py>,
    props: &model::Properties,
) -> PyResult<Bound<'py, PyAny>> {
    pythonize(py, &props.0).map_err(|e| {
        pyo3::exceptions::PyTypeError::new_err(format!(
            "failed to convert node/edge properties to Python: {e}"
        ))
    })
}

/// Convert a Python `dict[str, Any]` (or `None`) into a Properties map.
///
/// `None` becomes the empty `Properties` so callers that omit the
/// argument get the Rust default.
pub(crate) fn props_from_py(obj: Option<&Bound<'_, PyAny>>) -> PyResult<model::Properties> {
    match obj {
        None => Ok(model::Properties::default()),
        Some(any) if any.is_none() => Ok(model::Properties::default()),
        Some(any) => {
            let map: std::collections::HashMap<String, serde_json::Value> = depythonize(any)
                .map_err(|e| {
                    pyo3::exceptions::PyTypeError::new_err(format!(
                        "properties must be a dict[str, Any]: {e}"
                    ))
                })?;
            Ok(model::Properties(map))
        }
    }
}

// ── Node ───────────────────────────────────────────────────────────────

/// Frozen mirror of [`drevo::model::Node`].
#[pyclass(frozen, name = "Node")]
#[derive(Clone)]
pub struct Node {
    inner: model::Node,
}

impl Node {
    pub(crate) fn new(inner: model::Node) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl Node {
    /// Auto-increment node id.
    #[getter]
    fn id(&self) -> u64 {
        self.inner.id
    }

    /// UUID v7 as a 16-byte `bytes` object. The pure-Python shim in
    /// `drevo/__init__.py` wraps this in a `uuid.UUID(bytes=...)` so
    /// end-users see `Node.uuid: uuid.UUID` (RFC §3.2 Q-1).
    #[getter]
    fn uuid<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.inner.uuid)
    }

    /// Node classification (e.g. `"note"`, `"task"`).
    #[getter]
    fn kind(&self) -> &str {
        &self.inner.kind
    }

    /// Human-readable title.
    #[getter]
    fn title(&self) -> &str {
        &self.inner.title
    }

    /// Raw Markdown body.
    #[getter]
    fn body(&self) -> &str {
        &self.inner.body
    }

    /// Rendered HTML cache.
    #[getter]
    fn body_html(&self) -> &str {
        &self.inner.body_html
    }

    /// Creation timestamp (Unix ms).
    #[getter]
    fn created_at(&self) -> i64 {
        self.inner.created_at
    }

    /// Last-update timestamp (Unix ms).
    #[getter]
    fn updated_at(&self) -> i64 {
        self.inner.updated_at
    }

    /// JSON-compatible properties dict.
    #[getter]
    fn properties<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        props_to_py(py, &self.inner.properties)
    }

    fn __repr__(&self) -> String {
        format!(
            "Node(id={}, kind={:?}, title={:?})",
            self.inner.id, self.inner.kind, self.inner.title
        )
    }

    fn __eq__(&self, other: &Node) -> bool {
        self.inner == other.inner
    }
}

// ── Edge ───────────────────────────────────────────────────────────────

/// Frozen mirror of [`drevo::model::Edge`].
#[pyclass(frozen, name = "Edge")]
#[derive(Clone)]
pub struct Edge {
    inner: model::Edge,
}

impl Edge {
    pub(crate) fn new(inner: model::Edge) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl Edge {
    /// Auto-increment edge id.
    #[getter]
    fn id(&self) -> u64 {
        self.inner.id
    }

    /// UUID v7 as a 16-byte `bytes` object.
    #[getter]
    fn uuid<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.inner.uuid)
    }

    /// Source node id.
    #[getter]
    #[allow(clippy::wrong_self_convention)]
    fn from_id(&self) -> u64 {
        self.inner.from_id
    }

    /// Target node id.
    #[getter]
    fn to_id(&self) -> u64 {
        self.inner.to_id
    }

    /// Edge classification (e.g. `"links_to"`, `"tagged_with"`).
    #[getter]
    fn kind(&self) -> &str {
        &self.inner.kind
    }

    /// Ranking weight (always finite — NaN/±Inf rejected on write).
    #[getter]
    fn weight(&self) -> f32 {
        self.inner.weight
    }

    /// Creation timestamp (Unix ms).
    #[getter]
    fn created_at(&self) -> i64 {
        self.inner.created_at
    }

    /// JSON-compatible properties dict.
    #[getter]
    fn properties<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        props_to_py(py, &self.inner.properties)
    }

    fn __repr__(&self) -> String {
        format!(
            "Edge(id={}, from_id={}, to_id={}, kind={:?})",
            self.inner.id, self.inner.from_id, self.inner.to_id, self.inner.kind
        )
    }

    fn __eq__(&self, other: &Edge) -> bool {
        self.inner == other.inner
    }
}

// ── NewNode / NewEdge ──────────────────────────────────────────────────
//
// `New*` types are NOT frozen on the Python side because they are
// constructed by user code (`drevo.NewNode(kind=..., title=...)`); they
// only become a frozen Node once `Drevo.create_node` returns. Per RFC
// §3.2 the user-facing type is a Python dataclass — but PyO3 cannot
// generate `@dataclass` decorators; the equivalent is `#[pyclass]` with
// `__init__` accepting keyword args. We accept named kwargs in the
// constructor and surface read-only getters.

/// Input record for [`crate::handle::Drevo::create_node`].
#[pyclass(name = "NewNode")]
#[derive(Clone)]
pub struct NewNode {
    pub(crate) inner: model::NewNode,
}

#[pymethods]
impl NewNode {
    #[new]
    #[pyo3(signature = (*, kind, title, body = String::new(), body_html = String::new(), properties = None))]
    fn new(
        kind: String,
        title: String,
        body: String,
        body_html: String,
        properties: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        Ok(Self {
            inner: model::NewNode {
                kind,
                title,
                body,
                body_html,
                properties: props_from_py(properties)?,
            },
        })
    }

    #[getter]
    fn kind(&self) -> &str {
        &self.inner.kind
    }

    #[getter]
    fn title(&self) -> &str {
        &self.inner.title
    }

    #[getter]
    fn body(&self) -> &str {
        &self.inner.body
    }

    #[getter]
    fn body_html(&self) -> &str {
        &self.inner.body_html
    }

    #[getter]
    fn properties<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        props_to_py(py, &self.inner.properties)
    }

    fn __repr__(&self) -> String {
        format!(
            "NewNode(kind={:?}, title={:?})",
            self.inner.kind, self.inner.title
        )
    }
}

/// Input record for [`crate::handle::Drevo::create_edge`].
#[pyclass(name = "NewEdge")]
#[derive(Clone)]
pub struct NewEdge {
    pub(crate) inner: model::NewEdge,
}

#[pymethods]
impl NewEdge {
    #[new]
    #[pyo3(signature = (*, from_id, to_id, kind, weight = 1.0, properties = None))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        from_id: u64,
        to_id: u64,
        kind: String,
        weight: f32,
        properties: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        Ok(Self {
            inner: model::NewEdge {
                from_id,
                to_id,
                kind,
                weight,
                properties: props_from_py(properties)?,
            },
        })
    }

    #[getter]
    #[allow(clippy::wrong_self_convention)]
    fn from_id(&self) -> u64 {
        self.inner.from_id
    }

    #[getter]
    fn to_id(&self) -> u64 {
        self.inner.to_id
    }

    #[getter]
    fn kind(&self) -> &str {
        &self.inner.kind
    }

    #[getter]
    fn weight(&self) -> f32 {
        self.inner.weight
    }

    #[getter]
    fn properties<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        props_to_py(py, &self.inner.properties)
    }

    fn __repr__(&self) -> String {
        format!(
            "NewEdge(from_id={}, to_id={}, kind={:?}, weight={})",
            self.inner.from_id, self.inner.to_id, self.inner.kind, self.inner.weight
        )
    }
}

// ── NodePatch / EdgePatch ──────────────────────────────────────────────

/// Partial update for an existing node — see [`drevo::model::NodePatch`].
#[pyclass(name = "NodePatch")]
#[derive(Clone, Default)]
pub struct NodePatch {
    pub(crate) inner: model::NodePatch,
}

#[pymethods]
impl NodePatch {
    #[new]
    #[pyo3(signature = (*, kind = None, title = None, body = None, body_html = None, properties = None))]
    fn new(
        kind: Option<String>,
        title: Option<String>,
        body: Option<String>,
        body_html: Option<String>,
        properties: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        let properties = match properties {
            Some(p) if !p.is_none() => Some(props_from_py(Some(p))?),
            _ => None,
        };
        Ok(Self {
            inner: model::NodePatch {
                kind,
                title,
                body,
                body_html,
                properties,
            },
        })
    }

    fn __repr__(&self) -> String {
        format!(
            "NodePatch(kind={:?}, title={:?}, body={:?}, body_html={:?})",
            self.inner.kind, self.inner.title, self.inner.body, self.inner.body_html
        )
    }
}

/// Partial update for an existing edge — see [`drevo::model::EdgePatch`].
#[pyclass(name = "EdgePatch")]
#[derive(Clone, Default)]
pub struct EdgePatch {
    pub(crate) inner: model::EdgePatch,
}

#[pymethods]
impl EdgePatch {
    #[new]
    #[pyo3(signature = (*, kind = None, weight = None, properties = None))]
    fn new(
        kind: Option<String>,
        weight: Option<f32>,
        properties: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        let properties = match properties {
            Some(p) if !p.is_none() => Some(props_from_py(Some(p))?),
            _ => None,
        };
        Ok(Self {
            inner: model::EdgePatch {
                kind,
                weight,
                properties,
            },
        })
    }

    fn __repr__(&self) -> String {
        format!(
            "EdgePatch(kind={:?}, weight={:?})",
            self.inner.kind, self.inner.weight
        )
    }
}

// ── ScoredNode ─────────────────────────────────────────────────────────

/// FTS search result — pair of a node + its TF-IDF score.
#[pyclass(frozen, name = "ScoredNode")]
#[derive(Clone)]
pub struct ScoredNode {
    inner: model::ScoredNode,
}

impl ScoredNode {
    pub(crate) fn new(inner: model::ScoredNode) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl ScoredNode {
    #[getter]
    fn node(&self) -> Node {
        Node::new(self.inner.node.clone())
    }

    #[getter]
    fn score(&self) -> f32 {
        self.inner.score
    }

    fn __repr__(&self) -> String {
        format!(
            "ScoredNode(node_id={}, score={})",
            self.inner.node.id, self.inner.score
        )
    }
}

// ── SubGraph ───────────────────────────────────────────────────────────

/// Bounded-depth slice of the graph — pair of node + edge lists.
#[pyclass(frozen, name = "SubGraph")]
#[derive(Clone)]
pub struct SubGraph {
    inner: model::SubGraph,
}

impl SubGraph {
    pub(crate) fn new(inner: model::SubGraph) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl SubGraph {
    #[getter]
    fn nodes(&self) -> Vec<Node> {
        self.inner.nodes.iter().cloned().map(Node::new).collect()
    }

    #[getter]
    fn edges(&self) -> Vec<Edge> {
        self.inner.edges.iter().cloned().map(Edge::new).collect()
    }

    fn __repr__(&self) -> String {
        format!(
            "SubGraph(nodes={}, edges={})",
            self.inner.nodes.len(),
            self.inner.edges.len()
        )
    }
}

// ── CompactReport ──────────────────────────────────────────────────────

/// Result of [`crate::handle::Drevo::compact`] — operator-facing
/// before/after byte counts + checkpointed allocator state.
#[pyclass(frozen, name = "CompactReport")]
#[derive(Clone)]
pub struct CompactReport {
    inner: drevo::db::CompactReport,
}

impl CompactReport {
    pub(crate) fn new(inner: drevo::db::CompactReport) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl CompactReport {
    /// Storage size in bytes before compaction (None for ephemeral
    /// backends).
    #[getter]
    fn bytes_before(&self) -> Option<u64> {
        self.inner.bytes_before
    }

    /// Storage size in bytes after compaction.
    #[getter]
    fn bytes_after(&self) -> Option<u64> {
        self.inner.bytes_after
    }

    /// `bytes_before - bytes_after`, saturating at zero.
    #[getter]
    fn bytes_reclaimed(&self) -> u64 {
        self.inner.bytes_reclaimed
    }

    /// Next node id after the compaction checkpoint.
    #[getter]
    fn next_node_id(&self) -> u64 {
        self.inner.next_node_id
    }

    /// Next edge id after the compaction checkpoint.
    #[getter]
    fn next_edge_id(&self) -> u64 {
        self.inner.next_edge_id
    }

    fn as_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("bytes_before", self.inner.bytes_before)?;
        d.set_item("bytes_after", self.inner.bytes_after)?;
        d.set_item("bytes_reclaimed", self.inner.bytes_reclaimed)?;
        d.set_item("next_node_id", self.inner.next_node_id)?;
        d.set_item("next_edge_id", self.inner.next_edge_id)?;
        Ok(d)
    }

    fn __repr__(&self) -> String {
        format!(
            "CompactReport(bytes_reclaimed={}, next_node_id={}, next_edge_id={})",
            self.inner.bytes_reclaimed, self.inner.next_node_id, self.inner.next_edge_id
        )
    }
}

// ── ImportReport ───────────────────────────────────────────────────────

/// Result of [`crate::handle::Drevo::import_graphml`] /
/// [`crate::handle::Drevo::import_graphml_from_path`] — how many rows were
/// newly inserted vs. skipped as byte-equal duplicates (idempotent re-import).
#[pyclass(frozen, name = "ImportReport")]
#[derive(Clone)]
pub struct ImportReport {
    inner: drevo::dump::ImportReport,
}

impl ImportReport {
    pub(crate) fn new(inner: drevo::dump::ImportReport) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl ImportReport {
    /// Number of nodes inserted during this import.
    #[getter]
    fn nodes_imported(&self) -> usize {
        self.inner.nodes_imported
    }

    /// Number of edges inserted during this import.
    #[getter]
    fn edges_imported(&self) -> usize {
        self.inner.edges_imported
    }

    /// Number of nodes skipped because a byte-equal row already existed.
    #[getter]
    fn nodes_skipped(&self) -> usize {
        self.inner.nodes_skipped
    }

    /// Number of edges skipped because a byte-equal row already existed.
    #[getter]
    fn edges_skipped(&self) -> usize {
        self.inner.edges_skipped
    }

    fn as_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("nodes_imported", self.inner.nodes_imported)?;
        d.set_item("edges_imported", self.inner.edges_imported)?;
        d.set_item("nodes_skipped", self.inner.nodes_skipped)?;
        d.set_item("edges_skipped", self.inner.edges_skipped)?;
        Ok(d)
    }

    fn __repr__(&self) -> String {
        format!(
            "ImportReport(nodes_imported={}, edges_imported={}, nodes_skipped={}, edges_skipped={})",
            self.inner.nodes_imported,
            self.inner.edges_imported,
            self.inner.nodes_skipped,
            self.inner.edges_skipped
        )
    }
}

// ── BloatReport ────────────────────────────────────────────────────────

/// Result of [`crate::handle::Drevo::bloat_report`] (#253 slice 1) — physical
/// file size vs. logical data size and their ratio, so callers can detect
/// reclaimable copy-on-write high-water-mark bloat.
#[pyclass(frozen, name = "BloatReport")]
#[derive(Clone)]
pub struct BloatReport {
    inner: drevo::db::BloatReport,
}

impl BloatReport {
    pub(crate) fn new(inner: drevo::db::BloatReport) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl BloatReport {
    /// Physical on-disk size in bytes (None for the ephemeral in-memory
    /// backend).
    #[getter]
    fn file_bytes(&self) -> Option<u64> {
        self.inner.file_bytes
    }

    /// Summed size of **all** stored rows — records plus every secondary
    /// index. The real data footprint and the denominator of `bloat_ratio`.
    #[getter]
    fn stored_bytes(&self) -> u64 {
        self.inner.stored_bytes
    }

    /// Summed size of the node + edge record rows — the irreducible logical
    /// graph data, excluding indexes.
    #[getter]
    fn logical_bytes(&self) -> u64 {
        self.inner.logical_bytes
    }

    /// `stored_bytes - logical_bytes` — the secondary-index footprint (FTS,
    /// adjacency, property/uuid/title/kind keys, vectors). Legitimate overhead,
    /// not reclaimable bloat.
    #[getter]
    fn index_bytes(&self) -> u64 {
        self.inner.index_bytes
    }

    /// Number of node records.
    #[getter]
    fn node_count(&self) -> u64 {
        self.inner.node_count
    }

    /// Number of edge records.
    #[getter]
    fn edge_count(&self) -> u64 {
        self.inner.edge_count
    }

    /// `file_bytes / stored_bytes` — physical bytes per byte of real stored
    /// data. None when unmeasurable (in-memory backend) or there is no data
    /// yet. A value well above 1 signals reclaimable bloat; near 1 means the
    /// file is already minimal.
    #[getter]
    fn bloat_ratio(&self) -> Option<f64> {
        self.inner.bloat_ratio
    }

    fn as_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("file_bytes", self.inner.file_bytes)?;
        d.set_item("stored_bytes", self.inner.stored_bytes)?;
        d.set_item("logical_bytes", self.inner.logical_bytes)?;
        d.set_item("index_bytes", self.inner.index_bytes)?;
        d.set_item("node_count", self.inner.node_count)?;
        d.set_item("edge_count", self.inner.edge_count)?;
        d.set_item("bloat_ratio", self.inner.bloat_ratio)?;
        Ok(d)
    }

    fn __repr__(&self) -> String {
        format!(
            "BloatReport(file_bytes={:?}, stored_bytes={}, logical_bytes={}, index_bytes={}, node_count={}, edge_count={}, bloat_ratio={:?})",
            self.inner.file_bytes,
            self.inner.stored_bytes,
            self.inner.logical_bytes,
            self.inner.index_bytes,
            self.inner.node_count,
            self.inner.edge_count,
            self.inner.bloat_ratio
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_maps_to_model() {
        assert_eq!(
            model::Direction::from(Direction::Out),
            model::Direction::Outgoing
        );
        assert_eq!(
            model::Direction::from(Direction::In),
            model::Direction::Incoming
        );
        assert_eq!(
            model::Direction::from(Direction::Both),
            model::Direction::Both
        );
    }
}
