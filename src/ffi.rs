//! C FFI bindings for drevo.
//!
//! Exposes the [`crate::db::Drevo`] API through `extern "C"` functions for
//! consumption by C, Swift (iOS), Kotlin/JNI (Android), and other FFI-capable
//! languages.
//!
//! ## Design
//!
//! - **Opaque handle**: [`crate::ffi::DrevoHandle`] is a type alias for the
//!   Rust struct. C consumers receive `*mut DrevoHandle` — an opaque pointer.
//! - **JSON serialization**: complex types (Node, Edge, SubGraph, etc.) cross
//!   the FFI boundary as JSON-encoded C strings (`*mut c_char`).
//! - **Error reporting**: thread-local `LAST_ERROR` stores the most recent error
//!   message. On success it is cleared; on failure it is set. Call
//!   [`crate::ffi::drevo_last_error`] to retrieve (and the caller must free) it.
//! - **Memory ownership**: every `*mut c_char` returned by an FFI function is
//!   owned by the caller — free it with [`crate::ffi::drevo_free_string`].
//!
//! ## Panic safety — required by `drevo-rust` §"No panics across FFI"
//!
//! Panics across an `extern "C"` boundary are **undefined behavior**. Every
//! entry point in this module wraps its body in [`std::panic::catch_unwind`]
//! via the `ffi_guard_ptr!` / `ffi_guard_int!` macros. If a panic escapes the
//! body, the guard:
//!
//! 1. swallows the unwind so it never crosses the C ABI;
//! 2. records `"panic in <fn> (caught at FFI boundary)"` in the thread-local
//!    error;
//! 3. returns the function's "error sentinel" (`NULL` for pointer-returning
//!    entries, `-1` for `i32`-returning entries).
//!
//! Production paths in `Drevo` are panic-free after Phase 8.5 tasks
//! `00103`–`00109`, but the guard remains as defense-in-depth against:
//!
//! - allocator failures (`alloc::handle_alloc_error`),
//! - stack-overflow on pathological inputs,
//! - future refactors that inadvertently introduce a `unwrap()`.
//!
//! ## Lifecycle and double-free
//!
//! Every successful [`crate::ffi::drevo_open`] /
//! [`crate::ffi::drevo_open_in_memory`] returns an owned `*mut DrevoHandle`
//! that must be released by exactly one matching
//! [`crate::ffi::drevo_close`] call. The C contract is the standard one:
//!
//! - **Single owner.** Once [`crate::ffi::drevo_close`] returns, the pointer
//!   is invalid.
//! - **Double-free is UB.** Calling [`crate::ffi::drevo_close`] twice on the
//!   same pointer is undefined behavior — the second call dereferences freed
//!   memory. Callers MUST set their handle variable to `NULL` after closing.
//! - **Use-after-close is UB.** Calling any other `drevo_*` function on a
//!   closed handle is undefined behavior for the same reason.
//! - **`NULL` handle is detected.** Passing `NULL` to any function that
//!   accepts a handle returns the error sentinel and sets
//!   `"null db handle"` — never UB.
//!
//! Best-effort runtime double-free detection (e.g. a magic-sentinel pattern)
//! is deferred — see `audit/AUDIT-ffi.md` finding F2 for the trade-off
//! analysis.
//!
//! ## Thread-local error
//!
//! `LAST_ERROR` is `thread_local!`, so each OS thread has an independent
//! error slot. Cross-thread error reads are impossible by construction —
//! a C consumer that calls drevo from multiple threads must call
//! [`crate::ffi::drevo_last_error`] on the same thread that produced the
//! error.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::Path;
use std::ptr;

use crate::db::Drevo;
use crate::model::{Direction, EdgePatch, NewEdge, NewNode, NodePatch, Properties};

/// Opaque handle exposed to C consumers.
pub type DrevoHandle = Drevo;

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
// Panic guards
// ---------------------------------------------------------------------------
//
// Every `extern "C"` function in this module must route its body through one
// of these two macros — see the module-level "Panic safety" doc-comment.
//
// The macros wrap the body in `std::panic::catch_unwind` (via
// `AssertUnwindSafe`, because `LAST_ERROR` is a `RefCell` and therefore
// `!UnwindSafe`). The body executes inside an `unsafe { ... }` block so the
// existing unsafe operations (`*db`, `read_c_str`, `Box::from_raw`, …) keep
// compiling without per-call rewrites.
//
// On panic, the guard records a generic "panic in <fn>" message in the
// thread-local error and returns the appropriate error sentinel.

macro_rules! ffi_guard_ptr {
    ($name:literal, $body:block) => {{
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            // The `unsafe` block lets callers in `pub unsafe extern "C" fn`
            // bodies use unsafe ops (`*db`, `read_c_str`, ...) without per-
            // call rewrites. When the macro is invoked from a safe context
            // (panic-guard unit tests), the block is harmlessly unused.
            #[allow(unused_unsafe)]
            unsafe {
                $body
            }
        })) {
            Ok(result) => result,
            Err(_panic_payload) => {
                set_error(format!("panic in {} (caught at FFI boundary)", $name));
                ptr::null_mut()
            }
        }
    }};
}

macro_rules! ffi_guard_int {
    ($name:literal, $body:block) => {{
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            #[allow(unused_unsafe)]
            unsafe {
                $body
            }
        })) {
            Ok(result) => result,
            Err(_panic_payload) => {
                set_error(format!("panic in {} (caught at FFI boundary)", $name));
                -1
            }
        }
    }};
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Convert a Rust string to a heap-allocated C string, returning the pointer.
/// The caller owns the returned pointer and must free it with `drevo_free_string`.
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
/// [`drevo_last_error`]).
///
/// # Safety
/// `path` must be a valid NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn drevo_open(path: *const c_char) -> *mut DrevoHandle {
    ffi_guard_ptr!("drevo_open", {
        clear_error();
        let Some(p) = read_c_str(path, "path") else {
            return ptr::null_mut();
        };
        match Drevo::open(Path::new(p)) {
            Ok(db) => Box::into_raw(Box::new(db)),
            Err(e) => {
                set_error(format!("{e}"));
                ptr::null_mut()
            }
        }
    })
}

/// Open an in-memory (ephemeral) database.
///
/// Returns an opaque handle on success, or `NULL` on failure.
///
/// # Safety
/// No preconditions — this function is always safe to call.
#[no_mangle]
pub unsafe extern "C" fn drevo_open_in_memory() -> *mut DrevoHandle {
    ffi_guard_ptr!("drevo_open_in_memory", {
        clear_error();
        match Drevo::open_in_memory() {
            Ok(db) => Box::into_raw(Box::new(db)),
            Err(e) => {
                set_error(format!("{e}"));
                ptr::null_mut()
            }
        }
    })
}

/// Close the database and free the handle.
///
/// Returns 0 on success, -1 on failure. Passing `NULL` is an error.
///
/// # Safety
/// `db` must be a valid handle returned by `drevo_open*`, or `NULL`.
/// After this call the handle is invalid and must not be used.
/// Calling this function twice on the same pointer is undefined behavior
/// (see the module-level "Lifecycle and double-free" docs).
#[no_mangle]
pub unsafe extern "C" fn drevo_close(db: *mut DrevoHandle) -> i32 {
    ffi_guard_int!("drevo_close", {
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
    })
}

// ---------------------------------------------------------------------------
// Error retrieval
// ---------------------------------------------------------------------------

/// Return the last error message as a C string, or `NULL` if no error.
///
/// The caller owns the returned pointer and must free it with
/// [`drevo_free_string`].
///
/// # Safety
/// No preconditions — this function is always safe to call.
#[no_mangle]
pub unsafe extern "C" fn drevo_last_error() -> *mut c_char {
    ffi_guard_ptr!("drevo_last_error", {
        // Clone the message OUT of the RefCell before calling `to_c_string`,
        // because `to_c_string` may itself call `set_error` (on an interior
        // NUL byte) — which takes a `borrow_mut()` on the same cell. A
        // nested `borrow_mut()` over a live `borrow()` would panic with
        // `BorrowMutError`. Cloning releases the borrow first.
        let msg: Option<String> = LAST_ERROR.with(|e| e.borrow().clone());
        match msg {
            Some(msg) => to_c_string(&msg),
            None => ptr::null_mut(),
        }
    })
}

/// Free a C string previously returned by any `drevo_*` function.
///
/// Passing `NULL` is safe (no-op).
///
/// # Safety
/// `s` must be a pointer returned by a `drevo_*` function, or `NULL`.
#[no_mangle]
pub unsafe extern "C" fn drevo_free_string(s: *mut c_char) {
    // No panic guard: this entry point only touches `CString::from_raw`
    // and a null check. A panic here would itself be a Rust bug, not a
    // graph-engine condition. The function returns `()` so it has no
    // sentinel to fall back to anyway.
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
pub unsafe extern "C" fn drevo_create_node(
    db: *mut DrevoHandle,
    kind: *const c_char,
    title: *const c_char,
    body: *const c_char,
    body_html: *const c_char,
    properties_json: *const c_char,
) -> *mut c_char {
    ffi_guard_ptr!("drevo_create_node", {
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
    })
}

/// Get a node by ID. Returns JSON C string, or `NULL` if not found / error.
///
/// # Safety
/// `db` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn drevo_get_node(db: *mut DrevoHandle, id: u64) -> *mut c_char {
    ffi_guard_ptr!("drevo_get_node", {
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
    })
}

/// Update a node by ID with a JSON patch. Returns updated node as JSON, or `NULL`.
///
/// The patch JSON may contain any subset of: `title`, `kind`, `body`, `body_html`, `properties`.
///
/// # Safety
/// `db` must be a valid handle. `patch_json` must be valid JSON.
#[no_mangle]
pub unsafe extern "C" fn drevo_update_node(
    db: *mut DrevoHandle,
    id: u64,
    patch_json: *const c_char,
) -> *mut c_char {
    ffi_guard_ptr!("drevo_update_node", {
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
    })
}

/// Delete a node by ID. Returns 0 on success, -1 on error.
///
/// # Safety
/// `db` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn drevo_delete_node(db: *mut DrevoHandle, id: u64) -> i32 {
    ffi_guard_int!("drevo_delete_node", {
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
    })
}

// ---------------------------------------------------------------------------
// Edge CRUD
// ---------------------------------------------------------------------------

/// Create a new edge. Returns the created edge as JSON, or `NULL` on error.
///
/// # Safety
/// `db` must be a valid handle. String parameters must be valid NUL-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn drevo_create_edge(
    db: *mut DrevoHandle,
    from_id: u64,
    to_id: u64,
    kind: *const c_char,
    weight: f32,
    properties_json: *const c_char,
) -> *mut c_char {
    ffi_guard_ptr!("drevo_create_edge", {
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
    })
}

/// Get an edge by ID. Returns JSON, or `NULL` if not found / error.
///
/// # Safety
/// `db` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn drevo_get_edge(db: *mut DrevoHandle, id: u64) -> *mut c_char {
    ffi_guard_ptr!("drevo_get_edge", {
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
    })
}

/// Update an edge by ID with a JSON patch. Returns updated edge as JSON, or `NULL`.
///
/// The patch JSON may contain: `kind`, `weight`, `properties`.
///
/// # Safety
/// `db` must be a valid handle. `patch_json` must be valid JSON.
#[no_mangle]
pub unsafe extern "C" fn drevo_update_edge(
    db: *mut DrevoHandle,
    id: u64,
    patch_json: *const c_char,
) -> *mut c_char {
    ffi_guard_ptr!("drevo_update_edge", {
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
    })
}

/// Delete an edge by ID. Returns 0 on success, -1 on error.
///
/// # Safety
/// `db` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn drevo_delete_edge(db: *mut DrevoHandle, id: u64) -> i32 {
    ffi_guard_int!("drevo_delete_edge", {
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
    })
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
pub unsafe extern "C" fn drevo_neighbors(
    db: *mut DrevoHandle,
    node_id: u64,
    direction: i32,
    edge_kind: *const c_char,
) -> *mut c_char {
    ffi_guard_ptr!("drevo_neighbors", {
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
    })
}

/// BFS traversal from a start node. Returns JSON array of node IDs, or `NULL`.
///
/// `direction`: 0 = Outgoing, 1 = Incoming, 2 = Both.
/// `edge_kind`: optional edge kind filter (`NULL` for no filter).
///
/// # Safety
/// `db` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn drevo_bfs(
    db: *mut DrevoHandle,
    start_id: u64,
    max_depth: u8,
    direction: i32,
    edge_kind: *const c_char,
) -> *mut c_char {
    ffi_guard_ptr!("drevo_bfs", {
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
    })
}

/// DFS traversal from a start node. Returns JSON array of node IDs, or `NULL`.
///
/// `direction`: 0 = Outgoing, 1 = Incoming, 2 = Both.
/// `edge_kind`: optional edge kind filter (`NULL` for no filter).
///
/// # Safety
/// `db` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn drevo_dfs(
    db: *mut DrevoHandle,
    start_id: u64,
    max_depth: u8,
    direction: i32,
    edge_kind: *const c_char,
) -> *mut c_char {
    ffi_guard_ptr!("drevo_dfs", {
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
    })
}

/// Find the shortest path between two nodes. Returns JSON array of node IDs, or `NULL`.
///
/// Returns `NULL` with error "no path found" if no path exists.
///
/// # Safety
/// `db` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn drevo_shortest_path(
    db: *mut DrevoHandle,
    from_id: u64,
    to_id: u64,
) -> *mut c_char {
    ffi_guard_ptr!("drevo_shortest_path", {
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
    })
}

/// Extract a subgraph around a root node. Returns JSON with `nodes` and `edges` arrays, or `NULL`.
///
/// # Safety
/// `db` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn drevo_subgraph(
    db: *mut DrevoHandle,
    root_id: u64,
    depth: u8,
) -> *mut c_char {
    ffi_guard_ptr!("drevo_subgraph", {
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
    })
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

/// Full-text search. Returns JSON array of `{node, score}` objects, or `NULL`.
///
/// # Safety
/// `db` must be a valid handle. `query` must be valid NUL-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn drevo_search_fts(
    db: *mut DrevoHandle,
    query: *const c_char,
    limit: u64,
) -> *mut c_char {
    ffi_guard_ptr!("drevo_search_fts", {
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
    })
}

/// List nodes by kind with pagination. Returns JSON array of nodes, or `NULL`.
///
/// # Safety
/// `db` must be a valid handle. `kind` must be valid NUL-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn drevo_list_nodes_by_kind(
    db: *mut DrevoHandle,
    kind: *const c_char,
    limit: u64,
    offset: u64,
) -> *mut c_char {
    ffi_guard_ptr!("drevo_list_nodes_by_kind", {
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
    })
}

/// List recently updated nodes. Returns JSON array of nodes, or `NULL`.
///
/// # Safety
/// `db` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn drevo_list_recent(db: *mut DrevoHandle, limit: u64) -> *mut c_char {
    ffi_guard_ptr!("drevo_list_recent", {
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
    })
}

// ---------------------------------------------------------------------------
// Unit tests for panic-guard infrastructure (task 00110)
// ---------------------------------------------------------------------------
//
// These tests live inside the module so they can exercise the private
// `ffi_guard_ptr!` / `ffi_guard_int!` macros directly without exposing a
// panic-injection function on the public C ABI. Integration tests in
// `tests/ffi_tests.rs` cover the externally-visible behaviour.

#[cfg(test)]
mod panic_guard_tests {
    use super::*;
    use std::sync::Mutex;

    /// Global lock for tests that mutate the panic hook — otherwise parallel
    /// tests trample each other's hooks and noisy stack traces leak to stderr.
    static PANIC_HOOK_LOCK: Mutex<()> = Mutex::new(());

    fn with_silenced_panics<R>(f: impl FnOnce() -> R) -> R {
        let _g = PANIC_HOOK_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = f();
        std::panic::set_hook(prev);
        result
    }

    #[test]
    fn ffi_guard_ptr_catches_panic_returns_null_and_sets_error() {
        let result: *mut c_char = with_silenced_panics(|| {
            ffi_guard_ptr!("synthetic_ptr_fn", {
                panic!("intentional test panic — ptr variant");
            })
        });
        assert!(result.is_null(), "panic guard must return null");
        let msg = LAST_ERROR.with(|e| e.borrow().clone());
        let msg = msg.expect("LAST_ERROR must be set after a caught panic");
        assert!(
            msg.contains("synthetic_ptr_fn"),
            "error message must name the function: {msg}"
        );
        assert!(
            msg.contains("panic"),
            "error message must mention panic: {msg}"
        );
    }

    #[test]
    fn ffi_guard_int_catches_panic_returns_minus_one_and_sets_error() {
        let rc: i32 = with_silenced_panics(|| {
            ffi_guard_int!("synthetic_int_fn", {
                panic!("intentional test panic — int variant");
            })
        });
        assert_eq!(rc, -1, "panic guard must return -1");
        let msg = LAST_ERROR.with(|e| e.borrow().clone());
        let msg = msg.expect("LAST_ERROR must be set after a caught panic");
        assert!(msg.contains("synthetic_int_fn"), "msg: {msg}");
    }

    #[test]
    fn ffi_guard_ptr_passes_through_normal_return() {
        // Pre-set a value so we can prove the guard does not clobber it
        // when the body returns normally.
        let s = CString::new("ok").unwrap();
        let returned: *mut c_char = ffi_guard_ptr!("normal_return", { s.into_raw() });
        assert!(!returned.is_null());
        unsafe { drop(CString::from_raw(returned)) };
    }

    #[test]
    fn ffi_guard_int_passes_through_normal_return() {
        let rc: i32 = ffi_guard_int!("normal_return_int", { 0 });
        assert_eq!(rc, 0);
    }

    #[test]
    fn drevo_last_error_no_recursive_borrow_panic_on_nul_in_message() {
        // Pre-condition: LAST_ERROR contains a message that holds an
        // interior NUL byte. With the old implementation (`borrow()` held
        // across the `to_c_string` call) this would either:
        //   (a) trip a `BorrowMutError` panic from `set_error` re-borrowing
        //       a live cell, or
        //   (b) be caught by the panic guard but still leave the cell in
        //       an unhelpful state.
        // Both are visible as "did not return null cleanly".
        clear_error();
        set_error("contains\0nul byte".to_string());
        let p = unsafe { drevo_last_error() };
        assert!(
            p.is_null(),
            "expected NULL because the cached message has an interior NUL"
        );
        // The error slot should now describe the NUL-byte failure, not be
        // poisoned by a panic.
        let follow_up = LAST_ERROR.with(|e| e.borrow().clone());
        assert!(
            follow_up
                .as_deref()
                .is_some_and(|s| s.contains("interior NUL")),
            "after the NUL-byte failure, LAST_ERROR should hold the new explanation; got {follow_up:?}"
        );
    }

    #[test]
    fn thread_local_error_is_per_thread() {
        // Set an error on this thread.
        set_error("main-thread error".to_string());

        // A spawned thread starts with an empty error slot and can set
        // its own without disturbing the main thread.
        let join = std::thread::spawn(|| {
            let initial = LAST_ERROR.with(|e| e.borrow().clone());
            assert!(initial.is_none(), "child thread starts with no error");
            set_error("child-thread error".to_string());
            LAST_ERROR.with(|e| e.borrow().clone())
        });
        let child_value = join.join().expect("child thread panicked");
        assert_eq!(child_value.as_deref(), Some("child-thread error"));

        // Main thread's error survived intact.
        let main_value = LAST_ERROR.with(|e| e.borrow().clone());
        assert_eq!(main_value.as_deref(), Some("main-thread error"));
        clear_error();
    }
}
