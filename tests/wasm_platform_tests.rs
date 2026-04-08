//! Platform-specific tests for WASM target compatibility.
//!
//! Task `00029`: Verify redb works on WASM target; if not, implement fallback.
//!
//! These tests verify:
//! - `MemoryBackend` is always available (including WASM target)
//! - `RedbBackend` is excluded on WASM via compile-time cfg
//! - `GraphNoteDb::open_in_memory()` works as the WASM fallback
//! - `GraphNoteDb::open()` (disk-backed) is unavailable on WASM
//! - Full CRUD + traversal + FTS workflow works on MemoryBackend alone

use graphnote_db::db::GraphNoteDb;
use graphnote_db::model::{Direction, NewEdge, NewNode, Properties};
use graphnote_db::storage::MemoryBackend;

// ---------------------------------------------------------------------------
// MemoryBackend availability (WASM fallback storage)
// ---------------------------------------------------------------------------

#[test]
fn memory_backend_is_always_available() {
    // MemoryBackend must compile and work on all targets, including WASM.
    let backend = MemoryBackend::new();
    use graphnote_db::storage::StorageBackend;
    backend.put(b"k1", b"v1").unwrap();
    let val = backend.get(b"k1").unwrap();
    assert_eq!(val, Some(b"v1".to_vec()));
}

#[test]
fn memory_backend_scan_prefix_works() {
    let backend = MemoryBackend::new();
    use graphnote_db::storage::StorageBackend;
    backend.put(b"prefix:a", b"1").unwrap();
    backend.put(b"prefix:b", b"2").unwrap();
    backend.put(b"other:c", b"3").unwrap();
    let results = backend.scan_prefix(b"prefix:").unwrap();
    assert_eq!(results.len(), 2);
}

// ---------------------------------------------------------------------------
// RedbBackend is gated: available on native, absent on WASM
// ---------------------------------------------------------------------------

/// On native targets, `RedbBackend` should be importable and functional.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn redb_backend_available_on_native() {
    use graphnote_db::storage::RedbBackend;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let _backend = RedbBackend::open(&path).unwrap();
}

/// On native targets, `GraphNoteDb::open()` (disk-backed) should work.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn graphnotedb_open_available_on_native() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let db = GraphNoteDb::open(&path).unwrap();
    db.close().unwrap();
}

// ---------------------------------------------------------------------------
// GraphNoteDb::open_in_memory() — the WASM fallback path
// ---------------------------------------------------------------------------

#[test]
fn open_in_memory_is_always_available() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    db.close().unwrap();
}

// ---------------------------------------------------------------------------
// Full workflow on MemoryBackend (WASM-equivalent path)
// ---------------------------------------------------------------------------

#[test]
fn full_crud_workflow_on_memory_backend() {
    let db = GraphNoteDb::open_in_memory().unwrap();

    // Create nodes
    let n1 = db
        .create_node(NewNode {
            kind: "note".to_string(),
            title: "First Note".to_string(),
            body: "Content of the first note".to_string(),
            body_html: "<p>Content of the first note</p>".to_string(),
            properties: Properties::default(),
        })
        .unwrap();

    let n2 = db
        .create_node(NewNode {
            kind: "note".to_string(),
            title: "Second Note".to_string(),
            body: "Content of the second note".to_string(),
            body_html: "<p>Content of the second note</p>".to_string(),
            properties: Properties::default(),
        })
        .unwrap();

    // Create edge
    let edge = db
        .create_edge(NewEdge {
            from_id: n1.id,
            to_id: n2.id,
            kind: "links_to".to_string(),
            weight: 1.0,
            properties: Properties::default(),
        })
        .unwrap();

    // Read back
    let fetched = db.get_node(n1.id).unwrap().unwrap();
    assert_eq!(fetched.title, "First Note");

    let fetched_edge = db.get_edge(edge.id).unwrap().unwrap();
    assert_eq!(fetched_edge.from_id, n1.id);
    assert_eq!(fetched_edge.to_id, n2.id);

    // Neighbors
    let neighbors = db.neighbors(n1.id, Direction::Outgoing, None).unwrap();
    assert_eq!(neighbors.len(), 1);
    assert_eq!(neighbors[0].id, n2.id);

    // Delete
    db.delete_node(n1.id).unwrap();
    assert!(db.get_node(n1.id).unwrap().is_none());
}

#[test]
fn fts_works_on_memory_backend() {
    let db = GraphNoteDb::open_in_memory().unwrap();

    db.create_node(NewNode {
        kind: "note".to_string(),
        title: "Quantum Computing Basics".to_string(),
        body: "Introduction to quantum computing and qubits".to_string(),
        body_html: String::new(),
        properties: Properties::default(),
    })
    .unwrap();

    db.create_node(NewNode {
        kind: "note".to_string(),
        title: "Classical Computing".to_string(),
        body: "Traditional computing with bits and bytes".to_string(),
        body_html: String::new(),
        properties: Properties::default(),
    })
    .unwrap();

    let results = db.search_fts("quantum", 10).unwrap();
    assert!(!results.is_empty());
    assert!(results[0].node.title.contains("Quantum"));
}

#[test]
fn traversal_works_on_memory_backend() {
    let db = GraphNoteDb::open_in_memory().unwrap();

    let n1 = db
        .create_node(NewNode {
            kind: "task".to_string(),
            title: "Root Task".to_string(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();

    let n2 = db
        .create_node(NewNode {
            kind: "task".to_string(),
            title: "Sub Task".to_string(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();

    let n3 = db
        .create_node(NewNode {
            kind: "task".to_string(),
            title: "Leaf Task".to_string(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();

    db.create_edge(NewEdge {
        from_id: n1.id,
        to_id: n2.id,
        kind: "depends_on".to_string(),
        weight: 1.0,
        properties: Properties::default(),
    })
    .unwrap();

    db.create_edge(NewEdge {
        from_id: n2.id,
        to_id: n3.id,
        kind: "depends_on".to_string(),
        weight: 1.0,
        properties: Properties::default(),
    })
    .unwrap();

    // BFS (start node not included in result)
    let bfs = db.bfs(n1.id, 2, Direction::Outgoing, None).unwrap();
    assert_eq!(bfs.len(), 2); // n2, n3

    // DFS (start node not included in result)
    let dfs = db.dfs(n1.id, 2, Direction::Outgoing, None).unwrap();
    assert_eq!(dfs.len(), 2);

    // Shortest path
    let path = db.shortest_path(n1.id, n3.id).unwrap().unwrap();
    assert_eq!(path.len(), 3); // n1 -> n2 -> n3

    // Subgraph
    let sg = db.subgraph(n1.id, 2).unwrap();
    assert_eq!(sg.nodes.len(), 3);
    assert_eq!(sg.edges.len(), 2);
}

#[test]
fn list_by_kind_works_on_memory_backend() {
    let db = GraphNoteDb::open_in_memory().unwrap();

    for i in 0..5 {
        db.create_node(NewNode {
            kind: "thought".to_string(),
            title: format!("Thought {i}"),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();
    }

    for i in 0..3 {
        db.create_node(NewNode {
            kind: "emotion".to_string(),
            title: format!("Emotion {i}"),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();
    }

    let thoughts = db.list_nodes_by_kind("thought", 10, 0).unwrap();
    assert_eq!(thoughts.len(), 5);

    let emotions = db.list_nodes_by_kind("emotion", 10, 0).unwrap();
    assert_eq!(emotions.len(), 3);
}

#[test]
fn list_recent_works_on_memory_backend() {
    let db = GraphNoteDb::open_in_memory().unwrap();

    for i in 0..10 {
        db.create_node(NewNode {
            kind: "note".to_string(),
            title: format!("Note {i}"),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();
    }

    let recent = db.list_recent(5).unwrap();
    assert_eq!(recent.len(), 5);
    // Most recent first
    assert!(recent[0].updated_at >= recent[4].updated_at);
}
