//! WebAssembly bindings for drevo via `wasm-bindgen`.
//!
//! Exposes the [`Drevo`] API to JavaScript/TypeScript through
//! `wasm-bindgen`, enabling browser and Tauri v2 WASM usage.
//!
//! ## Design
//!
//! - **Wrapper struct**: [`WasmDrevo`] wraps the Rust [`Drevo`]
//!   and is exported as a JS class.
//! - **JSON serialization**: complex types (Node, Edge, SubGraph, ScoredNode)
//!   cross the WASM boundary as `JsValue` (parsed from JSON via serde).
//! - **Error handling**: Rust errors are converted to JavaScript exceptions
//!   via `JsValue::from_str`.
//! - **Memory-only**: WASM targets use `MemoryBackend` exclusively since
//!   filesystem access is unavailable in the browser.
//!
//! ## API
//!
//! All methods mirror the C FFI surface:
//! - Lifecycle: `new()`, `close()`
//! - Node CRUD: `create_node`, `get_node`, `update_node`, `delete_node`
//! - Edge CRUD: `create_edge`, `get_edge`, `update_edge`, `delete_edge`
//! - Traversal: `neighbors`, `bfs`, `dfs`, `shortest_path`, `subgraph`
//! - Search: `search_fts`, `list_nodes_by_kind`, `list_recent`

use wasm_bindgen::prelude::*;

use crate::db::Drevo;
use crate::model::{Direction, EdgePatch, NewEdge, NewNode, NodePatch, Properties};

// ---------------------------------------------------------------------------
// Error conversion
// ---------------------------------------------------------------------------

/// Convert a DrevoError into a JsValue for throwing as a JS exception.
fn to_js_err(e: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&e.to_string())
}

/// Serialize a Rust value to a JsValue via JSON.
fn to_js_value<T: serde::Serialize>(value: &T) -> Result<JsValue, JsValue> {
    let json = serde_json::to_string(value).map_err(to_js_err)?;
    js_sys::JSON::parse(&json)
}

/// Parse a JsValue (JS object) into a Rust type via JSON.
fn from_js_value<T: serde::de::DeserializeOwned>(value: &JsValue) -> Result<T, JsValue> {
    let json = js_sys::JSON::stringify(value)
        .map_err(|_| JsValue::from_str("failed to stringify JS value"))?;
    let json_str: String = json.into();
    serde_json::from_str(&json_str).map_err(to_js_err)
}

/// Parse a direction integer (0=Outgoing, 1=Incoming, 2=Both) into Direction.
fn parse_direction(d: i32) -> Result<Direction, JsValue> {
    match d {
        0 => Ok(Direction::Outgoing),
        1 => Ok(Direction::Incoming),
        2 => Ok(Direction::Both),
        _ => Err(JsValue::from_str(&format!(
            "invalid direction: {d} (expected 0=Outgoing, 1=Incoming, 2=Both)"
        ))),
    }
}

/// Parse optional properties from JsValue. If undefined/null, returns empty Properties.
fn parse_properties(val: &JsValue) -> Result<Properties, JsValue> {
    if val.is_undefined() || val.is_null() {
        return Ok(Properties::default());
    }
    from_js_value(val)
}

// ---------------------------------------------------------------------------
// WasmDrevo
// ---------------------------------------------------------------------------

/// drevo handle for WebAssembly.
///
/// Use `WasmDrevo.new()` to create an in-memory database.
/// All complex values (Node, Edge, etc.) are passed as JS objects.
#[wasm_bindgen]
pub struct WasmDrevo {
    db: Option<Drevo>,
}

#[wasm_bindgen]
impl WasmDrevo {
    // ----- Lifecycle -----

    /// Create a new in-memory Drevo database.
    ///
    /// WASM targets always use in-memory storage (no filesystem).
    #[wasm_bindgen(constructor)]
    pub fn new() -> Result<WasmDrevo, JsValue> {
        let db = Drevo::open_in_memory().map_err(to_js_err)?;
        Ok(WasmDrevo { db: Some(db) })
    }

    /// Close the database and release resources.
    ///
    /// After calling `close()`, all other methods will throw.
    #[wasm_bindgen]
    pub fn close(&mut self) -> Result<(), JsValue> {
        let db = self
            .db
            .take()
            .ok_or_else(|| JsValue::from_str("database already closed"))?;
        db.close().map_err(to_js_err)
    }

    // ----- Node CRUD -----

    /// Create a new node.
    ///
    /// Parameters: `kind`, `title`, `body`, `body_html` (strings),
    /// `properties` (JS object, optional).
    ///
    /// Returns the created Node as a JS object.
    #[wasm_bindgen]
    pub fn create_node(
        &self,
        kind: &str,
        title: &str,
        body: &str,
        body_html: &str,
        properties: &JsValue,
    ) -> Result<JsValue, JsValue> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| JsValue::from_str("database closed"))?;
        let props = parse_properties(properties)?;
        let new_node = NewNode {
            kind: kind.to_string(),
            title: title.to_string(),
            body: body.to_string(),
            body_html: body_html.to_string(),
            properties: props,
        };
        let node = db.create_node(new_node).map_err(to_js_err)?;
        to_js_value(&node)
    }

    /// Get a node by ID.
    ///
    /// Returns the Node as a JS object, or `null` if not found.
    #[wasm_bindgen]
    pub fn get_node(&self, id: u64) -> Result<JsValue, JsValue> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| JsValue::from_str("database closed"))?;
        match db.get_node(id).map_err(to_js_err)? {
            Some(node) => to_js_value(&node),
            None => Ok(JsValue::NULL),
        }
    }

    /// Update a node by ID.
    ///
    /// `patch` is a JS object with optional fields: `kind`, `title`,
    /// `body`, `body_html`, `properties`.
    ///
    /// Returns the updated Node as a JS object.
    #[wasm_bindgen]
    pub fn update_node(&self, id: u64, patch: &JsValue) -> Result<JsValue, JsValue> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| JsValue::from_str("database closed"))?;
        let patch: NodePatch = from_js_value(patch)?;
        let node = db.update_node(id, patch).map_err(to_js_err)?;
        to_js_value(&node)
    }

    /// Delete a node by ID.
    ///
    /// Also deletes all edges connected to the node.
    #[wasm_bindgen]
    pub fn delete_node(&self, id: u64) -> Result<(), JsValue> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| JsValue::from_str("database closed"))?;
        db.delete_node(id).map_err(to_js_err)
    }

    // ----- Edge CRUD -----

    /// Create a new edge.
    ///
    /// Parameters: `from_id`, `to_id` (node IDs), `kind` (string),
    /// `weight` (f32), `properties` (JS object, optional).
    ///
    /// Returns the created Edge as a JS object.
    #[wasm_bindgen]
    pub fn create_edge(
        &self,
        from_id: u64,
        to_id: u64,
        kind: &str,
        weight: f32,
        properties: &JsValue,
    ) -> Result<JsValue, JsValue> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| JsValue::from_str("database closed"))?;
        let props = parse_properties(properties)?;
        let new_edge = NewEdge {
            from_id,
            to_id,
            kind: kind.to_string(),
            weight,
            properties: props,
        };
        let edge = db.create_edge(new_edge).map_err(to_js_err)?;
        to_js_value(&edge)
    }

    /// Get an edge by ID.
    ///
    /// Returns the Edge as a JS object, or `null` if not found.
    #[wasm_bindgen]
    pub fn get_edge(&self, id: u64) -> Result<JsValue, JsValue> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| JsValue::from_str("database closed"))?;
        match db.get_edge(id).map_err(to_js_err)? {
            Some(edge) => to_js_value(&edge),
            None => Ok(JsValue::NULL),
        }
    }

    /// Update an edge by ID.
    ///
    /// `patch` is a JS object with optional fields: `kind`, `weight`, `properties`.
    ///
    /// Returns the updated Edge as a JS object.
    #[wasm_bindgen]
    pub fn update_edge(&self, id: u64, patch: &JsValue) -> Result<JsValue, JsValue> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| JsValue::from_str("database closed"))?;
        let patch: EdgePatch = from_js_value(patch)?;
        let edge = db.update_edge(id, patch).map_err(to_js_err)?;
        to_js_value(&edge)
    }

    /// Delete an edge by ID.
    #[wasm_bindgen]
    pub fn delete_edge(&self, id: u64) -> Result<(), JsValue> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| JsValue::from_str("database closed"))?;
        db.delete_edge(id).map_err(to_js_err)
    }

    // ----- Traversal -----

    /// Get neighbor nodes.
    ///
    /// `direction`: 0=Outgoing, 1=Incoming, 2=Both.
    /// `edge_kind`: optional edge kind filter (empty string = no filter).
    ///
    /// Returns an array of Node objects.
    #[wasm_bindgen]
    pub fn neighbors(
        &self,
        node_id: u64,
        direction: i32,
        edge_kind: &str,
    ) -> Result<JsValue, JsValue> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| JsValue::from_str("database closed"))?;
        let dir = parse_direction(direction)?;
        let kind_filter = if edge_kind.is_empty() {
            None
        } else {
            Some(edge_kind)
        };
        let nodes = db.neighbors(node_id, dir, kind_filter).map_err(to_js_err)?;
        to_js_value(&nodes)
    }

    /// Breadth-first search from a start node.
    ///
    /// `direction`: 0=Outgoing, 1=Incoming, 2=Both.
    /// `edge_kind`: optional edge kind filter (empty string = no filter).
    ///
    /// Returns an array of Node objects.
    #[wasm_bindgen]
    pub fn bfs(
        &self,
        start_id: u64,
        max_depth: u8,
        direction: i32,
        edge_kind: &str,
    ) -> Result<JsValue, JsValue> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| JsValue::from_str("database closed"))?;
        let dir = parse_direction(direction)?;
        let kind_filter = if edge_kind.is_empty() {
            None
        } else {
            Some(edge_kind.to_string())
        };
        let nodes = db
            .bfs(start_id, max_depth, dir, kind_filter.as_deref())
            .map_err(to_js_err)?;
        to_js_value(&nodes)
    }

    /// Depth-first search from a start node.
    ///
    /// `direction`: 0=Outgoing, 1=Incoming, 2=Both.
    /// `edge_kind`: optional edge kind filter (empty string = no filter).
    ///
    /// Returns an array of Node objects.
    #[wasm_bindgen]
    pub fn dfs(
        &self,
        start_id: u64,
        max_depth: u8,
        direction: i32,
        edge_kind: &str,
    ) -> Result<JsValue, JsValue> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| JsValue::from_str("database closed"))?;
        let dir = parse_direction(direction)?;
        let kind_filter = if edge_kind.is_empty() {
            None
        } else {
            Some(edge_kind.to_string())
        };
        let nodes = db
            .dfs(start_id, max_depth, dir, kind_filter.as_deref())
            .map_err(to_js_err)?;
        to_js_value(&nodes)
    }

    /// Find the shortest path between two nodes.
    ///
    /// Returns an array of node IDs (u64), or `null` if no path exists.
    #[wasm_bindgen]
    pub fn shortest_path(&self, from_id: u64, to_id: u64) -> Result<JsValue, JsValue> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| JsValue::from_str("database closed"))?;
        match db.shortest_path(from_id, to_id).map_err(to_js_err)? {
            Some(path) => to_js_value(&path),
            None => Ok(JsValue::NULL),
        }
    }

    /// Extract a subgraph rooted at a node, up to a given depth.
    ///
    /// Returns a JS object with `nodes` (array) and `edges` (array).
    #[wasm_bindgen]
    pub fn subgraph(&self, root_id: u64, depth: u8) -> Result<JsValue, JsValue> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| JsValue::from_str("database closed"))?;
        let sg = db.subgraph(root_id, depth).map_err(to_js_err)?;
        to_js_value(&sg)
    }

    // ----- Search -----

    /// Full-text search over node titles and bodies.
    ///
    /// Returns an array of `{ node, score }` objects, ranked by TF-IDF.
    #[wasm_bindgen]
    pub fn search_fts(&self, query: &str, limit: u32) -> Result<JsValue, JsValue> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| JsValue::from_str("database closed"))?;
        let results = db.search_fts(query, limit as usize).map_err(to_js_err)?;
        to_js_value(&results)
    }

    /// List nodes of a given kind with pagination.
    ///
    /// Returns an array of Node objects.
    #[wasm_bindgen]
    pub fn list_nodes_by_kind(
        &self,
        kind: &str,
        limit: u32,
        offset: u32,
    ) -> Result<JsValue, JsValue> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| JsValue::from_str("database closed"))?;
        let nodes = db
            .list_nodes_by_kind(kind, limit as usize, offset as usize)
            .map_err(to_js_err)?;
        to_js_value(&nodes)
    }

    /// List recently updated nodes.
    ///
    /// Returns an array of Node objects, newest first.
    #[wasm_bindgen]
    pub fn list_recent(&self, limit: u32) -> Result<JsValue, JsValue> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| JsValue::from_str("database closed"))?;
        let nodes = db.list_recent(limit as usize).map_err(to_js_err)?;
        to_js_value(&nodes)
    }
}

// Unit tests for wasm bindings require a JS runtime (wasm-pack test).
// Native integration tests are in tests/wasm_tests.rs — they exercise the
// same Drevo API surface that WasmDrevo delegates to, validating
// correctness of JSON roundtrips and error handling without a WASM runtime.
