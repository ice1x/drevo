//! Betweenness centrality over the native engine and `CALL drevo.betweenness()`
//! (RFC #307 Phase 8). Same pattern as the other analytics slices: the KV
//! `Drevo::betweenness_centrality` is the oracle, and a directed diamond
//! (1 -> {2,3} -> 4) pins the split-dependency behaviour. Gated on
//! `redb-backend` for the KV oracle.

#![cfg(feature = "redb-backend")]

use std::collections::HashMap;

use drevo::algorithms::betweenness_native;
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

/// Directed diamond 0 -> {1,2} -> 3. The pair (0,3) has two equally short
/// paths, so nodes 1 and 2 each carry half the dependency (0.5); the source
/// and sink carry none.
fn diamond<E: GraphEngine>(engine: &E) -> Vec<u64> {
    let ids: Vec<u64> = (0..4).map(|i| node(engine, &format!("n{i}"))).collect();
    edge(engine, ids[0], ids[1]);
    edge(engine, ids[0], ids[2]);
    edge(engine, ids[1], ids[3]);
    edge(engine, ids[2], ids[3]);
    ids
}

#[test]
fn native_betweenness_matches_kv_oracle() {
    let kv = Drevo::open_in_memory().expect("kv");
    let kids = diamond(&kv);
    let kv_bt = kv.betweenness_centrality().expect("kv betweenness");

    let native = NativeGraph::new();
    let nids = diamond(&native);
    let nat_bt = betweenness_native(&native);

    assert_eq!(kids, nids, "engines assigned different ids");
    assert_eq!(
        nat_bt, kv_bt,
        "native betweenness diverged from the KV oracle"
    );
}

/// `CALL drevo.betweenness() YIELD node, score` → title → score.
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

const BT_CYPHER: &str =
    "CALL drevo.betweenness() YIELD node, score RETURN node.title AS t, score AS s";

fn assert_diamond(scores: &HashMap<String, f64>) {
    assert_eq!(scores.len(), 4, "one row per node");
    assert!((scores["n1"] - 0.5).abs() < 1e-12, "n1 = {}", scores["n1"]);
    assert!((scores["n2"] - 0.5).abs() < 1e-12, "n2 = {}", scores["n2"]);
    assert_eq!(scores["n0"], 0.0);
    assert_eq!(scores["n3"], 0.0);
}

#[test]
fn call_drevo_betweenness_over_cypher_on_kv() {
    let kv = Drevo::open_in_memory().expect("kv");
    diamond(&kv);
    let q = parse(BT_CYPHER).expect("parse");
    let res = execute(&q, &kv, HashMap::new()).expect("execute");
    assert_diamond(&cypher_scores(&res.rows));
}

#[test]
fn call_drevo_betweenness_over_cypher_on_the_native_engine() {
    let native = NativeGraph::new();
    diamond(&native);
    let q = parse(BT_CYPHER).expect("parse");
    let res = execute_on_engine(&q, &native, HashMap::new()).expect("execute on native");
    assert_diamond(&cypher_scores(&res.rows));
}
