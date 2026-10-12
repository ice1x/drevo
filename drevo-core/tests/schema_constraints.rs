//! Composite UNIQUE, property-existence and NODE KEY constraints (issue #606).
//!
//! Uniqueness (single, composite or the key half of a NODE KEY) is checked on
//! every write. Existence (`PropertyExists`, the other half of a NODE KEY) is
//! checked on every write made outside a transaction, and at commit inside
//! one, so a transaction may create a node first and set its required
//! properties afterwards. Labels include secondary labels; `title` / `body`
//! read the node's own fields; `null` counts as absent.

use std::collections::HashMap;

use drevo_core::engine::GraphEngine;
use drevo_core::error::CoreError;
use drevo_core::model::{NewNode, NodePatch, Properties};
use drevo_core::native::{CommitError, Constraint, NativeGraph};
use serde_json::{json, Value};

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

fn props(v: Value) -> Properties {
    let Value::Object(map) = v else {
        panic!("props must be an object")
    };
    Properties(map.into_iter().collect())
}

fn exists(kind: &str, property: &str) -> Constraint {
    Constraint::PropertyExists {
        kind: kind.into(),
        property: property.into(),
    }
}

fn composite(kind: &str, properties: &[&str]) -> Constraint {
    Constraint::UniqueNodeProperties {
        kind: kind.into(),
        properties: properties.iter().map(|p| (*p).to_string()).collect(),
    }
}

fn node_key(kind: &str, properties: &[&str]) -> Constraint {
    Constraint::NodeKey {
        kind: kind.into(),
        properties: properties.iter().map(|p| (*p).to_string()).collect(),
    }
}

fn is_violation<T: std::fmt::Debug>(r: &drevo_core::error::Result<T>) -> bool {
    matches!(r, Err(CoreError::ConstraintViolation(_)))
}

#[test]
fn composite_unique_rejects_a_repeated_tuple_only() {
    let g = NativeGraph::new();
    g.add_constraint(composite("Book", &["title", "year"]))
        .unwrap();
    g.create_node(node("Book", "Dune", json!({"year": 1965})))
        .unwrap();
    // Same title, other year; same year, other title: both fine.
    g.create_node(node("Book", "Dune", json!({"year": 2021})))
        .unwrap();
    g.create_node(node("Book", "Emma", json!({"year": 1965})))
        .unwrap();
    let dup = g.create_node(node("Book", "Dune", json!({"year": 1965})));
    assert!(is_violation(&dup), "{dup:?}");
    // A node missing part of the tuple is not constrained.
    g.create_node(node("Book", "Dune", json!({}))).unwrap();
    g.create_node(node("Book", "Dune", json!({"year": null})))
        .unwrap();
    assert_eq!(g.node_count(), 5);
}

#[test]
fn composite_unique_checks_updates() {
    let g = NativeGraph::new();
    g.add_constraint(composite("Book", &["title", "year"]))
        .unwrap();
    g.create_node(node("Book", "Dune", json!({"year": 1965})))
        .unwrap();
    let b = g
        .create_node(node("Book", "Dune", json!({"year": 2021})))
        .unwrap();
    let clash = g.update_node(
        b.id,
        NodePatch {
            properties: Some(props(json!({"year": 1965}))),
            ..NodePatch::default()
        },
    );
    assert!(is_violation(&clash), "{clash:?}");
}

#[test]
fn existence_rejects_a_write_outside_a_transaction() {
    let g = NativeGraph::new();
    g.add_constraint(exists("User", "email")).unwrap();
    assert!(is_violation(&g.create_node(node("User", "u", json!({})))));
    assert!(is_violation(&g.create_node(node(
        "User",
        "u",
        json!({"email": null})
    ))));
    // A secondary label is covered too.
    assert!(is_violation(&g.create_node(node(
        "Note",
        "n",
        json!({"_labels": ["User"]})
    ))));
    assert_eq!(g.node_count(), 0);

    let u = g
        .create_node(node("User", "u", json!({"email": "a@x"})))
        .unwrap();
    // Removing the property is a violation; the node keeps its value.
    let removed = g.update_node(
        u.id,
        NodePatch {
            properties: Some(props(json!({}))),
            ..NodePatch::default()
        },
    );
    assert!(is_violation(&removed), "{removed:?}");
    assert_eq!(
        g.get_node(u.id).unwrap().unwrap().properties.0["email"],
        json!("a@x")
    );
    // Other labels are not constrained.
    g.create_node(node("Bot", "b", json!({}))).unwrap();
}

#[test]
fn existence_of_title_reads_the_node_field() {
    let g = NativeGraph::new();
    g.add_constraint(exists("Tag", "title")).unwrap();
    g.create_node(node("Tag", "rust", json!({}))).unwrap();
    assert!(is_violation(&g.create_node(node("Tag", "", json!({})))));
}

#[test]
fn existence_is_checked_at_commit_inside_a_transaction() {
    let g = NativeGraph::new();
    g.add_constraint(exists("User", "email")).unwrap();

    // Create first, set the required property later: fine at commit.
    let mut tx = g.begin();
    let u = tx.create_node(node("User", "u", json!({}))).unwrap();
    tx.update_node(
        u.id,
        NodePatch {
            properties: Some(props(json!({"email": "a@x"}))),
            ..NodePatch::default()
        },
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(g.node_count(), 1);

    // Still missing at commit: the whole transaction is rejected.
    let mut tx = g.begin();
    tx.create_node(node("User", "ok", json!({"email": "b@x"})))
        .unwrap();
    tx.create_node(node("User", "bad", json!({}))).unwrap();
    assert!(matches!(tx.commit(), Err(CommitError::Constraint(_))));
    assert_eq!(g.node_count(), 1);
}

#[test]
fn a_batch_missing_a_required_property_writes_nothing() {
    let g = NativeGraph::new();
    g.add_constraint(exists("User", "email")).unwrap();
    let batch = g.create_nodes(vec![
        node("User", "u1", json!({"email": "a@x"})),
        node("User", "u2", json!({})),
    ]);
    assert!(is_violation(&batch), "{batch:?}");
    assert_eq!(g.node_count(), 0);
}

#[test]
fn node_key_requires_every_property_and_a_unique_tuple() {
    let g = NativeGraph::new();
    g.add_constraint(node_key("Person", &["first", "last"]))
        .unwrap();
    assert!(is_violation(&g.create_node(node(
        "Person",
        "p",
        json!({"first": "ann"})
    ))));
    g.create_node(node("Person", "p1", json!({"first": "ann", "last": "lee"})))
        .unwrap();
    g.create_node(node("Person", "p2", json!({"first": "ann", "last": "kim"})))
        .unwrap();
    // The duplicate tuple is rejected by the write itself, in a transaction too.
    assert!(is_violation(&g.create_node(node(
        "Person",
        "p3",
        json!({"first": "ann", "last": "lee"})
    ))));
    let mut tx = g.begin();
    let dup = tx.create_node(node("Person", "p4", json!({"first": "ann", "last": "lee"})));
    assert!(is_violation(&dup), "{dup:?}");
    // The missing half is only checked at commit inside a transaction.
    tx.create_node(node("Person", "p5", json!({"first": "bob"})))
        .unwrap();
    assert!(matches!(tx.commit(), Err(CommitError::Constraint(_))));
    assert_eq!(g.node_count(), 2);
}

#[test]
fn constraints_are_refused_over_violating_data() {
    let g = NativeGraph::new();
    g.create_node(node("User", "u1", json!({}))).unwrap();
    g.create_node(node("Book", "Dune", json!({"year": 1965})))
        .unwrap();
    g.create_node(node("Book", "Dune", json!({"year": 1965})))
        .unwrap();
    assert!(g.add_constraint(exists("User", "email")).is_err());
    assert!(g.add_constraint(node_key("User", &["email"])).is_err());
    assert!(g
        .add_constraint(composite("Book", &["title", "year"]))
        .is_err());
    assert!(g.constraints().is_empty());
    // A satisfiable one is accepted and listed.
    g.add_constraint(composite("Book", &["title", "isbn"]))
        .unwrap();
    assert_eq!(g.constraints(), vec![composite("Book", &["title", "isbn"])]);
}

#[test]
fn dropping_a_node_key_keeps_an_equal_unique_constraint() {
    let g = NativeGraph::new();
    g.add_constraint(node_key("Person", &["first", "last"]))
        .unwrap();
    g.add_constraint(composite("Person", &["first", "last"]))
        .unwrap();
    g.create_node(node("Person", "p1", json!({"first": "ann", "last": "lee"})))
        .unwrap();
    assert!(g.remove_constraint(&node_key("Person", &["first", "last"])));
    // Existence is gone, uniqueness of the tuple stays.
    g.create_node(node("Person", "p2", json!({"first": "ann"})))
        .unwrap();
    assert!(is_violation(&g.create_node(node(
        "Person",
        "p3",
        json!({"first": "ann", "last": "lee"})
    ))));
    assert!(g.remove_constraint(&composite("Person", &["first", "last"])));
    g.create_node(node("Person", "p3", json!({"first": "ann", "last": "lee"})))
        .unwrap();
}

#[test]
fn existence_constraints_are_reported() {
    let g = NativeGraph::new();
    assert!(!g.has_existence_constraints());
    g.add_constraint(composite("Book", &["title", "year"]))
        .unwrap();
    assert!(!g.has_existence_constraints());
    g.add_constraint(exists("User", "email")).unwrap();
    assert!(g.has_existence_constraints());
    g.remove_constraint(&exists("User", "email"));
    g.add_constraint(node_key("Person", &["id"])).unwrap();
    assert!(g.has_existence_constraints());
}
