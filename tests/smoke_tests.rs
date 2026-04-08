//! Platform smoke tests — task 00031.
//!
//! Each test exercises the full workflow: open DB → create node → get node →
//! update node → create edge → search FTS → traversal → close.
//!
//! Three smoke-test variants:
//! 1. `MemoryBackend` (all platforms, including WASM)
//! 2. `RedbBackend` (native only — iOS, Android, desktop)
//! 3. FFI surface (native only — C API roundtrip)
//!
//! These tests are designed to run on every CI target:
//! - Host (Linux/macOS): all three variants
//! - WASM: memory variant only (via wasm target compilation check)
//! - iOS/Android: compiled as part of the static library

use graphnote_db::db::GraphNoteDb;
use graphnote_db::model::{Direction, NewEdge, NewNode, NodePatch, Properties};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn props(pairs: &[(&str, serde_json::Value)]) -> Properties {
    let mut map = HashMap::new();
    for (k, v) in pairs {
        map.insert(k.to_string(), v.clone());
    }
    Properties(map)
}

/// The core smoke test workflow exercised against a GraphNoteDb instance.
/// Returns Ok(()) if all steps pass.
fn run_smoke_workflow(db: &GraphNoteDb) -> Result<(), String> {
    // --- Step 1: Create nodes ---
    let node_a = db
        .create_node(NewNode {
            kind: "note".to_string(),
            title: "Smoke Test Alpha".to_string(),
            body: "This is the first smoke test node for platform verification".to_string(),
            body_html: "<p>This is the first smoke test node</p>".to_string(),
            properties: props(&[("priority", serde_json::json!(1))]),
        })
        .map_err(|e| format!("create_node A: {e}"))?;

    let node_b = db
        .create_node(NewNode {
            kind: "tag".to_string(),
            title: "Smoke Tag Beta".to_string(),
            body: "Tag node for smoke testing edges and traversal".to_string(),
            body_html: "<p>Tag node</p>".to_string(),
            properties: Properties::default(),
        })
        .map_err(|e| format!("create_node B: {e}"))?;

    let node_c = db
        .create_node(NewNode {
            kind: "note".to_string(),
            title: "Smoke Test Gamma".to_string(),
            body: "Third node for search and traversal verification".to_string(),
            body_html: "<p>Third node</p>".to_string(),
            properties: props(&[("priority", serde_json::json!(3))]),
        })
        .map_err(|e| format!("create_node C: {e}"))?;

    // --- Step 2: Read nodes back ---
    let fetched_a = db
        .get_node(node_a.id)
        .map_err(|e| format!("get_node by id: {e}"))?
        .ok_or("get_node by id returned None")?;
    if fetched_a.title != "Smoke Test Alpha" {
        return Err(format!("title mismatch: {}", fetched_a.title));
    }

    let fetched_by_uuid = db
        .get_node_by_uuid(&node_a.uuid)
        .map_err(|e| format!("get_node_by_uuid: {e}"))?
        .ok_or("get_node_by_uuid returned None")?;
    if fetched_by_uuid.id != node_a.id {
        return Err("uuid lookup id mismatch".to_string());
    }

    let fetched_by_title = db
        .get_node_by_title("Smoke Tag Beta")
        .map_err(|e| format!("get_node_by_title: {e}"))?
        .ok_or("get_node_by_title returned None")?;
    if fetched_by_title.id != node_b.id {
        return Err("title lookup id mismatch".to_string());
    }

    // --- Step 3: Update a node ---
    let updated = db
        .update_node(
            node_a.id,
            NodePatch {
                kind: None,
                title: Some("Smoke Test Alpha Updated".to_string()),
                body: Some("Updated body for platform smoke verification testing".to_string()),
                body_html: None,
                properties: None,
            },
        )
        .map_err(|e| format!("update_node: {e}"))?;
    if updated.title != "Smoke Test Alpha Updated" {
        return Err(format!("update title mismatch: {}", updated.title));
    }
    if updated.body != "Updated body for platform smoke verification testing" {
        return Err("update body mismatch".to_string());
    }

    // --- Step 4: Create edges ---
    let edge_ab = db
        .create_edge(NewEdge {
            from_id: node_a.id,
            to_id: node_b.id,
            kind: "tagged_with".to_string(),
            weight: 1.0,
            properties: Properties::default(),
        })
        .map_err(|e| format!("create_edge A→B: {e}"))?;

    let _edge_ac = db
        .create_edge(NewEdge {
            from_id: node_a.id,
            to_id: node_c.id,
            kind: "links_to".to_string(),
            weight: 0.5,
            properties: Properties::default(),
        })
        .map_err(|e| format!("create_edge A→C: {e}"))?;

    let _edge_bc = db
        .create_edge(NewEdge {
            from_id: node_b.id,
            to_id: node_c.id,
            kind: "links_to".to_string(),
            weight: 1.0,
            properties: Properties::default(),
        })
        .map_err(|e| format!("create_edge B→C: {e}"))?;

    // --- Step 5: Read edge ---
    let fetched_edge = db
        .get_edge(edge_ab.id)
        .map_err(|e| format!("get_edge: {e}"))?
        .ok_or("get_edge returned None")?;
    if fetched_edge.kind != "tagged_with" {
        return Err(format!("edge kind mismatch: {}", fetched_edge.kind));
    }

    // --- Step 6: Query edges_of ---
    let out_edges = db
        .edges_of(node_a.id, Direction::Outgoing)
        .map_err(|e| format!("edges_of: {e}"))?;
    if out_edges.len() != 2 {
        return Err(format!(
            "expected 2 outgoing edges, got {}",
            out_edges.len()
        ));
    }

    // --- Step 7: Neighbors ---
    let neighbors = db
        .neighbors(node_a.id, Direction::Outgoing, None)
        .map_err(|e| format!("neighbors: {e}"))?;
    if neighbors.len() != 2 {
        return Err(format!("expected 2 neighbors, got {}", neighbors.len()));
    }

    let tagged_neighbors = db
        .neighbors(node_a.id, Direction::Outgoing, Some("tagged_with"))
        .map_err(|e| format!("neighbors filtered: {e}"))?;
    if tagged_neighbors.len() != 1 {
        return Err(format!(
            "expected 1 tagged neighbor, got {}",
            tagged_neighbors.len()
        ));
    }

    // --- Step 8: BFS traversal ---
    let bfs_result = db
        .bfs(node_a.id, 2, Direction::Outgoing, None)
        .map_err(|e| format!("bfs: {e}"))?;
    // A→B, A→C, B→C — BFS from A depth 2 should find B and C
    if bfs_result.len() < 2 {
        return Err(format!("BFS expected >=2 nodes, got {}", bfs_result.len()));
    }

    // --- Step 9: DFS traversal ---
    let dfs_result = db
        .dfs(node_a.id, 2, Direction::Outgoing, None)
        .map_err(|e| format!("dfs: {e}"))?;
    if dfs_result.len() < 2 {
        return Err(format!("DFS expected >=2 nodes, got {}", dfs_result.len()));
    }

    // --- Step 10: Shortest path ---
    let path = db
        .shortest_path(node_a.id, node_c.id)
        .map_err(|e| format!("shortest_path: {e}"))?;
    if path.is_none() {
        return Err("shortest_path returned None, expected a path".to_string());
    }
    let path = path.unwrap();
    if path.is_empty() {
        return Err("shortest_path returned empty path".to_string());
    }

    // --- Step 11: Subgraph ---
    let subgraph = db
        .subgraph(node_a.id, 1)
        .map_err(|e| format!("subgraph: {e}"))?;
    if subgraph.nodes.len() < 2 {
        return Err(format!(
            "subgraph expected >=2 nodes, got {}",
            subgraph.nodes.len()
        ));
    }

    // --- Step 12: Full-text search ---
    let search_results = db
        .search_fts("smoke verification", 10)
        .map_err(|e| format!("search_fts: {e}"))?;
    if search_results.is_empty() {
        return Err("search_fts returned no results".to_string());
    }

    // --- Step 13: List by kind ---
    let notes = db
        .list_nodes_by_kind("note", 10, 0)
        .map_err(|e| format!("list_nodes_by_kind: {e}"))?;
    if notes.len() != 2 {
        return Err(format!("expected 2 notes, got {}", notes.len()));
    }

    // --- Step 14: List recent ---
    let recent = db
        .list_recent(10)
        .map_err(|e| format!("list_recent: {e}"))?;
    if recent.len() != 3 {
        return Err(format!("expected 3 recent, got {}", recent.len()));
    }

    // --- Step 15: Delete node (cascading edge deletion) ---
    db.delete_node(node_b.id)
        .map_err(|e| format!("delete_node: {e}"))?;

    let deleted = db
        .get_node(node_b.id)
        .map_err(|e| format!("get_node after delete: {e}"))?;
    if deleted.is_some() {
        return Err("deleted node still exists".to_string());
    }

    // Edge A→B should be gone (cascade)
    let edge_gone = db
        .get_edge(edge_ab.id)
        .map_err(|e| format!("get_edge after cascade: {e}"))?;
    if edge_gone.is_some() {
        return Err("cascaded edge still exists".to_string());
    }

    Ok(())
}

// ===========================================================================
// Smoke test 1: MemoryBackend (all platforms including WASM)
// ===========================================================================

#[test]
fn smoke_memory_backend_full_workflow() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    run_smoke_workflow(&db).unwrap();
    db.close().unwrap();
}

// ===========================================================================
// Smoke test 2: RedbBackend (native only — desktop, iOS, Android)
// ===========================================================================

#[cfg(feature = "redb-backend")]
#[test]
fn smoke_redb_backend_full_workflow() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("smoke_test.db");
    let db = GraphNoteDb::open(&db_path).unwrap();
    run_smoke_workflow(&db).unwrap();
    db.close().unwrap();

    // Verify persistence: reopen and check data survived
    let db2 = GraphNoteDb::open(&db_path).unwrap();
    let recent = db2.list_recent(10).unwrap();
    // After workflow: 3 created, 1 deleted = 2 remaining
    assert_eq!(
        recent.len(),
        2,
        "persistence check: expected 2 nodes after reopen"
    );

    let notes = db2.list_nodes_by_kind("note", 10, 0).unwrap();
    assert_eq!(notes.len(), 2, "persistence check: expected 2 notes");

    // Search should still work on persisted data
    let results = db2.search_fts("smoke", 10).unwrap();
    assert!(
        !results.is_empty(),
        "persistence check: FTS should find results"
    );

    db2.close().unwrap();
}

// ===========================================================================
// Smoke test 3: FFI surface (native only — C API roundtrip)
// ===========================================================================

#[cfg(not(target_arch = "wasm32"))]
mod ffi_smoke {
    use graphnote_db::ffi::*;
    use std::ffi::{CStr, CString};

    unsafe fn read_and_free(ptr: *mut std::os::raw::c_char) -> String {
        assert!(!ptr.is_null(), "FFI returned null string");
        let s = CStr::from_ptr(ptr).to_string_lossy().into_owned();
        graphnote_free_string(ptr);
        s
    }

    unsafe fn last_error() -> Option<String> {
        let ptr = graphnote_last_error();
        if ptr.is_null() {
            None
        } else {
            Some(read_and_free(ptr))
        }
    }

    #[test]
    fn smoke_ffi_full_workflow() {
        unsafe {
            // --- Open ---
            let db = graphnote_open_in_memory();
            assert!(!db.is_null(), "FFI open failed: {:?}", last_error());

            // --- Create node ---
            let kind = CString::new("note").unwrap();
            let title = CString::new("FFI Smoke Node").unwrap();
            let body = CString::new("Body for FFI platform smoke test").unwrap();
            let body_html = CString::new("<p>Body</p>").unwrap();

            let empty_props = CString::new("{}").unwrap();
            let json = graphnote_create_node(
                db,
                kind.as_ptr(),
                title.as_ptr(),
                body.as_ptr(),
                body_html.as_ptr(),
                empty_props.as_ptr(),
            );
            assert!(!json.is_null(), "create_node failed: {:?}", last_error());
            let node_json = read_and_free(json);

            // Parse node id from JSON
            let node: serde_json::Value = serde_json::from_str(&node_json).unwrap();
            let node_id = node["id"].as_u64().unwrap();

            // --- Get node ---
            let get_json = graphnote_get_node(db, node_id);
            assert!(!get_json.is_null(), "get_node failed: {:?}", last_error());
            let got: serde_json::Value = serde_json::from_str(&read_and_free(get_json)).unwrap();
            assert_eq!(got["title"].as_str().unwrap(), "FFI Smoke Node");

            // --- Update node ---
            let patch = CString::new(r#"{"title":"FFI Smoke Node Updated"}"#).unwrap();
            let upd_json = graphnote_update_node(db, node_id, patch.as_ptr());
            assert!(
                !upd_json.is_null(),
                "update_node failed: {:?}",
                last_error()
            );
            let upd: serde_json::Value = serde_json::from_str(&read_and_free(upd_json)).unwrap();
            assert_eq!(upd["title"].as_str().unwrap(), "FFI Smoke Node Updated");

            // --- Create second node + edge ---
            let kind2 = CString::new("tag").unwrap();
            let title2 = CString::new("FFI Tag").unwrap();
            let body2 = CString::new("Tag for FFI smoke").unwrap();
            let html2 = CString::new("<p>Tag</p>").unwrap();

            let json2 = graphnote_create_node(
                db,
                kind2.as_ptr(),
                title2.as_ptr(),
                body2.as_ptr(),
                html2.as_ptr(),
                empty_props.as_ptr(),
            );
            assert!(!json2.is_null(), "create_node 2 failed: {:?}", last_error());
            let node2: serde_json::Value = serde_json::from_str(&read_and_free(json2)).unwrap();
            let node2_id = node2["id"].as_u64().unwrap();

            let edge_kind = CString::new("tagged_with").unwrap();
            let edge_json = graphnote_create_edge(
                db,
                node_id,
                node2_id,
                edge_kind.as_ptr(),
                1.0,
                empty_props.as_ptr(),
            );
            assert!(
                !edge_json.is_null(),
                "create_edge failed: {:?}",
                last_error()
            );
            let _edge = read_and_free(edge_json);

            // --- Search FTS ---
            let query = CString::new("smoke").unwrap();
            let search_json = graphnote_search_fts(db, query.as_ptr(), 10);
            assert!(
                !search_json.is_null(),
                "search_fts failed: {:?}",
                last_error()
            );
            let results: serde_json::Value =
                serde_json::from_str(&read_and_free(search_json)).unwrap();
            let arr = results.as_array().unwrap();
            assert!(!arr.is_empty(), "FFI search_fts returned no results");

            // --- Neighbors ---
            let neighbors_json = graphnote_neighbors(db, node_id, 0, std::ptr::null()); // 0 = Outgoing
            assert!(
                !neighbors_json.is_null(),
                "neighbors failed: {:?}",
                last_error()
            );
            let neighbors: serde_json::Value =
                serde_json::from_str(&read_and_free(neighbors_json)).unwrap();
            let n_arr = neighbors.as_array().unwrap();
            assert_eq!(n_arr.len(), 1, "expected 1 neighbor via FFI");

            // --- BFS ---
            let bfs_json = graphnote_bfs(db, node_id, 2, 0, std::ptr::null());
            assert!(!bfs_json.is_null(), "bfs failed: {:?}", last_error());
            let bfs: serde_json::Value = serde_json::from_str(&read_and_free(bfs_json)).unwrap();
            assert!(
                !bfs.as_array().unwrap().is_empty(),
                "BFS returned no nodes via FFI"
            );

            // --- Subgraph ---
            let sg_json = graphnote_subgraph(db, node_id, 1);
            assert!(!sg_json.is_null(), "subgraph failed: {:?}", last_error());
            let sg: serde_json::Value = serde_json::from_str(&read_and_free(sg_json)).unwrap();
            assert!(
                sg["nodes"].as_array().unwrap().len() >= 2,
                "subgraph should have at least 2 nodes"
            );

            // --- Delete node ---
            let rc = graphnote_delete_node(db, node2_id);
            assert_eq!(rc, 0, "delete_node failed: {:?}", last_error());

            // Verify deleted
            let gone = graphnote_get_node(db, node2_id);
            assert!(gone.is_null(), "deleted node should return null");

            // --- Close ---
            let rc = graphnote_close(db);
            assert_eq!(rc, 0, "close failed: {:?}", last_error());
        }
    }
}

// ===========================================================================
// Smoke test 4: WASM-compatible API surface (compile-time verification)
// ===========================================================================

/// This test verifies that the entire API surface used by WasmGraphNoteDb
/// works correctly through the Rust API with MemoryBackend.
/// On WASM targets, this is the only available path.
#[test]
fn smoke_wasm_compatible_api_surface() {
    let db = GraphNoteDb::open_in_memory().unwrap();

    // Create
    let node = db
        .create_node(NewNode {
            kind: "thought".to_string(),
            title: "WASM Smoke Thought".to_string(),
            body: "A thought for WASM platform verification".to_string(),
            body_html: "<p>A thought</p>".to_string(),
            properties: Properties::default(),
        })
        .unwrap();

    // JSON roundtrip (simulates WASM boundary crossing)
    let json = serde_json::to_string(&node).unwrap();
    let deserialized: graphnote_db::model::Node = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.id, node.id);
    assert_eq!(deserialized.title, node.title);
    assert_eq!(deserialized.uuid, node.uuid);

    // UUID portability
    assert!(node.uuid != [0u8; 16]);

    // Properties roundtrip
    let node_with_props = db
        .create_node(NewNode {
            kind: "emotion".to_string(),
            title: "WASM Props Test".to_string(),
            body: "Testing properties serialization".to_string(),
            body_html: String::new(),
            properties: props(&[
                ("intensity", serde_json::json!(8)),
                ("label", serde_json::json!("anxiety")),
                ("tags", serde_json::json!(["cbt", "session1"])),
            ]),
        })
        .unwrap();

    let props_json = serde_json::to_string(&node_with_props.properties).unwrap();
    let props_back: Properties = serde_json::from_str(&props_json).unwrap();
    assert_eq!(
        props_back.0.get("intensity").unwrap(),
        &serde_json::json!(8)
    );

    // Edge + traversal through JSON roundtrip
    let edge = db
        .create_edge(NewEdge {
            from_id: node.id,
            to_id: node_with_props.id,
            kind: "triggers".to_string(),
            weight: 1.0,
            properties: Properties::default(),
        })
        .unwrap();

    let edge_json = serde_json::to_string(&edge).unwrap();
    let edge_back: graphnote_db::model::Edge = serde_json::from_str(&edge_json).unwrap();
    assert_eq!(edge_back.from_id, node.id);
    assert_eq!(edge_back.to_id, node_with_props.id);

    // Subgraph via JSON roundtrip
    let sg = db.subgraph(node.id, 1).unwrap();
    let sg_json = serde_json::to_string(&sg).unwrap();
    let sg_back: graphnote_db::model::SubGraph = serde_json::from_str(&sg_json).unwrap();
    assert!(sg_back.nodes.len() >= 2);

    // ScoredNode via JSON roundtrip
    let results = db.search_fts("WASM verification", 10).unwrap();
    if !results.is_empty() {
        let scored_json = serde_json::to_string(&results[0]).unwrap();
        let scored_back: graphnote_db::model::ScoredNode =
            serde_json::from_str(&scored_json).unwrap();
        assert!(scored_back.score > 0.0);
    }

    db.close().unwrap();
}

// ===========================================================================
// Smoke test 5: Disk-backed MemoryBackend persistence (native only)
// ===========================================================================

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn smoke_memory_backend_disk_persistence() {
    use graphnote_db::storage::{MemoryBackend, StorageBackend};

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("memory_smoke.db");

    // Write data
    {
        let backend = MemoryBackend::open(&db_path).unwrap();
        backend.put(b"smoke_key", b"smoke_value").unwrap();
        backend.flush().unwrap();
    }

    // Read data back
    {
        let backend = MemoryBackend::open(&db_path).unwrap();
        let val = backend.get(b"smoke_key").unwrap();
        assert_eq!(val, Some(b"smoke_value".to_vec()));
    }
}

// ===========================================================================
// Smoke test 6: Unicode and i18n (cross-platform portability)
// ===========================================================================

#[test]
fn smoke_unicode_cross_platform() {
    let db = GraphNoteDb::open_in_memory().unwrap();

    // CJK content
    let cjk_node = db
        .create_node(NewNode {
            kind: "note".to_string(),
            title: "日本語テスト".to_string(),
            body: "这是中文测试内容".to_string(),
            body_html: "<p>这是中文测试内容</p>".to_string(),
            properties: Properties::default(),
        })
        .unwrap();

    let fetched = db.get_node(cjk_node.id).unwrap().unwrap();
    assert_eq!(fetched.title, "日本語テスト");
    assert_eq!(fetched.body, "这是中文测试内容");

    // Emoji content
    let emoji_node = db
        .create_node(NewNode {
            kind: "note".to_string(),
            title: "Test with emoji 🧠💡🔗".to_string(),
            body: "Brain 🧠 connects to 💡 ideas".to_string(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();

    let fetched = db.get_node(emoji_node.id).unwrap().unwrap();
    assert!(fetched.title.contains("🧠"));

    // Cyrillic content
    let cyr_node = db
        .create_node(NewNode {
            kind: "note".to_string(),
            title: "Тестовая заметка".to_string(),
            body: "Содержимое на русском языке".to_string(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();

    let fetched = db.get_node(cyr_node.id).unwrap().unwrap();
    assert_eq!(fetched.title, "Тестовая заметка");

    // FTS should find CJK content
    let results = db.search_fts("中文测试", 10).unwrap();
    assert!(!results.is_empty(), "FTS should find CJK content");

    db.close().unwrap();
}
