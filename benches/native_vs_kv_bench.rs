//! Benchmark: the native graph engine vs. the KV-encoded store, on the same
//! workload, through the same [`GraphEngine`](drevo::engine::GraphEngine) seam
//! (RFC `docs/rfc-native-core.md`, #307).
//!
//! Both engines are driven as `&dyn GraphEngine` so the *only* variable is the
//! storage strategy: [`NativeGraph`] holds nodes/edges directly in memory with
//! maintained adjacency, while [`Drevo`] (in-memory `MemoryBackend`) encodes
//! every record and index as a byte-keyed `BTreeMap` row and re-serializes on
//! each access. This is the in-process, no-Memgraph scoreboard that quantifies
//! the index-free-adjacency thesis before the arena/CSR layout lands.
//!
//! Run with:
//!
//! ```sh
//! cargo bench --bench native_vs_kv_bench
//! ```
//!
//! Not part of the per-PR test path (criterion benches only run under
//! `cargo bench`); `clippy --all-targets` compile-checks it in CI.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use drevo::db::Drevo;
use drevo::engine::GraphEngine;
use drevo::model::{Direction, NewEdge, NewNode, Properties};
use drevo::native::NativeGraph;
use std::hint::black_box;

/// Nodes in the standard graph. Kept modest so a full build runs in well under
/// a second on either engine, while large enough that expansion/scan costs are
/// clearly measurable.
const NUM_NODES: usize = 20_000;
/// Outgoing edges per node — average degree 5 (100K edges total).
const EDGES_PER_NODE: usize = 5;

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

/// Populate an engine through the seam; return the allocated node ids.
fn populate(engine: &dyn GraphEngine) -> Vec<u64> {
    let mut node_ids = Vec::with_capacity(NUM_NODES);
    for i in 0..NUM_NODES {
        node_ids.push(engine.create_node(make_node(i)).unwrap().id);
    }
    let mut edge_idx = 0;
    for (i, &from_id) in node_ids.iter().enumerate() {
        for j in 0..EDGES_PER_NODE {
            let to_id = node_ids[(i + j + 1) % NUM_NODES];
            engine
                .create_edge(make_edge(from_id, to_id, edge_idx))
                .unwrap();
            edge_idx += 1;
        }
    }
    node_ids
}

// ---------------------------------------------------------------------------
// Build (write throughput): 20K nodes + 100K edges from empty.
// ---------------------------------------------------------------------------

fn bench_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("native_vs_kv/build_20k_nodes_100k_edges");
    group.sample_size(10);
    group.bench_function("native", |b| {
        b.iter_batched(
            NativeGraph::new,
            |engine| black_box(populate(&engine).len()),
            BatchSize::LargeInput,
        )
    });
    group.bench_function("kv", |b| {
        b.iter_batched(
            || Drevo::open_in_memory().unwrap(),
            |engine| black_box(populate(&engine).len()),
            BatchSize::LargeInput,
        )
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Read paths on a pre-built graph.
// ---------------------------------------------------------------------------

fn bench_reads(c: &mut Criterion) {
    let native = NativeGraph::new();
    let native_ids = populate(&native);
    let kv = Drevo::open_in_memory().unwrap();
    let kv_ids = populate(&kv);

    // Point lookup: get_node across all ids.
    let mut group = c.benchmark_group("native_vs_kv/get_node_all");
    group.bench_function("native", |b| {
        b.iter(|| {
            for &id in &native_ids {
                black_box(native.get_node(id).unwrap());
            }
        })
    });
    group.bench_function("kv", |b| {
        b.iter(|| {
            for &id in &kv_ids {
                black_box(kv.get_node(id).unwrap());
            }
        })
    });
    group.finish();

    // 1-hop fan-out: neighbor_ids across all nodes.
    let mut group = c.benchmark_group("native_vs_kv/neighbor_ids_all");
    group.bench_function("native", |b| {
        b.iter(|| {
            for &id in &native_ids {
                black_box(native.neighbor_ids(id, Direction::Outgoing, None).unwrap());
            }
        })
    });
    group.bench_function("kv", |b| {
        b.iter(|| {
            for &id in &kv_ids {
                black_box(kv.neighbor_ids(id, Direction::Outgoing, None).unwrap());
            }
        })
    });
    group.finish();

    // Full-edge expansion: edges_of across all nodes.
    let mut group = c.benchmark_group("native_vs_kv/edges_of_all");
    group.bench_function("native", |b| {
        b.iter(|| {
            for &id in &native_ids {
                black_box(native.edges_of(id, Direction::Both).unwrap());
            }
        })
    });
    group.bench_function("kv", |b| {
        b.iter(|| {
            for &id in &kv_ids {
                black_box(kv.edges_of(id, Direction::Both).unwrap());
            }
        })
    });
    group.finish();

    // Label scan: nodes_by_kind (one of ten kinds → ~2K matches).
    let mut group = c.benchmark_group("native_vs_kv/nodes_by_kind");
    group.bench_function("native", |b| {
        b.iter(|| black_box(native.nodes_by_kind("kind_3", 100_000, 0).unwrap().len()))
    });
    group.bench_function("kv", |b| {
        b.iter(|| black_box(kv.nodes_by_kind("kind_3", 100_000, 0).unwrap().len()))
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Zero-copy reads: owned deep-clone vs `Arc` handle on realistic (large-body)
// nodes. This is where the arena/`Arc`-backed storage pays off — the cost of an
// owned read scales with the node's body/property size, while an `Arc` handle
// is a constant-time refcount bump.
// ---------------------------------------------------------------------------

/// A node with a ~4 KiB body and a handful of properties — closer to a real
/// drevo record (a journal entry, a book chapter) than the tiny standard node.
fn make_large_node(i: usize) -> NewNode {
    let mut properties = Properties::default();
    for p in 0..8 {
        properties.0.insert(
            format!("prop_{p}"),
            serde_json::Value::String(format!("value {i}-{p} with some length to it")),
        );
    }
    NewNode {
        kind: format!("kind_{}", i % 10),
        title: format!("large_node_{i:08}"),
        body: "x".repeat(4096),
        body_html: String::new(),
        properties,
    }
}

fn bench_zero_copy(c: &mut Criterion) {
    const N: usize = 5_000;

    let native = NativeGraph::new();
    let mut ids = Vec::with_capacity(N);
    for i in 0..N {
        ids.push(native.create_node(make_large_node(i)).unwrap().id);
    }

    let mut group = c.benchmark_group("native/read_large_nodes");
    // Owned read: `GraphEngine::get_node` must deep-clone the 4 KiB body and the
    // property map on every access to honour its owned-return contract.
    group.bench_function("get_node_owned", |b| {
        b.iter(|| {
            for &id in &ids {
                black_box(GraphEngine::get_node(&native, id).unwrap());
            }
        })
    });
    // Zero-copy read: `get_node_arc` hands back an `Arc<Node>` — a refcount bump
    // regardless of node size.
    group.bench_function("get_node_arc", |b| {
        b.iter(|| {
            for &id in &ids {
                black_box(native.get_node_arc(id));
            }
        })
    });
    group.finish();
}

criterion_group!(benches, bench_build, bench_reads, bench_zero_copy);
criterion_main!(benches);
