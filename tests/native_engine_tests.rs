//! Differential + unit guards for the native graph engine (RFC
//! `docs/rfc-native-core.md`, #307, Phase 2).
//!
//! The core assertion: [`drevo::native::NativeGraph`] and [`drevo::db::Drevo`],
//! driven through the shared [`drevo::engine::GraphEngine`] seam, are
//! **observably identical** — same ids, same node/edge content, same adjacency,
//! and the same [`drevo::error::DrevoError`] variants on the error paths. That
//! makes `NativeGraph` a drop-in the query layers can be pointed at, and locks
//! the contract before the fast arena/CSR internals replace the `HashMap`
//! skeleton in a later slice.
//!
//! uuid and timestamp fields are generated non-deterministically (uuid v7 +
//! wall clock), so they are excluded from comparison; everything else must match.

use drevo::db::Drevo;
use drevo::engine::GraphEngine;
use drevo::error::DrevoError;
use drevo::model::{Direction, EdgePatch, NewEdge, NewNode, Node, NodePatch};

// ---------------------------------------------------------------------------
// Comparable projections (drop uuid / created_at / updated_at)
// ---------------------------------------------------------------------------

type NodeKey = (u64, String, String, String, String, String);
type EdgeKey = (u64, u64, u64, String, String, String);

fn node_key(n: &Node) -> NodeKey {
    (
        n.id,
        n.kind.clone(),
        n.title.clone(),
        n.body.clone(),
        n.body_html.clone(),
        serde_json::to_string(&n.properties).unwrap(),
    )
}

fn edge_key(e: &drevo::model::Edge) -> EdgeKey {
    (
        e.id,
        e.from_id,
        e.to_id,
        e.kind.clone(),
        format!("{:?}", e.weight),
        serde_json::to_string(&e.properties).unwrap(),
    )
}

fn new_node(kind: &str, title: &str) -> NewNode {
    NewNode {
        kind: kind.into(),
        title: title.into(),
        body: String::new(),
        body_html: String::new(),
        properties: Default::default(),
    }
}

fn new_edge(from: u64, to: u64, kind: &str, weight: f32) -> NewEdge {
    NewEdge {
        from_id: from,
        to_id: to,
        kind: kind.into(),
        weight,
        properties: Default::default(),
    }
}

// ---------------------------------------------------------------------------
// Op-parity checkers — run the same call on both engines, compare the outcome
// ---------------------------------------------------------------------------

fn ck_node(a: drevo::error::Result<Node>, b: drevo::error::Result<Node>) {
    match (a, b) {
        (Ok(x), Ok(y)) => assert_eq!(node_key(&x), node_key(&y), "node results diverged"),
        (Err(x), Err(y)) => assert_eq!(format!("{x:?}"), format!("{y:?}"), "node errors diverged"),
        (x, y) => panic!(
            "ok/err divergence: {:?} vs {:?}",
            x.map(|n| n.id).map_err(|e| format!("{e:?}")),
            y.map(|n| n.id).map_err(|e| format!("{e:?}"))
        ),
    }
}

fn ck_edge(
    a: drevo::error::Result<drevo::model::Edge>,
    b: drevo::error::Result<drevo::model::Edge>,
) {
    match (a, b) {
        (Ok(x), Ok(y)) => assert_eq!(edge_key(&x), edge_key(&y), "edge results diverged"),
        (Err(x), Err(y)) => assert_eq!(format!("{x:?}"), format!("{y:?}"), "edge errors diverged"),
        (x, y) => panic!(
            "ok/err divergence: {:?} vs {:?}",
            x.map(|e| e.id).map_err(|e| format!("{e:?}")),
            y.map(|e| e.id).map_err(|e| format!("{e:?}"))
        ),
    }
}

fn ck_unit(a: drevo::error::Result<()>, b: drevo::error::Result<()>) {
    assert_eq!(
        a.as_ref().map_err(|e| format!("{e:?}")),
        b.as_ref().map_err(|e| format!("{e:?}")),
        "unit-op outcomes diverged"
    );
}

/// Assert both engines expose an identical observable graph: node set, edge
/// set, and per-node adjacency (neighbor ids + full edges, both directions,
/// with and without a kind filter). Order-independent where the seam does not
/// contractually fix order.
fn assert_same_state(a: &dyn GraphEngine, b: &dyn GraphEngine) {
    let mut na: Vec<NodeKey> = a.all_nodes().unwrap().iter().map(node_key).collect();
    let mut nb: Vec<NodeKey> = b.all_nodes().unwrap().iter().map(node_key).collect();
    na.sort();
    nb.sort();
    assert_eq!(na, nb, "node sets diverged");

    let mut ea: Vec<EdgeKey> = a.all_edges().unwrap().iter().map(edge_key).collect();
    let mut eb: Vec<EdgeKey> = b.all_edges().unwrap().iter().map(edge_key).collect();
    ea.sort();
    eb.sort();
    assert_eq!(ea, eb, "edge sets diverged");

    for id in na.iter().map(|k| k.0) {
        for dir in [Direction::Outgoing, Direction::Incoming, Direction::Both] {
            let mut ia = a.neighbor_ids(id, dir, None).unwrap();
            let mut ib = b.neighbor_ids(id, dir, None).unwrap();
            ia.sort_unstable();
            ib.sort_unstable();
            assert_eq!(ia, ib, "neighbor_ids diverged for {id} {dir:?}");

            let mut fa: Vec<EdgeKey> = a.edges_of(id, dir).unwrap().iter().map(edge_key).collect();
            let mut fb: Vec<EdgeKey> = b.edges_of(id, dir).unwrap().iter().map(edge_key).collect();
            fa.sort();
            fb.sort();
            assert_eq!(fa, fb, "edges_of diverged for {id} {dir:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// The differential workload
// ---------------------------------------------------------------------------

#[test]
fn native_matches_drevo_on_a_scripted_workload() {
    let d = Drevo::open_in_memory().unwrap();
    let n = drevo::native::NativeGraph::new();
    let a: &dyn GraphEngine = &d;
    let b: &dyn GraphEngine = &n;

    // Build an identical graph on both engines, checking every result.
    ck_node(
        a.create_node(new_node("person", "alice")),
        b.create_node(new_node("person", "alice")),
    );
    ck_node(
        a.create_node(new_node("person", "bob")),
        b.create_node(new_node("person", "bob")),
    );
    ck_node(
        a.create_node(new_node("tag", "rust")),
        b.create_node(new_node("tag", "rust")),
    );
    assert_same_state(a, b);

    // Duplicate title → same DuplicateTitle error on both.
    ck_node(
        a.create_node(new_node("person", "alice")),
        b.create_node(new_node("person", "alice")),
    );

    // Edges, including an invalid-weight and a dangling-endpoint error.
    ck_edge(
        a.create_edge(new_edge(1, 2, "KNOWS", 1.0)),
        b.create_edge(new_edge(1, 2, "KNOWS", 1.0)),
    );
    ck_edge(
        a.create_edge(new_edge(1, 3, "TAGGED", 2.5)),
        b.create_edge(new_edge(1, 3, "TAGGED", 2.5)),
    );
    ck_edge(
        a.create_edge(new_edge(2, 3, "TAGGED", 1.0)),
        b.create_edge(new_edge(2, 3, "TAGGED", 1.0)),
    );
    ck_edge(
        a.create_edge(new_edge(1, 2, "BAD", f32::NAN)),
        b.create_edge(new_edge(1, 2, "BAD", f32::NAN)),
    );
    ck_edge(
        a.create_edge(new_edge(1, 999, "DANGLING", 1.0)),
        b.create_edge(new_edge(1, 999, "DANGLING", 1.0)),
    );
    assert_same_state(a, b);

    // Kind-filtered fan-out parity.
    let mut ka = a
        .neighbor_ids(1, Direction::Outgoing, Some("TAGGED"))
        .unwrap();
    let mut kb = b
        .neighbor_ids(1, Direction::Outgoing, Some("TAGGED"))
        .unwrap();
    ka.sort_unstable();
    kb.sort_unstable();
    assert_eq!(ka, kb);
    assert_eq!(ka, vec![3]);

    // Updates: node rename, node body, edge weight — plus error paths.
    ck_node(
        a.update_node(
            2,
            NodePatch {
                title: Some("bobby".into()),
                ..Default::default()
            },
        ),
        b.update_node(
            2,
            NodePatch {
                title: Some("bobby".into()),
                ..Default::default()
            },
        ),
    );
    // Rename onto an existing different title → DuplicateTitle on both.
    ck_node(
        a.update_node(
            2,
            NodePatch {
                title: Some("alice".into()),
                ..Default::default()
            },
        ),
        b.update_node(
            2,
            NodePatch {
                title: Some("alice".into()),
                ..Default::default()
            },
        ),
    );
    ck_node(
        a.update_node(
            999,
            NodePatch {
                body: Some("x".into()),
                ..Default::default()
            },
        ),
        b.update_node(
            999,
            NodePatch {
                body: Some("x".into()),
                ..Default::default()
            },
        ),
    );
    ck_edge(
        a.update_edge(
            1,
            EdgePatch {
                weight: Some(9.0),
                ..Default::default()
            },
        ),
        b.update_edge(
            1,
            EdgePatch {
                weight: Some(9.0),
                ..Default::default()
            },
        ),
    );
    ck_edge(
        a.update_edge(
            999,
            EdgePatch {
                weight: Some(1.0),
                ..Default::default()
            },
        ),
        b.update_edge(
            999,
            EdgePatch {
                weight: Some(1.0),
                ..Default::default()
            },
        ),
    );
    assert_same_state(a, b);

    // Deletes: an edge, then a node (cascading its remaining incident edges).
    ck_unit(a.delete_edge(3), b.delete_edge(3));
    ck_unit(a.delete_node(1), b.delete_node(1));
    ck_unit(a.delete_node(999), b.delete_node(999));
    assert_same_state(a, b);

    // Label-scan pagination parity.
    for (limit, offset) in [(10, 0), (1, 0), (1, 1), (10, 5)] {
        let pa: Vec<NodeKey> = a
            .nodes_by_kind("person", limit, offset)
            .unwrap()
            .iter()
            .map(node_key)
            .collect();
        let pb: Vec<NodeKey> = b
            .nodes_by_kind("person", limit, offset)
            .unwrap()
            .iter()
            .map(node_key)
            .collect();
        assert_eq!(
            pa, pb,
            "nodes_by_kind diverged at limit={limit} offset={offset}"
        );
    }
}

// ---------------------------------------------------------------------------
// Focused unit semantics of NativeGraph on its own
// ---------------------------------------------------------------------------

#[test]
fn native_enforces_core_semantics() {
    let g = drevo::native::NativeGraph::new();

    // Ids start at 1 and increment.
    let a = g.create_node(new_node("k", "a")).unwrap();
    let b = g.create_node(new_node("k", "b")).unwrap();
    assert_eq!((a.id, b.id), (1, 2));

    // Title uniqueness.
    assert!(matches!(
        g.create_node(new_node("k", "a")),
        Err(DrevoError::DuplicateTitle(t)) if t == "a"
    ));

    // Edge endpoint validation + invalid weight.
    assert!(matches!(
        g.create_edge(new_edge(1, 42, "E", 1.0)),
        Err(DrevoError::NodeNotFound(42))
    ));
    assert!(matches!(
        g.create_edge(new_edge(1, 2, "E", f32::INFINITY)),
        Err(DrevoError::InvalidWeight(_))
    ));

    // Self-loop contributes no neighbour.
    g.create_edge(new_edge(1, 1, "SELF", 1.0)).unwrap();
    assert!(g.neighbor_ids(1, Direction::Both, None).unwrap().is_empty());

    // Cascade delete removes incident edges.
    let e = g.create_edge(new_edge(1, 2, "KNOWS", 1.0)).unwrap();
    assert!(g.get_edge(e.id).unwrap().is_some());
    g.delete_node(2).unwrap();
    assert!(g.get_edge(e.id).unwrap().is_none());
    assert!(g.get_node(2).unwrap().is_none());
    // The self-loop on node 1 survives (node 1 still exists).
    assert_eq!(g.all_edges().unwrap().len(), 1);
}
