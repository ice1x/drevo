//! Adjacency-value denormalization — public-API behavior (#243 slice 1).
//!
//! Since #243, each `out:`/`in:` adjacency entry stores the **neighbor node
//! id + edge kind** in its value (previously empty). This lets `neighbor_ids`
//! answer "who is adjacent to X" straight from one prefix scan — no `get_edge`
//! per neighbor — and keeps kind-filtered fan-out cheap on supernodes.
//!
//! These tests pin the observable contract: `neighbor_ids` returns the right
//! distinct ids under every direction / kind filter, stays in lock-step with
//! `neighbors`, survives edge churn, and `backfill_adjacency_values` is a safe
//! idempotent no-op on an already-denormalized database. The read-count
//! *reduction* itself is proven by a counting-backend unit test in
//! `src/db.rs`; here we prove the results are correct.

use drevo::db::Drevo;
use drevo::model::{Direction, NewEdge, NewNode, Properties};

fn node(db: &Drevo, title: &str) -> u64 {
    db.create_node(NewNode {
        kind: "n".to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    })
    .expect("create node")
    .id
}

fn edge(db: &Drevo, from: u64, to: u64, kind: &str) {
    db.create_edge(NewEdge {
        from_id: from,
        to_id: to,
        kind: kind.to_string(),
        weight: 1.0,
        properties: Properties::default(),
    })
    .expect("create edge");
}

/// The distinct neighbor ids `neighbors()` would surface, so the two APIs can
/// be compared without depending on node-load ordering internals.
fn neighbor_ids_via_nodes(db: &Drevo, id: u64, dir: Direction, kind: Option<&str>) -> Vec<u64> {
    db.neighbors(id, dir, kind)
        .expect("neighbors")
        .into_iter()
        .map(|n| n.id)
        .collect()
}

#[test]
fn neighbor_ids_outgoing_incoming_both() {
    let db = Drevo::open_in_memory().unwrap();
    let a = node(&db, "a");
    let b = node(&db, "b");
    let c = node(&db, "c");
    edge(&db, a, b, "knows");
    edge(&db, a, c, "knows");
    edge(&db, c, a, "follows");

    let mut out = db.neighbor_ids(a, Direction::Outgoing, None).unwrap();
    out.sort_unstable();
    assert_eq!(out, vec![b, c], "a -> {{b, c}}");

    let inc = db.neighbor_ids(a, Direction::Incoming, None).unwrap();
    assert_eq!(inc, vec![c], "c -> a is the only incoming");

    let mut both = db.neighbor_ids(a, Direction::Both, None).unwrap();
    both.sort_unstable();
    assert_eq!(both, vec![b, c], "both directions, c counted once");
}

#[test]
fn neighbor_ids_kind_filter() {
    let db = Drevo::open_in_memory().unwrap();
    let a = node(&db, "a");
    let b = node(&db, "b");
    let c = node(&db, "c");
    edge(&db, a, b, "knows");
    edge(&db, a, c, "blocks");

    assert_eq!(
        db.neighbor_ids(a, Direction::Outgoing, Some("knows"))
            .unwrap(),
        vec![b]
    );
    assert_eq!(
        db.neighbor_ids(a, Direction::Outgoing, Some("blocks"))
            .unwrap(),
        vec![c]
    );
    assert!(db
        .neighbor_ids(a, Direction::Outgoing, Some("nope"))
        .unwrap()
        .is_empty());
}

#[test]
fn neighbor_ids_self_loop_excludes_self() {
    let db = Drevo::open_in_memory().unwrap();
    let a = node(&db, "a");
    edge(&db, a, a, "self");
    assert!(
        db.neighbor_ids(a, Direction::Both, None)
            .unwrap()
            .is_empty(),
        "a self-loop contributes no neighbor (the node itself is excluded)"
    );
}

#[test]
fn neighbor_ids_deduplicates_parallel_edges() {
    let db = Drevo::open_in_memory().unwrap();
    let a = node(&db, "a");
    let b = node(&db, "b");
    // Two parallel edges a -> b of different kinds.
    edge(&db, a, b, "knows");
    edge(&db, a, b, "likes");
    assert_eq!(
        db.neighbor_ids(a, Direction::Outgoing, None).unwrap(),
        vec![b],
        "b reported once despite two parallel edges"
    );
    // ...but the kind filter still selects the right parallel edge.
    assert_eq!(
        db.neighbor_ids(a, Direction::Outgoing, Some("likes"))
            .unwrap(),
        vec![b]
    );
}

#[test]
fn neighbor_ids_matches_neighbors_across_directions() {
    let db = Drevo::open_in_memory().unwrap();
    let ids: Vec<u64> = (0..8).map(|i| node(&db, &format!("n{i}"))).collect();
    // A little web with mixed kinds and directions.
    edge(&db, ids[0], ids[1], "a");
    edge(&db, ids[0], ids[2], "b");
    edge(&db, ids[3], ids[0], "a");
    edge(&db, ids[4], ids[0], "b");
    edge(&db, ids[0], ids[5], "a");
    edge(&db, ids[6], ids[0], "a");

    for &dir in &[Direction::Outgoing, Direction::Incoming, Direction::Both] {
        for kind in [None, Some("a"), Some("b")] {
            let via_ids = db.neighbor_ids(ids[0], dir, kind).unwrap();
            let via_nodes = neighbor_ids_via_nodes(&db, ids[0], dir, kind);
            assert_eq!(
                via_ids, via_nodes,
                "neighbor_ids must match neighbors() for dir={dir:?} kind={kind:?}"
            );
        }
    }
}

#[test]
fn neighbor_ids_survives_edge_delete() {
    let db = Drevo::open_in_memory().unwrap();
    let a = node(&db, "a");
    let b = node(&db, "b");
    let c = node(&db, "c");
    let e = db
        .create_edge(NewEdge {
            from_id: a,
            to_id: b,
            kind: "knows".to_string(),
            weight: 1.0,
            properties: Properties::default(),
        })
        .unwrap();
    edge(&db, a, c, "knows");

    db.delete_edge(e.id).unwrap();
    assert_eq!(
        db.neighbor_ids(a, Direction::Outgoing, None).unwrap(),
        vec![c],
        "the deleted edge's neighbor is gone; the surviving one remains"
    );
}

#[test]
fn backfill_is_idempotent_noop_on_denormalized_db() {
    // A database created since #243 is already denormalized, so backfill has
    // nothing to upgrade and can be run repeatedly with no effect.
    let db = Drevo::open_in_memory().unwrap();
    let a = node(&db, "a");
    let b = node(&db, "b");
    edge(&db, a, b, "knows");

    assert_eq!(db.backfill_adjacency_values().unwrap(), 0);
    assert_eq!(db.backfill_adjacency_values().unwrap(), 0);
    assert_eq!(
        db.neighbor_ids(a, Direction::Outgoing, None).unwrap(),
        vec![b]
    );
}
