//! Integration tests for cascading edge deletion on node removal (task 00013).
//!
//! When a node is deleted, all edges connected to it (both incoming and
//! outgoing) must be automatically deleted. This includes cleaning up
//! adjacency lists, UUID indexes, and kind indexes for each removed edge.

use graphnote_db::db::GraphNoteDb;
use graphnote_db::model::{Direction, NewEdge, NewNode, Properties};

fn node(kind: &str, title: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn edge(from_id: u64, to_id: u64, kind: &str) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: kind.to_string(),
        weight: 1.0,
        properties: Properties::default(),
    }
}

// --- Basic cascading deletion ---

#[test]
fn delete_node_removes_outgoing_edges() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(node("note", "A")).unwrap();
    let b = db.create_node(node("note", "B")).unwrap();
    let e = db.create_edge(edge(a.id, b.id, "links_to")).unwrap();

    db.delete_node(a.id).unwrap();

    // Edge should be gone
    assert!(db.get_edge(e.id).unwrap().is_none());
    // B's incoming edges should be empty
    assert!(db.edges_of(b.id, Direction::Incoming).unwrap().is_empty());
}

#[test]
fn delete_node_removes_incoming_edges() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(node("note", "A")).unwrap();
    let b = db.create_node(node("note", "B")).unwrap();
    let e = db.create_edge(edge(a.id, b.id, "links_to")).unwrap();

    db.delete_node(b.id).unwrap();

    // Edge should be gone
    assert!(db.get_edge(e.id).unwrap().is_none());
    // A's outgoing edges should be empty
    assert!(db.edges_of(a.id, Direction::Outgoing).unwrap().is_empty());
}

#[test]
fn delete_node_removes_multiple_edges() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(node("note", "A")).unwrap();
    let b = db.create_node(node("note", "B")).unwrap();
    let c = db.create_node(node("note", "C")).unwrap();

    let e1 = db.create_edge(edge(a.id, b.id, "links_to")).unwrap();
    let e2 = db.create_edge(edge(c.id, a.id, "references")).unwrap();
    let e3 = db.create_edge(edge(a.id, c.id, "derived_from")).unwrap();

    // Delete A — should cascade-delete e1, e2, e3
    db.delete_node(a.id).unwrap();

    assert!(db.get_edge(e1.id).unwrap().is_none());
    assert!(db.get_edge(e2.id).unwrap().is_none());
    assert!(db.get_edge(e3.id).unwrap().is_none());

    // B and C should have no edges
    assert!(db.edges_of(b.id, Direction::Both).unwrap().is_empty());
    assert!(db.edges_of(c.id, Direction::Both).unwrap().is_empty());
}

// --- Self-loop ---

#[test]
fn delete_node_removes_self_loop_edge() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(node("note", "A")).unwrap();
    let e = db.create_edge(edge(a.id, a.id, "self_ref")).unwrap();

    db.delete_node(a.id).unwrap();

    assert!(db.get_edge(e.id).unwrap().is_none());
}

// --- UUID index cleanup ---

#[test]
fn delete_node_cleans_edge_uuid_index() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(node("note", "A")).unwrap();
    let b = db.create_node(node("note", "B")).unwrap();
    let e = db.create_edge(edge(a.id, b.id, "links_to")).unwrap();
    let edge_uuid = e.uuid;

    db.delete_node(a.id).unwrap();

    // Edge UUID index should be cleaned up
    assert!(db.get_edge_by_uuid(&edge_uuid).unwrap().is_none());
}

// --- Kind index cleanup ---

#[test]
fn delete_node_cleans_edge_kind_index() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(node("note", "A")).unwrap();
    let b = db.create_node(node("note", "B")).unwrap();
    db.create_edge(edge(a.id, b.id, "links_to")).unwrap();

    db.delete_node(a.id).unwrap();

    // Edge kind index should be empty for "links_to"
    assert!(db
        .list_edges_by_kind("links_to", 100, 0)
        .unwrap()
        .is_empty());
}

// --- Surviving edges are untouched ---

#[test]
fn delete_node_does_not_affect_unrelated_edges() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(node("note", "A")).unwrap();
    let b = db.create_node(node("note", "B")).unwrap();
    let c = db.create_node(node("note", "C")).unwrap();

    db.create_edge(edge(a.id, b.id, "links_to")).unwrap();
    let e_bc = db.create_edge(edge(b.id, c.id, "links_to")).unwrap();

    db.delete_node(a.id).unwrap();

    // B->C edge should survive
    let surviving = db.get_edge(e_bc.id).unwrap();
    assert!(surviving.is_some());
    assert_eq!(surviving.unwrap().id, e_bc.id);

    // B should have one outgoing edge
    let b_out = db.edges_of(b.id, Direction::Outgoing).unwrap();
    assert_eq!(b_out.len(), 1);
    assert_eq!(b_out[0].id, e_bc.id);
}

// --- Surviving nodes are untouched ---

#[test]
fn delete_node_does_not_affect_other_nodes() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(node("note", "A")).unwrap();
    let b = db.create_node(node("note", "B")).unwrap();
    db.create_edge(edge(a.id, b.id, "links_to")).unwrap();

    db.delete_node(a.id).unwrap();

    // B should still exist
    let b_fetched = db.get_node(b.id).unwrap();
    assert!(b_fetched.is_some());
    assert_eq!(b_fetched.unwrap().title, "B");
}

// --- Node with no edges ---

#[test]
fn delete_node_without_edges_succeeds() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(node("note", "A")).unwrap();

    // Should succeed even without any edges
    db.delete_node(a.id).unwrap();
    assert!(db.get_node(a.id).unwrap().is_none());
}

// --- Hub node with many connections ---

#[test]
fn delete_hub_node_cascades_all_edges() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let hub = db.create_node(node("hub", "Hub")).unwrap();

    let mut edge_ids = Vec::new();
    for i in 0..10 {
        let n = db.create_node(node("note", &format!("Spoke-{i}"))).unwrap();
        let e = db.create_edge(edge(hub.id, n.id, "connected")).unwrap();
        edge_ids.push(e.id);
    }

    db.delete_node(hub.id).unwrap();

    // All edges should be gone
    for eid in &edge_ids {
        assert!(db.get_edge(*eid).unwrap().is_none());
    }

    // Kind index should be empty for "connected"
    assert!(db
        .list_edges_by_kind("connected", 100, 0)
        .unwrap()
        .is_empty());
}

// --- Bidirectional edges between two nodes ---

#[test]
fn delete_node_with_bidirectional_edges() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(node("note", "A")).unwrap();
    let b = db.create_node(node("note", "B")).unwrap();

    let e1 = db.create_edge(edge(a.id, b.id, "links_to")).unwrap();
    let e2 = db.create_edge(edge(b.id, a.id, "links_to")).unwrap();

    db.delete_node(a.id).unwrap();

    // Both edges should be gone
    assert!(db.get_edge(e1.id).unwrap().is_none());
    assert!(db.get_edge(e2.id).unwrap().is_none());

    // B should have no edges
    assert!(db.edges_of(b.id, Direction::Both).unwrap().is_empty());
}

// --- Create edge to deleted node fails ---

#[test]
fn create_edge_to_deleted_node_fails() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let a = db.create_node(node("note", "A")).unwrap();
    let b = db.create_node(node("note", "B")).unwrap();

    db.delete_node(a.id).unwrap();

    // Cannot create edge from deleted node
    let err = db.create_edge(edge(a.id, b.id, "links_to")).unwrap_err();
    assert!(matches!(
        err,
        graphnote_db::error::GraphNoteError::NodeNotFound(id) if id == a.id
    ));
}

// --- Persistence: cascade survives close/reopen ---

#[test]
fn cascade_delete_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cascade.db");

    {
        let db = GraphNoteDb::open(&path).unwrap();
        let a = db.create_node(node("note", "A")).unwrap();
        let b = db.create_node(node("note", "B")).unwrap();
        let _e = db.create_edge(edge(a.id, b.id, "links_to")).unwrap();

        db.delete_node(a.id).unwrap();
        db.close().unwrap();
    }

    {
        let db = GraphNoteDb::open(&path).unwrap();
        // Node A and its edge should be gone
        assert!(db.get_node(1).unwrap().is_none());
        // Edge 1 should be gone
        assert!(db.get_edge(1).unwrap().is_none());
        // Node B should still exist
        assert!(db.get_node(2).unwrap().is_some());
        // B should have no incoming edges
        assert!(db.edges_of(2, Direction::Incoming).unwrap().is_empty());
        db.close().unwrap();
    }
}
