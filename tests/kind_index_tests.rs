//! Integration tests for kind_index and list_nodes_by_kind (task 00012).

use drevo::db::Drevo;
use drevo::model::{NewNode, NodePatch, Properties};

fn node(kind: &str, title: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

// --- list_nodes_by_kind ---

#[test]
fn list_nodes_by_kind_empty_db_returns_empty() {
    let db = Drevo::open_in_memory().unwrap();
    let result = db.list_nodes_by_kind("note", 10, 0).unwrap();
    assert!(result.is_empty());
}

#[test]
fn list_nodes_by_kind_returns_matching_nodes() {
    let db = Drevo::open_in_memory().unwrap();
    db.create_node(node("note", "A")).unwrap();
    db.create_node(node("task", "B")).unwrap();
    db.create_node(node("note", "C")).unwrap();

    let notes = db.list_nodes_by_kind("note", 10, 0).unwrap();
    assert_eq!(notes.len(), 2);
    let titles: Vec<&str> = notes.iter().map(|n| n.title.as_str()).collect();
    assert!(titles.contains(&"A"));
    assert!(titles.contains(&"C"));
}

#[test]
fn list_nodes_by_kind_does_not_return_other_kinds() {
    let db = Drevo::open_in_memory().unwrap();
    db.create_node(node("note", "A")).unwrap();
    db.create_node(node("task", "B")).unwrap();

    let tasks = db.list_nodes_by_kind("task", 10, 0).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title, "B");
}

#[test]
fn list_nodes_by_kind_respects_limit() {
    let db = Drevo::open_in_memory().unwrap();
    for i in 0..5 {
        db.create_node(node("note", &format!("N{}", i))).unwrap();
    }

    let result = db.list_nodes_by_kind("note", 3, 0).unwrap();
    assert_eq!(result.len(), 3);
}

#[test]
fn list_nodes_by_kind_respects_offset() {
    let db = Drevo::open_in_memory().unwrap();
    for i in 0..5 {
        db.create_node(node("note", &format!("N{}", i))).unwrap();
    }

    let result = db.list_nodes_by_kind("note", 10, 2).unwrap();
    assert_eq!(result.len(), 3);
}

#[test]
fn list_nodes_by_kind_offset_beyond_count_returns_empty() {
    let db = Drevo::open_in_memory().unwrap();
    db.create_node(node("note", "A")).unwrap();

    let result = db.list_nodes_by_kind("note", 10, 100).unwrap();
    assert!(result.is_empty());
}

#[test]
fn list_nodes_by_kind_nonexistent_kind_returns_empty() {
    let db = Drevo::open_in_memory().unwrap();
    db.create_node(node("note", "A")).unwrap();

    let result = db.list_nodes_by_kind("nonexistent", 10, 0).unwrap();
    assert!(result.is_empty());
}

// --- kind_index maintenance on update_node ---

#[test]
fn update_node_kind_moves_to_new_kind_index() {
    let db = Drevo::open_in_memory().unwrap();
    let n = db.create_node(node("note", "A")).unwrap();

    db.update_node(
        n.id,
        NodePatch {
            kind: Some("task".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    let notes = db.list_nodes_by_kind("note", 10, 0).unwrap();
    assert!(notes.is_empty());

    let tasks = db.list_nodes_by_kind("task", 10, 0).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title, "A");
}

#[test]
fn update_node_kind_unchanged_keeps_index() {
    let db = Drevo::open_in_memory().unwrap();
    let n = db.create_node(node("note", "A")).unwrap();

    db.update_node(
        n.id,
        NodePatch {
            title: Some("B".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    let notes = db.list_nodes_by_kind("note", 10, 0).unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].title, "B");
}

// --- kind_index maintenance on delete_node ---

#[test]
fn delete_node_removes_from_kind_index() {
    let db = Drevo::open_in_memory().unwrap();
    let n = db.create_node(node("note", "A")).unwrap();

    db.delete_node(n.id).unwrap();

    let notes = db.list_nodes_by_kind("note", 10, 0).unwrap();
    assert!(notes.is_empty());
}

#[test]
fn delete_one_node_keeps_others_in_kind_index() {
    let db = Drevo::open_in_memory().unwrap();
    let n1 = db.create_node(node("note", "A")).unwrap();
    db.create_node(node("note", "B")).unwrap();

    db.delete_node(n1.id).unwrap();

    let notes = db.list_nodes_by_kind("note", 10, 0).unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].title, "B");
}

// --- edge kind index: list_edges_by_kind ---

#[test]
fn list_edges_by_kind_returns_matching_edges() {
    let db = Drevo::open_in_memory().unwrap();
    let n1 = db.create_node(node("note", "A")).unwrap();
    let n2 = db.create_node(node("note", "B")).unwrap();
    let n3 = db.create_node(node("note", "C")).unwrap();

    use drevo::model::{NewEdge, Properties as P};
    db.create_edge(NewEdge {
        from_id: n1.id,
        to_id: n2.id,
        kind: "links_to".to_string(),
        weight: 1.0,
        properties: P::default(),
    })
    .unwrap();
    db.create_edge(NewEdge {
        from_id: n2.id,
        to_id: n3.id,
        kind: "tagged_with".to_string(),
        weight: 1.0,
        properties: P::default(),
    })
    .unwrap();
    db.create_edge(NewEdge {
        from_id: n1.id,
        to_id: n3.id,
        kind: "links_to".to_string(),
        weight: 0.5,
        properties: P::default(),
    })
    .unwrap();

    let links = db.list_edges_by_kind("links_to", 10, 0).unwrap();
    assert_eq!(links.len(), 2);

    let tags = db.list_edges_by_kind("tagged_with", 10, 0).unwrap();
    assert_eq!(tags.len(), 1);
}

#[test]
fn list_edges_by_kind_respects_limit_and_offset() {
    let db = Drevo::open_in_memory().unwrap();
    let n1 = db.create_node(node("note", "A")).unwrap();
    let n2 = db.create_node(node("note", "B")).unwrap();

    use drevo::model::{NewEdge, Properties as P};
    for i in 0..5 {
        db.create_edge(NewEdge {
            from_id: n1.id,
            to_id: n2.id,
            kind: "link".to_string(),
            weight: i as f32,
            properties: P::default(),
        })
        .unwrap();
    }

    let result = db.list_edges_by_kind("link", 2, 1).unwrap();
    assert_eq!(result.len(), 2);
}

#[test]
fn delete_edge_removes_from_edge_kind_index() {
    let db = Drevo::open_in_memory().unwrap();
    let n1 = db.create_node(node("note", "A")).unwrap();
    let n2 = db.create_node(node("note", "B")).unwrap();

    use drevo::model::{NewEdge, Properties as P};
    let edge = db
        .create_edge(NewEdge {
            from_id: n1.id,
            to_id: n2.id,
            kind: "links_to".to_string(),
            weight: 1.0,
            properties: P::default(),
        })
        .unwrap();

    db.delete_edge(edge.id).unwrap();

    let links = db.list_edges_by_kind("links_to", 10, 0).unwrap();
    assert!(links.is_empty());
}

// --- disk-backed persistence ---

#[test]
fn kind_index_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    {
        let db = Drevo::open(&path).unwrap();
        db.create_node(node("note", "A")).unwrap();
        db.create_node(node("task", "B")).unwrap();
        db.create_node(node("note", "C")).unwrap();
        db.close().unwrap();
    }

    {
        let db = Drevo::open(&path).unwrap();
        let notes = db.list_nodes_by_kind("note", 10, 0).unwrap();
        assert_eq!(notes.len(), 2);
        let tasks = db.list_nodes_by_kind("task", 10, 0).unwrap();
        assert_eq!(tasks.len(), 1);
        db.close().unwrap();
    }
}
