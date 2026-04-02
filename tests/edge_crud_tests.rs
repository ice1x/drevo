//! Integration tests for Edge CRUD operations (task 00011).

use graphnote_db::db::GraphNoteDb;
use graphnote_db::error::GraphNoteError;
use graphnote_db::model::{Direction, EdgePatch, NewEdge, NewNode, Properties};
use serde_json::json;
use std::collections::HashMap;

fn sample_node(title: &str) -> NewNode {
    NewNode {
        kind: "note".to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn sample_edge(from_id: u64, to_id: u64) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: "links_to".to_string(),
        weight: 1.0,
        properties: Properties::default(),
    }
}

fn sample_edge_with_props(from_id: u64, to_id: u64) -> NewEdge {
    let mut props = HashMap::new();
    props.insert("label".to_string(), json!("related"));
    props.insert("strength".to_string(), json!(0.9));
    NewEdge {
        from_id,
        to_id,
        kind: "tagged_with".to_string(),
        weight: 2.5,
        properties: Properties::from(props),
    }
}

/// Helper: create a DB with two nodes and return (db, node1_id, node2_id).
fn db_with_two_nodes() -> (GraphNoteDb, u64, u64) {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let n1 = db.create_node(sample_node("Node A")).unwrap();
    let n2 = db.create_node(sample_node("Node B")).unwrap();
    (db, n1.id, n2.id)
}

// --- create_edge ---

#[test]
fn create_edge_returns_edge_with_id() {
    let (db, n1, n2) = db_with_two_nodes();
    let edge = db.create_edge(sample_edge(n1, n2)).unwrap();
    assert_eq!(edge.id, 1);
    assert_eq!(edge.from_id, n1);
    assert_eq!(edge.to_id, n2);
    assert_eq!(edge.kind, "links_to");
    assert_eq!(edge.weight, 1.0);
}

#[test]
fn create_edge_assigns_sequential_ids() {
    let (db, n1, n2) = db_with_two_nodes();
    let e1 = db.create_edge(sample_edge(n1, n2)).unwrap();
    let e2 = db.create_edge(sample_edge(n2, n1)).unwrap();
    assert_eq!(e1.id, 1);
    assert_eq!(e2.id, 2);
}

#[test]
fn create_edge_generates_uuid_v7() {
    let (db, n1, n2) = db_with_two_nodes();
    let edge = db.create_edge(sample_edge(n1, n2)).unwrap();
    let uuid = uuid::Uuid::from_bytes(edge.uuid);
    assert_eq!(uuid.get_version(), Some(uuid::Version::SortRand));
}

#[test]
fn create_edge_preserves_properties() {
    let (db, n1, n2) = db_with_two_nodes();
    let edge = db.create_edge(sample_edge_with_props(n1, n2)).unwrap();
    assert_eq!(edge.properties.get("label"), Some(&json!("related")));
    assert_eq!(edge.properties.get("strength"), Some(&json!(0.9)));
    assert_eq!(edge.kind, "tagged_with");
    assert_eq!(edge.weight, 2.5);
}

#[test]
fn create_edge_from_nonexistent_node_fails() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let n1 = db.create_node(sample_node("Only")).unwrap();
    let err = db.create_edge(sample_edge(999, n1.id)).unwrap_err();
    assert!(matches!(err, GraphNoteError::NodeNotFound(999)));
}

#[test]
fn create_edge_to_nonexistent_node_fails() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let n1 = db.create_node(sample_node("Only")).unwrap();
    let err = db.create_edge(sample_edge(n1.id, 999)).unwrap_err();
    assert!(matches!(err, GraphNoteError::NodeNotFound(999)));
}

#[test]
fn create_self_loop_edge() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let n1 = db.create_node(sample_node("Self")).unwrap();
    let edge = db.create_edge(sample_edge(n1.id, n1.id)).unwrap();
    assert_eq!(edge.from_id, n1.id);
    assert_eq!(edge.to_id, n1.id);
}

#[test]
fn create_multiple_edges_between_same_nodes() {
    let (db, n1, n2) = db_with_two_nodes();
    let e1 = db.create_edge(sample_edge(n1, n2)).unwrap();
    let e2 = db
        .create_edge(NewEdge {
            from_id: n1,
            to_id: n2,
            kind: "blocks".to_string(),
            weight: 1.0,
            properties: Properties::default(),
        })
        .unwrap();
    assert_ne!(e1.id, e2.id);
    assert_eq!(e1.from_id, e2.from_id);
    assert_eq!(e1.to_id, e2.to_id);
}

// --- get_edge ---

#[test]
fn get_edge_existing() {
    let (db, n1, n2) = db_with_two_nodes();
    let created = db.create_edge(sample_edge(n1, n2)).unwrap();
    let fetched = db.get_edge(created.id).unwrap();
    assert_eq!(fetched, Some(created));
}

#[test]
fn get_edge_nonexistent_returns_none() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    assert_eq!(db.get_edge(999).unwrap(), None);
}

// --- get_edge_by_uuid ---

#[test]
fn get_edge_by_uuid_existing() {
    let (db, n1, n2) = db_with_two_nodes();
    let created = db.create_edge(sample_edge(n1, n2)).unwrap();
    let fetched = db.get_edge_by_uuid(&created.uuid).unwrap();
    assert_eq!(fetched, Some(created));
}

#[test]
fn get_edge_by_uuid_nonexistent_returns_none() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let fake_uuid = [0u8; 16];
    assert_eq!(db.get_edge_by_uuid(&fake_uuid).unwrap(), None);
}

// --- update_edge ---

#[test]
fn update_edge_changes_kind() {
    let (db, n1, n2) = db_with_two_nodes();
    let edge = db.create_edge(sample_edge(n1, n2)).unwrap();

    let updated = db
        .update_edge(
            edge.id,
            EdgePatch {
                kind: Some("blocks".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(updated.kind, "blocks");
    assert_eq!(updated.from_id, n1); // unchanged
    assert_eq!(updated.to_id, n2); // unchanged
}

#[test]
fn update_edge_changes_weight() {
    let (db, n1, n2) = db_with_two_nodes();
    let edge = db.create_edge(sample_edge(n1, n2)).unwrap();

    let updated = db
        .update_edge(
            edge.id,
            EdgePatch {
                weight: Some(5.0),
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(updated.weight, 5.0);
}

#[test]
fn update_edge_changes_properties() {
    let (db, n1, n2) = db_with_two_nodes();
    let edge = db.create_edge(sample_edge(n1, n2)).unwrap();

    let mut new_props = HashMap::new();
    new_props.insert("color".to_string(), json!("red"));
    let updated = db
        .update_edge(
            edge.id,
            EdgePatch {
                properties: Some(Properties::from(new_props)),
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(updated.properties.get("color"), Some(&json!("red")));
}

#[test]
fn update_edge_nonexistent_fails() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let err = db
        .update_edge(
            999,
            EdgePatch {
                kind: Some("x".to_string()),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(matches!(err, GraphNoteError::EdgeNotFound(999)));
}

// --- delete_edge ---

#[test]
fn delete_edge_removes_from_storage() {
    let (db, n1, n2) = db_with_two_nodes();
    let edge = db.create_edge(sample_edge(n1, n2)).unwrap();

    db.delete_edge(edge.id).unwrap();

    assert_eq!(db.get_edge(edge.id).unwrap(), None);
}

#[test]
fn delete_edge_removes_uuid_index() {
    let (db, n1, n2) = db_with_two_nodes();
    let edge = db.create_edge(sample_edge(n1, n2)).unwrap();
    let uuid = edge.uuid;

    db.delete_edge(edge.id).unwrap();

    assert_eq!(db.get_edge_by_uuid(&uuid).unwrap(), None);
}

#[test]
fn delete_edge_removes_from_adjacency_lists() {
    let (db, n1, n2) = db_with_two_nodes();
    let edge = db.create_edge(sample_edge(n1, n2)).unwrap();

    db.delete_edge(edge.id).unwrap();

    // Outgoing edges of n1 should be empty
    let out = db.edges_of(n1, Direction::Outgoing).unwrap();
    assert!(out.is_empty());
    // Incoming edges of n2 should be empty
    let inc = db.edges_of(n2, Direction::Incoming).unwrap();
    assert!(inc.is_empty());
}

#[test]
fn delete_edge_nonexistent_fails() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let err = db.delete_edge(999).unwrap_err();
    assert!(matches!(err, GraphNoteError::EdgeNotFound(999)));
}

// --- edges_of ---

#[test]
fn edges_of_outgoing() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let n1 = db.create_node(sample_node("A")).unwrap();
    let n2 = db.create_node(sample_node("B")).unwrap();
    let n3 = db.create_node(sample_node("C")).unwrap();

    let e1 = db.create_edge(sample_edge(n1.id, n2.id)).unwrap();
    let e2 = db.create_edge(sample_edge(n1.id, n3.id)).unwrap();
    db.create_edge(sample_edge(n2.id, n3.id)).unwrap(); // not from n1

    let out = db.edges_of(n1.id, Direction::Outgoing).unwrap();
    assert_eq!(out.len(), 2);
    let ids: Vec<u64> = out.iter().map(|e| e.id).collect();
    assert!(ids.contains(&e1.id));
    assert!(ids.contains(&e2.id));
}

#[test]
fn edges_of_incoming() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let n1 = db.create_node(sample_node("A")).unwrap();
    let n2 = db.create_node(sample_node("B")).unwrap();
    let n3 = db.create_node(sample_node("C")).unwrap();

    db.create_edge(sample_edge(n1.id, n3.id)).unwrap();
    db.create_edge(sample_edge(n2.id, n3.id)).unwrap();
    db.create_edge(sample_edge(n1.id, n2.id)).unwrap(); // not to n3... wait, this is to n2

    let inc = db.edges_of(n3.id, Direction::Incoming).unwrap();
    assert_eq!(inc.len(), 2);
}

#[test]
fn edges_of_both() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let n1 = db.create_node(sample_node("A")).unwrap();
    let n2 = db.create_node(sample_node("B")).unwrap();
    let n3 = db.create_node(sample_node("C")).unwrap();

    let e1 = db.create_edge(sample_edge(n1.id, n2.id)).unwrap(); // n2 outgoing from n1
    let e2 = db.create_edge(sample_edge(n3.id, n2.id)).unwrap(); // n2 incoming from n3

    let both = db.edges_of(n2.id, Direction::Both).unwrap();
    assert_eq!(both.len(), 2);
    let ids: Vec<u64> = both.iter().map(|e| e.id).collect();
    assert!(ids.contains(&e1.id));
    assert!(ids.contains(&e2.id));
}

#[test]
fn edges_of_empty() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let n1 = db.create_node(sample_node("Lonely")).unwrap();
    let out = db.edges_of(n1.id, Direction::Outgoing).unwrap();
    assert!(out.is_empty());
}

#[test]
fn edges_of_self_loop() {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let n1 = db.create_node(sample_node("Self")).unwrap();
    let edge = db.create_edge(sample_edge(n1.id, n1.id)).unwrap();

    let out = db.edges_of(n1.id, Direction::Outgoing).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].id, edge.id);

    let inc = db.edges_of(n1.id, Direction::Incoming).unwrap();
    assert_eq!(inc.len(), 1);
    assert_eq!(inc[0].id, edge.id);

    // Both should deduplicate if same edge appears in both out and in
    let both = db.edges_of(n1.id, Direction::Both).unwrap();
    assert_eq!(both.len(), 1);
    assert_eq!(both[0].id, edge.id);
}

// --- Persistence ---

#[test]
fn edges_persist_across_close_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    let edge_uuid;
    {
        let db = GraphNoteDb::open(&path).unwrap();
        let n1 = db.create_node(sample_node("A")).unwrap();
        let n2 = db.create_node(sample_node("B")).unwrap();
        let edge = db.create_edge(sample_edge(n1.id, n2.id)).unwrap();
        edge_uuid = edge.uuid;
        db.close().unwrap();
    }

    {
        let db = GraphNoteDb::open(&path).unwrap();
        // Edge data persists
        let edge = db.get_edge(1).unwrap().expect("edge should persist");
        assert_eq!(edge.kind, "links_to");
        assert_eq!(edge.uuid, edge_uuid);
        // UUID index persists
        let by_uuid = db
            .get_edge_by_uuid(&edge_uuid)
            .unwrap()
            .expect("uuid index should persist");
        assert_eq!(by_uuid.id, 1);
        // Adjacency lists persist
        let out = db.edges_of(1, Direction::Outgoing).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, 1);
        // Next edge ID continues
        let n1 = db.create_node(sample_node("C")).unwrap();
        let e2 = db.create_edge(sample_edge(1, n1.id)).unwrap();
        assert_eq!(e2.id, 2);
        db.close().unwrap();
    }
}

// --- Full CRUD workflow ---

#[test]
fn edge_crud_workflow() {
    let (db, n1, n2) = db_with_two_nodes();

    // Create
    let edge = db.create_edge(sample_edge(n1, n2)).unwrap();
    assert_eq!(edge.id, 1);

    // Read
    let fetched = db.get_edge(1).unwrap().unwrap();
    assert_eq!(fetched, edge);

    // Update
    let updated = db
        .update_edge(
            1,
            EdgePatch {
                kind: Some("blocks".to_string()),
                weight: Some(3.0),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(updated.kind, "blocks");
    assert_eq!(updated.weight, 3.0);

    // Verify adjacency still works after update
    let out = db.edges_of(n1, Direction::Outgoing).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].kind, "blocks");

    // Delete
    db.delete_edge(1).unwrap();
    assert_eq!(db.get_edge(1).unwrap(), None);
    let out = db.edges_of(n1, Direction::Outgoing).unwrap();
    assert!(out.is_empty());
}
