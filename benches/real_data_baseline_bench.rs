//! Real-data baseline: KV vs native, through the Cypher executor, on a copy of
//! a production graph (RFC `docs/rfc-native-core.md`, #307, Phase 0/7).
//!
//! The synthetic `native_vs_kv_bench` quantifies the index-free-adjacency
//! thesis on a generated graph; this bench runs the *user-visible* path — the
//! Cypher executor — over a **GraphML copy of real data**, because synthetic
//! shapes have repeatedly misestimated real-world wins (the FTS posting-list
//! rewrite measured 2× off until validated on a live copy). It is the
//! scoreboard that defines "surpass" for the engine flip.
//!
//! # Running
//!
//! Point `DREVO_BASELINE_GRAPHML` at a GraphML export of the graph to measure
//! (e.g. a `~/drevo_backups/*.graphml` snapshot or a fresh
//! `GET /export/graphml`), then:
//!
//! ```sh
//! DREVO_BASELINE_GRAPHML=$HOME/drevo_backups/latest.graphml \
//!     cargo bench --bench real_data_baseline_bench
//! ```
//!
//! Without the variable the bench prints how to enable itself and exits
//! successfully, so CI (which has no real data) stays green while
//! `clippy --all-targets` still compile-checks it.
//!
//! # What is measured
//!
//! The same parsed query is executed on
//!
//! * the KV [`Drevo`](drevo::db::Drevo) via `execute` (today's production
//!   path), and
//! * the native [`NativeGraph`](drevo::native::NativeGraph) via
//!   `execute_on_engine_with_indexes` with the label + property indexes synced
//!   (the flip-target path),
//!
//! plus two seam-level adjacency expansions (`neighbor_ids` from the
//! highest-degree node) that isolate the index-free-adjacency claim from
//! executor overhead. Workload parameters (the densest kind, a mid-selectivity
//! property pair, the highest-degree node) are derived from the data itself so
//! the bench stays meaningful as the graph evolves.

use std::collections::HashMap;
use std::time::Duration;

use criterion::Criterion;
use std::hint::black_box;

use drevo::cypher::ast::Query;
use drevo::cypher::executor::{execute, execute_on_engine_with_indexes_and_values, ExecResult};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;
use drevo::engine::GraphEngine;
use drevo::migrate::migrate;
use drevo::model::Direction;
use drevo::native::NativeGraph;
use drevo::native_label_index::NativeLabelIndex;
use drevo::native_property_index::NativePropertyIndex;
use drevo::native_value_cache::NativeValueCache;

/// Everything the workloads need, loaded once.
struct Loaded {
    kv: Drevo,
    native: NativeGraph,
    labels: NativeLabelIndex,
    props: NativePropertyIndex,
    values: NativeValueCache,
    /// The most frequent node kind (label-scan workload).
    top_kind: String,
    /// A `(key, string value)` property pair with mid selectivity
    /// (property-equality workload).
    prop_pair: Option<(String, String)>,
    /// The node with the highest out-degree (adjacency workload).
    hub_id: u64,
}

fn load() -> Result<Loaded, Box<dyn std::error::Error>> {
    let path = std::env::var("DREVO_BASELINE_GRAPHML")?;
    let kv = Drevo::open_in_memory()?;
    let report = kv.import_graphml_from_path(std::path::Path::new(&path))?;
    eprintln!(
        "loaded {}: {} nodes, {} edges",
        path, report.nodes_imported, report.edges_imported
    );

    let native = NativeGraph::new();
    migrate(&kv, &native)?;
    let mut labels = NativeLabelIndex::new();
    let mut props = NativePropertyIndex::new();
    let mut values = NativeValueCache::new();
    labels.sync(&native);
    props.sync(&native);
    values.sync(&native);

    // Derive workload parameters from the data itself (explicit trait
    // dispatch — NativeGraph also has inherent snapshot accessors).
    let nodes = GraphEngine::all_nodes(&native)?;
    let mut kind_freq: HashMap<&str, usize> = HashMap::new();
    let mut prop_freq: HashMap<(&str, &str), usize> = HashMap::new();
    for n in &nodes {
        *kind_freq.entry(n.kind.as_str()).or_default() += 1;
        for (k, v) in n.properties.0.iter() {
            if k == "_labels" || k == "title" || k == "body" {
                continue;
            }
            if let serde_json::Value::String(s) = v {
                *prop_freq.entry((k.as_str(), s.as_str())).or_default() += 1;
            }
        }
    }
    let top_kind = kind_freq
        .iter()
        .max_by_key(|(_, c)| **c)
        .map(|(k, _)| k.to_string())
        .ok_or("empty graph")?;
    // Mid selectivity: the pair whose frequency is closest to 1% of the nodes
    // (at least 2 matches, so the lookup does real work).
    let target = (nodes.len() / 100).max(2);
    let prop_pair = prop_freq
        .iter()
        .filter(|(_, c)| **c >= 2)
        .min_by_key(|(_, c)| c.abs_diff(target))
        .map(|((k, v), _)| (k.to_string(), v.to_string()));

    let mut hub_id = nodes.first().map(|n| n.id).ok_or("empty graph")?;
    let mut hub_deg = 0usize;
    for n in &nodes {
        let deg = GraphEngine::neighbor_ids(&native, n.id, Direction::Outgoing, None)?.len();
        if deg > hub_deg {
            hub_deg = deg;
            hub_id = n.id;
        }
    }
    eprintln!(
        "workload params: top_kind={top_kind:?} ({} nodes), prop_pair={prop_pair:?}, \
         hub id {hub_id} (out-degree {hub_deg})",
        kind_freq[top_kind.as_str()]
    );

    Ok(Loaded {
        kv,
        native,
        labels,
        props,
        values,
        top_kind,
        prop_pair,
        hub_id,
    })
}

fn run_kv(l: &Loaded, q: &Query) -> ExecResult {
    execute(q, &l.kv, HashMap::new()).expect("kv execute")
}

fn run_native(l: &Loaded, q: &Query) -> ExecResult {
    execute_on_engine_with_indexes_and_values(
        q,
        &l.native,
        None,
        Some(&l.labels),
        Some(&l.props),
        Some(&l.values),
        HashMap::new(),
    )
    .expect("native execute")
}

/// Bench one query on both engines under the same group, asserting up front
/// that the two engines agree on the result (a wrong-answer speedup is not a
/// win).
fn bench_query(c: &mut Criterion, l: &Loaded, group: &str, source: &str) {
    let q = parse(source).expect("parse");
    let kv_rows = run_kv(l, &q).rows;
    let native_rows = run_native(l, &q).rows;
    assert_eq!(
        kv_rows, native_rows,
        "engines disagree on `{source}` — fix parity before benchmarking"
    );

    let mut g = c.benchmark_group(group.to_string());
    g.bench_function("kv", |b| b.iter(|| black_box(run_kv(l, &q).rows.len())));
    g.bench_function("native_indexed", |b| {
        b.iter(|| black_box(run_native(l, &q).rows.len()))
    });
    g.finish();
}

fn main() {
    let l = match load() {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "real_data_baseline_bench skipped ({e}). Set DREVO_BASELINE_GRAPHML to a \
                 GraphML export of the graph to measure, e.g.:\n  \
                 DREVO_BASELINE_GRAPHML=$HOME/drevo_backups/<snapshot>.graphml \
                 cargo bench --bench real_data_baseline_bench"
            );
            return;
        }
    };

    let mut c = Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3))
        .sample_size(20)
        .configure_from_args();

    bench_query(&mut c, &l, "count_all_nodes", "MATCH (n) RETURN count(*)");
    bench_query(
        &mut c,
        &l,
        "label_scan_count",
        &format!("MATCH (n:{}) RETURN count(*)", l.top_kind),
    );
    if let Some((k, v)) = &l.prop_pair {
        // Real-data property keys here are snake_case and values plain
        // strings; escape single quotes defensively anyway.
        let literal = v.replace('\\', "\\\\").replace('\'', "\\'");
        bench_query(
            &mut c,
            &l,
            "property_equality_count",
            &format!("MATCH (n {{{k}: '{literal}'}}) RETURN count(*)"),
        );
    }
    bench_query(
        &mut c,
        &l,
        "one_hop_from_hub_cypher",
        &format!("MATCH (a)-->(b) WHERE id(a) = {} RETURN count(b)", l.hub_id),
    );

    // Seam-level adjacency — isolates index-free adjacency from the executor.
    {
        let mut g = c.benchmark_group("one_hop_from_hub_seam");
        let hub = l.hub_id;
        g.bench_function("kv", |b| {
            b.iter(|| {
                black_box(
                    l.kv.neighbor_ids(hub, Direction::Outgoing, None)
                        .unwrap()
                        .len(),
                )
            })
        });
        g.bench_function("native", |b| {
            b.iter(|| {
                black_box(
                    GraphEngine::neighbor_ids(&l.native, hub, Direction::Outgoing, None)
                        .unwrap()
                        .len(),
                )
            })
        });
        g.finish();
    }

    c.final_summary();
}
