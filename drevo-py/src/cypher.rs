//! Cypher from Python — `Drevo.execute(query, params=None)` (issue #553).
//!
//! The executor's [`Value`] tree is converted to plain Python values on the
//! way out (`None`, `bool`, `int`, `float`, `str`, `list`, `dict`) plus three
//! frozen graph classes, [`CypherNode`], [`CypherRelationship`] and
//! [`CypherPath`]. Parameters come in as a `dict` of named `$params`. Errors
//! split into syntax errors ([`CypherSyntaxError`]), statement timeouts
//! ([`QueryTimeoutError`]), missing parameters ([`ParameterMissingError`]) and
//! every other executor error ([`CypherError`], their common base). Storage
//! failures keep their existing classes ([`crate::errors::map_err`]), so a
//! duplicate title from Cypher is the same `DuplicateTitleError` as from
//! `create_node`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use drevo::cypher::executor::{
    ExecError, ExecResult, ExecStats, NodeValue, PathValue, RelationshipValue, Value,
};
use drevo::cypher::parser::ParseError;
use pyo3::exceptions::{PyOverflowError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyBytes, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple};

use crate::errors::{
    map_err, CypherError, CypherSyntaxError, ParameterMissingError, QueryTimeoutError,
};

/// A failed `execute`: the statement did not parse, or the executor failed.
pub(crate) enum CypherFailure {
    Parse(ParseError),
    Exec(ExecError),
}

/// Map a [`CypherFailure`] onto the Python exception hierarchy.
pub(crate) fn map_cypher_err(failure: CypherFailure) -> PyErr {
    match failure {
        CypherFailure::Parse(e) => CypherSyntaxError::new_err(e.to_string()),
        CypherFailure::Exec(ExecError::Storage(e)) => map_err(e),
        CypherFailure::Exec(ExecError::Timeout { limit_ms }) => QueryTimeoutError::new_err((
            format!("statement exceeded the {limit_ms} ms statement timeout"),
            limit_ms,
        )),
        CypherFailure::Exec(e @ ExecError::MissingParameter(_)) => {
            ParameterMissingError::new_err(e.to_string())
        }
        CypherFailure::Exec(e) => CypherError::new_err(e.to_string()),
    }
}

// ── Parameters: Python → Value ─────────────────────────────────────────

/// Convert the `params` dict into executor parameters.
pub(crate) fn params_from_py(
    params: Option<&Bound<'_, PyAny>>,
) -> PyResult<HashMap<String, Value>> {
    let Some(params) = params else {
        return Ok(HashMap::new());
    };
    if params.is_none() {
        return Ok(HashMap::new());
    }
    let dict = params
        .downcast::<PyDict>()
        .map_err(|_| PyTypeError::new_err("params must be a dict of named parameters"))?;
    let mut out = HashMap::with_capacity(dict.len());
    for (key, value) in dict.iter() {
        let key: String = key
            .extract()
            .map_err(|_| PyTypeError::new_err("parameter names must be str"))?;
        out.insert(key, value_from_py(&value)?);
    }
    Ok(out)
}

/// One Python value as a Cypher [`Value`].
fn value_from_py(obj: &Bound<'_, PyAny>) -> PyResult<Value> {
    if obj.is_none() {
        return Ok(Value::Null);
    }
    // `bool` before `int`: Python's bool is an int subclass.
    if obj.is_instance_of::<PyBool>() {
        return Ok(Value::Bool(obj.extract()?));
    }
    if obj.is_instance_of::<PyInt>() {
        return obj
            .extract::<i64>()
            .map(Value::Integer)
            .map_err(|_| PyOverflowError::new_err("int parameter does not fit in 64 bits"));
    }
    if obj.is_instance_of::<PyFloat>() {
        return Ok(Value::Float(obj.extract()?));
    }
    if obj.is_instance_of::<PyString>() {
        return Ok(Value::String(obj.extract()?));
    }
    if obj.is_instance_of::<PyBytes>() {
        return Err(PyTypeError::new_err(
            "bytes parameters are not supported (Cypher has no byte strings)",
        ));
    }
    if obj.is_instance_of::<PyList>() || obj.is_instance_of::<PyTuple>() {
        let mut items = Vec::new();
        for item in obj.try_iter()? {
            items.push(value_from_py(&item?)?);
        }
        return Ok(Value::List(items));
    }
    if let Ok(dict) = obj.downcast::<PyDict>() {
        let mut map = BTreeMap::new();
        for (key, value) in dict.iter() {
            let key: String = key
                .extract()
                .map_err(|_| PyTypeError::new_err("map parameter keys must be str"))?;
            map.insert(key, value_from_py(&value)?);
        }
        return Ok(Value::Map(map));
    }
    // `uuid.UUID` → its canonical hyphenated string.
    let uuid_type = obj.py().import("uuid")?.getattr("UUID")?;
    if obj.is_instance(&uuid_type)? {
        return Ok(Value::String(obj.str()?.extract()?));
    }
    Err(PyTypeError::new_err(format!(
        "unsupported parameter type {}: use None, bool, int, float, str, uuid.UUID, list or dict",
        obj.get_type().name()?
    )))
}

// ── Results: Value → Python ────────────────────────────────────────────

fn value_to_py(py: Python<'_>, value: &Value) -> PyResult<PyObject> {
    Ok(match value {
        Value::Null => py.None(),
        Value::Bool(b) => b.into_pyobject(py)?.to_owned().into_any().unbind(),
        Value::Integer(i) => i.into_pyobject(py)?.into_any().unbind(),
        Value::Float(f) => f.into_pyobject(py)?.into_any().unbind(),
        Value::String(s) => s.into_pyobject(py)?.into_any().unbind(),
        Value::List(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(value_to_py(py, item)?)?;
            }
            list.into_any().unbind()
        }
        Value::Map(map) => map_to_py(py, map)?.into_any().unbind(),
        Value::Node(node) => Py::new(py, CypherNode::new(py, node)?)?.into_any(),
        Value::Relationship(rel) => Py::new(py, CypherRelationship::new(py, rel)?)?.into_any(),
        Value::Path(path) => Py::new(py, CypherPath::new(py, path)?)?.into_any(),
    })
}

fn map_to_py<'py>(py: Python<'py>, map: &BTreeMap<String, Value>) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    for (key, value) in map {
        dict.set_item(key, value_to_py(py, value)?)?;
    }
    Ok(dict)
}

fn stats_to_py<'py>(py: Python<'py>, stats: &ExecStats) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("nodes_created", stats.nodes_created)?;
    dict.set_item("relationships_created", stats.relationships_created)?;
    dict.set_item("properties_set", stats.properties_set)?;
    dict.set_item("nodes_deleted", stats.nodes_deleted)?;
    dict.set_item("relationships_deleted", stats.relationships_deleted)?;
    dict.set_item("labels_added", stats.labels_added)?;
    dict.set_item("labels_removed", stats.labels_removed)?;
    Ok(dict)
}

/// Build the Python [`CypherResult`] for an executor result.
pub(crate) fn result_to_py(py: Python<'_>, result: &ExecResult) -> PyResult<CypherResult> {
    let rows = PyList::empty(py);
    for row in &result.rows {
        let dict = PyDict::new(py);
        for (column, value) in result.columns.iter().zip(row) {
            dict.set_item(column, value_to_py(py, value)?)?;
        }
        rows.append(dict)?;
    }
    Ok(CypherResult {
        columns: result.columns.clone(),
        rows: rows.unbind(),
        stats: stats_to_py(py, &result.stats)?.unbind(),
    })
}

// ── Python classes ─────────────────────────────────────────────────────

/// The result of one Cypher statement: `columns`, rows as dicts (iterate,
/// index, or `.rows`), and the write `stats`.
#[pyclass(frozen, name = "CypherResult")]
pub struct CypherResult {
    columns: Vec<String>,
    rows: Py<PyList>,
    stats: Py<PyDict>,
}

#[pymethods]
impl CypherResult {
    /// Column names, in `RETURN` order (empty for a statement without one).
    #[getter]
    fn columns(&self) -> Vec<String> {
        self.columns.clone()
    }

    /// Every row as a `dict` keyed by column name.
    #[getter]
    fn rows<'py>(&self, py: Python<'py>) -> Bound<'py, PyList> {
        // A fresh list each time, so callers cannot mutate the result.
        PyList::new(py, self.rows.bind(py).iter()).unwrap_or_else(|_| PyList::empty(py))
    }

    /// Write counters: `nodes_created`, `relationships_created`,
    /// `properties_set`, `nodes_deleted`, `relationships_deleted`,
    /// `labels_added`, `labels_removed`.
    #[getter]
    fn stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        self.stats.bind(py).copy()
    }

    fn __len__(&self, py: Python<'_>) -> usize {
        self.rows.bind(py).len()
    }

    fn __getitem__<'py>(&self, py: Python<'py>, index: isize) -> PyResult<Bound<'py, PyAny>> {
        self.rows.bind(py).as_any().get_item(index)
    }

    fn __iter__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        Ok(self.rows.bind(py).as_any().try_iter()?.into_any())
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        format!(
            "CypherResult(columns={:?}, rows={})",
            self.columns,
            self.rows.bind(py).len()
        )
    }
}

/// A node returned by Cypher.
#[pyclass(frozen, name = "CypherNode")]
pub struct CypherNode {
    id: u64,
    uuid: [u8; 16],
    labels: Vec<String>,
    properties: Py<PyDict>,
}

impl CypherNode {
    fn new(py: Python<'_>, node: &Arc<NodeValue>) -> PyResult<Self> {
        Ok(Self {
            id: node.id,
            uuid: node.uuid,
            labels: node.labels.clone(),
            properties: map_to_py(py, &node.properties)?.unbind(),
        })
    }
}

#[pymethods]
impl CypherNode {
    /// Storage id (the same as `Node.id`).
    #[getter]
    fn id(&self) -> u64 {
        self.id
    }

    /// UUID v7 as 16 bytes; `drevo/__init__.py` wraps it as `uuid.UUID`.
    #[getter]
    fn uuid<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.uuid)
    }

    /// Cypher labels; the first is the node's `kind`.
    #[getter]
    fn labels(&self) -> Vec<String> {
        self.labels.clone()
    }

    /// Properties as a `dict` (a real `title`/`body` appears here too).
    #[getter]
    fn properties<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        self.properties.bind(py).copy()
    }

    fn __repr__(&self) -> String {
        format!("CypherNode(id={}, labels={:?})", self.id, self.labels)
    }
}

/// A relationship returned by Cypher.
#[pyclass(frozen, name = "CypherRelationship")]
pub struct CypherRelationship {
    id: u64,
    uuid: [u8; 16],
    kind: String,
    start_id: u64,
    end_id: u64,
    properties: Py<PyDict>,
}

impl CypherRelationship {
    fn new(py: Python<'_>, rel: &Arc<RelationshipValue>) -> PyResult<Self> {
        Ok(Self {
            id: rel.id,
            uuid: rel.uuid,
            kind: rel.kind.clone(),
            start_id: rel.from_id,
            end_id: rel.to_id,
            properties: map_to_py(py, &rel.properties)?.unbind(),
        })
    }
}

#[pymethods]
impl CypherRelationship {
    /// Storage id (the same as `Edge.id`).
    #[getter]
    fn id(&self) -> u64 {
        self.id
    }

    /// UUID v7 as 16 bytes; `drevo/__init__.py` wraps it as `uuid.UUID`.
    #[getter]
    fn uuid<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.uuid)
    }

    /// Relationship type (the edge `kind`).
    #[getter]
    #[pyo3(name = "type")]
    fn rel_type(&self) -> &str {
        &self.kind
    }

    /// Id of the start node.
    #[getter]
    fn start_id(&self) -> u64 {
        self.start_id
    }

    /// Id of the end node.
    #[getter]
    fn end_id(&self) -> u64 {
        self.end_id
    }

    /// Properties as a `dict`.
    #[getter]
    fn properties<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        self.properties.bind(py).copy()
    }

    fn __repr__(&self) -> String {
        format!(
            "CypherRelationship(id={}, type={:?}, start_id={}, end_id={})",
            self.id, self.kind, self.start_id, self.end_id
        )
    }
}

/// A path returned by Cypher: alternating nodes and relationships.
#[pyclass(frozen, name = "CypherPath")]
pub struct CypherPath {
    nodes: Vec<Py<CypherNode>>,
    relationships: Vec<Py<CypherRelationship>>,
}

impl CypherPath {
    fn new(py: Python<'_>, path: &Arc<PathValue>) -> PyResult<Self> {
        Ok(Self {
            nodes: path
                .nodes
                .iter()
                .map(|n| Py::new(py, CypherNode::new(py, n)?))
                .collect::<PyResult<_>>()?,
            relationships: path
                .relationships
                .iter()
                .map(|r| Py::new(py, CypherRelationship::new(py, r)?))
                .collect::<PyResult<_>>()?,
        })
    }
}

#[pymethods]
impl CypherPath {
    /// The path's nodes, start to end.
    #[getter]
    fn nodes(&self, py: Python<'_>) -> Vec<Py<CypherNode>> {
        self.nodes.iter().map(|n| n.clone_ref(py)).collect()
    }

    /// The path's relationships, in traversal order.
    #[getter]
    fn relationships(&self, py: Python<'_>) -> Vec<Py<CypherRelationship>> {
        self.relationships.iter().map(|r| r.clone_ref(py)).collect()
    }

    /// Path length: the number of relationships.
    fn __len__(&self) -> usize {
        self.relationships.len()
    }

    fn __repr__(&self) -> String {
        format!("CypherPath(length={})", self.relationships.len())
    }
}
