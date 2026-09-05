//! Parallel vs serial PageRank (RFC #307 Phase 8 slice 3) — the parallel
//! speedup the MVCC-snapshot design buys, measured on synthetic graphs large
//! enough for the rayon fan-out to pay for itself.
//!
//! Both functions compute the same ranks (locked by `tests/native_pagerank_tests.rs`);
//! this only times them. On a small graph the parallel overhead can lose; the
//! win shows up as the node/edge count grows, which is the point.
//!
//! Run: `cargo bench --bench pagerank_bench`

use std::hint::black_box;
use std::time::Duration;

use criterion::Criterion;

use drevo::algorithms::{pagerank, pagerank_parallel, AdjacencyList, PageRankConfig};

/// A deterministic scale-free-ish graph: `n` nodes, ~`avg_deg` out-edges each to
/// pseudo-random targets, plus a supernode (id 1) many nodes point at — so the
/// power iteration does real, uneven work.
fn build_graph(n: usize, avg_deg: usize) -> AdjacencyList {
    let node_ids: Vec<u64> = (1..=n as u64).collect();
    let mut edges: Vec<(u64, u64, f32)> = Vec::with_capacity(n * (avg_deg + 1));
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    for from in 1..=n as u64 {
        for _ in 0..avg_deg {
            // xorshift — a dependency-free deterministic mixer.
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let to = (x % n as u64) + 1;
            edges.push((from, to, 1.0));
        }
        if from % 3 == 0 {
            edges.push((from, 1, 1.0)); // feed the supernode
        }
    }
    AdjacencyList::from_parts(node_ids, edges)
}

fn main() {
    let mut c = Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3))
        .sample_size(20)
        .configure_from_args();

    let cfg = PageRankConfig::default();
    for &n in &[10_000usize, 100_000, 500_000] {
        let g = build_graph(n, 6);
        eprintln!(
            "pagerank graph: {n} nodes, {} edges",
            // out-edge total via a cheap serial run's setup is not exposed; just
            // report the node count and approximate degree.
            n * 6
        );

        // Parity sanity before timing: the two implementations must agree on the
        // most-central node (full agreement is pinned by the unit tests).
        let serial_top = pagerank(&g, &cfg).ranked()[0].0;
        let parallel_top = pagerank_parallel(&g, &cfg).ranked()[0].0;
        assert_eq!(
            serial_top, parallel_top,
            "serial and parallel disagree on the top node at n={n}"
        );

        let mut grp = c.benchmark_group(format!("pagerank_{n}"));
        grp.bench_function("serial", |b| {
            b.iter(|| black_box(pagerank(&g, &cfg).iterations))
        });
        grp.bench_function("parallel", |b| {
            b.iter(|| black_box(pagerank_parallel(&g, &cfg).iterations))
        });
        grp.finish();
    }

    c.final_summary();
}
