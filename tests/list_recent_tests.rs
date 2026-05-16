//! Integration tests for `Drevo::list_recent`.
//!
//! Verifies the updated_at index is correctly maintained across
//! create, update, and delete operations, and that `list_recent`
//! returns nodes ordered by most recently updated first.

use drevo::db::Drevo;
use drevo::model::{NewNode, NodePatch, Properties};

fn make_node(title: &str) -> NewNode {
    NewNode {
        kind: "note".to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn make_typed_node(kind: &str, title: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

// --- Basic behavior ---

#[test]
fn list_recent_empty_db_returns_empty() {
    let db = Drevo::open_in_memory().unwrap();
    let nodes = db.list_recent(10).unwrap();
    assert!(nodes.is_empty());
}

#[test]
fn list_recent_single_node() {
    let db = Drevo::open_in_memory().unwrap();
    let n = db.create_node(make_node("Only")).unwrap();
    let nodes = db.list_recent(10).unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].id, n.id);
}

#[test]
fn list_recent_newest_first_ordering() {
    let db = Drevo::open_in_memory().unwrap();
    let n1 = db.create_node(make_node("First")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let n2 = db.create_node(make_node("Second")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let n3 = db.create_node(make_node("Third")).unwrap();

    let nodes = db.list_recent(10).unwrap();
    assert_eq!(nodes.len(), 3);
    assert_eq!(nodes[0].id, n3.id);
    assert_eq!(nodes[1].id, n2.id);
    assert_eq!(nodes[2].id, n1.id);
}

// --- Limit ---

#[test]
fn list_recent_limit_truncates_results() {
    let db = Drevo::open_in_memory().unwrap();
    for i in 0..10 {
        db.create_node(make_node(&format!("Node {}", i))).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    let nodes = db.list_recent(5).unwrap();
    assert_eq!(nodes.len(), 5);
    // Most recently created should be first
    assert_eq!(nodes[0].title, "Node 9");
}

#[test]
fn list_recent_limit_larger_than_count() {
    let db = Drevo::open_in_memory().unwrap();
    db.create_node(make_node("A")).unwrap();
    db.create_node(make_node("B")).unwrap();

    let nodes = db.list_recent(100).unwrap();
    assert_eq!(nodes.len(), 2);
}

#[test]
fn list_recent_zero_limit_returns_empty() {
    let db = Drevo::open_in_memory().unwrap();
    db.create_node(make_node("A")).unwrap();
    let nodes = db.list_recent(0).unwrap();
    assert!(nodes.is_empty());
}

// --- Update moves node to top ---

#[test]
fn list_recent_update_node_moves_to_top() {
    let db = Drevo::open_in_memory().unwrap();
    let n1 = db.create_node(make_node("Old")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let n2 = db.create_node(make_node("New")).unwrap();

    // n2 is on top initially
    let nodes = db.list_recent(10).unwrap();
    assert_eq!(nodes[0].id, n2.id);

    std::thread::sleep(std::time::Duration::from_millis(2));

    // Update n1 — it should now be on top
    db.update_node(
        n1.id,
        NodePatch {
            body: Some("updated".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    let nodes = db.list_recent(10).unwrap();
    assert_eq!(nodes[0].id, n1.id);
    assert_eq!(nodes[1].id, n2.id);
}

#[test]
fn list_recent_multiple_updates_same_node() {
    let db = Drevo::open_in_memory().unwrap();
    let n1 = db.create_node(make_node("Target")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let _n2 = db.create_node(make_node("Other")).unwrap();

    // Update n1 multiple times — should have exactly one entry in index
    for i in 0..5 {
        std::thread::sleep(std::time::Duration::from_millis(2));
        db.update_node(
            n1.id,
            NodePatch {
                body: Some(format!("v{}", i)),
                ..Default::default()
            },
        )
        .unwrap();
    }

    let nodes = db.list_recent(10).unwrap();
    // Should still have exactly 2 nodes, not duplicates
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0].id, n1.id);
}

// --- Delete removes from index ---

#[test]
fn list_recent_delete_node_removes_from_index() {
    let db = Drevo::open_in_memory().unwrap();
    let n1 = db.create_node(make_node("Keeper")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let n2 = db.create_node(make_node("Deleted")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let n3 = db.create_node(make_node("Also kept")).unwrap();

    db.delete_node(n2.id).unwrap();

    let nodes = db.list_recent(10).unwrap();
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0].id, n3.id);
    assert_eq!(nodes[1].id, n1.id);
}

#[test]
fn list_recent_delete_all_nodes_returns_empty() {
    let db = Drevo::open_in_memory().unwrap();
    let n1 = db.create_node(make_node("A")).unwrap();
    let n2 = db.create_node(make_node("B")).unwrap();

    db.delete_node(n1.id).unwrap();
    db.delete_node(n2.id).unwrap();

    let nodes = db.list_recent(10).unwrap();
    assert!(nodes.is_empty());
}

// --- Mixed kinds ---

#[test]
fn list_recent_returns_all_kinds() {
    let db = Drevo::open_in_memory().unwrap();
    db.create_node(make_typed_node("note", "Note A")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    db.create_node(make_typed_node("task", "Task B")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    db.create_node(make_typed_node("thought", "Thought C"))
        .unwrap();

    let nodes = db.list_recent(10).unwrap();
    assert_eq!(nodes.len(), 3);
    assert_eq!(nodes[0].kind, "thought");
    assert_eq!(nodes[1].kind, "task");
    assert_eq!(nodes[2].kind, "note");
}

// --- Cascade delete removes updated index ---

#[test]
fn list_recent_cascade_delete_cleans_index() {
    let db = Drevo::open_in_memory().unwrap();
    let n1 = db.create_node(make_node("Parent")).unwrap();
    let n2 = db.create_node(make_node("Child")).unwrap();

    // Create an edge between them
    db.create_edge(drevo::model::NewEdge {
        from_id: n1.id,
        to_id: n2.id,
        kind: "links_to".to_string(),
        weight: 1.0,
        properties: Properties::default(),
    })
    .unwrap();

    // Delete parent — should cascade edges but only remove parent from recent
    db.delete_node(n1.id).unwrap();

    let nodes = db.list_recent(10).unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].id, n2.id);
}

// --- Use-case scenario: CBT Journal ---

#[test]
fn list_recent_cbt_journal_scenario() {
    let db = Drevo::open_in_memory().unwrap();

    // Patient creates several thoughts over time
    let t1 = db
        .create_node(make_typed_node("thought", "I always fail at everything"))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let _t2 = db
        .create_node(make_typed_node("emotion", "Sadness (intensity: 8)"))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let t3 = db
        .create_node(make_typed_node(
            "rational_response",
            "I succeeded at X, Y, Z recently",
        ))
        .unwrap();

    // Most recent entry shows up first
    let recent = db.list_recent(2).unwrap();
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].id, t3.id);

    std::thread::sleep(std::time::Duration::from_millis(2));

    // Patient updates old thought with a reframe
    db.update_node(
        t1.id,
        NodePatch {
            body: Some("Reframed: I have had many successes".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    // Updated thought is now most recent
    let recent = db.list_recent(1).unwrap();
    assert_eq!(recent[0].id, t1.id);
}

// --- Use-case scenario: IT Task Manager ---

#[test]
fn list_recent_task_manager_scenario() {
    let db = Drevo::open_in_memory().unwrap();

    let task1 = db
        .create_node(make_typed_node("task", "Fix login bug"))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let _task2 = db
        .create_node(make_typed_node("task", "Add dark mode"))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let task3 = db
        .create_node(make_typed_node("task", "Update dependencies"))
        .unwrap();

    // Dashboard shows 2 most recently touched tasks
    let recent = db.list_recent(2).unwrap();
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].title, "Update dependencies");
    assert_eq!(recent[1].title, "Add dark mode");

    std::thread::sleep(std::time::Duration::from_millis(2));

    // Developer starts working on task1 — update its status
    db.update_node(
        task1.id,
        NodePatch {
            body: Some("Status: in progress".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    let recent = db.list_recent(2).unwrap();
    assert_eq!(recent[0].title, "Fix login bug");
    assert_eq!(recent[1].title, "Update dependencies");

    // Complete and archive task3
    db.delete_node(task3.id).unwrap();

    let recent = db.list_recent(10).unwrap();
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].title, "Fix login bug");
    assert_eq!(recent[1].title, "Add dark mode");
}

// --- Persistence (RedbBackend) ---

#[test]
fn list_recent_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    // Create nodes and close
    {
        let db = Drevo::open(&path).unwrap();
        db.create_node(make_node("First")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        db.create_node(make_node("Second")).unwrap();
        db.close().unwrap();
    }

    // Reopen and verify list_recent works
    {
        let db = Drevo::open(&path).unwrap();
        let nodes = db.list_recent(10).unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].title, "Second");
        assert_eq!(nodes[1].title, "First");
        db.close().unwrap();
    }
}

#[test]
fn list_recent_update_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    let n1_id;
    // Create, update, close
    {
        let db = Drevo::open(&path).unwrap();
        let n1 = db.create_node(make_node("First")).unwrap();
        n1_id = n1.id;
        std::thread::sleep(std::time::Duration::from_millis(2));
        db.create_node(make_node("Second")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        db.update_node(
            n1_id,
            NodePatch {
                body: Some("updated".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        db.close().unwrap();
    }

    // Reopen and verify updated node is first
    {
        let db = Drevo::open(&path).unwrap();
        let nodes = db.list_recent(10).unwrap();
        assert_eq!(nodes[0].id, n1_id);
        db.close().unwrap();
    }
}
