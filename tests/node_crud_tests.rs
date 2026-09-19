//! Integration tests for Node CRUD operations (task 00010).

use drevo::db::Drevo;
use drevo::error::DrevoError;
use drevo::model::{Direction, NewEdge, NewNode, NodePatch, Properties};
use serde_json::json;
use std::collections::HashMap;

fn sample_node(title: &str) -> NewNode {
    NewNode {
        kind: "note".to_string(),
        title: title.to_string(),
        body: "# Hello".to_string(),
        body_html: "<h1>Hello</h1>".to_string(),
        properties: Properties::default(),
    }
}

fn sample_node_with_props(title: &str) -> NewNode {
    let mut props = HashMap::new();
    props.insert("priority".to_string(), json!(1));
    props.insert("tags".to_string(), json!(["rust", "graph"]));
    NewNode {
        kind: "task".to_string(),
        title: title.to_string(),
        body: "Task body".to_string(),
        body_html: "<p>Task body</p>".to_string(),
        properties: Properties::from(props),
    }
}

// --- create_node ---

#[test]
fn create_node_returns_node_with_id() {
    let db = Drevo::open_in_memory().unwrap();
    let node = db.create_node(sample_node("First")).unwrap();
    assert_eq!(node.id, 1);
    assert_eq!(node.title, "First");
    assert_eq!(node.kind, "note");
}

#[test]
fn create_node_assigns_sequential_ids() {
    let db = Drevo::open_in_memory().unwrap();
    let n1 = db.create_node(sample_node("A")).unwrap();
    let n2 = db.create_node(sample_node("B")).unwrap();
    let n3 = db.create_node(sample_node("C")).unwrap();
    assert_eq!(n1.id, 1);
    assert_eq!(n2.id, 2);
    assert_eq!(n3.id, 3);
}

#[test]
fn create_node_generates_uuid_v7() {
    let db = Drevo::open_in_memory().unwrap();
    let node = db.create_node(sample_node("UUID test")).unwrap();
    let uuid = uuid::Uuid::from_bytes(node.uuid);
    assert_eq!(uuid.get_version(), Some(uuid::Version::SortRand));
}

#[test]
fn create_node_preserves_properties() {
    let db = Drevo::open_in_memory().unwrap();
    let node = db.create_node(sample_node_with_props("Props")).unwrap();
    assert_eq!(node.properties.get("priority"), Some(&json!(1)));
    assert_eq!(node.properties.get("tags"), Some(&json!(["rust", "graph"])));
}

#[test]
fn create_node_duplicate_title_fails() {
    let db = Drevo::open_in_memory().unwrap();
    db.create_node(sample_node("Dup")).unwrap();
    let err = db.create_node(sample_node("Dup")).unwrap_err();
    assert!(matches!(err, DrevoError::DuplicateTitle(t) if t == "Dup"));
}

// --- get_node ---

#[test]
fn get_node_existing() {
    let db = Drevo::open_in_memory().unwrap();
    let created = db.create_node(sample_node("Get me")).unwrap();
    let fetched = db.get_node(created.id).unwrap();
    assert_eq!(fetched, Some(created));
}

#[test]
fn get_node_nonexistent_returns_none() {
    let db = Drevo::open_in_memory().unwrap();
    assert_eq!(db.get_node(999).unwrap(), None);
}

// --- get_node_by_uuid ---

#[test]
fn get_node_by_uuid_existing() {
    let db = Drevo::open_in_memory().unwrap();
    let created = db.create_node(sample_node("UUID lookup")).unwrap();
    let fetched = db.get_node_by_uuid(&created.uuid).unwrap();
    assert_eq!(fetched, Some(created));
}

#[test]
fn get_node_by_uuid_nonexistent_returns_none() {
    let db = Drevo::open_in_memory().unwrap();
    let fake_uuid = [0u8; 16];
    assert_eq!(db.get_node_by_uuid(&fake_uuid).unwrap(), None);
}

// --- get_node_by_title ---

#[test]
fn get_node_by_title_existing() {
    let db = Drevo::open_in_memory().unwrap();
    let created = db.create_node(sample_node("Title lookup")).unwrap();
    let fetched = db.get_node_by_title("Title lookup").unwrap();
    assert_eq!(fetched, Some(created));
}

#[test]
fn get_node_by_title_nonexistent_returns_none() {
    let db = Drevo::open_in_memory().unwrap();
    assert_eq!(db.get_node_by_title("no such title").unwrap(), None);
}

// --- update_node ---

#[test]
fn update_node_changes_title() {
    let db = Drevo::open_in_memory().unwrap();
    let node = db.create_node(sample_node("Old Title")).unwrap();

    let updated = db
        .update_node(
            node.id,
            NodePatch {
                title: Some("New Title".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(updated.title, "New Title");
    assert_eq!(updated.id, node.id);
    // Old title index should be removed
    assert_eq!(db.get_node_by_title("Old Title").unwrap(), None);
    // New title index should work
    assert_eq!(db.get_node_by_title("New Title").unwrap(), Some(updated));
}

#[test]
fn update_node_partial_patch() {
    let db = Drevo::open_in_memory().unwrap();
    let node = db.create_node(sample_node("Partial")).unwrap();

    let updated = db
        .update_node(
            node.id,
            NodePatch {
                body: Some("Updated body".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(updated.body, "Updated body");
    assert_eq!(updated.title, "Partial"); // unchanged
    assert_eq!(updated.kind, "note"); // unchanged
}

#[test]
fn update_node_updates_timestamp() {
    let db = Drevo::open_in_memory().unwrap();
    let node = db.create_node(sample_node("Timestamp")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));

    let updated = db
        .update_node(
            node.id,
            NodePatch {
                body: Some("changed".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

    assert!(updated.updated_at > node.updated_at);
}

#[test]
fn update_node_nonexistent_fails() {
    let db = Drevo::open_in_memory().unwrap();
    let err = db
        .update_node(
            999,
            NodePatch {
                title: Some("X".to_string()),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(matches!(err, DrevoError::NodeNotFound(999)));
}

#[test]
fn update_node_duplicate_title_fails() {
    let db = Drevo::open_in_memory().unwrap();
    db.create_node(sample_node("Existing")).unwrap();
    let node2 = db.create_node(sample_node("Other")).unwrap();

    let err = db
        .update_node(
            node2.id,
            NodePatch {
                title: Some("Existing".to_string()),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(matches!(err, DrevoError::DuplicateTitle(t) if t == "Existing"));
}

#[test]
fn update_node_same_title_is_ok() {
    let db = Drevo::open_in_memory().unwrap();
    let node = db.create_node(sample_node("Keep")).unwrap();

    // Updating to the same title should succeed
    let updated = db
        .update_node(
            node.id,
            NodePatch {
                title: Some("Keep".to_string()),
                body: Some("new body".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(updated.title, "Keep");
    assert_eq!(updated.body, "new body");
}

// --- delete_node ---

#[test]
fn delete_node_removes_from_storage() {
    let db = Drevo::open_in_memory().unwrap();
    let node = db.create_node(sample_node("Delete me")).unwrap();

    db.delete_node(node.id).unwrap();

    assert_eq!(db.get_node(node.id).unwrap(), None);
}

#[test]
fn delete_node_removes_indexes() {
    let db = Drevo::open_in_memory().unwrap();
    let node = db.create_node(sample_node("Indexed")).unwrap();
    let uuid = node.uuid;

    db.delete_node(node.id).unwrap();

    assert_eq!(db.get_node_by_title("Indexed").unwrap(), None);
    assert_eq!(db.get_node_by_uuid(&uuid).unwrap(), None);
}

#[test]
fn delete_node_nonexistent_fails() {
    let db = Drevo::open_in_memory().unwrap();
    let err = db.delete_node(999).unwrap_err();
    assert!(matches!(err, DrevoError::NodeNotFound(999)));
}

// --- delete_nodes: batched multi-delete (#441) ---
//
// The motivating workflow: a note-taking app deletes a folder subtree (the
// folder plus its child notes and the `contains` edges) in one call, instead
// of a per-node `delete_node` loop that costs one fsync per node.

fn note(title: &str, body: &str) -> NewNode {
    NewNode {
        kind: "note".to_string(),
        title: title.to_string(),
        body: body.to_string(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

#[test]
fn delete_nodes_removes_folder_subtree_in_one_call() {
    let db = Drevo::open_in_memory().unwrap();
    let folder = db.create_node(note("Folder", "")).unwrap();
    let mut child_ids = Vec::new();
    for i in 0..5 {
        let child = db
            .create_node(note(&format!("Note {i}"), "shared keyword body"))
            .unwrap();
        db.create_edge(NewEdge {
            from_id: folder.id,
            to_id: child.id,
            kind: "contains".into(),
            weight: 1.0,
            properties: Default::default(),
        })
        .unwrap();
        child_ids.push(child.id);
    }

    // One call deletes the folder and every child.
    let mut targets = child_ids.clone();
    targets.push(folder.id);
    let removed = db.delete_nodes(&targets).unwrap();
    assert_eq!(removed, 6);

    assert_eq!(db.get_node(folder.id).unwrap(), None);
    for id in &child_ids {
        assert_eq!(db.get_node(*id).unwrap(), None);
    }
    // No dangling adjacency and no stray full-text postings.
    assert!(db.search_fts("keyword", 10).unwrap().is_empty());
}

#[test]
fn delete_nodes_leaves_untargeted_siblings_and_their_index_intact() {
    // A batch delete of one subtree must not disturb a sibling that shares
    // full-text tokens with the deleted nodes.
    let db = Drevo::open_in_memory().unwrap();
    let doomed = db.create_node(note("doomed", "apricot marmalade")).unwrap();
    let kept = db.create_node(note("kept", "apricot preserve")).unwrap();
    let link = db
        .create_edge(NewEdge {
            from_id: doomed.id,
            to_id: kept.id,
            kind: "links_to".into(),
            weight: 1.0,
            properties: Default::default(),
        })
        .unwrap();

    assert_eq!(db.delete_nodes(&[doomed.id]).unwrap(), 1);

    // The survivor and its adjacency-free FTS entry are untouched.
    assert!(db.get_node(kept.id).unwrap().is_some());
    assert_eq!(db.get_edge(link.id).unwrap(), None);
    let hits = db.search_fts("apricot", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node.id, kept.id);
    assert!(db.edges_of(kept.id, Direction::Both).unwrap().is_empty());
}

#[test]
fn delete_nodes_matches_sequential_loop_end_state() {
    // Build the same graph twice; delete a subtree via the batched call in one
    // DB and a per-node loop in the other, then assert identical survivor state.
    fn build(db: &Drevo) -> (Vec<u64>, Vec<u64>) {
        let mut doomed = Vec::new();
        let mut kept = Vec::new();
        for i in 0..4 {
            let d = db
                .create_node(note(&format!("doomed {i}"), "quokka thicket"))
                .unwrap();
            doomed.push(d.id);
        }
        for i in 0..3 {
            let k = db
                .create_node(note(&format!("kept {i}"), "quokka meadow"))
                .unwrap();
            kept.push(k.id);
        }
        // Edges within the doomed set and crossing into the kept set.
        db.create_edge(NewEdge {
            from_id: doomed[0],
            to_id: doomed[1],
            kind: "links_to".into(),
            weight: 1.0,
            properties: Default::default(),
        })
        .unwrap();
        db.create_edge(NewEdge {
            from_id: doomed[2],
            to_id: kept[0],
            kind: "links_to".into(),
            weight: 1.0,
            properties: Default::default(),
        })
        .unwrap();
        (doomed, kept)
    }

    let batched = Drevo::open_in_memory().unwrap();
    let (doomed_b, kept_b) = build(&batched);
    let sequential = Drevo::open_in_memory().unwrap();
    let (doomed_s, kept_s) = build(&sequential);

    batched.delete_nodes(&doomed_b).unwrap();
    for id in &doomed_s {
        sequential.delete_node(*id).unwrap();
    }

    // Same survivors, same full-text index, same edge-free survivor adjacency.
    for (kb, ks) in kept_b.iter().zip(kept_s.iter()) {
        assert_eq!(
            batched.get_node(*kb).unwrap().map(|n| n.title),
            sequential.get_node(*ks).unwrap().map(|n| n.title),
        );
    }
    assert_eq!(
        batched.search_fts("quokka", 10).unwrap().len(),
        sequential.search_fts("quokka", 10).unwrap().len(),
    );
    for id in &doomed_b {
        assert_eq!(batched.get_node(*id).unwrap(), None);
    }
}

// --- Persistence across close/reopen ---

#[test]
fn nodes_persist_across_close_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    // Create a node, close
    let uuid;
    {
        let db = Drevo::open(&path).unwrap();
        let node = db.create_node(sample_node("Persistent")).unwrap();
        uuid = node.uuid;
        db.close().unwrap();
    }

    // Reopen and verify
    {
        let db = Drevo::open(&path).unwrap();
        let node = db.get_node(1).unwrap().expect("node should persist");
        assert_eq!(node.title, "Persistent");
        assert_eq!(node.uuid, uuid);
        // Title index should also persist
        let by_title = db
            .get_node_by_title("Persistent")
            .unwrap()
            .expect("title index should persist");
        assert_eq!(by_title.id, 1);
        // UUID index should also persist
        let by_uuid = db
            .get_node_by_uuid(&uuid)
            .unwrap()
            .expect("uuid index should persist");
        assert_eq!(by_uuid.id, 1);
        // Next ID should continue from 2
        let node2 = db.create_node(sample_node("Second")).unwrap();
        assert_eq!(node2.id, 2);
        db.close().unwrap();
    }
}

// --- Edge case: empty title ---

#[test]
fn create_node_with_empty_title() {
    let db = Drevo::open_in_memory().unwrap();
    let node = db
        .create_node(NewNode {
            kind: "note".to_string(),
            title: String::new(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .unwrap();
    assert_eq!(node.title, "");
    // Should be retrievable by empty title
    let fetched = db.get_node_by_title("").unwrap();
    assert_eq!(fetched, Some(node));
}

// --- Multiple operations ---

#[test]
fn crud_workflow() {
    let db = Drevo::open_in_memory().unwrap();

    // Create
    let node = db.create_node(sample_node("Workflow")).unwrap();
    assert_eq!(node.id, 1);

    // Read
    let fetched = db.get_node(1).unwrap().unwrap();
    assert_eq!(fetched, node);

    // Update
    let updated = db
        .update_node(
            1,
            NodePatch {
                title: Some("Updated Workflow".to_string()),
                body: Some("New body".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(updated.title, "Updated Workflow");
    assert_eq!(updated.body, "New body");

    // Verify old title gone, new title works
    assert_eq!(db.get_node_by_title("Workflow").unwrap(), None);
    assert_eq!(
        db.get_node_by_title("Updated Workflow").unwrap(),
        Some(updated.clone())
    );

    // Delete
    db.delete_node(1).unwrap();
    assert_eq!(db.get_node(1).unwrap(), None);
    assert_eq!(db.get_node_by_title("Updated Workflow").unwrap(), None);
}
