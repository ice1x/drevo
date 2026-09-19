//! Triangle counting + local clustering coefficient over the native engine and
//! `CALL drevo.triangles()` (RFC #307 Phase 8). Same pattern as the other
//! analytics slices: the KV `Drevo::triangle_counts` is the oracle, and a
//! hand-built graph — a triangle {0,1,2} with a pendant 2-3 — pins the counts
//! and coefficients. Gated on `redb-backend` for the `CALL drevo.<algo>()` procedure path.

use std::collections::HashMap;

use drevo::algorithms::{triangles, triangles_native, AdjacencyList};
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

/// Triangle {0,1,2} plus a pendant edge 2 -> 3. One triangle; node 2 has three
/// neighbours of which one pair is adjacent → coefficient 1/3; node 3 is a
/// degree-1 pendant → coefficient 0.
fn triangle_with_pendant<E: GraphEngine>(engine: &E) -> Vec<u64> {
    let ids: Vec<u64> = (0..4).map(|i| node(engine, &format!("n{i}"))).collect();
    edge(engine, ids[0], ids[1]);
    edge(engine, ids[1], ids[2]);
    edge(engine, ids[2], ids[0]);
    edge(engine, ids[2], ids[3]); // pendant
    ids
}

#[test]
fn native_triangles_match_the_reference_over_the_raw_edge_list() {
    let native = NativeGraph::new();
    let ids = triangle_with_pendant(&native);
    let nat_tri = triangles_native(&native);

    // Reference: the same serial triangle count over an adjacency list built
    // straight from the known edge list (triangle {0,1,2} + pendant 2->3).
    let reference = triangles(&AdjacencyList::from_parts(
        ids.clone(),
        vec![
            (ids[0], ids[1], 1.0f32),
            (ids[1], ids[2], 1.0),
            (ids[2], ids[0], 1.0),
            (ids[2], ids[3], 1.0),
        ],
    ));
    assert_eq!(
        nat_tri, reference,
        "native triangles diverged from the reference"
    );
    assert_eq!(nat_tri.total_triangles, 1);
}

/// `CALL drevo.triangles() YIELD node, triangles, coefficient` → title → (t, c).
fn cypher_rows(rows: &[Vec<Value>]) -> HashMap<String, (i64, f64)> {
    rows.iter()
        .map(|r| {
            let title = match &r[0] {
                Value::String(t) => t.clone(),
                other => panic!("expected title string, got {other:?}"),
            };
            let count = match &r[1] {
                Value::Integer(c) => *c,
                other => panic!("expected integer triangles, got {other:?}"),
            };
            let coeff = match &r[2] {
                Value::Float(f) => *f,
                other => panic!("expected float coefficient, got {other:?}"),
            };
            (title, (count, coeff))
        })
        .collect()
}

const TRI_CYPHER: &str = "CALL drevo.triangles() YIELD node, triangles, coefficient \
                          RETURN node.title AS t, triangles AS tri, coefficient AS c";

fn assert_triangle_with_pendant(rows: &HashMap<String, (i64, f64)>) {
    assert_eq!(rows.len(), 4, "one row per node");
    // The three triangle members each see exactly one triangle.
    assert_eq!(rows["n0"].0, 1);
    assert_eq!(rows["n1"].0, 1);
    assert_eq!(rows["n2"].0, 1);
    // n0 and n1 have degree 2 with their two neighbours adjacent → coefficient 1.
    assert!((rows["n0"].1 - 1.0).abs() < 1e-12);
    assert!((rows["n1"].1 - 1.0).abs() < 1e-12);
    // n2 has degree 3, one adjacent pair of three → coefficient 1/3.
    assert!((rows["n2"].1 - 1.0 / 3.0).abs() < 1e-12);
    // The pendant is in no triangle, degree 1 → coefficient 0.
    assert_eq!(rows["n3"], (0, 0.0));
}

#[test]
fn call_drevo_triangles_over_cypher_on_the_native_engine() {
    let native = NativeGraph::new();
    triangle_with_pendant(&native);
    let q = parse(TRI_CYPHER).expect("parse");
    let res = execute_on_engine(&q, &native, HashMap::new()).expect("execute on native");
    assert_triangle_with_pendant(&cypher_rows(&res.rows));
}
