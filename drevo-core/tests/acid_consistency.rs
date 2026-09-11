//! ACID conformance — **C (Consistency)** for the default `native-durable`
//! engine, exercised as **Rust** tests directly against the `drevo-core`
//! [`NativeGraph`] seam (no HTTP layer).
//!
//! Consistency here means two things: (1) every declared schema
//! [`Constraint`](drevo_core::native::Constraint) — UNIQUE, property EXISTS,
//! NODE KEY — is enforced at commit, and a transaction whose writes would
//! violate one aborts atomically (leaving the pre-transaction state intact,
//! tying into Atomicity, issue #424); and (2) the engine's structural
//! invariants — title uniqueness, edge-endpoint existence, finite edge weight,
//! cascade edge-deletion — always hold.
//!
//! Part of the ACID conformance set (issue #425).

use std::collections::HashMap;

use drevo_core::engine::GraphEngine;
use drevo_core::error::CoreError;
use drevo_core::model::{Direction, NewEdge, NewNode, Properties};
use drevo_core::native::{CommitError, Constraint, NativeGraph};

fn node(kind: &str, title: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties(Default::default()),
    }
}

fn node_with_props(kind: &str, title: &str, props: &[(&str, &str)]) -> NewNode {
    let mut nn = node(kind, title);
    nn.properties = Properties(
        props
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::Value::String((*v).to_string())))
            .collect::<HashMap<_, _>>(),
    );
    nn
}

fn edge(from_id: u64, to_id: u64, kind: &str, weight: f32) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: kind.to_string(),
        weight,
        properties: Properties(Default::default()),
    }
}

// ── declared constraints, enforced at commit ──────────────────────────────

/// UNIQUE(kind, property): two nodes of the kind sharing a value abort the
/// whole commit; distinct values commit; another kind is unaffected.
#[test]
fn unique_node_property_is_enforced_at_commit() {
    let g = NativeGraph::new();
    g.add_constraint(Constraint::UniqueNodeProperty {
        kind: "user".into(),
        property: "email".into(),
    })
    .unwrap();

    let mut tx = g.begin();
    tx.create_node(node_with_props("user", "u1", &[("email", "a@x")]))
        .unwrap();
    tx.create_node(node_with_props("user", "u2", &[("email", "a@x")]))
        .unwrap();
    assert!(matches!(tx.commit(), Err(CommitError::Constraint(_))));
    assert_eq!(g.node_count(), 0, "the violating commit landed nothing");

    let mut tx = g.begin();
    tx.create_node(node_with_props("user", "u1", &[("email", "a@x")]))
        .unwrap();
    tx.create_node(node_with_props("user", "u2", &[("email", "b@x")]))
        .unwrap();
    tx.commit().expect("distinct values satisfy the constraint");
    assert_eq!(g.node_count(), 2);
}

/// PROPERTY EXISTS(kind, property): a node of the kind missing the property
/// aborts the commit.
#[test]
fn property_exists_constraint_is_enforced_at_commit() {
    let g = NativeGraph::new();
    g.add_constraint(Constraint::PropertyExists {
        kind: "account".into(),
        property: "owner".into(),
    })
    .unwrap();

    let mut tx = g.begin();
    tx.create_node(node("account", "no-owner")).unwrap();
    assert!(
        matches!(tx.commit(), Err(CommitError::Constraint(_))),
        "a node of the kind lacking the required property must be rejected"
    );
    assert_eq!(g.node_count(), 0);

    let mut tx = g.begin();
    tx.create_node(node_with_props("account", "ok", &[("owner", "ada")]))
        .unwrap();
    tx.commit().expect("carrying the property satisfies EXISTS");
    assert_eq!(g.node_count(), 1);
}

/// NODE KEY(kind, [props]): the tuple must be present and unique; a duplicate
/// tuple aborts the commit.
#[test]
fn node_key_constraint_is_enforced_at_commit() {
    let g = NativeGraph::new();
    g.add_constraint(Constraint::NodeKey {
        kind: "book".into(),
        properties: vec!["isbn".into()],
    })
    .unwrap();

    let mut tx = g.begin();
    tx.create_node(node_with_props("book", "b1", &[("isbn", "111")]))
        .unwrap();
    tx.create_node(node_with_props("book", "b2", &[("isbn", "111")]))
        .unwrap();
    assert!(
        matches!(tx.commit(), Err(CommitError::Constraint(_))),
        "a duplicate node-key tuple must be rejected"
    );
    assert_eq!(g.node_count(), 0);
}

/// A constraint cannot be declared over data that already violates it, and
/// declaring it must not partially "take": the graph is unchanged afterwards.
#[test]
fn add_constraint_rejects_preexisting_violation_and_is_not_stored() {
    let g = NativeGraph::new();
    let mut tx = g.begin();
    tx.create_node(node_with_props("acct", "x", &[("code", "1")]))
        .unwrap();
    tx.create_node(node_with_props("acct", "y", &[("code", "1")]))
        .unwrap();
    tx.commit().unwrap();

    assert!(
        g.add_constraint(Constraint::UniqueNodeProperty {
            kind: "acct".into(),
            property: "code".into(),
        })
        .is_err(),
        "declaring UNIQUE over already-duplicated data must fail"
    );

    // Not stored → a later duplicate still commits.
    let mut tx = g.begin();
    tx.create_node(node_with_props("acct", "z", &[("code", "1")]))
        .unwrap();
    assert!(
        tx.commit().is_ok(),
        "the rejected constraint was not silently installed"
    );
}

// ── structural invariants (no explicit constraint needed) ──────────────────

/// Title uniqueness is a global invariant.
#[test]
fn title_uniqueness_invariant_holds() {
    let g = NativeGraph::new();
    g.create_node(node("note", "dup")).unwrap();
    assert!(
        matches!(
            g.create_node(node("note", "dup")),
            Err(CoreError::DuplicateTitle(_))
        ),
        "a second node with the same title must be rejected"
    );
    assert_eq!(g.node_count(), 1);
}

/// An edge cannot reference a non-existent endpoint.
#[test]
fn edge_endpoint_existence_invariant_holds() {
    let g = NativeGraph::new();
    let a = g.create_node(node("n", "a")).unwrap();
    assert!(
        matches!(
            g.create_edge(edge(a.id, 9_999, "links", 1.0)),
            Err(CoreError::NodeNotFound(_))
        ),
        "an edge to a missing node must be rejected"
    );
    assert_eq!(g.edge_count(), 0);
}

/// Edge weight must be finite (NaN / ±Inf rejected) — the graph derives
/// `PartialEq`, which a NaN weight would break.
#[test]
fn edge_weight_finiteness_invariant_holds() {
    let g = NativeGraph::new();
    let a = g.create_node(node("n", "a")).unwrap();
    let b = g.create_node(node("n", "b")).unwrap();
    assert!(matches!(
        g.create_edge(edge(a.id, b.id, "links", f32::NAN)),
        Err(CoreError::InvalidWeight(_))
    ));
    assert!(matches!(
        g.create_edge(edge(a.id, b.id, "links", f32::INFINITY)),
        Err(CoreError::InvalidWeight(_))
    ));
    assert_eq!(g.edge_count(), 0);
    g.create_edge(edge(a.id, b.id, "links", 1.5))
        .expect("a finite weight is accepted");
    assert_eq!(g.edge_count(), 1);
}

/// Deleting a node cascades to its incident edges — no dangling edge is left.
#[test]
fn cascade_edge_deletion_invariant_holds() {
    let g = NativeGraph::new();
    let a = g.create_node(node("n", "a")).unwrap();
    let b = g.create_node(node("n", "b")).unwrap();
    g.create_edge(edge(a.id, b.id, "links", 1.0)).unwrap();
    assert_eq!(g.edge_count(), 1);

    g.delete_node(a.id).unwrap();
    assert_eq!(
        g.edge_count(),
        0,
        "removing an endpoint cascades to its incident edges"
    );
    assert!(
        g.neighbor_ids(b.id, Direction::Both, None)
            .unwrap()
            .is_empty(),
        "no dangling adjacency remains after cascade"
    );
}
