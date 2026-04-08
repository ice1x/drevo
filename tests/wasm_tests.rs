//! Integration tests for WASM bindings.
//!
//! These tests validate the WASM binding logic by exercising the same
//! GraphNoteDb API surface that [`WasmGraphNoteDb`] delegates to.
//! They run natively (not in a WASM runtime) to test correctness of
//! the binding layer's data flow: create → serialize → deserialize roundtrip.
//!
//! The actual `#[wasm_bindgen]` methods require a JS runtime (wasm-pack test),
//! so these tests verify the Rust-side logic without that dependency.

use graphnote_db::db::GraphNoteDb;
use graphnote_db::model::{
    Direction, Edge, EdgePatch, NewEdge, NewNode, Node, NodePatch, Properties, SubGraph,
};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_db() -> GraphNoteDb {
    GraphNoteDb::open_in_memory().unwrap()
}

fn make_node(db: &GraphNoteDb, kind: &str, title: &str) -> Node {
    db.create_node(NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: format!("Body of {title}"),
        body_html: format!("<p>Body of {title}</p>"),
        properties: Properties::default(),
    })
    .unwrap()
}

fn make_edge(db: &GraphNoteDb, from: u64, to: u64, kind: &str) -> Edge {
    db.create_edge(NewEdge {
        from_id: from,
        to_id: to,
        kind: kind.to_string(),
        weight: 1.0,
        properties: Properties::default(),
    })
    .unwrap()
}

/// Simulate the WASM binding's JSON roundtrip: serialize to JSON, parse back.
fn json_roundtrip<T: serde::Serialize + serde::de::DeserializeOwned>(val: &T) -> T {
    let json = serde_json::to_string(val).unwrap();
    serde_json::from_str(&json).unwrap()
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

#[test]
fn wasm_lifecycle_open_close() {
    let db = make_db();
    db.close().unwrap();
}

#[test]
fn wasm_lifecycle_multiple_instances() {
    let db1 = make_db();
    let db2 = make_db();
    make_node(&db1, "note", "In DB1");
    make_node(&db2, "note", "In DB2");
    // Instances are independent
    assert!(db1.get_node_by_title("In DB2").unwrap().is_none());
    assert!(db2.get_node_by_title("In DB1").unwrap().is_none());
    db1.close().unwrap();
    db2.close().unwrap();
}

// ---------------------------------------------------------------------------
// Node CRUD — JSON roundtrip
// ---------------------------------------------------------------------------

#[test]
fn wasm_node_create_json_roundtrip() {
    let db = make_db();
    let node = make_node(&db, "note", "Test Node");
    let roundtrip: Node = json_roundtrip(&node);
    assert_eq!(roundtrip.id, node.id);
    assert_eq!(roundtrip.kind, "note");
    assert_eq!(roundtrip.title, "Test Node");
    assert_eq!(roundtrip.body, "Body of Test Node");
}

#[test]
fn wasm_node_with_properties_json_roundtrip() {
    let db = make_db();
    let mut props = HashMap::new();
    props.insert("priority".to_string(), serde_json::json!(1));
    props.insert("tags".to_string(), serde_json::json!(["rust", "wasm"]));
    props.insert(
        "nested".to_string(),
        serde_json::json!({"a": {"b": [1, 2, 3]}}),
    );

    let node = db
        .create_node(NewNode {
            kind: "task".to_string(),
            title: "Complex Props".to_string(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::from(props),
        })
        .unwrap();

    let rt: Node = json_roundtrip(&node);
    assert_eq!(
        rt.properties.get("priority"),
        node.properties.get("priority")
    );
    assert_eq!(rt.properties.get("tags"), node.properties.get("tags"));
    assert_eq!(rt.properties.get("nested"), node.properties.get("nested"));
}

#[test]
fn wasm_node_get_returns_none_for_missing() {
    let db = make_db();
    assert!(db.get_node(999).unwrap().is_none());
}

#[test]
fn wasm_node_update_json_roundtrip() {
    let db = make_db();
    let node = make_node(&db, "note", "Original");
    let updated = db
        .update_node(
            node.id,
            NodePatch {
                title: Some("Updated".to_string()),
                body: Some("New body".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

    let rt: Node = json_roundtrip(&updated);
    assert_eq!(rt.title, "Updated");
    assert_eq!(rt.body, "New body");
    assert_eq!(rt.kind, "note"); // unchanged
}

#[test]
fn wasm_node_delete() {
    let db = make_db();
    let node = make_node(&db, "note", "To Delete");
    db.delete_node(node.id).unwrap();
    assert!(db.get_node(node.id).unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Edge CRUD — JSON roundtrip
// ---------------------------------------------------------------------------

#[test]
fn wasm_edge_create_json_roundtrip() {
    let db = make_db();
    let n1 = make_node(&db, "note", "A");
    let n2 = make_node(&db, "note", "B");
    let edge = make_edge(&db, n1.id, n2.id, "links_to");

    let rt: Edge = json_roundtrip(&edge);
    assert_eq!(rt.from_id, n1.id);
    assert_eq!(rt.to_id, n2.id);
    assert_eq!(rt.kind, "links_to");
    assert_eq!(rt.weight, 1.0);
}

#[test]
fn wasm_edge_update_json_roundtrip() {
    let db = make_db();
    let n1 = make_node(&db, "note", "X");
    let n2 = make_node(&db, "note", "Y");
    let edge = make_edge(&db, n1.id, n2.id, "links_to");

    let updated = db
        .update_edge(
            edge.id,
            EdgePatch {
                kind: Some("blocks".to_string()),
                weight: Some(2.5),
                ..Default::default()
            },
        )
        .unwrap();

    let rt: Edge = json_roundtrip(&updated);
    assert_eq!(rt.kind, "blocks");
    assert_eq!(rt.weight, 2.5);
}

#[test]
fn wasm_edge_delete() {
    let db = make_db();
    let n1 = make_node(&db, "note", "P");
    let n2 = make_node(&db, "note", "Q");
    let edge = make_edge(&db, n1.id, n2.id, "links_to");
    db.delete_edge(edge.id).unwrap();
    assert!(db.get_edge(edge.id).unwrap().is_none());
}

#[test]
fn wasm_edge_cascading_delete() {
    let db = make_db();
    let n1 = make_node(&db, "note", "Hub");
    let n2 = make_node(&db, "note", "Leaf");
    let edge = make_edge(&db, n1.id, n2.id, "links_to");
    db.delete_node(n1.id).unwrap();
    assert!(db.get_edge(edge.id).unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Traversal — JSON roundtrip
// ---------------------------------------------------------------------------

fn setup_chain(db: &GraphNoteDb) -> Vec<Node> {
    // A -> B -> C
    let a = make_node(db, "note", "Chain_A");
    let b = make_node(db, "note", "Chain_B");
    let c = make_node(db, "note", "Chain_C");
    make_edge(db, a.id, b.id, "next");
    make_edge(db, b.id, c.id, "next");
    vec![a, b, c]
}

#[test]
fn wasm_neighbors_json_roundtrip() {
    let db = make_db();
    let nodes = setup_chain(&db);

    let neighbors = db
        .neighbors(nodes[0].id, Direction::Outgoing, None)
        .unwrap();
    let rt: Vec<Node> = json_roundtrip(&neighbors);
    assert_eq!(rt.len(), 1);
    assert_eq!(rt[0].id, nodes[1].id);
}

#[test]
fn wasm_neighbors_with_kind_filter() {
    let db = make_db();
    let a = make_node(&db, "note", "FilterA");
    let b = make_node(&db, "note", "FilterB");
    let c = make_node(&db, "note", "FilterC");
    make_edge(&db, a.id, b.id, "links_to");
    make_edge(&db, a.id, c.id, "blocks");

    let links = db
        .neighbors(a.id, Direction::Outgoing, Some("links_to"))
        .unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].id, b.id);

    let blocks = db
        .neighbors(a.id, Direction::Outgoing, Some("blocks"))
        .unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].id, c.id);
}

#[test]
fn wasm_bfs_json_roundtrip() {
    let db = make_db();
    let nodes = setup_chain(&db);

    let bfs = db.bfs(nodes[0].id, 2, Direction::Outgoing, None).unwrap();
    let rt: Vec<Node> = json_roundtrip(&bfs);
    assert!(rt.len() >= 2); // B and C
}

#[test]
fn wasm_dfs_json_roundtrip() {
    let db = make_db();
    let nodes = setup_chain(&db);

    let dfs = db.dfs(nodes[0].id, 2, Direction::Outgoing, None).unwrap();
    let rt: Vec<Node> = json_roundtrip(&dfs);
    assert!(rt.len() >= 2);
}

#[test]
fn wasm_shortest_path_json_roundtrip() {
    let db = make_db();
    let nodes = setup_chain(&db);

    let path = db.shortest_path(nodes[0].id, nodes[2].id).unwrap().unwrap();
    let rt: Vec<u64> = json_roundtrip(&path);
    assert_eq!(rt, vec![nodes[0].id, nodes[1].id, nodes[2].id]);
}

#[test]
fn wasm_shortest_path_no_path() {
    let db = make_db();
    let a = make_node(&db, "note", "Island1");
    let b = make_node(&db, "note", "Island2");

    let path = db.shortest_path(a.id, b.id).unwrap();
    assert!(path.is_none());
}

#[test]
fn wasm_subgraph_json_roundtrip() {
    let db = make_db();
    let nodes = setup_chain(&db);

    let sg = db.subgraph(nodes[0].id, 1).unwrap();
    let rt: SubGraph = json_roundtrip(&sg);
    assert_eq!(rt.nodes.len(), 2); // A and B
    assert_eq!(rt.edges.len(), 1);
}

#[test]
fn wasm_subgraph_depth_2() {
    let db = make_db();
    let nodes = setup_chain(&db);

    let sg = db.subgraph(nodes[0].id, 2).unwrap();
    let rt: SubGraph = json_roundtrip(&sg);
    assert_eq!(rt.nodes.len(), 3); // A, B, C
    assert_eq!(rt.edges.len(), 2);
}

// ---------------------------------------------------------------------------
// Search — JSON roundtrip
// ---------------------------------------------------------------------------

#[test]
fn wasm_search_fts_json_roundtrip() {
    let db = make_db();

    db.create_node(NewNode {
        kind: "article".to_string(),
        title: "Rust Programming".to_string(),
        body: "Rust is a systems programming language focused on safety".to_string(),
        body_html: String::new(),
        properties: Properties::default(),
    })
    .unwrap();

    db.create_node(NewNode {
        kind: "article".to_string(),
        title: "Go Programming".to_string(),
        body: "Go is designed for simplicity and concurrency".to_string(),
        body_html: String::new(),
        properties: Properties::default(),
    })
    .unwrap();

    let results = db.search_fts("systems programming language", 10).unwrap();
    assert!(!results.is_empty());

    // Verify ScoredNode serializes correctly
    let json = serde_json::to_string(&results).unwrap();
    let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    assert!(parsed[0]["node"]["title"].is_string());
    assert!(parsed[0]["score"].is_number());
}

#[test]
fn wasm_search_fts_empty_query() {
    let db = make_db();
    make_node(&db, "note", "Anything");
    let results = db.search_fts("", 10).unwrap();
    assert!(results.is_empty());
}

#[test]
fn wasm_list_nodes_by_kind_json_roundtrip() {
    let db = make_db();

    for i in 0..5 {
        make_node(&db, "tag", &format!("Tag_{i}"));
    }
    for i in 0..3 {
        make_node(&db, "note", &format!("Note_{i}"));
    }

    let tags = db.list_nodes_by_kind("tag", 10, 0).unwrap();
    let rt: Vec<Node> = json_roundtrip(&tags);
    assert_eq!(rt.len(), 5);

    let notes = db.list_nodes_by_kind("note", 10, 0).unwrap();
    assert_eq!(notes.len(), 3);
}

#[test]
fn wasm_list_nodes_by_kind_pagination() {
    let db = make_db();

    for i in 0..10 {
        make_node(&db, "item", &format!("Item_{i}"));
    }

    let page1 = db.list_nodes_by_kind("item", 3, 0).unwrap();
    let page2 = db.list_nodes_by_kind("item", 3, 3).unwrap();
    assert_eq!(page1.len(), 3);
    assert_eq!(page2.len(), 3);
    // Pages should not overlap
    assert_ne!(page1[0].id, page2[0].id);
}

#[test]
fn wasm_list_recent_json_roundtrip() {
    let db = make_db();

    for i in 0..5 {
        make_node(&db, "note", &format!("Recent_{i}"));
    }

    let recent = db.list_recent(3).unwrap();
    let rt: Vec<Node> = json_roundtrip(&recent);
    assert_eq!(rt.len(), 3);
    // Newest first
    assert!(rt[0].updated_at >= rt[1].updated_at);
}

// ---------------------------------------------------------------------------
// Error cases
// ---------------------------------------------------------------------------

#[test]
fn wasm_error_update_nonexistent_node() {
    let db = make_db();
    let err = db
        .update_node(
            999,
            NodePatch {
                title: Some("Nope".to_string()),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(err.to_string().contains("node not found"));
}

#[test]
fn wasm_error_delete_nonexistent_edge() {
    let db = make_db();
    let err = db.delete_edge(999).unwrap_err();
    assert!(err.to_string().contains("edge not found"));
}

#[test]
fn wasm_error_duplicate_title() {
    let db = make_db();
    make_node(&db, "note", "DupTitle");
    let err = db
        .create_node(NewNode {
            kind: "note".to_string(),
            title: "DupTitle".to_string(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap_err();
    assert!(err.to_string().contains("duplicate title"));
}

#[test]
fn wasm_error_edge_to_nonexistent_node() {
    let db = make_db();
    let n = make_node(&db, "note", "Exists");
    let err = db
        .create_edge(NewEdge {
            from_id: n.id,
            to_id: 999,
            kind: "links_to".to_string(),
            weight: 1.0,
            properties: Properties::default(),
        })
        .unwrap_err();
    assert!(err.to_string().contains("not found"));
}

// ---------------------------------------------------------------------------
// Direction encoding (mirrors WASM integer convention)
// ---------------------------------------------------------------------------

#[test]
fn wasm_direction_encoding() {
    // The WASM API uses integers: 0=Outgoing, 1=Incoming, 2=Both
    let db = make_db();
    let a = make_node(&db, "note", "DirA");
    let b = make_node(&db, "note", "DirB");
    make_edge(&db, a.id, b.id, "links_to");

    // Outgoing from A -> B
    let out = db.neighbors(a.id, Direction::Outgoing, None).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].id, b.id);

    // Incoming to B <- A
    let inc = db.neighbors(b.id, Direction::Incoming, None).unwrap();
    assert_eq!(inc.len(), 1);
    assert_eq!(inc[0].id, a.id);

    // Both from A: B (outgoing)
    let both_a = db.neighbors(a.id, Direction::Both, None).unwrap();
    assert_eq!(both_a.len(), 1);
    assert_eq!(both_a[0].id, b.id);

    // Both from B: A (incoming)
    let both_b = db.neighbors(b.id, Direction::Both, None).unwrap();
    assert_eq!(both_b.len(), 1);
    assert_eq!(both_b[0].id, a.id);
}

// ---------------------------------------------------------------------------
// Complex scenario: CBT journal pattern via WASM-style API
// ---------------------------------------------------------------------------

#[test]
fn wasm_cbt_journal_scenario() {
    let db = make_db();

    // Create CBT entities
    let thought = db
        .create_node(NewNode {
            kind: "thought".to_string(),
            title: "I always fail".to_string(),
            body: "Negative automatic thought about failure".to_string(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();

    let distortion = db
        .create_node(NewNode {
            kind: "cognitive_distortion".to_string(),
            title: "Overgeneralization".to_string(),
            body: "Making broad conclusions from single events".to_string(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();

    let response = db
        .create_node(NewNode {
            kind: "rational_response".to_string(),
            title: "I succeeded many times".to_string(),
            body: "Evidence of past successes contradicts this thought".to_string(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();

    // Create relationships
    make_edge(&db, thought.id, distortion.id, "exhibits");
    make_edge(&db, distortion.id, response.id, "challenged_by");

    // Traverse from thought to find reframe
    let path = db.shortest_path(thought.id, response.id).unwrap().unwrap();
    assert_eq!(path.len(), 3);

    // Subgraph for AI context
    let sg = db.subgraph(thought.id, 2).unwrap();
    let rt: SubGraph = json_roundtrip(&sg);
    assert_eq!(rt.nodes.len(), 3);
    assert_eq!(rt.edges.len(), 2);

    // FTS: find distortions
    let results = db.search_fts("overgeneralization", 10).unwrap();
    assert!(!results.is_empty());

    // List by kind
    let thoughts = db.list_nodes_by_kind("thought", 10, 0).unwrap();
    assert_eq!(thoughts.len(), 1);
}
