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

// ---------------------------------------------------------------------------
// Phase 2.2 — randomized differential parity.
//
// A deterministic LCG drives a long, mixed operation stream (creates with a
// small title pool so uniqueness collides; edges with dangling endpoints and
// non-finite weights; updates and deletes against live *and* stale ids)
// against NativeGraph and Drevo in lockstep. Every op's outcome is compared,
// and the full observable state is compared at intervals. Deterministic seed =
// reproducible failures. This is the safety net that lets the HashMap
// internals be swapped for an arena/CSR layout later without regressing
// behaviour.
// ---------------------------------------------------------------------------

/// A tiny deterministic PRNG (LCG). No external dependency, and a fixed seed so
/// any divergence reproduces exactly.
struct Lcg(u64);
impl Lcg {
    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

#[test]
fn native_matches_drevo_under_randomized_workload() {
    let d = Drevo::open_in_memory().unwrap();
    let n = drevo::native::NativeGraph::new();
    let a: &dyn GraphEngine = &d;
    let b: &dyn GraphEngine = &n;

    let kinds = ["person", "tag", "note", "concept"];
    let mut rng = Lcg(0x1234_5678_9abc_def0);
    // Successful create_node count == the max allocated node id (monotonic,
    // never reused), so ids in 1..=created+1 mix live, deleted, and never-used.
    let mut created: u64 = 0;

    for step in 0..3000u32 {
        let kind = kinds[(rng.below(kinds.len() as u64)) as usize];
        let pick_id = |rng: &mut Lcg, created: u64| 1 + rng.below(created + 2);

        match rng.below(100) {
            0..=33 => {
                // create_node — title pool of 40 → frequent DuplicateTitle.
                let title = format!("t{}", rng.below(40));
                let nn = new_node(kind, &title);
                let ra = a.create_node(nn.clone());
                let ok = ra.is_ok();
                ck_node(ra, b.create_node(nn));
                if ok {
                    created += 1;
                }
            }
            34..=63 => {
                // create_edge — dangling endpoints + occasional bad weight.
                let from = pick_id(&mut rng, created);
                let to = pick_id(&mut rng, created);
                let w = match rng.below(25) {
                    0 => f32::NAN,
                    1 => f32::INFINITY,
                    _ => (rng.below(200) as f32) / 10.0,
                };
                let ne = new_edge(from, to, kind, w);
                ck_edge(a.create_edge(ne.clone()), b.create_edge(ne));
            }
            64..=75 => {
                // update_node — rename (collision-prone) or body change.
                let id = pick_id(&mut rng, created);
                let patch = if rng.below(2) == 0 {
                    NodePatch {
                        title: Some(format!("t{}", rng.below(40))),
                        ..Default::default()
                    }
                } else {
                    NodePatch {
                        body: Some(format!("b{}", rng.next_u64())),
                        ..Default::default()
                    }
                };
                ck_node(a.update_node(id, patch.clone()), b.update_node(id, patch));
            }
            76..=85 => {
                // update_edge — weight (maybe non-finite) or kind.
                let id = pick_id(&mut rng, created);
                let patch = if rng.below(4) == 0 {
                    let w = if rng.below(10) == 0 {
                        f32::NAN
                    } else {
                        (rng.below(200) as f32) / 10.0
                    };
                    EdgePatch {
                        weight: Some(w),
                        ..Default::default()
                    }
                } else {
                    EdgePatch {
                        kind: Some(kind.to_string()),
                        ..Default::default()
                    }
                };
                ck_edge(a.update_edge(id, patch.clone()), b.update_edge(id, patch));
            }
            86..=93 => {
                // delete_edge — mostly stale ids (edge ids != node ids).
                let id = pick_id(&mut rng, created);
                ck_unit(a.delete_edge(id), b.delete_edge(id));
            }
            _ => {
                // delete_node — cascades incident edges.
                let id = pick_id(&mut rng, created);
                ck_unit(a.delete_node(id), b.delete_node(id));
            }
        }

        if step % 250 == 0 {
            assert_same_state(a, b);
        }
    }
    assert_same_state(a, b);
    // The workload actually built a non-trivial graph.
    assert!(a.all_nodes().unwrap().len() > 5);
    assert!(a.all_edges().unwrap().len() > 5);
}

// ---------------------------------------------------------------------------
// Phase 3.1 — snapshot isolation for reads.
//
// A GraphSnapshot is a frozen, consistent view: writes to the engine after the
// snapshot is taken must not change what the snapshot reports, and a fresh
// snapshot must reflect them.
// ---------------------------------------------------------------------------

#[test]
fn snapshot_is_isolated_from_later_writes() {
    use drevo::native::NativeGraph;

    let g = NativeGraph::new();
    let a = g.create_node(new_node("person", "a")).unwrap();
    let b = g.create_node(new_node("person", "b")).unwrap();
    let e = g.create_edge(new_edge(a.id, b.id, "KNOWS", 1.0)).unwrap();

    // Freeze the state.
    let snap = g.snapshot();
    assert_eq!(snap.all_nodes().len(), 2);
    assert_eq!(snap.all_edges().len(), 1);
    assert_eq!(
        snap.neighbor_ids(a.id, Direction::Outgoing, None),
        vec![b.id]
    );

    // Mutate the live engine every which way.
    let c = g.create_node(new_node("person", "c")).unwrap();
    g.create_edge(new_edge(a.id, c.id, "KNOWS", 1.0)).unwrap();
    g.delete_edge(e.id).unwrap();
    g.update_node(
        b.id,
        NodePatch {
            title: Some("b2".into()),
            ..Default::default()
        },
    )
    .unwrap();

    // The snapshot is unchanged — still the frozen 2-node, 1-edge graph.
    assert_eq!(snap.all_nodes().len(), 2);
    assert_eq!(snap.all_edges().len(), 1);
    assert_eq!(
        snap.neighbor_ids(a.id, Direction::Outgoing, None),
        vec![b.id]
    );
    assert_eq!(snap.get_node(b.id).unwrap().title, "b");
    assert!(snap.get_node(c.id).is_none());

    // A fresh snapshot reflects the writes.
    let snap2 = g.snapshot();
    assert_eq!(snap2.all_nodes().len(), 3);
    assert_eq!(snap2.all_edges().len(), 1); // e deleted, a->c added
    let mut ns = snap2.neighbor_ids(a.id, Direction::Outgoing, None);
    ns.sort_unstable();
    assert_eq!(ns, vec![c.id]);
    assert_eq!(snap2.get_node(b.id).unwrap().title, "b2");
}

// ---------------------------------------------------------------------------
// Phase 3.2 — transactions: snapshot-isolated buffered writes, atomic commit,
// rollback, and optimistic conflict detection.
// ---------------------------------------------------------------------------

#[test]
fn native_tx_buffers_writes_until_commit() {
    use drevo::native::NativeGraph;
    let g = NativeGraph::new();
    let seed = g.create_node(new_node("k", "seed")).unwrap();

    let mut tx = g.begin();
    let a = tx.create_node(new_node("k", "a")).unwrap();
    tx.create_edge(new_edge(seed.id, a.id, "E", 1.0)).unwrap();

    // The tx sees its own writes; the live engine does not yet.
    assert!(tx.get_node(a.id).is_some());
    assert!(g.get_node(a.id).unwrap().is_none());
    assert_eq!(g.all_nodes().unwrap().len(), 1);

    tx.commit().unwrap();

    // After commit the live engine reflects everything atomically.
    assert!(g.get_node(a.id).unwrap().is_some());
    assert_eq!(g.all_nodes().unwrap().len(), 2);
    assert_eq!(
        g.neighbor_ids(seed.id, Direction::Outgoing, None).unwrap(),
        vec![a.id]
    );
}

#[test]
fn native_tx_rollback_discards_writes() {
    use drevo::native::NativeGraph;
    let g = NativeGraph::new();
    g.create_node(new_node("k", "keep")).unwrap();
    let before = g.all_nodes().unwrap().len();

    let mut tx = g.begin();
    tx.create_node(new_node("k", "ghost")).unwrap();
    tx.rollback();

    assert_eq!(g.all_nodes().unwrap().len(), before);
}

#[test]
fn native_tx_detects_write_conflict() {
    use drevo::native::NativeGraph;
    let g = NativeGraph::new();

    // Two transactions from the same base.
    let mut t1 = g.begin();
    let mut t2 = g.begin();
    t1.create_node(new_node("k", "x")).unwrap();
    t2.create_node(new_node("k", "y")).unwrap();

    // First commit wins; the second sees the graph moved under it and conflicts.
    t1.commit().unwrap();
    assert!(t2.commit().is_err());

    // The winner's write is the only one applied.
    let titles: Vec<String> = g
        .all_nodes()
        .unwrap()
        .into_iter()
        .map(|n| n.title)
        .collect();
    assert_eq!(titles, vec!["x".to_string()]);
}

// ---------------------------------------------------------------------------
// Phase 3.3 — constraints (ACID "C"): UNIQUE(kind, property) validated at
// transaction commit, and when the constraint is declared over existing data.
// ---------------------------------------------------------------------------

fn node_with_prop(kind: &str, title: &str, prop: &str, val: &str) -> NewNode {
    let mut nn = new_node(kind, title);
    nn.properties = drevo::model::Properties(std::collections::HashMap::from([(
        prop.to_string(),
        serde_json::Value::String(val.to_string()),
    )]));
    nn
}

#[test]
fn native_unique_constraint_enforced_at_commit() {
    use drevo::native::{CommitError, Constraint, NativeGraph};

    let g = NativeGraph::new();
    g.add_constraint(Constraint::UniqueNodeProperty {
        kind: "user".into(),
        property: "email".into(),
    })
    .unwrap();

    // Two users with the same email → the whole commit is rejected, nothing lands.
    let mut tx = g.begin();
    tx.create_node(node_with_prop("user", "u1", "email", "a@x"))
        .unwrap();
    tx.create_node(node_with_prop("user", "u2", "email", "a@x"))
        .unwrap();
    assert!(matches!(tx.commit(), Err(CommitError::Constraint(_))));
    assert_eq!(g.all_nodes().unwrap().len(), 0);

    // Distinct emails commit fine.
    let mut tx = g.begin();
    tx.create_node(node_with_prop("user", "u1", "email", "a@x"))
        .unwrap();
    tx.create_node(node_with_prop("user", "u2", "email", "b@x"))
        .unwrap();
    tx.commit().unwrap();
    assert_eq!(g.all_nodes().unwrap().len(), 2);

    // A different kind with the same property value is unaffected by the constraint.
    let mut tx = g.begin();
    tx.create_node(node_with_prop("bot", "b1", "email", "a@x"))
        .unwrap();
    tx.commit().unwrap();
    assert_eq!(g.all_nodes().unwrap().len(), 3);
}

#[test]
fn native_add_constraint_rejects_violating_existing_data() {
    use drevo::native::{Constraint, NativeGraph};

    let g = NativeGraph::new();
    // Seed two accounts sharing a code.
    let mut tx = g.begin();
    tx.create_node(node_with_prop("acct", "x", "code", "1"))
        .unwrap();
    tx.create_node(node_with_prop("acct", "y", "code", "1"))
        .unwrap();
    tx.commit().unwrap();

    // Declaring UNIQUE over the already-duplicated data fails and is not stored.
    assert!(g
        .add_constraint(Constraint::UniqueNodeProperty {
            kind: "acct".into(),
            property: "code".into(),
        })
        .is_err());

    // Since it was not stored, a later duplicate still commits (constraint absent).
    let mut tx = g.begin();
    tx.create_node(node_with_prop("acct", "z", "code", "1"))
        .unwrap();
    assert!(tx.commit().is_ok());
}

// ---------------------------------------------------------------------------
// Phase 3.4 — WAL substrate (ACID "D" foundation): a logical operation log the
// engine's state can be dumped to and deterministically replayed from.
// ---------------------------------------------------------------------------

#[test]
fn wal_dump_and_replay_roundtrip() {
    use drevo::native::NativeGraph;

    let g = NativeGraph::new();
    let a = g.create_node(new_node("person", "a")).unwrap();
    let b = g.create_node(new_node("person", "b")).unwrap();
    let t = g.create_node(new_node("tag", "t")).unwrap();
    g.create_edge(new_edge(a.id, b.id, "KNOWS", 2.0)).unwrap();
    g.create_edge(new_edge(a.id, t.id, "TAGGED", 1.0)).unwrap();
    g.update_node(
        b.id,
        NodePatch {
            body: Some("hi".into()),
            ..Default::default()
        },
    )
    .unwrap();

    // Dump the state as an op log, round-trip it through JSON, replay into a
    // fresh engine, and assert observable parity.
    let ops = g.dump_wal();
    let json = serde_json::to_string(&ops).unwrap();
    let ops2: Vec<drevo::native::WalOp> = serde_json::from_str(&json).unwrap();
    let g2 = NativeGraph::replay(ops2);

    assert_same_state(&g, &g2);
}

#[test]
fn wal_replay_honours_delete_ops_and_advances_ids() {
    use drevo::native::{NativeGraph, WalOp};

    // Build source records with real ids/uuids.
    let src = NativeGraph::new();
    let a = src.create_node(new_node("k", "a")).unwrap();
    let b = src.create_node(new_node("k", "b")).unwrap();
    let e = src.create_edge(new_edge(a.id, b.id, "E", 1.0)).unwrap();

    // An explicit log: create both nodes + edge, then delete the edge and node b.
    let log = vec![
        WalOp::UpsertNode(a.clone()),
        WalOp::UpsertNode(b.clone()),
        WalOp::UpsertEdge(e.clone()),
        WalOp::DeleteEdge(e.id),
        WalOp::DeleteNode(b.id),
    ];
    let g = NativeGraph::replay(log);

    assert_eq!(g.all_nodes().unwrap().len(), 1);
    assert!(g.get_node(a.id).unwrap().is_some());
    assert!(g.get_node(b.id).unwrap().is_none());
    assert!(g.all_edges().unwrap().is_empty());

    // Replayed ids advance the counters: a fresh create gets a new id.
    let c = g.create_node(new_node("k", "c")).unwrap();
    assert!(c.id > a.id && c.id > b.id);
}

// ---------------------------------------------------------------------------
// Phase 3.5 — file-backed WAL (ACID "D"): direct writes are fsynced to a log,
// and reopening the path reconstructs the graph after a "crash" (drop).
// ---------------------------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn durable_wal_survives_reopen() {
    use drevo::native::NativeGraph;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.wal");

    let (a_id, b_id, e_id) = {
        let g = NativeGraph::open_durable(&path).unwrap();
        let a = g.create_node(new_node("k", "a")).unwrap();
        let b = g.create_node(new_node("k", "b")).unwrap();
        let e = g.create_edge(new_edge(a.id, b.id, "KNOWS", 2.0)).unwrap();
        g.update_node(
            b.id,
            NodePatch {
                body: Some("hi".into()),
                ..Default::default()
            },
        )
        .unwrap();
        // A create+delete pair must not resurrect on replay.
        let ghost = g.create_node(new_node("k", "ghost")).unwrap();
        g.delete_node(ghost.id).unwrap();
        (a.id, b.id, e.id)
    }; // engine dropped — equivalent to a crash (every write already fsynced)

    // Reopen: the graph is reconstructed purely from the WAL.
    let g2 = NativeGraph::open_durable(&path).unwrap();
    assert_eq!(g2.all_nodes().unwrap().len(), 2);
    assert_eq!(g2.get_node(b_id).unwrap().unwrap().body, "hi");
    assert_eq!(
        g2.neighbor_ids(a_id, Direction::Outgoing, None).unwrap(),
        vec![b_id]
    );
    assert_eq!(g2.get_edge(e_id).unwrap().unwrap().weight, 2.0);

    // Recovery advanced the id counters: a new node gets a fresh id.
    let n = g2.create_node(new_node("k", "after")).unwrap();
    assert!(n.id > b_id);
}

// ---------------------------------------------------------------------------
// Phase 3.6 — durable transactions: a committed transaction's writes are WAL-
// logged and survive reopen; a rolled-back transaction leaves no trace.
// ---------------------------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn durable_tx_commit_persists_and_rollback_does_not() {
    use drevo::native::NativeGraph;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tx.wal");

    let committed_node;
    {
        let g = NativeGraph::open_durable(&path).unwrap();

        // A committed transaction is durable as one atomic batch.
        let mut tx = g.begin();
        let a = tx.create_node(new_node("k", "a")).unwrap();
        let b = tx.create_node(new_node("k", "b")).unwrap();
        tx.create_edge(new_edge(a.id, b.id, "E", 1.5)).unwrap();
        committed_node = a.id;
        tx.commit().unwrap();

        // A rolled-back transaction writes nothing to the log.
        let mut tx2 = g.begin();
        tx2.create_node(new_node("k", "ghost")).unwrap();
        tx2.rollback();
    } // crash

    let g2 = NativeGraph::open_durable(&path).unwrap();
    assert_eq!(g2.all_nodes().unwrap().len(), 2); // a, b — ghost never persisted
    assert!(g2.get_node(committed_node).unwrap().is_some());
    assert_eq!(g2.all_edges().unwrap().len(), 1);
    let edges = g2.all_edges().unwrap();
    assert_eq!(edges[0].weight, 1.5);
}

// ---------------------------------------------------------------------------
// Phase 3.7 — WAL compaction: rewrite the log as the compact snapshot form so
// it does not grow without bound, without changing the recovered graph.
// ---------------------------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn compact_wal_shrinks_log_and_preserves_state() {
    use drevo::native::NativeGraph;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.wal");

    let node_id;
    {
        let g = NativeGraph::open_durable(&path).unwrap();
        let a = g.create_node(new_node("k", "a")).unwrap();
        let b = g.create_node(new_node("k", "b")).unwrap();
        g.create_edge(new_edge(a.id, b.id, "E", 1.0)).unwrap();
        node_id = a.id;

        // Many overwrites of the same node append many superseded log lines.
        for i in 0..50 {
            g.update_node(
                a.id,
                NodePatch {
                    body: Some(format!("v{i}")),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let lines_before = std::fs::read_to_string(&path).unwrap().lines().count();
        assert!(lines_before > 50);

        g.compact_wal().unwrap();
        let lines_after = std::fs::read_to_string(&path).unwrap().lines().count();
        assert_eq!(lines_after, 3); // 2 nodes + 1 edge, snapshot form
        assert!(lines_after < lines_before);

        // The engine keeps working; a further write appends to the compacted log.
        g.create_node(new_node("k", "c")).unwrap();
    } // crash

    // Reopen: the compacted snapshot plus the post-compaction write replay to
    // the correct final state.
    let g2 = NativeGraph::open_durable(&path).unwrap();
    assert_eq!(g2.all_nodes().unwrap().len(), 3);
    assert_eq!(g2.get_node(node_id).unwrap().unwrap().body, "v49");
    assert_eq!(g2.all_edges().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Phase 3.8 — EXISTS and NODE KEY constraints, rounding out ACID "C" to full
// Neo4j schema-constraint parity. All validated at transaction commit.
// ---------------------------------------------------------------------------

fn node_with_props(kind: &str, title: &str, props: &[(&str, &str)]) -> NewNode {
    let mut nn = new_node(kind, title);
    nn.properties = drevo::model::Properties(
        props
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
            .collect(),
    );
    nn
}

#[test]
fn native_property_exists_constraint() {
    use drevo::native::{CommitError, Constraint, NativeGraph};

    let g = NativeGraph::new();
    g.add_constraint(Constraint::PropertyExists {
        kind: "user".into(),
        property: "email".into(),
    })
    .unwrap();

    // A user without the required property is rejected at commit.
    let mut tx = g.begin();
    tx.create_node(new_node("user", "no-email")).unwrap();
    assert!(matches!(tx.commit(), Err(CommitError::Constraint(_))));
    assert_eq!(g.all_nodes().unwrap().len(), 0);

    // With the property, it commits; other kinds are unaffected.
    let mut tx = g.begin();
    tx.create_node(node_with_props("user", "u", &[("email", "a@x")]))
        .unwrap();
    tx.create_node(new_node("bot", "b")).unwrap();
    tx.commit().unwrap();
    assert_eq!(g.all_nodes().unwrap().len(), 2);
}

#[test]
fn native_node_key_constraint() {
    use drevo::native::{CommitError, Constraint, NativeGraph};

    let g = NativeGraph::new();
    g.add_constraint(Constraint::NodeKey {
        kind: "person".into(),
        properties: vec!["first".into(), "last".into()],
    })
    .unwrap();

    // Missing a key property → rejected.
    let mut tx = g.begin();
    tx.create_node(node_with_props("person", "p", &[("first", "ann")]))
        .unwrap();
    assert!(matches!(tx.commit(), Err(CommitError::Constraint(_))));

    // Duplicate (first,last) tuple → rejected.
    let mut tx = g.begin();
    tx.create_node(node_with_props(
        "person",
        "p1",
        &[("first", "ann"), ("last", "lee")],
    ))
    .unwrap();
    tx.create_node(node_with_props(
        "person",
        "p2",
        &[("first", "ann"), ("last", "lee")],
    ))
    .unwrap();
    assert!(matches!(tx.commit(), Err(CommitError::Constraint(_))));
    assert_eq!(g.all_nodes().unwrap().len(), 0);

    // Distinct tuples commit.
    let mut tx = g.begin();
    tx.create_node(node_with_props(
        "person",
        "p1",
        &[("first", "ann"), ("last", "lee")],
    ))
    .unwrap();
    tx.create_node(node_with_props(
        "person",
        "p2",
        &[("first", "ann"), ("last", "kim")],
    ))
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(g.all_nodes().unwrap().len(), 2);
}

#[test]
fn native_add_exists_constraint_rejects_violating_data() {
    use drevo::native::{Constraint, NativeGraph};

    let g = NativeGraph::new();
    let mut tx = g.begin();
    tx.create_node(new_node("user", "no-email")).unwrap();
    tx.commit().unwrap();

    assert!(g
        .add_constraint(Constraint::PropertyExists {
            kind: "user".into(),
            property: "email".into(),
        })
        .is_err());
}
