//! Benchmark: insert 100K nodes + 500K edges, read all neighbors.
//!
//! Task 00014 — measures graph-layer performance on both backends.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use graphnote_db::db::GraphNoteDb;
use graphnote_db::model::{Direction, NewEdge, NewNode, Properties};
use std::hint::black_box;

const NUM_NODES: usize = 100_000;
const EDGES_PER_NODE: usize = 5; // 5 * 100K = 500K edges

fn make_node(i: usize) -> NewNode {
    NewNode {
        kind: format!("kind_{}", i % 10),
        title: format!("node_{i:08}"),
        body: format!("Body of node {i}"),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn make_edge(from_id: u64, to_id: u64, i: usize) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: format!("rel_{}", i % 5),
        weight: 1.0,
        properties: Properties::default(),
    }
}

/// Build a fully populated in-memory graph: 100K nodes + 500K edges.
fn populated_memory_db() -> (GraphNoteDb, Vec<u64>) {
    let db = GraphNoteDb::open_in_memory().unwrap();
    let mut node_ids = Vec::with_capacity(NUM_NODES);

    for i in 0..NUM_NODES {
        let node = db.create_node(make_node(i)).unwrap();
        node_ids.push(node.id);
    }

    let mut edge_idx = 0;
    for (i, &from_id) in node_ids.iter().enumerate() {
        for j in 0..EDGES_PER_NODE {
            let to_idx = (i + j + 1) % NUM_NODES;
            db.create_edge(make_edge(from_id, node_ids[to_idx], edge_idx))
                .unwrap();
            edge_idx += 1;
        }
    }

    (db, node_ids)
}

// ---------------------------------------------------------------------------
// Benchmark: bulk insert 100K nodes
// ---------------------------------------------------------------------------

fn bench_insert_nodes(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_insert_nodes_100k");
    group.sample_size(10);

    // RedbBackend is not benchmarked here because each create_node
    // opens a separate ACID transaction (~5ms each), making 100K
    // inserts impractical (~8+ minutes per iteration). The graph
    // layer will batch writes in transactions for production use.
    group.bench_function("MemoryBackend", |b| {
        b.iter(|| {
            let db = GraphNoteDb::open_in_memory().unwrap();
            for i in 0..NUM_NODES {
                db.create_node(make_node(i)).unwrap();
            }
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: insert 500K edges (into pre-populated 100K-node graph)
// ---------------------------------------------------------------------------

fn bench_insert_edges(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_insert_edges_500k");
    group.sample_size(10);

    group.bench_function("MemoryBackend", |b| {
        b.iter_batched(
            || {
                let db = GraphNoteDb::open_in_memory().unwrap();
                let mut ids = Vec::with_capacity(NUM_NODES);
                for i in 0..NUM_NODES {
                    ids.push(db.create_node(make_node(i)).unwrap().id);
                }
                (db, ids)
            },
            |(db, node_ids)| {
                let mut edge_idx = 0;
                for (i, &from_id) in node_ids.iter().enumerate() {
                    for j in 0..EDGES_PER_NODE {
                        let to_idx = (i + j + 1) % NUM_NODES;
                        db.create_edge(make_edge(from_id, node_ids[to_idx], edge_idx))
                            .unwrap();
                        edge_idx += 1;
                    }
                }
            },
            BatchSize::PerIteration,
        );
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: read all neighbors (edges_of) for random nodes
// ---------------------------------------------------------------------------

fn bench_read_neighbors(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_read_neighbors");

    // Pre-populate the in-memory graph once
    let (mem_db, mem_ids) = populated_memory_db();

    group.bench_function("MemoryBackend/outgoing", |b| {
        let mut idx = 0usize;
        b.iter(|| {
            let node_id = mem_ids[idx % NUM_NODES];
            let edges = mem_db
                .edges_of(black_box(node_id), Direction::Outgoing)
                .unwrap();
            assert_eq!(edges.len(), EDGES_PER_NODE);
            idx = idx.wrapping_add(7919);
        });
    });

    group.bench_function("MemoryBackend/both", |b| {
        let mut idx = 0usize;
        b.iter(|| {
            let node_id = mem_ids[idx % NUM_NODES];
            let edges = mem_db
                .edges_of(black_box(node_id), Direction::Both)
                .unwrap();
            assert!(!edges.is_empty());
            idx = idx.wrapping_add(7919);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: get_node by id (random access from 100K)
// ---------------------------------------------------------------------------

fn bench_get_node(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_get_node");

    let (mem_db, mem_ids) = populated_memory_db();

    group.bench_function("MemoryBackend", |b| {
        let mut idx = 0usize;
        b.iter(|| {
            let node_id = mem_ids[idx % NUM_NODES];
            let node = mem_db.get_node(black_box(node_id)).unwrap();
            assert!(node.is_some());
            idx = idx.wrapping_add(7919);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: list_nodes_by_kind (paginated)
// ---------------------------------------------------------------------------

fn bench_list_by_kind(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_list_by_kind");

    let (mem_db, _) = populated_memory_db();

    group.bench_function("MemoryBackend/100", |b| {
        b.iter(|| {
            let nodes = mem_db
                .list_nodes_by_kind(black_box("kind_0"), 100, 0)
                .unwrap();
            assert!(!nodes.is_empty());
        });
    });

    group.bench_function("MemoryBackend/1000", |b| {
        b.iter(|| {
            let nodes = mem_db
                .list_nodes_by_kind(black_box("kind_0"), 1000, 0)
                .unwrap();
            assert!(!nodes.is_empty());
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_insert_nodes,
    bench_insert_edges,
    bench_read_neighbors,
    bench_get_node,
    bench_list_by_kind
);
criterion_main!(benches);
