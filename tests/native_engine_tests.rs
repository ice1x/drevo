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
