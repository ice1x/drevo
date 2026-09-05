//! Strongly connected components over the native engine + `CALL drevo.scc()`
//! (RFC #307 Phase 8). Same pattern as the WCC slice, but direction matters:
//! the KV `Drevo::strongly_connected_components` is the oracle, and a hand-built
//! graph with two directed cycles joined by a one-way bridge plus an isolated
//! node pins the structure. Gated on `redb-backend` for the KV oracle.

#![cfg(feature = "redb-backend")]

use std::collections::HashMap;

use drevo::algorithms::scc_native;
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

/// Cycle {0,1}, cycle {2,3}, a one-way bridge 1 -> 2, and an isolated node 4.
/// Three strongly connected components: {0,1}, {2,3}, {4}. Note that as a
/// *weak* graph this is only two components — the directed bridge makes the
/// difference SCC captures.
fn three_sccs<E: GraphEngine>(engine: &E) -> Vec<u64> {
    let ids: Vec<u64> = (0..5).map(|i| node(engine, &format!("n{i}"))).collect();
    edge(engine, ids[0], ids[1]);
    edge(engine, ids[1], ids[0]); // cycle {0,1}
    edge(engine, ids[2], ids[3]);
    edge(engine, ids[3], ids[2]); // cycle {2,3}
    edge(engine, ids[1], ids[2]); // one-way bridge, does NOT merge the cycles
                                  // ids[4] isolated
    ids
}

#[test]
fn native_scc_matches_kv_oracle() {
    let kv = Drevo::open_in_memory().expect("kv");
    let kids = three_sccs(&kv);
    let kv_scc = kv
        .strongly_connected_components()
        .expect("kv scc")
        .components;

    let native = NativeGraph::new();
    let nids = three_sccs(&native);
    let nat_scc = scc_native(&native).components;

    assert_eq!(kids, nids, "engines assigned different ids");
    assert_eq!(nat_scc, kv_scc, "native SCC diverged from the KV oracle");
}

/// `CALL drevo.scc() YIELD node, component` → title → component id.
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

const SCC_CYPHER: &str =
    "CALL drevo.scc() YIELD node, component RETURN node.title AS t, component AS c";

fn assert_three_sccs(comp: &HashMap<String, i64>) {
    assert_eq!(comp.len(), 5, "one row per node");
    // The two cycles are each one component...
    assert_eq!(comp["n0"], comp["n1"]);
    assert_eq!(comp["n2"], comp["n3"]);
    // ...but the one-way bridge does NOT merge them.
    assert_ne!(comp["n0"], comp["n2"]);
    // The isolated node is its own component.
    assert_ne!(comp["n4"], comp["n0"]);
    assert_ne!(comp["n4"], comp["n2"]);
}

#[test]
fn call_drevo_scc_over_cypher_finds_three_components_on_kv() {
    let kv = Drevo::open_in_memory().expect("kv");
    three_sccs(&kv);
    let q = parse(SCC_CYPHER).expect("parse");
    let res = execute(&q, &kv, HashMap::new()).expect("execute");
    assert_three_sccs(&cypher_components(&res.rows));
}

#[test]
fn call_drevo_scc_over_cypher_runs_on_the_native_engine() {
    let native = NativeGraph::new();
    three_sccs(&native);
    let q = parse(SCC_CYPHER).expect("parse");
    let res = execute_on_engine(&q, &native, HashMap::new()).expect("execute on native");
    assert_three_sccs(&cypher_components(&res.rows));
}
