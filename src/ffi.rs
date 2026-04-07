//! C FFI bindings for GraphNote DB.
//!
//! Exposes the [`GraphNoteDb`] API through `extern "C"` functions for
//! consumption by C, Swift (iOS), Kotlin/JNI (Android), and other FFI-capable
//! languages.
//!
//! ## Design
//!
//! - **Opaque handle**: [`GraphNoteDbHandle`] is a type alias for the Rust struct.
//!   C consumers receive `*mut GraphNoteDbHandle` — an opaque pointer.
//! - **JSON serialization**: complex types (Node, Edge, SubGraph, etc.) cross
//!   the FFI boundary as JSON-encoded C strings (`*mut c_char`).
//! - **Error reporting**: thread-local `LAST_ERROR` stores the most recent error
//!   message. On success it is cleared; on failure it is set. Call
//!   [`graphnote_last_error`] to retrieve (and the caller must free) it.
//! - **Memory ownership**: every `*mut c_char` returned by an FFI function is
//!   owned by the caller — free it with [`graphnote_free_string`].

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::Path;
use std::ptr;

use crate::db::GraphNoteDb;
use crate::model::{Direction, EdgePatch, NewEdge, NewNode, NodePatch, Properties};

/// Opaque handle exposed to C consumers.
pub type GraphNoteDbHandle = GraphNoteDb;

// ---------------------------------------------------------------------------
// Thread-local error
// ---------------------------------------------------------------------------

thread_local! {
    static LAST_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn set_error(msg: String) {
    LAST_ERROR.with(|e| *e.borrow_mut() = Some(msg));
}

fn clear_error() {
    LAST_ERROR.with(|e| *e.borrow_mut() = None);
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Convert a Rust string to a heap-allocated C string, returning the pointer.
/// The caller owns the returned pointer and must free it with `graphnote_free_string`.
fn to_c_string(s: &str) -> *mut c_char {
    match CString::new(s) {
        Ok(cs) => cs.into_raw(),
        Err(_) => {
            set_error("string contains interior NUL byte".to_string());
            ptr::null_mut()
        }
    }
}

/// Serialize a value to JSON, then to a C string.
fn to_json_c_string<T: serde::Serialize>(value: &T) -> *mut c_char {
    match serde_json::to_string(value) {
        Ok(json) => to_c_string(&json),
        Err(e) => {
            set_error(format!("JSON serialization error: {e}"));
            ptr::null_mut()
        }
    }
}

/// Read a `*const c_char` into a Rust `&str`. Returns `None` and sets error
/// if the pointer is null or not valid UTF-8.
///
/// # Safety
/// The pointer must be valid and NUL-terminated.
unsafe fn read_c_str<'a>(ptr: *const c_char, name: &str) -> Option<&'a str> {
    if ptr.is_null() {
        set_error(format!("{name} is null"));
        return None;
    }
    match CStr::from_ptr(ptr).to_str() {
        Ok(s) => Some(s),
        Err(e) => {
            set_error(format!("{name} is not valid UTF-8: {e}"));
            None
        }
    }
}

/// Parse a JSON string from C into `HashMap<String, serde_json::Value>`.
///
/// # Safety
/// The pointer must be valid and NUL-terminated.
unsafe fn parse_properties(ptr: *const c_char) -> Option<Properties> {
    let s = read_c_str(ptr, "properties")?;
    match serde_json::from_str::<HashMap<String, serde_json::Value>>(s) {
        Ok(map) => Some(Properties::from(map)),
        Err(e) => {
            set_error(format!("invalid properties JSON: {e}"));
            None
        }
    }
}

/// Convert an integer direction code to [`Direction`].
/// 0 = Outgoing, 1 = Incoming, 2 = Both.
fn direction_from_int(d: i32) -> Option<Direction> {
    match d {
        0 => Some(Direction::Outgoing),
        1 => Some(Direction::Incoming),
        2 => Some(Direction::Both),
        _ => {
            set_error(format!(
                "invalid direction: {d} (expected 0=Outgoing, 1=Incoming, 2=Both)"
            ));
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Open a disk-backed database at the given path.
///
/// Returns an opaque handle on success, or `NULL` on failure (check
/// [`graphnote_last_error`]).
///
/// # Safety
/// `path` must be a valid NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn graphnote_open(path: *const c_char) -> *mut GraphNoteDbHandle {
    clear_error();
    let Some(p) = read_c_str(path, "path") else {
        return ptr::null_mut();
    };
    match GraphNoteDb::open(Path::new(p)) {
        Ok(db) => Box::into_raw(Box::new(db)),
        Err(e) => {
            set_error(format!("{e}"));
            ptr::null_mut()
        }
    }
}

/// Open an in-memory (ephemeral) database.
///
/// Returns an opaque handle on success, or `NULL` on failure.
///
/// # Safety
/// No preconditions — this function is always safe to call.
#[no_mangle]
pub unsafe extern "C" fn graphnote_open_in_memory() -> *mut GraphNoteDbHandle {
    clear_error();
    match GraphNoteDb::open_in_memory() {
        Ok(db) => Box::into_raw(Box::new(db)),
        Err(e) => {
            set_error(format!("{e}"));
            ptr::null_mut()
        }
    }
}

/// Close the database and free the handle.
///
/// Returns 0 on success, -1 on failure. Passing `NULL` is an error.
///
/// # Safety
/// `db` must be a valid handle returned by `graphnote_open*`, or `NULL`.
/// After this call the handle is invalid and must not be used.
#[no_mangle]
pub unsafe extern "C" fn graphnote_close(db: *mut GraphNoteDbHandle) -> i32 {
    clear_error();
    if db.is_null() {
        set_error("null db handle".to_string());
        return -1;
    }
    let db = Box::from_raw(db);
    match db.close() {
        Ok(()) => 0,
        Err(e) => {
            set_error(format!("{e}"));
            -1
        }
    }
}

// ---------------------------------------------------------------------------
// Error retrieval
// ---------------------------------------------------------------------------

/// Return the last error message as a C string, or `NULL` if no error.
///
/// The caller owns the returned pointer and must free it with
/// [`graphnote_free_string`].
///
/// # Safety
/// No preconditions — this function is always safe to call.
#[no_mangle]
pub unsafe extern "C" fn graphnote_last_error() -> *mut c_char {
    LAST_ERROR.with(|e| {
        let borrowed = e.borrow();
        match borrowed.as_ref() {
            Some(msg) => to_c_string(msg),
            None => ptr::null_mut(),
        }
    })
}

/// Free a C string previously returned by any `graphnote_*` function.
///
/// Passing `NULL` is safe (no-op).
///
/// # Safety
/// `s` must be a pointer returned by a `graphnote_*` function, or `NULL`.
#[no_mangle]
pub unsafe extern "C" fn graphnote_free_string(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}

// ---------------------------------------------------------------------------
// Node CRUD
// ---------------------------------------------------------------------------

/// Create a new node. Returns the created node as a JSON C string, or `NULL` on error.
///
/// # Safety
/// All string parameters must be valid NUL-terminated UTF-8.
/// `properties_json` must be valid JSON object string.
#[no_mangle]
pub unsafe extern "C" fn graphnote_create_node(
    db: *mut GraphNoteDbHandle,
    kind: *const c_char,
    title: *const c_char,
    body: *const c_char,
    body_html: *const c_char,
    properties_json: *const c_char,
) -> *mut c_char {
    clear_error();
    if db.is_null() {
        set_error("null db handle".to_string());
        return ptr::null_mut();
    }
    let db = &*db;

    let Some(kind) = read_c_str(kind, "kind") else {
        return ptr::null_mut();
    };
    let Some(title) = read_c_str(title, "title") else {
        return ptr::null_mut();
    };
    let Some(body) = read_c_str(body, "body") else {
        return ptr::null_mut();
    };
    let Some(body_html) = read_c_str(body_html, "body_html") else {
        return ptr::null_mut();
    };
    let Some(properties) = parse_properties(properties_json) else {
        return ptr::null_mut();
    };

    let new_node = NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        body_html: body_html.to_string(),
        properties,
    };

    match db.create_node(new_node) {
        Ok(node) => to_json_c_string(&node),
        Err(e) => {
            set_error(format!("{e}"));
            ptr::null_mut()
        }
    }
}

/// Get a node by ID. Returns JSON C string, or `NULL` if not found / error.
///
/// # Safety
/// `db` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn graphnote_get_node(db: *mut GraphNoteDbHandle, id: u64) -> *mut c_char {
    clear_error();
    if db.is_null() {
        set_error("null db handle".to_string());
        return ptr::null_mut();
    }
    let db = &*db;

    match db.get_node(id) {
        Ok(Some(node)) => to_json_c_string(&node),
        Ok(None) => {
            set_error(format!("node not found: {id}"));
            ptr::null_mut()
        }
        Err(e) => {
            set_error(format!("{e}"));
            ptr::null_mut()
        }
    }
}

/// Update a node by ID with a JSON patch. Returns updated node as JSON, or `NULL`.
///
/// The patch JSON may contain any subset of: `title`, `kind`, `body`, `body_html`, `properties`.
///
/// # Safety
/// `db` must be a valid handle. `patch_json` must be valid JSON.
#[no_mangle]
pub unsafe extern "C" fn graphnote_update_node(
    db: *mut GraphNoteDbHandle,
    id: u64,
    patch_json: *const c_char,
) -> *mut c_char {
    clear_error();
    if db.is_null() {
        set_error("null db handle".to_string());
        return ptr::null_mut();
    }
    let db = &*db;
    let Some(patch_str) = read_c_str(patch_json, "patch_json") else {
        return ptr::null_mut();
    };

    // Parse the JSON patch into individual fields
    let patch_value: serde_json::Value = match serde_json::from_str(patch_str) {
        Ok(v) => v,
        Err(e) => {
            set_error(format!("invalid patch JSON: {e}"));
            return ptr::null_mut();
        }
    };

    let patch = NodePatch {
        kind: patch_value
            .get("kind")
            .and_then(|v| v.as_str())
            .map(String::from),
        title: patch_value
            .get("title")
            .and_then(|v| v.as_str())
            .map(String::from),
        body: patch_value
            .get("body")
            .and_then(|v| v.as_str())
            .map(String::from),
        body_html: patch_value
            .get("body_html")
            .and_then(|v| v.as_str())
            .map(String::from),
        properties: patch_value.get("properties").and_then(|v| {
            serde_json::from_value::<HashMap<String, serde_json::Value>>(v.clone())
                .ok()
                .map(Properties::from)
        }),
    };

    match db.update_node(id, patch) {
        Ok(node) => to_json_c_string(&node),
        Err(e) => {
            set_error(format!("{e}"));
            ptr::null_mut()
        }
    }
}

/// Delete a node by ID. Returns 0 on success, -1 on error.
///
/// # Safety
/// `db` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn graphnote_delete_node(db: *mut GraphNoteDbHandle, id: u64) -> i32 {
    clear_error();
    if db.is_null() {
        set_error("null db handle".to_string());
        return -1;
    }
    let db = &*db;

    match db.delete_node(id) {
        Ok(()) => 0,
        Err(e) => {
            set_error(format!("{e}"));
            -1
        }
    }
}

// ---------------------------------------------------------------------------
// Edge CRUD
// ---------------------------------------------------------------------------

/// Create a new edge. Returns the created edge as JSON, or `NULL` on error.
///
/// # Safety
/// `db` must be a valid handle. String parameters must be valid NUL-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn graphnote_create_edge(
    db: *mut GraphNoteDbHandle,
    from_id: u64,
    to_id: u64,
    kind: *const c_char,
    weight: f32,
    properties_json: *const c_char,
) -> *mut c_char {
    clear_error();
    if db.is_null() {
        set_error("null db handle".to_string());
        return ptr::null_mut();
    }
    let db = &*db;

    let Some(kind) = read_c_str(kind, "kind") else {
        return ptr::null_mut();
    };
    let Some(properties) = parse_properties(properties_json) else {
        return ptr::null_mut();
    };

    let new_edge = NewEdge {
        from_id,
        to_id,
        kind: kind.to_string(),
        weight,
        properties,
    };

    match db.create_edge(new_edge) {
        Ok(edge) => to_json_c_string(&edge),
        Err(e) => {
            set_error(format!("{e}"));
            ptr::null_mut()
        }
    }
}

/// Get an edge by ID. Returns JSON, or `NULL` if not found / error.
///
/// # Safety
/// `db` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn graphnote_get_edge(db: *mut GraphNoteDbHandle, id: u64) -> *mut c_char {
    clear_error();
    if db.is_null() {
        set_error("null db handle".to_string());
        return ptr::null_mut();
    }
    let db = &*db;

    match db.get_edge(id) {
        Ok(Some(edge)) => to_json_c_string(&edge),
        Ok(None) => {
            set_error(format!("edge not found: {id}"));
            ptr::null_mut()
        }
        Err(e) => {
            set_error(format!("{e}"));
            ptr::null_mut()
        }
    }
}

/// Update an edge by ID with a JSON patch. Returns updated edge as JSON, or `NULL`.
///
/// The patch JSON may contain: `kind`, `weight`, `properties`.
///
/// # Safety
/// `db` must be a valid handle. `patch_json` must be valid JSON.
#[no_mangle]
pub unsafe extern "C" fn graphnote_update_edge(
    db: *mut GraphNoteDbHandle,
    id: u64,
    patch_json: *const c_char,
) -> *mut c_char {
    clear_error();
    if db.is_null() {
        set_error("null db handle".to_string());
        return ptr::null_mut();
    }
    let db = &*db;
    let Some(patch_str) = read_c_str(patch_json, "patch_json") else {
        return ptr::null_mut();
    };

    let patch_value: serde_json::Value = match serde_json::from_str(patch_str) {
        Ok(v) => v,
        Err(e) => {
            set_error(format!("invalid patch JSON: {e}"));
            return ptr::null_mut();
        }
    };

    let patch = EdgePatch {
        kind: patch_value
            .get("kind")
            .and_then(|v| v.as_str())
            .map(String::from),
        weight: patch_value
            .get("weight")
            .and_then(|v| v.as_f64())
            .map(|f| f as f32),
        properties: patch_value.get("properties").and_then(|v| {
            serde_json::from_value::<HashMap<String, serde_json::Value>>(v.clone())
                .ok()
                .map(Properties::from)
        }),
    };

    match db.update_edge(id, patch) {
        Ok(edge) => to_json_c_string(&edge),
        Err(e) => {
            set_error(format!("{e}"));
            ptr::null_mut()
        }
    }
}

/// Delete an edge by ID. Returns 0 on success, -1 on error.
///
/// # Safety
/// `db` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn graphnote_delete_edge(db: *mut GraphNoteDbHandle, id: u64) -> i32 {
    clear_error();
    if db.is_null() {
        set_error("null db handle".to_string());
        return -1;
    }
    let db = &*db;

    match db.delete_edge(id) {
        Ok(()) => 0,
        Err(e) => {
            set_error(format!("{e}"));
            -1
        }
    }
}

// ---------------------------------------------------------------------------
// Traversal
// ---------------------------------------------------------------------------

/// Get neighbors of a node. Returns JSON array of nodes, or `NULL` on error.
///
/// `direction`: 0 = Outgoing, 1 = Incoming, 2 = Both.
/// `edge_kind`: optional edge kind filter (pass `NULL` for no filter).
///
/// # Safety
/// `db` must be a valid handle. `edge_kind` must be valid UTF-8 or `NULL`.
#[no_mangle]
pub unsafe extern "C" fn graphnote_neighbors(
    db: *mut GraphNoteDbHandle,
    node_id: u64,
    direction: i32,
    edge_kind: *const c_char,
) -> *mut c_char {
    clear_error();
    if db.is_null() {
        set_error("null db handle".to_string());
        return ptr::null_mut();
    }
    let db = &*db;

    let Some(dir) = direction_from_int(direction) else {
        return ptr::null_mut();
    };
    let kind_filter = if edge_kind.is_null() {
        None
    } else {
        let Some(k) = read_c_str(edge_kind, "edge_kind") else {
            return ptr::null_mut();
        };
        Some(k)
    };

    match db.neighbors(node_id, dir, kind_filter) {
        Ok(nodes) => to_json_c_string(&nodes),
        Err(e) => {
            set_error(format!("{e}"));
            ptr::null_mut()
        }
    }
}

/// BFS traversal from a start node. Returns JSON array of node IDs, or `NULL`.
///
/// `direction`: 0 = Outgoing, 1 = Incoming, 2 = Both.
/// `edge_kind`: optional edge kind filter (`NULL` for no filter).
///
/// # Safety
/// `db` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn graphnote_bfs(
    db: *mut GraphNoteDbHandle,
    start_id: u64,
    max_depth: u8,
    direction: i32,
    edge_kind: *const c_char,
) -> *mut c_char {
    clear_error();
    if db.is_null() {
        set_error("null db handle".to_string());
        return ptr::null_mut();
    }
    let db = &*db;

    let Some(dir) = direction_from_int(direction) else {
        return ptr::null_mut();
    };
    let kind_filter: Option<&str> = if edge_kind.is_null() {
        None
    } else {
        let Some(k) = read_c_str(edge_kind, "edge_kind") else {
            return ptr::null_mut();
        };
        Some(k)
    };

    match db.bfs(start_id, max_depth, dir, kind_filter) {
        Ok(ids) => to_json_c_string(&ids),
        Err(e) => {
            set_error(format!("{e}"));
            ptr::null_mut()
        }
    }
}

/// DFS traversal from a start node. Returns JSON array of node IDs, or `NULL`.
///
/// `direction`: 0 = Outgoing, 1 = Incoming, 2 = Both.
/// `edge_kind`: optional edge kind filter (`NULL` for no filter).
///
/// # Safety
/// `db` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn graphnote_dfs(
    db: *mut GraphNoteDbHandle,
    start_id: u64,
    max_depth: u8,
    direction: i32,
    edge_kind: *const c_char,
) -> *mut c_char {
    clear_error();
    if db.is_null() {
        set_error("null db handle".to_string());
        return ptr::null_mut();
    }
    let db = &*db;

    let Some(dir) = direction_from_int(direction) else {
        return ptr::null_mut();
    };
    let kind_filter: Option<&str> = if edge_kind.is_null() {
        None
    } else {
        let Some(k) = read_c_str(edge_kind, "edge_kind") else {
            return ptr::null_mut();
        };
        Some(k)
    };

    match db.dfs(start_id, max_depth, dir, kind_filter) {
        Ok(ids) => to_json_c_string(&ids),
        Err(e) => {
            set_error(format!("{e}"));
            ptr::null_mut()
        }
    }
}

/// Find the shortest path between two nodes. Returns JSON array of node IDs, or `NULL`.
///
/// Returns `NULL` with error "no path found" if no path exists.
///
/// # Safety
/// `db` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn graphnote_shortest_path(
    db: *mut GraphNoteDbHandle,
    from_id: u64,
    to_id: u64,
) -> *mut c_char {
    clear_error();
    if db.is_null() {
        set_error("null db handle".to_string());
        return ptr::null_mut();
    }
    let db = &*db;

    match db.shortest_path(from_id, to_id) {
        Ok(Some(path)) => to_json_c_string(&path),
        Ok(None) => {
            set_error(format!("no path found from {from_id} to {to_id}"));
            ptr::null_mut()
        }
        Err(e) => {
            set_error(format!("{e}"));
            ptr::null_mut()
        }
    }
}

/// Extract a subgraph around a root node. Returns JSON with `nodes` and `edges` arrays, or `NULL`.
///
/// # Safety
/// `db` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn graphnote_subgraph(
    db: *mut GraphNoteDbHandle,
    root_id: u64,
    depth: u8,
) -> *mut c_char {
    clear_error();
    if db.is_null() {
        set_error("null db handle".to_string());
        return ptr::null_mut();
    }
    let db = &*db;

    match db.subgraph(root_id, depth) {
        Ok(sg) => to_json_c_string(&sg),
        Err(e) => {
            set_error(format!("{e}"));
            ptr::null_mut()
        }
    }
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

/// Full-text search. Returns JSON array of `{node, score}` objects, or `NULL`.
///
/// # Safety
/// `db` must be a valid handle. `query` must be valid NUL-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn graphnote_search_fts(
    db: *mut GraphNoteDbHandle,
    query: *const c_char,
    limit: u64,
) -> *mut c_char {
    clear_error();
    if db.is_null() {
        set_error("null db handle".to_string());
        return ptr::null_mut();
    }
    let db = &*db;
    let Some(q) = read_c_str(query, "query") else {
        return ptr::null_mut();
    };

    match db.search_fts(q, limit as usize) {
        Ok(results) => to_json_c_string(&results),
        Err(e) => {
            set_error(format!("{e}"));
            ptr::null_mut()
        }
    }
}

/// List nodes by kind with pagination. Returns JSON array of nodes, or `NULL`.
///
/// # Safety
/// `db` must be a valid handle. `kind` must be valid NUL-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn graphnote_list_nodes_by_kind(
    db: *mut GraphNoteDbHandle,
    kind: *const c_char,
    limit: u64,
    offset: u64,
) -> *mut c_char {
    clear_error();
    if db.is_null() {
        set_error("null db handle".to_string());
        return ptr::null_mut();
    }
    let db = &*db;
    let Some(k) = read_c_str(kind, "kind") else {
        return ptr::null_mut();
    };

    match db.list_nodes_by_kind(k, limit as usize, offset as usize) {
        Ok(nodes) => to_json_c_string(&nodes),
        Err(e) => {
            set_error(format!("{e}"));
            ptr::null_mut()
        }
    }
}

/// List recently updated nodes. Returns JSON array of nodes, or `NULL`.
///
/// # Safety
/// `db` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn graphnote_list_recent(
    db: *mut GraphNoteDbHandle,
    limit: u64,
) -> *mut c_char {
    clear_error();
    if db.is_null() {
        set_error("null db handle".to_string());
        return ptr::null_mut();
    }
    let db = &*db;

    match db.list_recent(limit as usize) {
        Ok(nodes) => to_json_c_string(&nodes),
        Err(e) => {
            set_error(format!("{e}"));
            ptr::null_mut()
        }
    }
}
