//! Ollivier–Ricci edge curvature over the native engine and
//! `CALL drevo.ricciCurvature()` (issue #526). Same pattern as the other
//! analytics slices: a "barbell" of two triangles joined by a single bridge
//! edge pins the sign contract — the bridge is negatively curved (it lies
//! between the two clusters) while the intra-triangle edges are positive — and
//! the raw-edge-list reference cross-checks the native snapshot path.

use std::collections::HashMap;

use drevo::algorithms::{ricci_curvature, ricci_curvature_native, AdjacencyList, RicciConfig};
use drevo::cypher::executor::{execute_on_engine, Value};
use drevo::cypher::parser::parse;
use drevo::engine::GraphEngine;
use drevo::model::{NewEdge, NewNode, Properties};
use drevo::native::NativeGraph;

fn node(engine: &impl GraphEngine, title: &str) -> u64 {
    engine
        .create_node(NewNode {
            kind: "n".into(),
            title: title.into(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        })
        .expect("create node")
        .id
}

fn edge(engine: &impl GraphEngine, from: u64, to: u64) {
    engine
        .create_edge(NewEdge {
            from_id: from,
            to_id: to,
            kind: "links".into(),
            weight: 1.0,
            properties: Properties::default(),
        })
        .expect("create edge");
}

/// Two triangles {n0,n1,n2} and {n3,n4,n5} joined by a single bridge n2 -> n3.
/// The bridge is the only path between the clusters, so it is negatively
/// curved; the six intra-triangle edges are positively curved.
fn barbell<E: GraphEngine>(engine: &E) -> Vec<u64> {
    let ids: Vec<u64> = (0..6).map(|i| node(engine, &format!("n{i}"))).collect();
    // First triangle.
    edge(engine, ids[0], ids[1]);
    edge(engine, ids[1], ids[2]);
    edge(engine, ids[2], ids[0]);
    // Second triangle.
    edge(engine, ids[3], ids[4]);
    edge(engine, ids[4], ids[5]);
    edge(engine, ids[5], ids[3]);
    // Bridge.
    edge(engine, ids[2], ids[3]);
    ids
}

#[test]
fn native_ricci_matches_the_reference_over_the_raw_edge_list() {
    let native = NativeGraph::new();
    let ids = barbell(&native);
    let cfg = RicciConfig::default();
    let nat = ricci_curvature_native(&native, &cfg);

    // Reference: the same solver over an adjacency list built straight from the
    // known edge list.
    let reference = ricci_curvature(
        &AdjacencyList::from_parts(
            ids.clone(),
            vec![
                (ids[0], ids[1], 1.0f32),
                (ids[1], ids[2], 1.0),
                (ids[2], ids[0], 1.0),
                (ids[3], ids[4], 1.0),
                (ids[4], ids[5], 1.0),
                (ids[5], ids[3], 1.0),
                (ids[2], ids[3], 1.0),
            ],
        ),
        &cfg,
    );
    assert_eq!(nat, reference, "native ricci diverged from the reference");
    assert_eq!(nat.per_edge.len(), 7, "one row per undirected edge");
}

/// `CALL drevo.ricciCurvature() YIELD from, to, curvature` → (from.title,
/// to.title) → curvature.
fn cypher_curvatures(rows: &[Vec<Value>]) -> HashMap<(String, String), f64> {
    rows.iter()
        .map(|r| {
            let from = match &r[0] {
                Value::String(t) => t.clone(),
                other => panic!("expected from title string, got {other:?}"),
            };
            let to = match &r[1] {
                Value::String(t) => t.clone(),
                other => panic!("expected to title string, got {other:?}"),
            };
            let curvature = match &r[2] {
                Value::Float(f) => *f,
                other => panic!("expected float curvature, got {other:?}"),
            };
            ((from, to), curvature)
        })
        .collect()
}

const RICCI_CYPHER: &str = "CALL drevo.ricciCurvature() YIELD from, to, curvature \
     RETURN from.title AS f, to.title AS t, curvature AS c";

#[test]
fn call_drevo_ricci_curvature_over_cypher_on_the_native_engine() {
    let native = NativeGraph::new();
    barbell(&native);
    let q = parse(RICCI_CYPHER).expect("parse");
    let res = execute_on_engine(&q, &native, HashMap::new()).expect("execute on native");
    let curv = cypher_curvatures(&res.rows);

    assert_eq!(curv.len(), 7, "one row per undirected edge");

    // The bridge n2-n3 is negatively curved.
    let bridge = curv[&("n2".to_string(), "n3".to_string())];
    assert!(
        bridge < 0.0,
        "bridge n2-n3 should be negative, got {bridge}"
    );

    // Every intra-triangle edge is positively curved and strictly above the
    // bridge.
    for (a, b) in [
        ("n0", "n1"),
        ("n1", "n2"),
        ("n0", "n2"),
        ("n3", "n4"),
        ("n4", "n5"),
        ("n3", "n5"),
    ] {
        let k = curv[&(a.to_string(), b.to_string())];
        assert!(k > 0.0, "intra edge {a}-{b} should be positive, got {k}");
        assert!(
            k > bridge,
            "intra edge {a}-{b} ({k}) should exceed bridge ({bridge})"
        );
    }
}
