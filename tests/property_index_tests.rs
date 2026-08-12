//! Integration tests for Phase 14 task `00088` — persistent property index.
//!
//! Task `00085`–`00087` built the cost-based planner that *chooses* a
//! `NodeIndexSeek`; `00088` is the durable index that makes that choice
//! real. A `(property key, value) -> node ids` map is maintained on every
//! node mutation (create / update / delete / import / transaction
//! rollback) alongside the kind and FTS indexes, and surfaced through
//! [`Drevo::nodes_by_property`] / [`Drevo::count_nodes_by_property`].
//!
//! These tests exercise the contract end to end against the real graph
//! store — including the redb backend, so the index is shown to survive a
//! close/reopen — and across the domain workflows drevo targets (bug
//! tracker statuses, task-manager priorities, ERP records).

use drevo::db::Drevo;
use drevo::model::{NewNode, NodePatch, Properties};
use serde_json::{json, Value};
use std::collections::HashMap;

/// Build a `NewNode` with the given kind, title, and property pairs.
fn node(kind: &str, title: &str, props: &[(&str, Value)]) -> NewNode {
    let mut map = HashMap::new();
    for (k, v) in props {
        map.insert((*k).to_string(), v.clone());
    }
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties(map),
    }
}

/// Sorted ids of the nodes returned for a `(key, value)` lookup.
fn lookup_ids(db: &Drevo, key: &str, value: Value) -> Vec<u64> {
    let mut ids: Vec<u64> = db
        .nodes_by_property(key, &value)
        .unwrap()
        .into_iter()
        .map(|n| n.id)
        .collect();
    ids.sort_unstable();
    ids
}

// ---------------------------------------------------------------
// Create + lookup
// ---------------------------------------------------------------

#[test]
fn lookup_on_empty_db_returns_nothing() {
    let db = Drevo::open_in_memory().unwrap();
    assert!(db
        .nodes_by_property("status", &json!("open"))
        .unwrap()
        .is_empty());
    assert_eq!(
        db.count_nodes_by_property("status", &json!("open"))
            .unwrap(),
        0
    );
}

#[test]
fn created_node_is_findable_by_its_property() {
    let db = Drevo::open_in_memory().unwrap();
    let bug = db
        .create_node(node("bug", "Crash on save", &[("status", json!("open"))]))
        .unwrap();

    let found = db.nodes_by_property("status", &json!("open")).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, bug.id);
}

#[test]
fn lookup_returns_every_matching_node() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db
        .create_node(node("bug", "A", &[("status", json!("open"))]))
        .unwrap();
    let b = db
        .create_node(node("bug", "B", &[("status", json!("open"))]))
        .unwrap();
    let _c = db
        .create_node(node("bug", "C", &[("status", json!("closed"))]))
        .unwrap();

    assert_eq!(lookup_ids(&db, "status", json!("open")), vec![a.id, b.id]);
    assert_eq!(
        db.count_nodes_by_property("status", &json!("open"))
            .unwrap(),
        2
    );
}

#[test]
fn lookup_distinguishes_different_values_of_a_key() {
    let db = Drevo::open_in_memory().unwrap();
    let p1 = db
        .create_node(node("task", "T1", &[("priority", json!(1))]))
        .unwrap();
    let p2 = db
        .create_node(node("task", "T2", &[("priority", json!(2))]))
        .unwrap();

    assert_eq!(lookup_ids(&db, "priority", json!(1)), vec![p1.id]);
    assert_eq!(lookup_ids(&db, "priority", json!(2)), vec![p2.id]);
}

#[test]
fn lookup_distinguishes_json_types() {
    let db = Drevo::open_in_memory().unwrap();
    // Same key "v", values that are equal-ish across types must not collide.
    let n_int = db
        .create_node(node("x", "int", &[("v", json!(1))]))
        .unwrap();
    let n_str = db
        .create_node(node("x", "str", &[("v", json!("1"))]))
        .unwrap();
    let n_bool = db
        .create_node(node("x", "bool", &[("v", json!(true))]))
        .unwrap();

    assert_eq!(lookup_ids(&db, "v", json!(1)), vec![n_int.id]);
    assert_eq!(lookup_ids(&db, "v", json!("1")), vec![n_str.id]);
    assert_eq!(lookup_ids(&db, "v", json!(true)), vec![n_bool.id]);
}

#[test]
fn object_values_match_regardless_of_key_order() {
    let db = Drevo::open_in_memory().unwrap();
    let n = db
        .create_node(node("x", "n", &[("meta", json!({"a": 1, "b": 2}))]))
        .unwrap();
    // Querying with the keys written in the opposite order still matches
    // because values are canonicalized to sorted-key JSON.
    assert_eq!(lookup_ids(&db, "meta", json!({"b": 2, "a": 1})), vec![n.id]);
}

#[test]
fn node_with_multiple_properties_is_indexed_under_each() {
    let db = Drevo::open_in_memory().unwrap();
    let n = db
        .create_node(node(
            "task",
            "Important",
            &[("priority", json!(1)), ("assignee", json!("alice"))],
        ))
        .unwrap();

    assert_eq!(lookup_ids(&db, "priority", json!(1)), vec![n.id]);
    assert_eq!(lookup_ids(&db, "assignee", json!("alice")), vec![n.id]);
}

// ---------------------------------------------------------------
// Update
// ---------------------------------------------------------------

#[test]
fn updating_a_property_moves_it_in_the_index() {
    let db = Drevo::open_in_memory().unwrap();
    let bug = db
        .create_node(node("bug", "Flaky test", &[("status", json!("open"))]))
        .unwrap();

    let mut map = HashMap::new();
    map.insert("status".to_string(), json!("closed"));
    db.update_node(
        bug.id,
        NodePatch {
            properties: Some(Properties(map)),
            ..Default::default()
        },
    )
    .unwrap();

    assert!(lookup_ids(&db, "status", json!("open")).is_empty());
    assert_eq!(lookup_ids(&db, "status", json!("closed")), vec![bug.id]);
}

#[test]
fn removing_a_property_via_update_deindexes_it() {
    let db = Drevo::open_in_memory().unwrap();
    let n = db
        .create_node(node(
            "task",
            "T",
            &[("priority", json!(1)), ("tag", json!("x"))],
        ))
        .unwrap();

    // Replace properties with a map that drops "tag".
    let mut map = HashMap::new();
    map.insert("priority".to_string(), json!(1));
    db.update_node(
        n.id,
        NodePatch {
            properties: Some(Properties(map)),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(lookup_ids(&db, "priority", json!(1)), vec![n.id]);
    assert!(lookup_ids(&db, "tag", json!("x")).is_empty());
}

#[test]
fn update_that_does_not_touch_properties_keeps_the_index() {
    let db = Drevo::open_in_memory().unwrap();
    let n = db
        .create_node(node("bug", "Title", &[("status", json!("open"))]))
        .unwrap();

    // Patch only the title; the property index must be untouched.
    db.update_node(
        n.id,
        NodePatch {
            title: Some("New title".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(lookup_ids(&db, "status", json!("open")), vec![n.id]);
}

// ---------------------------------------------------------------
// Delete + cascade
// ---------------------------------------------------------------

#[test]
fn deleting_a_node_removes_it_from_the_index() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db
        .create_node(node("bug", "A", &[("status", json!("open"))]))
        .unwrap();
    let b = db
        .create_node(node("bug", "B", &[("status", json!("open"))]))
        .unwrap();

    db.delete_node(a.id).unwrap();

    assert_eq!(lookup_ids(&db, "status", json!("open")), vec![b.id]);
}

#[test]
fn deleting_the_last_match_leaves_an_empty_lookup() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db
        .create_node(node("bug", "A", &[("status", json!("open"))]))
        .unwrap();
    db.delete_node(a.id).unwrap();
    assert!(db
        .nodes_by_property("status", &json!("open"))
        .unwrap()
        .is_empty());
}

// ---------------------------------------------------------------
// Transaction rollback restores the index
// ---------------------------------------------------------------

#[test]
fn rolling_back_a_create_unindexes_the_property() {
    let db = Drevo::open_in_memory().unwrap();
    let tx = db.begin_transaction();
    let _n = db
        .create_node(node("bug", "Temp", &[("status", json!("open"))]))
        .unwrap();
    tx.rollback().unwrap();

    assert!(db
        .nodes_by_property("status", &json!("open"))
        .unwrap()
        .is_empty());
}

#[test]
fn rolling_back_a_property_update_restores_the_old_value() {
    let db = Drevo::open_in_memory().unwrap();
    let bug = db
        .create_node(node("bug", "B", &[("status", json!("open"))]))
        .unwrap();

    let tx = db.begin_transaction();
    let mut map = HashMap::new();
    map.insert("status".to_string(), json!("closed"));
    db.update_node(
        bug.id,
        NodePatch {
            properties: Some(Properties(map)),
            ..Default::default()
        },
    )
    .unwrap();
    tx.rollback().unwrap();

    assert_eq!(lookup_ids(&db, "status", json!("open")), vec![bug.id]);
    assert!(lookup_ids(&db, "status", json!("closed")).is_empty());
}

#[test]
fn rolling_back_a_delete_reindexes_the_property() {
    let db = Drevo::open_in_memory().unwrap();
    let bug = db
        .create_node(node("bug", "B", &[("status", json!("open"))]))
        .unwrap();

    let tx = db.begin_transaction();
    db.delete_node(bug.id).unwrap();
    tx.rollback().unwrap();

    assert_eq!(lookup_ids(&db, "status", json!("open")), vec![bug.id]);
}

// ---------------------------------------------------------------
// Domain workflow: bug-tracker triage by status
// ---------------------------------------------------------------

#[test]
fn bug_tracker_triage_by_status() {
    let db = Drevo::open_in_memory().unwrap();
    for (title, status) in [
        ("Login fails", "open"),
        ("Slow query", "open"),
        ("Typo in footer", "closed"),
        ("Memory leak", "in_progress"),
    ] {
        db.create_node(node("bug", title, &[("status", json!(status))]))
            .unwrap();
    }

    assert_eq!(
        db.count_nodes_by_property("status", &json!("open"))
            .unwrap(),
        2
    );
    assert_eq!(
        db.count_nodes_by_property("status", &json!("closed"))
            .unwrap(),
        1
    );
    assert_eq!(
        db.count_nodes_by_property("status", &json!("in_progress"))
            .unwrap(),
        1
    );

    let open_titles: Vec<String> = {
        let mut v: Vec<String> = db
            .nodes_by_property("status", &json!("open"))
            .unwrap()
            .into_iter()
            .map(|n| n.title)
            .collect();
        v.sort();
        v
    };
    assert_eq!(open_titles, vec!["Login fails", "Slow query"]);
}

// ---------------------------------------------------------------
// redb backend: the index is durable across a close/reopen
// ---------------------------------------------------------------

#[cfg(feature = "redb-backend")]
#[test]
fn property_index_survives_reopen_on_redb() {
    use tempfile::TempDir;

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("graph.redb");

    let open_id;
    {
        let db = Drevo::open(&path).unwrap();
        let open_bug = db
            .create_node(node("bug", "Persisted", &[("status", json!("open"))]))
            .unwrap();
        let _closed = db
            .create_node(node("bug", "Done", &[("status", json!("closed"))]))
            .unwrap();
        open_id = open_bug.id;
    }

    // Reopen: the persisted property index must answer the same lookup
    // without any rebuild step.
    let db = Drevo::open(&path).unwrap();
    let found = db.nodes_by_property("status", &json!("open")).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, open_id);
    assert_eq!(
        db.count_nodes_by_property("status", &json!("closed"))
            .unwrap(),
        1
    );
}
