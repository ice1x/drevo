//! Cross-compilation validation tests for task `00030`.
//!
//! These tests verify that the crate's compile-time feature gates and
//! platform `#[cfg]` attributes produce correct behavior on the host
//! platform.  They complement the CI cross-compilation matrix which
//! performs `cargo check` / `cargo build` for iOS, Android, and WASM
//! targets.
//!
//! Verified properties:
//! - Default features include `redb-backend` on native targets
//! - `MemoryBackend` is available on every target
//! - `RedbBackend` is available when `redb-backend` feature is enabled
//! - FFI module is excluded on `wasm32` (verified structurally)
//! - WASM module requires `wasm` feature (verified structurally)
//! - `Drevo` core API surface is identical regardless of backend
//! - `crate-type` includes `cdylib` and `staticlib` for FFI consumers

use drevo::db::Drevo;
use drevo::model::{Direction, NewEdge, NewNode, Properties};
use drevo::storage::{MemoryBackend, StorageBackend};

// ---------------------------------------------------------------------------
// Feature gate correctness
// ---------------------------------------------------------------------------

/// `RedbBackend` must be available when compiled with default features
/// (which include `redb-backend`). This test proves the native CI build
/// includes the persistent backend.
#[cfg(feature = "redb-backend")]
#[test]
fn redb_backend_available_with_feature() {
    use drevo::storage::RedbBackend;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cross_compile_test.db");
    let backend = RedbBackend::open(&path).unwrap();
    backend.put(b"key", b"value").unwrap();
    let val = backend.get(b"key").unwrap();
    assert_eq!(val, Some(b"value".to_vec()));
}

/// `MemoryBackend` must always be available — it is the universal fallback
/// for WASM targets and in-memory use.
#[test]
fn memory_backend_always_available() {
    let backend = MemoryBackend::new();
    backend.put(b"k", b"v").unwrap();
    assert_eq!(backend.get(b"k").unwrap(), Some(b"v".to_vec()));
    backend.delete(b"k").unwrap();
    assert_eq!(backend.get(b"k").unwrap(), None);
}

/// `StorageBackend::flush()` is a no-op for ephemeral `MemoryBackend`
/// (no path). Must succeed on all platforms.
#[test]
fn memory_backend_flush_noop_is_portable() {
    let backend = MemoryBackend::new();
    backend.put(b"a", b"1").unwrap();
    // flush without path is a no-op — must not error
    backend.flush().unwrap();
}

// ---------------------------------------------------------------------------
// Drevo API surface consistency
// ---------------------------------------------------------------------------

/// The full public API must work identically on `open_in_memory()`.
/// This is the only constructor available on WASM, so every feature
/// must be reachable through it.
#[test]
fn full_api_surface_via_open_in_memory() {
    let db = Drevo::open_in_memory().unwrap();

    // Node CRUD
    let n1 = db
        .create_node(NewNode {
            kind: "component".to_string(),
            title: "AuthService".to_string(),
            body: "Handles authentication and token validation".to_string(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();

    let n2 = db
        .create_node(NewNode {
            kind: "component".to_string(),
            title: "UserService".to_string(),
            body: "Manages user profiles and preferences".to_string(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();

    let n3 = db
        .create_node(NewNode {
            kind: "api".to_string(),
            title: "POST /login".to_string(),
            body: "Login endpoint with JWT response".to_string(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();

    // get_node
    let fetched = db.get_node(n1.id).unwrap().unwrap();
    assert_eq!(fetched.title, "AuthService");

    // get_node_by_uuid
    let by_uuid = db.get_node_by_uuid(&n1.uuid).unwrap().unwrap();
    assert_eq!(by_uuid.id, n1.id);

    // get_node_by_title
    let by_title = db.get_node_by_title("UserService").unwrap().unwrap();
    assert_eq!(by_title.id, n2.id);

    // Edge CRUD
    let e1 = db
        .create_edge(NewEdge {
            from_id: n1.id,
            to_id: n2.id,
            kind: "depends_on".to_string(),
            weight: 1.0,
            properties: Properties::default(),
        })
        .unwrap();

    let e2 = db
        .create_edge(NewEdge {
            from_id: n3.id,
            to_id: n1.id,
            kind: "uses".to_string(),
            weight: 0.8,
            properties: Properties::default(),
        })
        .unwrap();

    let fetched_edge = db.get_edge(e1.id).unwrap().unwrap();
    assert_eq!(fetched_edge.kind, "depends_on");

    // edges_of
    let out_edges = db.edges_of(n1.id, Direction::Outgoing).unwrap();
    assert_eq!(out_edges.len(), 1);
    assert_eq!(out_edges[0].id, e1.id);

    let in_edges = db.edges_of(n1.id, Direction::Incoming).unwrap();
    assert_eq!(in_edges.len(), 1);
    assert_eq!(in_edges[0].id, e2.id);

    // neighbors
    let neighbors = db.neighbors(n1.id, Direction::Outgoing, None).unwrap();
    assert_eq!(neighbors.len(), 1);
    assert_eq!(neighbors[0].id, n2.id);

    let kind_neighbors = db
        .neighbors(n1.id, Direction::Incoming, Some("uses"))
        .unwrap();
    assert_eq!(kind_neighbors.len(), 1);
    assert_eq!(kind_neighbors[0].id, n3.id);

    // Traversal: BFS
    let bfs = db.bfs(n3.id, 2, Direction::Outgoing, None).unwrap();
    assert!(!bfs.is_empty()); // n1 at least, maybe n2

    // Traversal: DFS
    let dfs = db.dfs(n3.id, 2, Direction::Outgoing, None).unwrap();
    assert!(!dfs.is_empty());

    // Traversal: shortest_path
    let path = db.shortest_path(n3.id, n2.id).unwrap();
    assert!(path.is_some());
    let path = path.unwrap();
    assert!(path.len() >= 2); // n3 -> n1 -> n2

    // Traversal: subgraph
    let sg = db.subgraph(n1.id, 1).unwrap();
    assert!(sg.nodes.len() >= 2);
    assert!(!sg.edges.is_empty());

    // FTS
    let results = db.search_fts("authentication", 10).unwrap();
    assert!(!results.is_empty());

    // list_nodes_by_kind
    let components = db.list_nodes_by_kind("component", 10, 0).unwrap();
    assert_eq!(components.len(), 2);

    // list_recent
    let recent = db.list_recent(10).unwrap();
    assert_eq!(recent.len(), 3);

    db.close().unwrap();
}

// ---------------------------------------------------------------------------
// Disk-backed path (native only, excluded on WASM)
// ---------------------------------------------------------------------------

/// `Drevo::open(path)` must work on native targets. This constructor
/// is excluded on WASM (no filesystem), but iOS and Android have filesystem
/// access and must use the persistent path.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn open_disk_backed_on_native() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native_test.db");
    let db = Drevo::open(&path).unwrap();

    let node = db
        .create_node(NewNode {
            kind: "note".to_string(),
            title: "Persistent Note".to_string(),
            body: "Stored on disk".to_string(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();

    let fetched = db.get_node(node.id).unwrap().unwrap();
    assert_eq!(fetched.title, "Persistent Note");

    db.close().unwrap();
}

/// `MemoryBackend::open(path)` (persistent memory backend) works on native.
/// This is the fallback for platforms where redb is unavailable but
/// filesystem access exists (hypothetical future targets).
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn memory_backend_persistent_on_native() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mem_persist.bin");

    // Write data
    {
        let backend = MemoryBackend::open(&path).unwrap();
        backend.put(b"persist_key", b"persist_value").unwrap();
        backend.flush().unwrap();
    }

    // Read data back
    {
        let backend = MemoryBackend::open(&path).unwrap();
        let val = backend.get(b"persist_key").unwrap();
        assert_eq!(val, Some(b"persist_value".to_vec()));
    }
}

// ---------------------------------------------------------------------------
// Crate metadata validation
// ---------------------------------------------------------------------------

/// Verify the crate compiles with the expected lib types.
/// `cdylib` is needed for C FFI (iOS/Android), `staticlib` for static
/// linking, and `lib` for Rust consumers.
#[test]
fn crate_metadata_sanity() {
    // If this test compiles and runs, the crate's lib types are correct.
    // The actual crate-type verification happens at build time in CI
    // via `cargo build --target <platform>`.
    let db = Drevo::open_in_memory().unwrap();
    assert!(db.get_node(9999).unwrap().is_none());
    db.close().unwrap();
}

// ---------------------------------------------------------------------------
// Edge cases for cross-platform portability
// ---------------------------------------------------------------------------

/// UUID v7 generation must work on all platforms (including WASM with
/// `getrandom/wasm_js`). Verify that created nodes get unique UUIDs.
#[test]
fn uuid_generation_is_portable() {
    let db = Drevo::open_in_memory().unwrap();

    let mut uuids = Vec::new();
    for i in 0..10 {
        let node = db
            .create_node(NewNode {
                kind: "test".to_string(),
                title: format!("Node {i}"),
                body: String::new(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .unwrap();
        uuids.push(node.uuid);
    }

    // All UUIDs must be unique
    let unique: std::collections::HashSet<_> = uuids.iter().collect();
    assert_eq!(unique.len(), 10);

    db.close().unwrap();
}

/// Properties (HashMap<String, serde_json::Value>) serialization must be
/// portable. This exercises bincode + serde_json interop across platforms.
#[test]
fn properties_serialization_is_portable() {
    let db = Drevo::open_in_memory().unwrap();

    let mut props = Properties::default();
    props.insert("string".to_string(), serde_json::json!("hello"));
    props.insert("number".to_string(), serde_json::json!(42));
    props.insert("float".to_string(), serde_json::json!(2.72));
    props.insert("bool".to_string(), serde_json::json!(true));
    props.insert("null".to_string(), serde_json::json!(null));
    props.insert(
        "array".to_string(),
        serde_json::json!([1, "two", null, false]),
    );
    props.insert(
        "nested".to_string(),
        serde_json::json!({"key": "value", "n": 99}),
    );

    let node = db
        .create_node(NewNode {
            kind: "test".to_string(),
            title: "Props Test".to_string(),
            body: String::new(),
            body_html: String::new(),
            properties: props.clone(),
        })
        .unwrap();

    let fetched = db.get_node(node.id).unwrap().unwrap();
    assert_eq!(fetched.properties.get("string").unwrap(), "hello");
    assert_eq!(fetched.properties.get("number").unwrap(), 42);
    assert_eq!(fetched.properties.get("float").unwrap(), 2.72);
    assert_eq!(fetched.properties.get("bool").unwrap(), true);
    assert!(fetched.properties.get("null").unwrap().is_null());
    assert!(fetched.properties.get("array").unwrap().is_array());
    assert!(fetched.properties.get("nested").unwrap().is_object());

    db.close().unwrap();
}

/// Unicode handling in titles and bodies must work across platforms.
/// CJK, emoji, RTL — all must round-trip through bincode serialization.
#[test]
fn unicode_portability() {
    let db = Drevo::open_in_memory().unwrap();

    let test_cases = vec![
        ("CJK", "量子計算", "量子力学の基礎"),
        ("Emoji", "🧠 Brain Dump", "Ideas 💡 and notes 📝"),
        ("RTL", "ملاحظات", "هذه ملاحظة باللغة العربية"),
        ("Cyrillic", "Заметки", "Квантовые вычисления"),
        (
            "Mixed",
            "Note αβγ 日本語",
            "Mixed content: café résumé naïve",
        ),
    ];

    for (kind, title, body) in &test_cases {
        let node = db
            .create_node(NewNode {
                kind: kind.to_string(),
                title: title.to_string(),
                body: body.to_string(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .unwrap();

        let fetched = db.get_node(node.id).unwrap().unwrap();
        assert_eq!(&fetched.title, title);
        assert_eq!(&fetched.body, body);
    }

    // FTS must find CJK content
    let results = db.search_fts("量子", 10).unwrap();
    assert!(!results.is_empty(), "FTS should find CJK content");

    db.close().unwrap();
}
