//! Weakly connected components over the native engine + `CALL drevo.wcc()`
//! (RFC #307 Phase 8). Same pattern as the PageRank / Louvain slices: the KV
//! `Drevo::weakly_connected_components` is the oracle, and a hand-built graph
//! with two disjoint clusters plus an isolated node pins the structure.
//! Gated on `redb-backend` for the KV oracle.

#![cfg(feature = "redb-backend")]

use std::collections::HashMap;

use drevo::algorithms::wcc_native;
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

/// A directed chain 0 -> 1 -> 2, a separate edge 3 -> 4, and an isolated node 5.
/// Weakly connected, that is three components: {0,1,2}, {3,4}, {5}. Note the
/// chain edges are *directed* one way only — WCC must still merge them.
fn three_components<E: GraphEngine>(engine: &E) -> Vec<u64> {
    let ids: Vec<u64> = (0..6).map(|i| node(engine, &format!("n{i}"))).collect();
    edge(engine, ids[0], ids[1]);
    edge(engine, ids[1], ids[2]);
    edge(engine, ids[3], ids[4]);
    // ids[5] left isolated on purpose.
    ids
}

#[test]
fn native_wcc_matches_kv_oracle() {
    let kv = Drevo::open_in_memory().expect("kv");
    let kids = three_components(&kv);
    let kv_wcc = kv.weakly_connected_components().expect("kv wcc").components;

    let native = NativeGraph::new();
    let nids = three_components(&native);
    let nat_wcc = wcc_native(&native).components;

    assert_eq!(kids, nids, "engines assigned different ids");
    assert_eq!(nat_wcc, kv_wcc, "native WCC diverged from the KV oracle");
}

/// `CALL drevo.wcc() YIELD node, component` → title → component id.
fn cypher_components(rows: &[Vec<Value>]) -> HashMap<String, i64> {
    rows.iter()
        .map(|r| {
            let title = match &r[0] {
                Value::String(t) => t.clone(),
                other => panic!("expected title string, got {other:?}"),
            };
            let component = match &r[1] {
                Value::Integer(c) => *c,
                other => panic!("expected integer component, got {other:?}"),
            };
            (title, component)
        })
        .collect()
}

const WCC_CYPHER: &str =
    "CALL drevo.wcc() YIELD node, component RETURN node.title AS t, component AS c";

fn assert_three_components(comp: &HashMap<String, i64>) {
    assert_eq!(comp.len(), 6, "one row per node");
    // {0,1,2} is one component regardless of edge direction.
    assert_eq!(comp["n0"], comp["n1"]);
    assert_eq!(comp["n1"], comp["n2"]);
    // {3,4} is another.
    assert_eq!(comp["n3"], comp["n4"]);
    // Three distinct components overall.
    assert_ne!(comp["n0"], comp["n3"]);
    assert_ne!(comp["n0"], comp["n5"]);
    assert_ne!(comp["n3"], comp["n5"]);
}

#[test]
fn call_drevo_wcc_over_cypher_finds_three_components_on_kv() {
    let kv = Drevo::open_in_memory().expect("kv");
    three_components(&kv);
    let q = parse(WCC_CYPHER).expect("parse");
    let res = execute(&q, &kv, HashMap::new()).expect("execute");
    assert_three_components(&cypher_components(&res.rows));
}

#[test]
fn call_drevo_wcc_over_cypher_runs_on_the_native_engine() {
    let native = NativeGraph::new();
    three_components(&native);
    let q = parse(WCC_CYPHER).expect("parse");
    let res = execute_on_engine(&q, &native, HashMap::new()).expect("execute on native");
    assert_three_components(&cypher_components(&res.rows));
}
