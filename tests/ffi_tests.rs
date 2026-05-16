//! Tests for C FFI bindings.
//!
//! These tests exercise the `extern "C"` functions from `src/ffi.rs`
//! to verify correctness of the FFI layer: opaque handle lifecycle,
//! JSON serialization across the boundary, string ownership, and
//! error propagation via `drevo_last_error`.

use drevo::ffi::*;
use std::ffi::{CStr, CString};
use std::ptr;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read a C string returned by an FFI function and convert to owned String.
/// Frees the C string after reading.
unsafe fn read_and_free(ptr: *mut std::os::raw::c_char) -> String {
    assert!(!ptr.is_null(), "FFI returned null string");
    let s = CStr::from_ptr(ptr).to_string_lossy().into_owned();
    drevo_free_string(ptr);
    s
}

/// Read the last error string and free it.
unsafe fn last_error() -> Option<String> {
    let ptr = drevo_last_error();
    if ptr.is_null() {
        None
    } else {
        Some(read_and_free(ptr))
    }
}

/// Open an in-memory database, panicking on failure.
unsafe fn open_memory_db() -> *mut DrevoHandle {
    let db = drevo_open_in_memory();
    assert!(
        !db.is_null(),
        "Failed to open in-memory DB: {:?}",
        last_error()
    );
    db
}

// ---------------------------------------------------------------------------
// Lifecycle tests
// ---------------------------------------------------------------------------

#[test]
fn test_open_in_memory_and_close() {
    unsafe {
        let db = open_memory_db();
        let rc = drevo_close(db);
        assert_eq!(rc, 0);
    }
}

#[test]
fn test_open_disk_and_close() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let c_path = CString::new(path.to_str().unwrap()).unwrap();

    unsafe {
        let db = drevo_open(c_path.as_ptr());
        assert!(!db.is_null(), "Failed to open disk DB: {:?}", last_error());
        let rc = drevo_close(db);
        assert_eq!(rc, 0);
    }
}

#[test]
fn test_close_null_returns_error() {
    unsafe {
        let rc = drevo_close(ptr::null_mut());
        assert_ne!(rc, 0);
    }
}

// ---------------------------------------------------------------------------
// Node CRUD
// ---------------------------------------------------------------------------

#[test]
fn test_create_and_get_node() {
    unsafe {
        let db = open_memory_db();
        let kind = CString::new("note").unwrap();
        let title = CString::new("Hello FFI").unwrap();
        let body = CString::new("Body text").unwrap();
        let body_html = CString::new("<p>Body text</p>").unwrap();
        let props = CString::new("{}").unwrap();

        let json = drevo_create_node(
            db,
            kind.as_ptr(),
            title.as_ptr(),
            body.as_ptr(),
            body_html.as_ptr(),
            props.as_ptr(),
        );
        assert!(!json.is_null(), "create_node failed: {:?}", last_error());
        let node_json = read_and_free(json);
        let node: serde_json::Value = serde_json::from_str(&node_json).unwrap();
        assert_eq!(node["title"], "Hello FFI");
        assert_eq!(node["kind"], "note");
        let node_id = node["id"].as_u64().unwrap();

        // get_node
        let json2 = drevo_get_node(db, node_id);
        assert!(!json2.is_null());
        let get_json = read_and_free(json2);
        let got: serde_json::Value = serde_json::from_str(&get_json).unwrap();
        assert_eq!(got["id"], node_id);
        assert_eq!(got["title"], "Hello FFI");

        drevo_close(db);
    }
}

#[test]
fn test_get_node_not_found_returns_null() {
    unsafe {
        let db = open_memory_db();
        let json = drevo_get_node(db, 9999);
        assert!(json.is_null());
        // last_error should be set
        let err = last_error();
        assert!(err.is_some());
        drevo_close(db);
    }
}

#[test]
fn test_update_node() {
    unsafe {
        let db = open_memory_db();
        let kind = CString::new("note").unwrap();
        let title = CString::new("Original").unwrap();
        let body = CString::new("").unwrap();
        let props = CString::new("{}").unwrap();

        let json = drevo_create_node(
            db,
            kind.as_ptr(),
            title.as_ptr(),
            body.as_ptr(),
            body.as_ptr(),
            props.as_ptr(),
        );
        let node_json = read_and_free(json);
        let node: serde_json::Value = serde_json::from_str(&node_json).unwrap();
        let node_id = node["id"].as_u64().unwrap();

        let patch = CString::new(r#"{"title": "Updated"}"#).unwrap();
        let updated = drevo_update_node(db, node_id, patch.as_ptr());
        assert!(!updated.is_null(), "update_node failed: {:?}", last_error());
        let updated_json = read_and_free(updated);
        let upd: serde_json::Value = serde_json::from_str(&updated_json).unwrap();
        assert_eq!(upd["title"], "Updated");

        drevo_close(db);
    }
}

#[test]
fn test_delete_node() {
    unsafe {
        let db = open_memory_db();
        let kind = CString::new("note").unwrap();
        let title = CString::new("To Delete").unwrap();
        let body = CString::new("").unwrap();
        let props = CString::new("{}").unwrap();

        let json = drevo_create_node(
            db,
            kind.as_ptr(),
            title.as_ptr(),
            body.as_ptr(),
            body.as_ptr(),
            props.as_ptr(),
        );
        let node_json = read_and_free(json);
        let node: serde_json::Value = serde_json::from_str(&node_json).unwrap();
        let node_id = node["id"].as_u64().unwrap();

        let rc = drevo_delete_node(db, node_id);
        assert_eq!(rc, 0);

        // Should no longer exist
        let json2 = drevo_get_node(db, node_id);
        assert!(json2.is_null());

        drevo_close(db);
    }
}

// ---------------------------------------------------------------------------
// Edge CRUD
// ---------------------------------------------------------------------------

#[test]
fn test_create_and_get_edge() {
    unsafe {
        let db = open_memory_db();
        // Create two nodes first
        let kind = CString::new("note").unwrap();
        let t1 = CString::new("Node A").unwrap();
        let t2 = CString::new("Node B").unwrap();
        let body = CString::new("").unwrap();
        let props = CString::new("{}").unwrap();

        let j1 = drevo_create_node(
            db,
            kind.as_ptr(),
            t1.as_ptr(),
            body.as_ptr(),
            body.as_ptr(),
            props.as_ptr(),
        );
        let n1: serde_json::Value = serde_json::from_str(&read_and_free(j1)).unwrap();
        let id1 = n1["id"].as_u64().unwrap();

        let j2 = drevo_create_node(
            db,
            kind.as_ptr(),
            t2.as_ptr(),
            body.as_ptr(),
            body.as_ptr(),
            props.as_ptr(),
        );
        let n2: serde_json::Value = serde_json::from_str(&read_and_free(j2)).unwrap();
        let id2 = n2["id"].as_u64().unwrap();

        // Create edge
        let edge_kind = CString::new("links_to").unwrap();
        let edge_json = drevo_create_edge(db, id1, id2, edge_kind.as_ptr(), 1.5, props.as_ptr());
        assert!(
            !edge_json.is_null(),
            "create_edge failed: {:?}",
            last_error()
        );
        let edge_str = read_and_free(edge_json);
        let edge: serde_json::Value = serde_json::from_str(&edge_str).unwrap();
        assert_eq!(edge["from_id"], id1);
        assert_eq!(edge["to_id"], id2);
        assert_eq!(edge["kind"], "links_to");
        let edge_id = edge["id"].as_u64().unwrap();

        // Get edge
        let got = drevo_get_edge(db, edge_id);
        assert!(!got.is_null());
        let got_str = read_and_free(got);
        let got_edge: serde_json::Value = serde_json::from_str(&got_str).unwrap();
        assert_eq!(got_edge["id"], edge_id);

        drevo_close(db);
    }
}

#[test]
fn test_delete_edge() {
    unsafe {
        let db = open_memory_db();
        let kind = CString::new("note").unwrap();
        let t1 = CString::new("A").unwrap();
        let t2 = CString::new("B").unwrap();
        let body = CString::new("").unwrap();
        let props = CString::new("{}").unwrap();

        let j1 = drevo_create_node(
            db,
            kind.as_ptr(),
            t1.as_ptr(),
            body.as_ptr(),
            body.as_ptr(),
            props.as_ptr(),
        );
        let n1: serde_json::Value = serde_json::from_str(&read_and_free(j1)).unwrap();
        let j2 = drevo_create_node(
            db,
            kind.as_ptr(),
            t2.as_ptr(),
            body.as_ptr(),
            body.as_ptr(),
            props.as_ptr(),
        );
        let n2: serde_json::Value = serde_json::from_str(&read_and_free(j2)).unwrap();

        let ek = CString::new("links_to").unwrap();
        let ej = drevo_create_edge(
            db,
            n1["id"].as_u64().unwrap(),
            n2["id"].as_u64().unwrap(),
            ek.as_ptr(),
            1.0,
            props.as_ptr(),
        );
        let edge: serde_json::Value = serde_json::from_str(&read_and_free(ej)).unwrap();
        let edge_id = edge["id"].as_u64().unwrap();

        let rc = drevo_delete_edge(db, edge_id);
        assert_eq!(rc, 0);

        // Should not exist
        let got = drevo_get_edge(db, edge_id);
        assert!(got.is_null());

        drevo_close(db);
    }
}

// ---------------------------------------------------------------------------
// Traversal
// ---------------------------------------------------------------------------

#[test]
fn test_neighbors() {
    unsafe {
        let db = open_memory_db();
        let kind = CString::new("note").unwrap();
        let body = CString::new("").unwrap();
        let props = CString::new("{}").unwrap();

        // Create 3 nodes: A -> B, A -> C
        let ta = CString::new("A").unwrap();
        let tb = CString::new("B").unwrap();
        let tc = CString::new("C").unwrap();

        let ja = drevo_create_node(
            db,
            kind.as_ptr(),
            ta.as_ptr(),
            body.as_ptr(),
            body.as_ptr(),
            props.as_ptr(),
        );
        let a: serde_json::Value = serde_json::from_str(&read_and_free(ja)).unwrap();
        let jb = drevo_create_node(
            db,
            kind.as_ptr(),
            tb.as_ptr(),
            body.as_ptr(),
            body.as_ptr(),
            props.as_ptr(),
        );
        let b: serde_json::Value = serde_json::from_str(&read_and_free(jb)).unwrap();
        let jc = drevo_create_node(
            db,
            kind.as_ptr(),
            tc.as_ptr(),
            body.as_ptr(),
            body.as_ptr(),
            props.as_ptr(),
        );
        let c: serde_json::Value = serde_json::from_str(&read_and_free(jc)).unwrap();

        let ek = CString::new("links_to").unwrap();
        let _ = read_and_free(drevo_create_edge(
            db,
            a["id"].as_u64().unwrap(),
            b["id"].as_u64().unwrap(),
            ek.as_ptr(),
            1.0,
            props.as_ptr(),
        ));
        let _ = read_and_free(drevo_create_edge(
            db,
            a["id"].as_u64().unwrap(),
            c["id"].as_u64().unwrap(),
            ek.as_ptr(),
            1.0,
            props.as_ptr(),
        ));

        // Outgoing neighbors of A (direction=0 Outgoing)
        let result = drevo_neighbors(db, a["id"].as_u64().unwrap(), 0, ptr::null());
        assert!(!result.is_null(), "neighbors failed: {:?}", last_error());
        let result_str = read_and_free(result);
        let nodes: Vec<serde_json::Value> = serde_json::from_str(&result_str).unwrap();
        assert_eq!(nodes.len(), 2);

        drevo_close(db);
    }
}

#[test]
fn test_bfs() {
    unsafe {
        let db = open_memory_db();
        let kind = CString::new("note").unwrap();
        let body = CString::new("").unwrap();
        let props = CString::new("{}").unwrap();

        let ta = CString::new("BFS-A").unwrap();
        let tb = CString::new("BFS-B").unwrap();

        let ja = drevo_create_node(
            db,
            kind.as_ptr(),
            ta.as_ptr(),
            body.as_ptr(),
            body.as_ptr(),
            props.as_ptr(),
        );
        let a: serde_json::Value = serde_json::from_str(&read_and_free(ja)).unwrap();
        let jb = drevo_create_node(
            db,
            kind.as_ptr(),
            tb.as_ptr(),
            body.as_ptr(),
            body.as_ptr(),
            props.as_ptr(),
        );
        let b: serde_json::Value = serde_json::from_str(&read_and_free(jb)).unwrap();

        let ek = CString::new("links_to").unwrap();
        let _ = read_and_free(drevo_create_edge(
            db,
            a["id"].as_u64().unwrap(),
            b["id"].as_u64().unwrap(),
            ek.as_ptr(),
            1.0,
            props.as_ptr(),
        ));

        // BFS from A, depth 1, direction Outgoing (0)
        let result = drevo_bfs(db, a["id"].as_u64().unwrap(), 1, 0, ptr::null());
        assert!(!result.is_null(), "bfs failed: {:?}", last_error());
        let bfs_str = read_and_free(result);
        let nodes: Vec<serde_json::Value> = serde_json::from_str(&bfs_str).unwrap();
        let node_ids: Vec<u64> = nodes.iter().map(|n| n["id"].as_u64().unwrap()).collect();
        assert!(node_ids.contains(&b["id"].as_u64().unwrap()));

        drevo_close(db);
    }
}

#[test]
fn test_shortest_path() {
    unsafe {
        let db = open_memory_db();
        let kind = CString::new("note").unwrap();
        let body = CString::new("").unwrap();
        let props = CString::new("{}").unwrap();

        let ta = CString::new("SP-A").unwrap();
        let tb = CString::new("SP-B").unwrap();
        let tc = CString::new("SP-C").unwrap();

        let ja = drevo_create_node(
            db,
            kind.as_ptr(),
            ta.as_ptr(),
            body.as_ptr(),
            body.as_ptr(),
            props.as_ptr(),
        );
        let a: serde_json::Value = serde_json::from_str(&read_and_free(ja)).unwrap();
        let jb = drevo_create_node(
            db,
            kind.as_ptr(),
            tb.as_ptr(),
            body.as_ptr(),
            body.as_ptr(),
            props.as_ptr(),
        );
        let b: serde_json::Value = serde_json::from_str(&read_and_free(jb)).unwrap();
        let jc = drevo_create_node(
            db,
            kind.as_ptr(),
            tc.as_ptr(),
            body.as_ptr(),
            body.as_ptr(),
            props.as_ptr(),
        );
        let c: serde_json::Value = serde_json::from_str(&read_and_free(jc)).unwrap();

        let ek = CString::new("links_to").unwrap();
        let _ = read_and_free(drevo_create_edge(
            db,
            a["id"].as_u64().unwrap(),
            b["id"].as_u64().unwrap(),
            ek.as_ptr(),
            1.0,
            props.as_ptr(),
        ));
        let _ = read_and_free(drevo_create_edge(
            db,
            b["id"].as_u64().unwrap(),
            c["id"].as_u64().unwrap(),
            ek.as_ptr(),
            1.0,
            props.as_ptr(),
        ));

        let result = drevo_shortest_path(db, a["id"].as_u64().unwrap(), c["id"].as_u64().unwrap());
        assert!(
            !result.is_null(),
            "shortest_path failed: {:?}",
            last_error()
        );
        let sp_str = read_and_free(result);
        let path: Vec<u64> = serde_json::from_str(&sp_str).unwrap();
        assert_eq!(path.len(), 3);
        assert_eq!(path[0], a["id"].as_u64().unwrap());
        assert_eq!(path[2], c["id"].as_u64().unwrap());

        drevo_close(db);
    }
}

#[test]
fn test_subgraph() {
    unsafe {
        let db = open_memory_db();
        let kind = CString::new("note").unwrap();
        let body = CString::new("").unwrap();
        let props = CString::new("{}").unwrap();

        let ta = CString::new("SG-A").unwrap();
        let tb = CString::new("SG-B").unwrap();

        let ja = drevo_create_node(
            db,
            kind.as_ptr(),
            ta.as_ptr(),
            body.as_ptr(),
            body.as_ptr(),
            props.as_ptr(),
        );
        let a: serde_json::Value = serde_json::from_str(&read_and_free(ja)).unwrap();
        let jb = drevo_create_node(
            db,
            kind.as_ptr(),
            tb.as_ptr(),
            body.as_ptr(),
            body.as_ptr(),
            props.as_ptr(),
        );
        let b: serde_json::Value = serde_json::from_str(&read_and_free(jb)).unwrap();

        let ek = CString::new("links_to").unwrap();
        let _ = read_and_free(drevo_create_edge(
            db,
            a["id"].as_u64().unwrap(),
            b["id"].as_u64().unwrap(),
            ek.as_ptr(),
            1.0,
            props.as_ptr(),
        ));

        let result = drevo_subgraph(db, a["id"].as_u64().unwrap(), 1);
        assert!(!result.is_null(), "subgraph failed: {:?}", last_error());
        let sg_str = read_and_free(result);
        let sg: serde_json::Value = serde_json::from_str(&sg_str).unwrap();
        assert!(sg["nodes"].as_array().unwrap().len() >= 2);
        assert!(!sg["edges"].as_array().unwrap().is_empty());

        drevo_close(db);
    }
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

#[test]
fn test_search_fts() {
    unsafe {
        let db = open_memory_db();
        let kind = CString::new("note").unwrap();
        let body = CString::new("The quick brown fox jumps over the lazy dog").unwrap();
        let props = CString::new("{}").unwrap();
        let title = CString::new("Fox Story").unwrap();
        let body_html = CString::new("").unwrap();

        let _ = read_and_free(drevo_create_node(
            db,
            kind.as_ptr(),
            title.as_ptr(),
            body.as_ptr(),
            body_html.as_ptr(),
            props.as_ptr(),
        ));

        let query = CString::new("quick brown fox jumps").unwrap();
        let result = drevo_search_fts(db, query.as_ptr(), 10);
        assert!(!result.is_null(), "search_fts failed: {:?}", last_error());
        let result_str = read_and_free(result);
        let results: Vec<serde_json::Value> = serde_json::from_str(&result_str).unwrap();
        assert!(!results.is_empty(), "FTS returned no results for query");
        assert_eq!(results[0]["node"]["title"], "Fox Story");

        drevo_close(db);
    }
}

// ---------------------------------------------------------------------------
// List operations
// ---------------------------------------------------------------------------

#[test]
fn test_list_nodes_by_kind() {
    unsafe {
        let db = open_memory_db();
        let kind = CString::new("task").unwrap();
        let body = CString::new("").unwrap();
        let props = CString::new("{}").unwrap();

        for i in 0..3 {
            let t = CString::new(format!("Task {}", i)).unwrap();
            let j = drevo_create_node(
                db,
                kind.as_ptr(),
                t.as_ptr(),
                body.as_ptr(),
                body.as_ptr(),
                props.as_ptr(),
            );
            let _ = read_and_free(j);
        }

        let k = CString::new("task").unwrap();
        let result = drevo_list_nodes_by_kind(db, k.as_ptr(), 10, 0);
        assert!(!result.is_null());
        let result_str = read_and_free(result);
        let nodes: Vec<serde_json::Value> = serde_json::from_str(&result_str).unwrap();
        assert_eq!(nodes.len(), 3);

        drevo_close(db);
    }
}

#[test]
fn test_list_recent() {
    unsafe {
        let db = open_memory_db();
        let kind = CString::new("note").unwrap();
        let body = CString::new("").unwrap();
        let props = CString::new("{}").unwrap();

        for i in 0..5 {
            let t = CString::new(format!("Recent {}", i)).unwrap();
            let j = drevo_create_node(
                db,
                kind.as_ptr(),
                t.as_ptr(),
                body.as_ptr(),
                body.as_ptr(),
                props.as_ptr(),
            );
            let _ = read_and_free(j);
        }

        let result = drevo_list_recent(db, 3);
        assert!(!result.is_null());
        let result_str = read_and_free(result);
        let nodes: Vec<serde_json::Value> = serde_json::from_str(&result_str).unwrap();
        assert_eq!(nodes.len(), 3);

        drevo_close(db);
    }
}

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

#[test]
fn test_last_error_cleared_on_success() {
    unsafe {
        let db = open_memory_db();
        // Trigger an error
        let _ = drevo_get_node(db, 9999);
        assert!(last_error().is_some());

        // Successful operation should clear the error
        let kind = CString::new("note").unwrap();
        let title = CString::new("OK").unwrap();
        let body = CString::new("").unwrap();
        let props = CString::new("{}").unwrap();
        let j = drevo_create_node(
            db,
            kind.as_ptr(),
            title.as_ptr(),
            body.as_ptr(),
            body.as_ptr(),
            props.as_ptr(),
        );
        let _ = read_and_free(j);

        let err = drevo_last_error();
        assert!(err.is_null(), "Expected no error after success");

        drevo_close(db);
    }
}

#[test]
fn test_free_null_string_is_safe() {
    unsafe {
        drevo_free_string(ptr::null_mut()); // should not crash
    }
}

// ---------------------------------------------------------------------------
// Edge update
// ---------------------------------------------------------------------------

#[test]
fn test_update_edge() {
    unsafe {
        let db = open_memory_db();
        let kind = CString::new("note").unwrap();
        let body = CString::new("").unwrap();
        let props = CString::new("{}").unwrap();

        let t1 = CString::new("EU-A").unwrap();
        let t2 = CString::new("EU-B").unwrap();

        let j1 = drevo_create_node(
            db,
            kind.as_ptr(),
            t1.as_ptr(),
            body.as_ptr(),
            body.as_ptr(),
            props.as_ptr(),
        );
        let n1: serde_json::Value = serde_json::from_str(&read_and_free(j1)).unwrap();
        let j2 = drevo_create_node(
            db,
            kind.as_ptr(),
            t2.as_ptr(),
            body.as_ptr(),
            body.as_ptr(),
            props.as_ptr(),
        );
        let n2: serde_json::Value = serde_json::from_str(&read_and_free(j2)).unwrap();

        let ek = CString::new("links_to").unwrap();
        let ej = drevo_create_edge(
            db,
            n1["id"].as_u64().unwrap(),
            n2["id"].as_u64().unwrap(),
            ek.as_ptr(),
            1.0,
            props.as_ptr(),
        );
        let edge: serde_json::Value = serde_json::from_str(&read_and_free(ej)).unwrap();
        let edge_id = edge["id"].as_u64().unwrap();

        let patch = CString::new(r#"{"weight": 3.5, "kind": "blocks"}"#).unwrap();
        let updated = drevo_update_edge(db, edge_id, patch.as_ptr());
        assert!(!updated.is_null(), "update_edge failed: {:?}", last_error());
        let upd: serde_json::Value = serde_json::from_str(&read_and_free(updated)).unwrap();
        assert_eq!(upd["weight"], 3.5);
        assert_eq!(upd["kind"], "blocks");

        drevo_close(db);
    }
}

// ---------------------------------------------------------------------------
// Neighbors with kind filter
// ---------------------------------------------------------------------------

#[test]
fn test_neighbors_with_kind_filter() {
    unsafe {
        let db = open_memory_db();
        let nk = CString::new("note").unwrap();
        let body = CString::new("").unwrap();
        let props = CString::new("{}").unwrap();

        let ta = CString::new("FK-A").unwrap();
        let tb = CString::new("FK-B").unwrap();
        let tc = CString::new("FK-C").unwrap();

        let ja = drevo_create_node(
            db,
            nk.as_ptr(),
            ta.as_ptr(),
            body.as_ptr(),
            body.as_ptr(),
            props.as_ptr(),
        );
        let a: serde_json::Value = serde_json::from_str(&read_and_free(ja)).unwrap();
        let jb = drevo_create_node(
            db,
            nk.as_ptr(),
            tb.as_ptr(),
            body.as_ptr(),
            body.as_ptr(),
            props.as_ptr(),
        );
        let b: serde_json::Value = serde_json::from_str(&read_and_free(jb)).unwrap();
        let jc = drevo_create_node(
            db,
            nk.as_ptr(),
            tc.as_ptr(),
            body.as_ptr(),
            body.as_ptr(),
            props.as_ptr(),
        );
        let c: serde_json::Value = serde_json::from_str(&read_and_free(jc)).unwrap();

        let ek1 = CString::new("links_to").unwrap();
        let ek2 = CString::new("blocks").unwrap();
        let _ = read_and_free(drevo_create_edge(
            db,
            a["id"].as_u64().unwrap(),
            b["id"].as_u64().unwrap(),
            ek1.as_ptr(),
            1.0,
            props.as_ptr(),
        ));
        let _ = read_and_free(drevo_create_edge(
            db,
            a["id"].as_u64().unwrap(),
            c["id"].as_u64().unwrap(),
            ek2.as_ptr(),
            1.0,
            props.as_ptr(),
        ));

        // Filter by "blocks" — should return only C
        let filter = CString::new("blocks").unwrap();
        let result = drevo_neighbors(db, a["id"].as_u64().unwrap(), 0, filter.as_ptr());
        assert!(!result.is_null());
        let result_str = read_and_free(result);
        let nodes: Vec<serde_json::Value> = serde_json::from_str(&result_str).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0]["title"], "FK-C");

        drevo_close(db);
    }
}

// ---------------------------------------------------------------------------
// Properties via FFI
// ---------------------------------------------------------------------------

#[test]
fn test_create_node_with_properties() {
    unsafe {
        let db = open_memory_db();
        let kind = CString::new("task").unwrap();
        let title = CString::new("Props Test").unwrap();
        let body = CString::new("").unwrap();
        let body_html = CString::new("").unwrap();
        let props = CString::new(r#"{"priority": 1, "tags": ["rust", "ffi"]}"#).unwrap();

        let json = drevo_create_node(
            db,
            kind.as_ptr(),
            title.as_ptr(),
            body.as_ptr(),
            body_html.as_ptr(),
            props.as_ptr(),
        );
        assert!(!json.is_null());
        let node_json = read_and_free(json);
        let node: serde_json::Value = serde_json::from_str(&node_json).unwrap();
        assert_eq!(node["properties"]["priority"], 1);
        assert_eq!(node["properties"]["tags"][0], "rust");
        assert_eq!(node["properties"]["tags"][1], "ffi");

        drevo_close(db);
    }
}
