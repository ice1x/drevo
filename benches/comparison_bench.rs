//! Benchmark: drevo vs. competitors — Phase 15 task `00101`.
//!
//! A single, **reproducible** workload that mirrors the operation mix property
//! graph databases are routinely compared on (the same shape KuzuDB, Memgraph,
//! and Neo4j publish numbers for): bulk load, point lookups (by id, by indexed
//! title, by indexed property), 1-hop neighbour expansion, 2-hop traversal, and
//! full-text search. The point of this file is that drevo's side of the
//! comparison is **measured here, on this machine, with these exact queries** —
//! never copied from a vendor slide — and the identical workload is specified in
//! [`docs/benchmarks.md`](../docs/benchmarks.md) as runnable Cypher / driver code
//! against each competitor, so the cross-engine numbers are reproducible rather
//! than asserted.
//!
//! Run with:
//!
//! ```sh
//! cargo bench --bench comparison_bench
//! ```
//!
//! This bench is **not** part of the per-PR test path (criterion benches only
//! run under `cargo bench`, never `cargo test`), so it never contends for the
//! shared CI runner; the scheduled / manual `benchmarks.yml` workflow is the
//! only place it runs in CI.
//!
//! The in-memory backend is used throughout: the redb backend opens one ACID
//! transaction per write (~5 ms), which makes a bulk load of this size
//! impractical to micro-benchmark (the existing `graph_bench` documents the
//! same trade-off). Persistence latency is a separate axis, not the
//! cross-engine *query* comparison this workload targets.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use drevo::db::Drevo;
use drevo::model::{Direction, NewEdge, NewNode, Properties};
use std::hint::black_box;

/// Nodes in the standard comparison graph. Kept modest so the populated graph
/// builds in well under a second on the in-memory backend, while staying large
/// enough that an `O(N)` scan is clearly distinguishable from an indexed lookup.
const NUM_NODES: usize = 10_000;
/// Outgoing edges per node — average degree 4, a sparse social-graph shape.
const EDGES_PER_NODE: usize = 4;
/// A handful of cities cycled across nodes so a property lookup returns a
/// realistic, multi-row result set rather than a single node.
const CITIES: [&str; 8] = [
    "London", "Paris", "Berlin", "Tokyo", "Cairo", "Lima", "Oslo", "Delhi",
];

/// One person node. `title` is globally unique (drevo enforces this and indexes
/// it), `city` is an indexed property with low cardinality, and `body` carries a
/// per-node FTS token plus a shared token every node contains.
fn make_node(i: usize) -> NewNode {
    let mut properties = Properties::default();
    properties.insert(
        "city".to_string(),
        serde_json::Value::String(CITIES[i % CITIES.len()].to_string()),
    );
    properties.insert("age".to_string(), serde_json::Value::from(20 + (i % 60)));
    NewNode {
        kind: "Person".to_string(),
        title: format!("person_{i:08}"),
        body: format!("member profile graph node identifier_{i:08} searchable"),
        body_html: String::new(),
        properties,
    }
}

fn make_edge(from_id: u64, to_id: u64) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: "KNOWS".to_string(),
        weight: 1.0,
        properties: Properties::default(),
    }
}

/// Build the standard comparison graph on the in-memory backend and return it
/// alongside the assigned node ids (in insertion order).
fn populated_db() -> (Drevo, Vec<u64>) {
    let db = Drevo::open_in_memory().unwrap();
    let mut node_ids = Vec::with_capacity(NUM_NODES);
    for i in 0..NUM_NODES {
        node_ids.push(db.create_node(make_node(i)).unwrap().id);
    }
    for (i, &from_id) in node_ids.iter().enumerate() {
        for j in 0..EDGES_PER_NODE {
            let to_idx = (i + j + 1) % NUM_NODES;
            db.create_edge(make_edge(from_id, node_ids[to_idx]))
                .unwrap();
        }
    }
    (db, node_ids)
}

// ---------------------------------------------------------------------------
// Load
// ---------------------------------------------------------------------------

fn bench_bulk_load(c: &mut Criterion) {
    let mut group = c.benchmark_group("comparison_bulk_load_10k_nodes_40k_edges");
    group.sample_size(10);
    group.bench_function("drevo_memory", |b| {
        b.iter(|| {
            let (db, ids) = populated_db();
            black_box((db, ids));
        });
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Point lookups
// ---------------------------------------------------------------------------

fn bench_point_lookups(c: &mut Criterion) {
    let (db, ids) = populated_db();
    let mid_id = ids[NUM_NODES / 2];
    let mid_title = format!("person_{:08}", NUM_NODES / 2);

    let mut group = c.benchmark_group("comparison_point_lookup");
    group.bench_function("by_id", |b| {
        b.iter(|| black_box(db.get_node(black_box(mid_id)).unwrap()));
    });
    group.bench_function("by_indexed_title", |b| {
        b.iter(|| black_box(db.get_node_by_title(black_box(&mid_title)).unwrap()));
    });
    group.bench_function("by_indexed_property", |b| {
        // `city = "Berlin"` — an indexed equality predicate (task 00088).
        let value = serde_json::Value::String("Berlin".to_string());
        b.iter(|| {
            black_box(
                db.nodes_by_property(black_box("city"), black_box(&value))
                    .unwrap(),
            )
        });
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Traversal
// ---------------------------------------------------------------------------

fn bench_traversal(c: &mut Criterion) {
    let (db, ids) = populated_db();
    let start = ids[0];

    let mut group = c.benchmark_group("comparison_traversal");
    group.bench_function("neighbors_1hop", |b| {
        b.iter(|| {
            black_box(
                db.neighbors(black_box(start), Direction::Outgoing, None)
                    .unwrap(),
            )
        });
    });
    group.bench_function("bfs_2hop", |b| {
        b.iter(|| {
            black_box(
                db.bfs(black_box(start), 2, Direction::Outgoing, None)
                    .unwrap(),
            )
        });
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Full-text search
// ---------------------------------------------------------------------------

fn bench_fts(c: &mut Criterion) {
    let (db, _ids) = populated_db();

    let mut group = c.benchmark_group("comparison_fts");
    group.sample_size(20);
    group.bench_function("search_top10", |b| {
        // `identifier_00005000` matches a single node; `searchable` is in every
        // body, so the two probe a selective and a broad query respectively.
        b.iter_batched(
            || (),
            |_| black_box(db.search_fts(black_box("identifier_00005000"), 10).unwrap()),
            BatchSize::SmallInput,
        );
    });
    group.bench_function("search_broad_top10", |b| {
        b.iter(|| black_box(db.search_fts(black_box("searchable"), 10).unwrap()));
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_bulk_load,
    bench_point_lookups,
    bench_traversal,
    bench_fts
);
criterion_main!(benches);
