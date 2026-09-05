//! Louvain community detection over the native engine + `CALL drevo.louvain()`
//! (RFC #307 Phase 8). Same pattern as the PageRank slice: the proven
//! single-threaded `algorithms::louvain` (via KV `Drevo::louvain_communities`)
//! is the oracle, and a hand-built two-cluster graph pins the structure.
//! Gated on `redb-backend` for the KV oracle.

#![cfg(feature = "redb-backend")]

use std::collections::HashMap;

use drevo::algorithms::{louvain_native, LouvainConfig};
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

/// Two triangles {0,1,2} and {3,4,5}, each edge added both directions, joined by
/// a single weak link 2→3 — a textbook two-community graph.
fn two_clusters<E: GraphEngine>(engine: &E) -> Vec<u64> {
    let ids: Vec<u64> = (0..6).map(|i| node(engine, &format!("n{i}"))).collect();
    let e = |a: usize, b: usize| {
        edge(engine, ids[a], ids[b]);
        edge(engine, ids[b], ids[a]);
    };
    e(0, 1);
    e(1, 2);
    e(2, 0);
    e(3, 4);
    e(4, 5);
    e(5, 3);
    edge(engine, ids[2], ids[3]); // the one bridge
    ids
}

#[test]
fn native_louvain_matches_kv_oracle() {
    let cfg = LouvainConfig::default();

    let kv = Drevo::open_in_memory().expect("kv");
    let kids = two_clusters(&kv);
    let kv_comm = kv
        .louvain_communities(&cfg)
        .expect("kv louvain")
        .communities;

    let native = NativeGraph::new();
    let nids = two_clusters(&native);
    let nat_comm = louvain_native(&native, &cfg).communities;

    assert_eq!(kids, nids, "engines assigned different ids");
    assert_eq!(
        nat_comm, kv_comm,
        "native Louvain diverged from the KV oracle"
    );
}

/// `CALL drevo.louvain() YIELD node, community` → title → community id.
fn cypher_communities(rows: &[Vec<Value>]) -> HashMap<String, i64> {
    rows.iter()
        .map(|r| {
            let title = match &r[0] {
                Value::String(t) => t.clone(),
                other => panic!("expected title string, got {other:?}"),
            };
            let community = match &r[1] {
                Value::Integer(c) => *c,
                other => panic!("expected integer community, got {other:?}"),
            };
            (title, community)
        })
        .collect()
}

const LOUVAIN_CYPHER: &str =
    "CALL drevo.louvain() YIELD node, community RETURN node.title AS t, community AS c";

fn assert_two_communities(comm: &HashMap<String, i64>) {
    assert_eq!(comm.len(), 6, "one row per node");
    // The two triangles must each be internally uniform and distinct.
    assert_eq!(comm["n0"], comm["n1"]);
    assert_eq!(comm["n1"], comm["n2"]);
    assert_eq!(comm["n3"], comm["n4"]);
    assert_eq!(comm["n4"], comm["n5"]);
    assert_ne!(comm["n0"], comm["n3"], "the two clusters must differ");
}

#[test]
fn call_drevo_louvain_over_cypher_finds_two_clusters_on_kv() {
    let kv = Drevo::open_in_memory().expect("kv");
    two_clusters(&kv);
    let q = parse(LOUVAIN_CYPHER).expect("parse");
    let res = execute(&q, &kv, HashMap::new()).expect("execute");
    assert_two_communities(&cypher_communities(&res.rows));
}

#[test]
fn call_drevo_louvain_over_cypher_runs_on_the_native_engine() {
    let native = NativeGraph::new();
    two_clusters(&native);
    let q = parse(LOUVAIN_CYPHER).expect("parse");
    let res = execute_on_engine(&q, &native, HashMap::new()).expect("execute on native");
    assert_two_communities(&cypher_communities(&res.rows));
}
