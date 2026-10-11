//! UNIQUE node-property constraints, enforced on every write (issue #604).
//!
//! `Constraint::UniqueNodeProperty { kind, property }` covers every node
//! carrying the label, primary or secondary. `title` / `body` read the node's
//! own fields unless the property map has its own entry. Nodes without the
//! value are not constrained. A violating write fails at once with
//! `CoreError::ConstraintViolation` and changes nothing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use drevo_core::engine::GraphEngine;
use drevo_core::error::CoreError;
use drevo_core::model::{NewNode, NodePatch, Properties};
use drevo_core::native::{Constraint, NativeGraph};
use serde_json::{json, Value};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn node(kind: &str, title: &str, props: Value) -> NewNode {
    let Value::Object(map) = props else {
        panic!("props must be an object")
    };
    NewNode {
        kind: kind.into(),
        title: title.into(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties(map.into_iter().collect::<HashMap<_, _>>()),
    }
}

fn unique(kind: &str, property: &str) -> Constraint {
    Constraint::UniqueNodeProperty {
        kind: kind.into(),
        property: property.into(),
    }
}

fn is_violation<T: std::fmt::Debug>(r: &drevo_core::error::Result<T>) -> bool {
    matches!(r, Err(CoreError::ConstraintViolation(_)))
}

#[test]
fn a_duplicate_create_fails_and_writes_nothing() {
    let g = NativeGraph::new();
    g.add_constraint(unique("User", "email")).unwrap();
    g.create_node(node("User", "u1", json!({"email": "a@x"})))
        .unwrap();
    let dup = g.create_node(node("User", "u2", json!({"email": "a@x"})));
    assert!(is_violation(&dup), "{dup:?}");
    assert_eq!(g.node_count(), 1);
    // Other values, other labels and nodes without the property are fine.
    g.create_node(node("User", "u3", json!({"email": "b@x"})))
        .unwrap();
    g.create_node(node("Admin", "a1", json!({"email": "a@x"})))
        .unwrap();
    g.create_node(node("User", "u4", json!({}))).unwrap();
    g.create_node(node("User", "u5", json!({}))).unwrap();
    assert_eq!(g.node_count(), 5);
}

#[test]
fn updates_are_checked_against_other_nodes_only() {
    let g = NativeGraph::new();
    g.add_constraint(unique("User", "email")).unwrap();
    let a = g
        .create_node(node("User", "u1", json!({"email": "a@x"})))
        .unwrap();
    let b = g
        .create_node(node("User", "u2", json!({"email": "b@x"})))
        .unwrap();
    // Re-saving a node with its own value is not a clash.
    g.update_node(
        a.id,
        NodePatch {
            properties: Some(Properties(
                [
                    ("email".to_string(), json!("a@x")),
                    ("n".to_string(), json!(1)),
                ]
                .into(),
            )),
            ..NodePatch::default()
        },
    )
    .unwrap();
    let clash = g.update_node(
        b.id,
        NodePatch {
            properties: Some(Properties([("email".to_string(), json!("a@x"))].into())),
            ..NodePatch::default()
        },
    );
    assert!(is_violation(&clash), "{clash:?}");
    assert_eq!(
        g.get_node(b.id).unwrap().unwrap().properties.0["email"],
        json!("b@x"),
        "the rejected update changed nothing"
    );
    // Moving a node onto the label is checked too.
    let c = g
        .create_node(node("Guest", "g1", json!({"email": "b@x"})))
        .unwrap();
    let relabel = g.update_node(
        c.id,
        NodePatch {
            kind: Some("User".into()),
            ..NodePatch::default()
        },
    );
    assert!(is_violation(&relabel), "{relabel:?}");
}

#[test]
fn deleting_or_changing_frees_the_value() {
    let g = NativeGraph::new();
    g.add_constraint(unique("User", "email")).unwrap();
    let a = g
        .create_node(node("User", "u1", json!({"email": "a@x"})))
        .unwrap();
    g.delete_node(a.id).unwrap();
    let b = g
        .create_node(node("User", "u2", json!({"email": "a@x"})))
        .unwrap();
    g.update_node(
        b.id,
        NodePatch {
            properties: Some(Properties([("email".to_string(), json!("c@x"))].into())),
            ..NodePatch::default()
        },
    )
    .unwrap();
    g.create_node(node("User", "u3", json!({"email": "a@x"})))
        .unwrap();
}

#[test]
fn secondary_labels_and_title_are_covered() {
    let g = NativeGraph::new();
    g.add_constraint(unique("Tag", "title")).unwrap();
    g.create_node(node("Tag", "rust", json!({}))).unwrap();
    // The same title on a node whose secondary label is Tag.
    let secondary = g.create_node(node("Note", "rust", json!({"_labels": ["Tag"]})));
    assert!(is_violation(&secondary), "{secondary:?}");
    // Without the label, the same title is fine; an empty title is no value.
    g.create_node(node("Note", "rust", json!({}))).unwrap();
    g.create_node(node("Tag", "", json!({}))).unwrap();
    g.create_node(node("Tag", "", json!({}))).unwrap();
}

#[test]
fn values_compare_by_type() {
    let g = NativeGraph::new();
    g.add_constraint(unique("Item", "code")).unwrap();
    g.create_node(node("Item", "i1", json!({"code": 1})))
        .unwrap();
    g.create_node(node("Item", "i2", json!({"code": "1"})))
        .unwrap();
    assert!(is_violation(&g.create_node(node(
        "Item",
        "i3",
        json!({"code": 1})
    ))));
}

#[test]
fn batches_and_transactions_are_checked() {
    let g = NativeGraph::new();
    g.add_constraint(unique("User", "email")).unwrap();
    let batch = g.create_nodes(vec![
        node("User", "u1", json!({"email": "a@x"})),
        node("User", "u2", json!({"email": "a@x"})),
    ]);
    assert!(is_violation(&batch), "{batch:?}");
    assert_eq!(g.node_count(), 0, "the batch is all-or-nothing");

    let mut tx = g.begin();
    tx.create_node(node("User", "u1", json!({"email": "a@x"})))
        .unwrap();
    let dup = tx.create_node(node("User", "u2", json!({"email": "a@x"})));
    assert!(is_violation(&dup), "{dup:?}");
    tx.commit().unwrap();
    assert_eq!(g.node_count(), 1);
}

#[test]
fn constraints_can_be_listed_and_removed() {
    let g = NativeGraph::new();
    g.add_constraint(unique("User", "email")).unwrap();
    assert_eq!(g.constraints(), vec![unique("User", "email")]);
    g.create_node(node("User", "u1", json!({"email": "a@x"})))
        .unwrap();
    assert!(g.remove_constraint(&unique("User", "email")));
    assert!(!g.remove_constraint(&unique("User", "email")));
    g.create_node(node("User", "u2", json!({"email": "a@x"})))
        .unwrap();
    // Adding it back now fails: the data violates it.
    assert!(g.add_constraint(unique("User", "email")).is_err());
    assert!(g.constraints().is_empty());
}

#[test]
fn the_index_is_rebuilt_after_replay() {
    let dir = std::env::temp_dir().join(format!(
        "drevo_unique_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let wal = dir.join("graph.wal");
    {
        let g = NativeGraph::open_durable(&wal).unwrap();
        g.create_node(node("User", "u1", json!({"email": "a@x"})))
            .unwrap();
    }
    // Constraints are schema kept by the caller; after reopening they are
    // declared again and see the replayed data.
    let g = NativeGraph::open_durable(&wal).unwrap();
    g.add_constraint(unique("User", "email")).unwrap();
    assert!(is_violation(&g.create_node(node(
        "User",
        "u2",
        json!({"email": "a@x"})
    ))));
    let _ = std::fs::remove_dir_all(&dir);
}
