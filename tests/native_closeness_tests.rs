//! Harmonic closeness centrality over the native engine and
//! `CALL drevo.closeness()` (RFC #307 Phase 8). Same pattern as the other
//! analytics slices: the KV `Drevo::closeness_centrality` is the oracle, and a
//! directed path 0 -> 1 -> 2 with an unreachable extra node pins the reciprocal
//! sums and the disconnected-stays-finite behaviour. Gated on `redb-backend`
//! for the KV oracle.

#![cfg(feature = "redb-backend")]

use std::collections::HashMap;

use drevo::algorithms::closeness_native;
use drevo::cypher::executor::{execute, execute_on_engine, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;
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

/// Directed path 0 -> 1 -> 2, plus an isolated node 3 that nothing reaches and
/// that reaches nothing. From 0: reach 1 (d=1) and 2 (d=2) → 1 + 1/2 = 1.5;
/// from 1: 1.0; from 2 and 3: 0.
fn path_with_isolate<E: GraphEngine>(engine: &E) -> Vec<u64> {
    let ids: Vec<u64> = (0..4).map(|i| node(engine, &format!("n{i}"))).collect();
    edge(engine, ids[0], ids[1]);
    edge(engine, ids[1], ids[2]);
    // ids[3] isolated.
    ids
}

#[test]
fn native_closeness_matches_kv_oracle() {
    let kv = Drevo::open_in_memory().expect("kv");
    let kids = path_with_isolate(&kv);
    let kv_cl = kv.closeness_centrality().expect("kv closeness");

    let native = NativeGraph::new();
    let nids = path_with_isolate(&native);
    let nat_cl = closeness_native(&native);

    assert_eq!(kids, nids, "engines assigned different ids");
    assert_eq!(
        nat_cl, kv_cl,
        "native closeness diverged from the KV oracle"
    );
}

/// `CALL drevo.closeness() YIELD node, score` → title → score.
fn cypher_scores(rows: &[Vec<Value>]) -> HashMap<String, f64> {
    rows.iter()
        .map(|r| {
            let title = match &r[0] {
                Value::String(t) => t.clone(),
                other => panic!("expected title string, got {other:?}"),
            };
            let score = match &r[1] {
                Value::Float(f) => *f,
                other => panic!("expected float score, got {other:?}"),
            };
            (title, score)
        })
        .collect()
}

const CL_CYPHER: &str =
    "CALL drevo.closeness() YIELD node, score RETURN node.title AS t, score AS s";

fn assert_path_with_isolate(scores: &HashMap<String, f64>) {
    assert_eq!(scores.len(), 4, "one row per node");
    assert!((scores["n0"] - 1.5).abs() < 1e-12, "n0 = {}", scores["n0"]);
    assert!((scores["n1"] - 1.0).abs() < 1e-12, "n1 = {}", scores["n1"]);
    assert_eq!(scores["n2"], 0.0);
    assert_eq!(scores["n3"], 0.0);
}

#[test]
fn call_drevo_closeness_over_cypher_on_kv() {
    let kv = Drevo::open_in_memory().expect("kv");
    path_with_isolate(&kv);
    let q = parse(CL_CYPHER).expect("parse");
    let res = execute(&q, &kv, HashMap::new()).expect("execute");
    assert_path_with_isolate(&cypher_scores(&res.rows));
}

#[test]
fn call_drevo_closeness_over_cypher_on_the_native_engine() {
    let native = NativeGraph::new();
    path_with_isolate(&native);
    let q = parse(CL_CYPHER).expect("parse");
    let res = execute_on_engine(&q, &native, HashMap::new()).expect("execute on native");
    assert_path_with_isolate(&cypher_scores(&res.rows));
}
