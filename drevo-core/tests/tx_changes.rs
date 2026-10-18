//! `NativeGraph::tx_changes`: what a registered transaction has changed so
//! far, as before/after pairs of every node and relationship it touched.

use std::sync::Arc;

use drevo_core::engine::GraphEngine;
use drevo_core::model::{Edge, NewEdge, NewNode, Node, NodePatch, Properties};
use drevo_core::native::{Change, NativeGraph};

fn new_node(title: &str) -> NewNode {
    NewNode {
        kind: "Task".to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn new_edge(from_id: u64, to_id: u64) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: "DEPENDS_ON".to_string(),
        weight: 1.0,
        properties: Properties::default(),
    }
}

fn status(value: &str) -> Properties {
    let mut p = Properties::default();
    p.insert("status".to_string(), serde_json::json!(value));
    p
}

fn before<T: Clone>(c: &Change<T>) -> Option<T> {
    c.before.as_deref().cloned()
}

fn after<T: Clone>(c: &Change<T>) -> Option<T> {
    c.after.as_deref().cloned()
}

#[test]
fn a_transaction_that_only_read_changed_nothing() {
    let g = NativeGraph::new();
    g.create_node(new_node("existing")).unwrap();
    let tx = g.tx_begin();
    g.tx_engine(tx).unwrap().get_node(1).unwrap();
    let changes = g.tx_changes(tx).unwrap();
    assert!(changes.nodes.is_empty());
    assert!(changes.edges.is_empty());
    assert!(changes.is_empty());
}

#[test]
fn an_unknown_or_closed_transaction_has_no_changes() {
    let g = NativeGraph::new();
    let tx = g.tx_begin();
    g.tx_rollback(tx);
    assert!(g.tx_changes(tx).is_none());
}

#[test]
fn created_nodes_and_relationships_have_no_before() {
    let g = NativeGraph::new();
    let tx = g.tx_begin();
    let e = g.tx_engine(tx).unwrap();
    let a = e.create_node(new_node("a")).unwrap();
    let b = e.create_node(new_node("b")).unwrap();
    let rel = e.create_edge(new_edge(a.id, b.id)).unwrap();

    let changes = g.tx_changes(tx).unwrap();
    let nodes: Vec<(Option<Node>, Option<Node>)> = changes
        .nodes
        .iter()
        .map(|c| (before(c), after(c)))
        .collect();
    assert_eq!(nodes, vec![(None, Some(a)), (None, Some(b))]);
    let edges: Vec<(Option<Edge>, Option<Edge>)> = changes
        .edges
        .iter()
        .map(|c| (before(c), after(c)))
        .collect();
    assert_eq!(edges, vec![(None, Some(rel))]);
}

#[test]
fn an_updated_node_reports_its_old_and_new_version() {
    let g = NativeGraph::new();
    let old = g.create_node(new_node("ship")).unwrap();
    let tx = g.tx_begin();
    let e = g.tx_engine(tx).unwrap();
    e.update_node(
        old.id,
        NodePatch {
            properties: Some(status("open")),
            ..NodePatch::default()
        },
    )
    .unwrap();
    // A second write to the same node is folded into one change.
    let new = e
        .update_node(
            old.id,
            NodePatch {
                properties: Some(status("done")),
                ..NodePatch::default()
            },
        )
        .unwrap();

    let changes = g.tx_changes(tx).unwrap();
    assert_eq!(changes.nodes.len(), 1);
    assert_eq!(before(&changes.nodes[0]), Some(old));
    assert_eq!(after(&changes.nodes[0]), Some(new));
    assert!(changes.edges.is_empty());
}

#[test]
fn a_deleted_node_takes_its_relationships_with_it() {
    let g = NativeGraph::new();
    let a = g.create_node(new_node("a")).unwrap();
    let b = g.create_node(new_node("b")).unwrap();
    let c = g.create_node(new_node("c")).unwrap();
    let ab = g.create_edge(new_edge(a.id, b.id)).unwrap();
    let ca = g.create_edge(new_edge(c.id, a.id)).unwrap();
    let bc = g.create_edge(new_edge(b.id, c.id)).unwrap();

    let tx = g.tx_begin();
    g.tx_engine(tx).unwrap().delete_node(a.id).unwrap();

    let changes = g.tx_changes(tx).unwrap();
    let nodes: Vec<(Option<Node>, Option<Node>)> = changes
        .nodes
        .iter()
        .map(|c| (before(c), after(c)))
        .collect();
    assert_eq!(nodes, vec![(Some(a), None)]);
    let edges: Vec<(Option<Edge>, Option<Edge>)> = changes
        .edges
        .iter()
        .map(|c| (before(c), after(c)))
        .collect();
    // Both relationships of `a`, in id order; `b -> c` is untouched.
    assert_eq!(edges, vec![(Some(ab), None), (Some(ca), None)]);
    assert!(changes
        .edges
        .iter()
        .all(|c| before(c).map(|e| e.id) != Some(bc.id)));
}

#[test]
fn a_deleted_relationship_reports_its_old_version() {
    let g = NativeGraph::new();
    let a = g.create_node(new_node("a")).unwrap();
    let b = g.create_node(new_node("b")).unwrap();
    let rel = g.create_edge(new_edge(a.id, b.id)).unwrap();
    let tx = g.tx_begin();
    g.tx_engine(tx).unwrap().delete_edge(rel.id).unwrap();

    let changes = g.tx_changes(tx).unwrap();
    assert!(changes.nodes.is_empty());
    assert_eq!(changes.edges.len(), 1);
    assert_eq!(before(&changes.edges[0]), Some(rel));
    assert_eq!(after(&changes.edges[0]), None);
}

#[test]
fn something_created_and_deleted_in_the_same_transaction_is_not_reported() {
    let g = NativeGraph::new();
    let keep = g.create_node(new_node("keep")).unwrap();
    let tx = g.tx_begin();
    let e = g.tx_engine(tx).unwrap();
    let temp = e.create_node(new_node("temp")).unwrap();
    e.create_edge(new_edge(keep.id, temp.id)).unwrap();
    e.delete_node(temp.id).unwrap();

    let changes = g.tx_changes(tx).unwrap();
    assert!(changes.is_empty(), "{changes:?}");
}

#[test]
fn changes_follow_the_working_copy_between_statements() {
    let g = NativeGraph::new();
    let tx = g.tx_begin();
    let first = g
        .tx_engine(tx)
        .unwrap()
        .create_node(new_node("one"))
        .unwrap();
    assert_eq!(g.tx_changes(tx).unwrap().nodes.len(), 1);
    let second = g
        .tx_engine(tx)
        .unwrap()
        .create_node(new_node("two"))
        .unwrap();
    let ids: Vec<u64> = g
        .tx_changes(tx)
        .unwrap()
        .nodes
        .iter()
        .filter_map(|c| c.after.as_ref().map(|n| n.id))
        .collect();
    assert_eq!(ids, vec![first.id, second.id]);
}

#[test]
fn changes_share_the_stored_records() {
    // `before` is the committed record itself, not a copy of it.
    let g = NativeGraph::new();
    let n = g.create_node(new_node("shared")).unwrap();
    let tx = g.tx_begin();
    g.tx_engine(tx).unwrap().delete_node(n.id).unwrap();
    let changes = g.tx_changes(tx).unwrap();
    let stored = g.get_node_arc(n.id).unwrap();
    let reported = changes.nodes[0].before.as_ref().unwrap();
    assert!(Arc::ptr_eq(&stored, reported));
}

#[test]
fn committing_still_applies_the_reported_changes() {
    let g = NativeGraph::new();
    let tx = g.tx_begin();
    let n = g.tx_engine(tx).unwrap().create_node(new_node("x")).unwrap();
    let reported = g.tx_changes(tx).unwrap();
    assert_eq!(reported.nodes.len(), 1);
    g.tx_commit(tx).unwrap();
    assert_eq!(g.get_node_arc(n.id).as_deref(), Some(&n));
    assert!(g.tx_changes(tx).is_none());
}
