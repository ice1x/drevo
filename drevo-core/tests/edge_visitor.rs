//! `for_each_edge_of` visits the same relationships, in the same order, as
//! `edges_of`, without copying them: on the store, a snapshot, and inside a
//! transaction (including what the transaction wrote).
//!
//! The scenario is a small street network: one-way and two-way roads around
//! a junction.

use drevo_core::engine::GraphEngine;
use drevo_core::model::{Direction, Edge, NewEdge, NewNode, Properties};
use drevo_core::native::NativeGraph;

fn junction(name: &str) -> NewNode {
    NewNode {
        kind: "Junction".to_string(),
        title: name.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::new(),
    }
}

fn road(from: u64, to: u64, kind: &str, secs: f64) -> NewEdge {
    NewEdge {
        from_id: from,
        to_id: to,
        kind: kind.to_string(),
        weight: 1.0,
        properties: Properties::from([("car_s", serde_json::json!(secs))]),
    }
}

fn ids(edges: &[Edge]) -> Vec<u64> {
    edges.iter().map(|e| e.id).collect()
}

fn visited(engine: &dyn GraphEngine, node: u64, dir: Direction) -> Vec<u64> {
    let mut out = Vec::new();
    engine
        .for_each_edge_of(node, dir, &mut |e| out.push(e.id))
        .unwrap();
    out
}

#[test]
fn the_visitor_matches_edges_of_everywhere() {
    let g = NativeGraph::new();
    let [hub, a, b, c] = ["hub", "a", "b", "c"].map(|n| g.create_node(junction(n)).unwrap().id);
    g.create_edges(vec![
        road(hub, a, "ROAD", 12.0),
        road(a, hub, "ROAD", 12.0),
        road(hub, b, "PATH", 40.0),
        road(c, hub, "ROAD", 9.5),
        road(hub, hub, "ROAD", 0.0),
    ])
    .unwrap();

    let dirs = [Direction::Outgoing, Direction::Incoming, Direction::Both];
    for dir in dirs {
        let expected = ids(&GraphEngine::edges_of(&g, hub, dir).unwrap());
        assert!(!expected.is_empty());
        assert_eq!(visited(&g, hub, dir), expected, "store, {dir:?}");
        let snap = g.snapshot();
        let mut seen = Vec::new();
        snap.for_each_edge_of(hub, dir, |e| seen.push(e.id));
        assert_eq!(seen, ids(&snap.edges_of(hub, dir)), "snapshot, {dir:?}");
    }

    // The visitor reads the fields a search needs, by reference.
    let mut costs = Vec::new();
    g.for_each_edge_of(hub, Direction::Outgoing, &mut |e| {
        costs.push((e.to_id, e.kind.clone(), e.properties.get("car_s").cloned()));
    })
    .unwrap();
    assert!(costs.contains(&(b, "PATH".to_string(), Some(serde_json::json!(40.0)))));

    // A transaction sees its own writes, and the store does not.
    let tx = g.tx_begin();
    let engine = g.tx_engine(tx).unwrap();
    let new_road = engine.create_edge(road(hub, c, "ROAD", 7.0)).unwrap();
    for dir in dirs {
        let expected = ids(&engine.edges_of(hub, dir).unwrap());
        assert_eq!(visited(&engine, hub, dir), expected, "transaction, {dir:?}");
    }
    assert!(visited(&engine, hub, Direction::Outgoing).contains(&new_road.id));
    assert!(!visited(&g, hub, Direction::Outgoing).contains(&new_road.id));
    g.tx_rollback(tx);

    // A node with no relationships, or none at all, visits nothing.
    assert!(visited(&g, 9_999, Direction::Both).is_empty());
}
