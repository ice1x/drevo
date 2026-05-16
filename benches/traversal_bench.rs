//! Benchmark: BFS, DFS, shortest_path, subgraph on a 100K-node graph
//! with average degree 10 (1M edges).
//!
//! Task 00026 — measures traversal performance on MemoryBackend.

use criterion::{criterion_group, criterion_main, Criterion};
use drevo::db::Drevo;
use drevo::model::{Direction, NewEdge, NewNode, Properties};
use std::hint::black_box;

const NUM_NODES: usize = 100_000;
const DEGREE: usize = 10; // average outgoing degree → 1M edges total

/// Build a graph with NUM_NODES nodes, each with DEGREE outgoing edges
/// to deterministic pseudo-random targets.
fn populated_db() -> (Drevo, Vec<u64>) {
    let db = Drevo::open_in_memory().unwrap();
    let mut ids = Vec::with_capacity(NUM_NODES);

    for i in 0..NUM_NODES {
        let node = db
            .create_node(NewNode {
                kind: format!("kind_{}", i % 10),
                title: format!("node_{i:08}"),
                body: format!("Body of node {i}"),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .unwrap();
        ids.push(node.id);
    }

    for (i, &from_id) in ids.iter().enumerate() {
        for j in 0..DEGREE {
            let to_idx = (i * 7 + j * 13 + 1) % NUM_NODES;
            if to_idx == i {
                continue;
            }
            db.create_edge(NewEdge {
                from_id,
                to_id: ids[to_idx],
                kind: format!("rel_{}", j % 5),
                weight: ((j % 10) as f32) + 1.0,
                properties: Properties::default(),
            })
            .unwrap();
        }
    }

    (db, ids)
}

// ---------------------------------------------------------------------------
// BFS benchmarks
// ---------------------------------------------------------------------------

fn bench_bfs(c: &mut Criterion) {
    let mut group = c.benchmark_group("traversal_bfs");
    group.sample_size(20);

    let (db, ids) = populated_db();

    // BFS outgoing depth 1 (just neighbors)
    group.bench_function("outgoing/depth_1", |b| {
        let mut idx = 0usize;
        b.iter(|| {
            let node_id = ids[idx % NUM_NODES];
            let result = db
                .bfs(black_box(node_id), 1, Direction::Outgoing, None)
                .unwrap();
            black_box(&result);
            idx = idx.wrapping_add(7919);
        });
    });

    // BFS outgoing depth 2
    group.bench_function("outgoing/depth_2", |b| {
        let mut idx = 0usize;
        b.iter(|| {
            let node_id = ids[idx % NUM_NODES];
            let result = db
                .bfs(black_box(node_id), 2, Direction::Outgoing, None)
                .unwrap();
            black_box(&result);
            idx = idx.wrapping_add(7919);
        });
    });

    // BFS outgoing depth 3 — primary benchmark target
    group.bench_function("outgoing/depth_3", |b| {
        let mut idx = 0usize;
        b.iter(|| {
            let node_id = ids[idx % NUM_NODES];
            let result = db
                .bfs(black_box(node_id), 3, Direction::Outgoing, None)
                .unwrap();
            black_box(&result);
            idx = idx.wrapping_add(7919);
        });
    });

    // BFS both directions depth 2
    group.bench_function("both/depth_2", |b| {
        let mut idx = 0usize;
        b.iter(|| {
            let node_id = ids[idx % NUM_NODES];
            let result = db
                .bfs(black_box(node_id), 2, Direction::Both, None)
                .unwrap();
            black_box(&result);
            idx = idx.wrapping_add(7919);
        });
    });

    // BFS with edge kind filter depth 2
    group.bench_function("filtered/depth_2", |b| {
        let mut idx = 0usize;
        b.iter(|| {
            let node_id = ids[idx % NUM_NODES];
            let result = db
                .bfs(black_box(node_id), 2, Direction::Outgoing, Some("rel_0"))
                .unwrap();
            black_box(&result);
            idx = idx.wrapping_add(7919);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// DFS benchmarks
// ---------------------------------------------------------------------------

fn bench_dfs(c: &mut Criterion) {
    let mut group = c.benchmark_group("traversal_dfs");
    group.sample_size(20);

    let (db, ids) = populated_db();

    // DFS outgoing depth 3
    group.bench_function("outgoing/depth_3", |b| {
        let mut idx = 0usize;
        b.iter(|| {
            let node_id = ids[idx % NUM_NODES];
            let result = db
                .dfs(black_box(node_id), 3, Direction::Outgoing, None)
                .unwrap();
            black_box(&result);
            idx = idx.wrapping_add(7919);
        });
    });

    // DFS both directions depth 2
    group.bench_function("both/depth_2", |b| {
        let mut idx = 0usize;
        b.iter(|| {
            let node_id = ids[idx % NUM_NODES];
            let result = db
                .dfs(black_box(node_id), 2, Direction::Both, None)
                .unwrap();
            black_box(&result);
            idx = idx.wrapping_add(7919);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Shortest path benchmarks
// ---------------------------------------------------------------------------

fn bench_shortest_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("traversal_shortest_path");
    group.sample_size(20);

    let (db, ids) = populated_db();

    // Short path (nearby nodes — low index distance)
    group.bench_function("nearby_nodes", |b| {
        let mut idx = 0usize;
        b.iter(|| {
            let from = ids[idx % NUM_NODES];
            let to = ids[(idx + 10) % NUM_NODES];
            let result = db.shortest_path(black_box(from), black_box(to)).unwrap();
            black_box(&result);
            idx = idx.wrapping_add(7919);
        });
    });

    // Longer path (distant nodes)
    group.bench_function("distant_nodes", |b| {
        let mut idx = 0usize;
        b.iter(|| {
            let from = ids[idx % NUM_NODES];
            let to = ids[(idx + NUM_NODES / 2) % NUM_NODES];
            let result = db.shortest_path(black_box(from), black_box(to)).unwrap();
            black_box(&result);
            idx = idx.wrapping_add(7919);
        });
    });

    // Same node (trivial)
    group.bench_function("same_node", |b| {
        let mut idx = 0usize;
        b.iter(|| {
            let node = ids[idx % NUM_NODES];
            let result = db.shortest_path(black_box(node), black_box(node)).unwrap();
            black_box(&result);
            idx = idx.wrapping_add(7919);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Subgraph benchmarks
// ---------------------------------------------------------------------------

fn bench_subgraph(c: &mut Criterion) {
    let mut group = c.benchmark_group("traversal_subgraph");
    group.sample_size(20);

    let (db, ids) = populated_db();

    // Subgraph depth 1
    group.bench_function("depth_1", |b| {
        let mut idx = 0usize;
        b.iter(|| {
            let node_id = ids[idx % NUM_NODES];
            let result = db.subgraph(black_box(node_id), 1).unwrap();
            black_box(&result);
            idx = idx.wrapping_add(7919);
        });
    });

    // Subgraph depth 2
    group.bench_function("depth_2", |b| {
        let mut idx = 0usize;
        b.iter(|| {
            let node_id = ids[idx % NUM_NODES];
            let result = db.subgraph(black_box(node_id), 2).unwrap();
            black_box(&result);
            idx = idx.wrapping_add(7919);
        });
    });

    // Subgraph depth 3
    group.bench_function("depth_3", |b| {
        let mut idx = 0usize;
        b.iter(|| {
            let node_id = ids[idx % NUM_NODES];
            let result = db.subgraph(black_box(node_id), 3).unwrap();
            black_box(&result);
            idx = idx.wrapping_add(7919);
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_bfs,
    bench_dfs,
    bench_shortest_path,
    bench_subgraph
);
criterion_main!(benches);
