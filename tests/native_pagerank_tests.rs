//! Parallel PageRank over the native engine (RFC #307 Phase 8, slice 1).
//!
//! The proven single-threaded `algorithms::pagerank` (reached here through the
//! KV `Drevo::pagerank`) is the correctness oracle: the new parallel,
//! pull-based `pagerank_native` must agree with it within tolerance on the same
//! graph, plus the usual PageRank invariants (mass conserved, structure
//! reflected). Gated on `redb-backend` for the KV oracle side.

#![cfg(feature = "redb-backend")]

use std::collections::HashMap;

use drevo::algorithms::{pagerank_native, PageRankConfig, PageRankResult};
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

/// Build the same `n`-node directed graph (edges are `(from_idx, to_idx)`) in
/// an engine; ids come back in creation order, so both engines get identical
/// ids (1..=n).
fn build<E: GraphEngine>(engine: &E, n: usize, edges: &[(usize, usize)]) -> Vec<u64> {
    let ids: Vec<u64> = (0..n).map(|i| node(engine, &format!("n{i}"))).collect();
    for &(a, b) in edges {
        edge(engine, ids[a], ids[b]);
    }
    ids
}

fn as_map(r: &PageRankResult) -> HashMap<u64, f64> {
    r.ranked().into_iter().collect()
}

/// Run native-parallel PageRank and the KV-serial oracle on the same graph and
/// assert every node's rank agrees within `tol`.
fn assert_native_matches_kv(n: usize, edges: &[(usize, usize)], tol: f64) -> HashMap<u64, f64> {
    let cfg = PageRankConfig::default();

    let kv = Drevo::open_in_memory().expect("kv");
    let kids = build(&kv, n, edges);
    let kv_ranks = as_map(&kv.pagerank(&cfg).expect("kv pagerank"));

    let native = NativeGraph::new();
    let nids = build(&native, n, edges);
    let nat = pagerank_native(&native, &cfg);
    let nat_ranks = as_map(&nat);

    assert_eq!(kids, nids, "engines assigned different ids");
    assert_eq!(
        nat_ranks.len(),
        kv_ranks.len(),
        "rank vector length mismatch"
    );
    for (id, kv_r) in &kv_ranks {
        let nat_r = nat_ranks.get(id).copied().unwrap_or(f64::NAN);
        assert!(
            (nat_r - kv_r).abs() <= tol,
            "node {id}: native {nat_r} vs kv {kv_r} (Δ {:.3e} > {tol:.0e})",
            (nat_r - kv_r).abs()
        );
    }
    nat_ranks
}

#[test]
fn native_parallel_matches_kv_serial_on_varied_shapes() {
    // chain, cycle, star, a dangling sink, parallel edges, and a disconnected mix.
    assert_native_matches_kv(4, &[(0, 1), (1, 2), (2, 3)], 1e-6); // chain (3 is dangling)
    assert_native_matches_kv(3, &[(0, 1), (1, 2), (2, 0)], 1e-6); // 3-cycle
    assert_native_matches_kv(4, &[(0, 1), (0, 2), (0, 3)], 1e-6); // star out of 0
    assert_native_matches_kv(5, &[(0, 4), (1, 4), (2, 4), (3, 4)], 1e-6); // hub sink 4
    assert_native_matches_kv(2, &[(0, 1), (0, 1), (0, 1)], 1e-6); // parallel edges
    assert_native_matches_kv(6, &[(0, 1), (1, 0), (2, 3), (4, 5)], 1e-6); // disjoint bits
}

#[test]
fn ranks_conserve_mass_and_reflect_structure() {
    // A hub that everyone points to must outrank every leaf, and total rank ≈ 1.
    let ranks = assert_native_matches_kv(5, &[(0, 4), (1, 4), (2, 4), (3, 4)], 1e-6);
    let total: f64 = ranks.values().sum();
    assert!((total - 1.0).abs() < 1e-6, "mass not conserved: {total}");
    let hub = ranks[&5]; // id 5 = index 4, the sink everyone links to
    for leaf in [1u64, 2, 3, 4] {
        assert!(
            hub > ranks[&leaf],
            "hub {hub} should outrank leaf {}",
            ranks[&leaf]
        );
    }
}

/// `CALL drevo.pagerank() YIELD node, score` — the top row's title and the
/// score column, extracted from the Cypher result of `execute`d on `db`.
fn cypher_pagerank_scores(rows: &[Vec<Value>]) -> (String, Vec<f64>) {
    let top_title = match &rows[0][0] {
        Value::String(t) => t.clone(),
        other => panic!("expected a title string in column 0, got {other:?}"),
    };
    let scores = rows
        .iter()
        .map(|r| match &r[1] {
            Value::Float(f) => *f,
            other => panic!("expected a float score in column 1, got {other:?}"),
        })
        .collect();
    (top_title, scores)
}

const PAGERANK_CYPHER: &str =
    "CALL drevo.pagerank() YIELD node, score RETURN node.title AS t, score AS s ORDER BY s DESC";

#[test]
fn call_drevo_pagerank_over_cypher_ranks_the_hub_first_on_kv() {
    // 0,1,2,3 → 4; index 4 (id 5, title "n4") is the hub everyone points to.
    let kv = Drevo::open_in_memory().expect("kv");
    build(&kv, 5, &[(0, 4), (1, 4), (2, 4), (3, 4)]);
    let q = parse(PAGERANK_CYPHER).expect("parse");
    let res = execute(&q, &kv, HashMap::new()).expect("execute");

    assert_eq!(res.rows.len(), 5, "one row per node");
    let (top, scores) = cypher_pagerank_scores(&res.rows);
    assert_eq!(top, "n4", "the hub must rank first");
    assert!(
        (scores.iter().sum::<f64>() - 1.0).abs() < 1e-6,
        "mass ≈ 1: {scores:?}"
    );
    assert!(
        scores.windows(2).all(|w| w[0] >= w[1]),
        "scores descending: {scores:?}"
    );
}

#[test]
fn call_drevo_pagerank_over_cypher_runs_on_the_native_engine() {
    let native = NativeGraph::new();
    build(&native, 5, &[(0, 4), (1, 4), (2, 4), (3, 4)]);
    let q = parse(PAGERANK_CYPHER).expect("parse");
    let res = execute_on_engine(&q, &native, HashMap::new()).expect("execute on native");

    assert_eq!(res.rows.len(), 5);
    let (top, _scores) = cypher_pagerank_scores(&res.rows);
    assert_eq!(
        top, "n4",
        "native CALL drevo.pagerank must rank the hub first"
    );
}

#[test]
fn empty_and_single_node_graphs() {
    let cfg = PageRankConfig::default();

    let empty = NativeGraph::new();
    let r = pagerank_native(&empty, &cfg);
    assert!(r.ranked().is_empty(), "empty graph → empty ranks");

    let one = NativeGraph::new();
    let _ = node(&one, "solo");
    let r = pagerank_native(&one, &cfg);
    let ranks = as_map(&r);
    assert_eq!(ranks.len(), 1);
    assert!(
        (ranks.values().next().unwrap() - 1.0).abs() < 1e-9,
        "lone node = 1.0"
    );
}
