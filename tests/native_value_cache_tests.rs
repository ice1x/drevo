//! The [`NativeValueCache`] contract (RFC `docs/rfc-native-core.md`, #307):
//! enumeration reuses each unchanged node's cached `NodeValue` projection —
//! observable as `Arc` identity across queries — while a hit is validated
//! against the live record with `Arc::ptr_eq`, so a stale or never-resynced
//! cache can only cost speed, never serve a wrong answer.

use std::collections::HashMap;
use std::sync::Arc;

use drevo::cypher::executor::{
    execute_on_engine, execute_on_engine_with_indexes_and_values, ExecResult, Value,
};
use drevo::cypher::parser::parse;
use drevo::engine::GraphEngine;
use drevo::model::{NewNode, NodePatch};
use drevo::native::NativeGraph;
use drevo::native_value_cache::NativeValueCache;

fn person(title: &str) -> NewNode {
    NewNode {
        kind: "person".into(),
        title: title.into(),
        body: "body text".into(),
        body_html: String::new(),
        properties: Default::default(),
    }
}

fn graph(titles: &[&str]) -> NativeGraph {
    let g = NativeGraph::new();
    for t in titles {
        g.create_node(person(t)).expect("create");
    }
    g
}

fn run_cached(g: &NativeGraph, cache: &NativeValueCache, source: &str) -> ExecResult {
    let q = parse(source).expect("parse");
    execute_on_engine_with_indexes_and_values(&q, g, None, None, None, Some(cache), HashMap::new())
        .expect("execute")
}

/// The `Arc<NodeValue>`s of a result's single column of nodes.
fn node_arcs(res: &ExecResult) -> Vec<Arc<drevo::cypher::executor::NodeValue>> {
    res.rows
        .iter()
        .map(|row| match &row[0] {
            Value::Node(n) => Arc::clone(n),
            other => panic!("expected node, got {other:?}"),
        })
        .collect()
}

#[test]
fn cached_projections_are_reused_across_queries() {
    let g = graph(&["ada", "bob"]);
    let mut cache = NativeValueCache::new();
    cache.sync(&g);
    assert_eq!(cache.len(), 2);

    let first = node_arcs(&run_cached(&g, &cache, "MATCH (n) RETURN n"));
    let second = node_arcs(&run_cached(&g, &cache, "MATCH (n) RETURN n"));
    assert_eq!(first.len(), 2);
    for (a, b) in first.iter().zip(&second) {
        assert!(
            Arc::ptr_eq(a, b),
            "unchanged nodes must reuse the cached NodeValue projection"
        );
    }
}

#[test]
fn a_stale_cache_never_serves_the_old_value() {
    let g = graph(&["ada"]);
    let mut cache = NativeValueCache::new();
    cache.sync(&g);

    // Mutate WITHOUT re-syncing the cache: the ptr_eq validity check must
    // reject the stale entry and rebuild from the live record.
    GraphEngine::update_node(
        &g,
        1,
        NodePatch {
            title: Some("ada2".into()),
            ..Default::default()
        },
    )
    .expect("update");

    let res = run_cached(&g, &cache, "MATCH (n) RETURN n.title");
    assert_eq!(res.rows, vec![vec![Value::String("ada2".into())]]);
}

#[test]
fn intra_statement_writes_are_visible_with_a_synced_cache() {
    let g = graph(&["ada"]);
    let mut cache = NativeValueCache::new();
    cache.sync(&g);

    // A statement that writes and then reads in the same execution must see
    // its own write even though the cache was synced before the statement.
    let res = run_cached(
        &g,
        &cache,
        "CREATE (b:person {title: 'bob'}) WITH b MATCH (n:person) RETURN count(n)",
    );
    assert_eq!(res.rows, vec![vec![Value::Integer(2)]]);

    // And a same-statement update is reflected, not the cached projection.
    let res = run_cached(
        &g,
        &cache,
        "MATCH (n:person {title: 'ada'}) SET n.mood = 'good' \
         WITH n MATCH (m:person {title: 'ada'}) RETURN m.mood",
    );
    assert_eq!(res.rows, vec![vec![Value::String("good".into())]]);
}

#[test]
fn cache_results_match_the_uncached_run() {
    let g = graph(&["ada", "bob", "cy"]);
    GraphEngine::create_edge(
        &g,
        drevo::model::NewEdge {
            from_id: 1,
            to_id: 2,
            kind: "KNOWS".into(),
            weight: 1.0,
            properties: Default::default(),
        },
    )
    .expect("edge");
    let mut cache = NativeValueCache::new();
    cache.sync(&g);

    for source in [
        "MATCH (n) RETURN n ORDER BY n.title",
        "MATCH (n:person) RETURN n.title, labels(n) ORDER BY n.title",
        "MATCH (a)-[:KNOWS]->(b) RETURN a.title, b.title",
        "MATCH (n) WHERE id(n) = 2 RETURN n",
    ] {
        let q = parse(source).unwrap();
        let plain = execute_on_engine(&q, &g, HashMap::new()).unwrap();
        let cached = run_cached(&g, &cache, source);
        assert_eq!(plain.rows, cached.rows, "diverged on `{source}`");
        assert_eq!(plain.columns, cached.columns);
    }
}

#[test]
fn sync_tracks_updates_and_deletes() {
    let g = graph(&["ada", "bob"]);
    let mut cache = NativeValueCache::new();
    cache.sync(&g);
    assert_eq!(cache.len(), 2);

    GraphEngine::update_node(
        &g,
        1,
        NodePatch {
            title: Some("ada2".into()),
            ..Default::default()
        },
    )
    .unwrap();
    GraphEngine::delete_node(&g, 2).unwrap();
    cache.sync(&g);
    assert_eq!(cache.len(), 1);

    // After the re-sync the fresh entry is served (and reused).
    let a = node_arcs(&run_cached(&g, &cache, "MATCH (n) RETURN n"));
    let b = node_arcs(&run_cached(&g, &cache, "MATCH (n) RETURN n"));
    assert_eq!(a.len(), 1);
    assert!(Arc::ptr_eq(&a[0], &b[0]));
    assert_eq!(
        a[0].properties.get("title"),
        Some(&Value::String("ada2".into()))
    );
}
