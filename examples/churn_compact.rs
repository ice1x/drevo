//! Churn → compact → recovery harness (#241 slice 2).
//!
//! Measures how read performance and on-disk footprint degrade after heavy
//! delete/insert/update churn against the **redb (on-disk)** backend, and how
//! much `Drevo::compact()` recovers. Prints a JSON report (three phases +
//! compaction stats + file sizes) to stdout and a human table to stderr.
//!
//! This quantifies the degradation the #240 adjacency-layout investigation
//! predicts: a COW B-tree file holds its high-water mark and scatters live
//! pages across the freelist under churn until compaction rewrites it.
//!
//! Measurement tool, not a `cargo test`; kept off `ci-fast`, run on demand.
//! The equivalent flow is covered at small scale by the `#[ignore]`d
//! `redb_three_phase_churn_compact_recovers` in `tests/compaction_tests.rs`
//! (which runs in `slow-tests.yml`).
//!
//! Run:
//! ```text
//! cargo run --release --example churn_compact
//! NODES=20000 EDGES=40000 CHURN=40000 PROBE=20000 cargo run --release --example churn_compact
//! ```

use std::path::Path;
use std::time::Instant;

use drevo::db::Drevo;
use drevo::model::{Direction, NewEdge, NewNode, NodePatch, Properties};
use serde::Serialize;
use tempfile::TempDir;

// --- latency summary (mirrors examples/load_harness.rs) ---------------------

#[derive(Debug, Clone, Default, Serialize)]
struct LatencySummary {
    count: u64,
    min_us: u64,
    max_us: u64,
    mean_us: u64,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
}

fn percentile(sorted_us: &[u64], q: f64) -> u64 {
    if sorted_us.is_empty() {
        return 0;
    }
    let n = sorted_us.len();
    let rank = (q / 100.0 * n as f64).ceil() as usize;
    let idx = rank.clamp(1, n) - 1;
    sorted_us[idx]
}

fn summarize(latencies_us: &mut [u64]) -> LatencySummary {
    if latencies_us.is_empty() {
        return LatencySummary::default();
    }
    latencies_us.sort_unstable();
    let count = latencies_us.len() as u64;
    let sum: u128 = latencies_us.iter().map(|&v| v as u128).sum();
    LatencySummary {
        count,
        min_us: latencies_us[0],
        max_us: latencies_us[latencies_us.len() - 1],
        mean_us: (sum / count as u128) as u64,
        p50_us: percentile(latencies_us, 50.0),
        p95_us: percentile(latencies_us, 95.0),
        p99_us: percentile(latencies_us, 99.0),
    }
}

// --- phase report -----------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct Phase {
    name: String,
    probe_ops: u64,
    reads_ok: u64,
    throughput_ops_sec: f64,
    file_bytes: u64,
    latency: LatencySummary,
}

/// A single-threaded read probe: `get_node` + 1-hop `neighbors` over a
/// deterministic pseudo-random walk of the seed ids. Reads isolate the
/// adjacency-scan locality that churn erodes and compaction restores.
fn probe(db: &Drevo, ids: &[u64], ops: u64, path: &Path, name: &str) -> Phase {
    let n = ids.len().max(1) as u64;
    let mut lat = Vec::with_capacity(ops as usize);
    let mut ok: u64 = 0;
    let start = Instant::now();
    for i in 0..ops {
        let id = ids[(i.wrapping_mul(0x9E37_79B9) % n) as usize];
        let t0 = Instant::now();
        let got = db
            .get_node(id)
            .and_then(|_| db.neighbors(id, Direction::Outgoing, None));
        lat.push(t0.elapsed().as_micros() as u64);
        if got.is_ok() {
            ok += 1;
        }
    }
    let wall = start.elapsed().as_secs_f64().max(f64::MIN_POSITIVE);
    Phase {
        name: name.to_string(),
        probe_ops: ops,
        reads_ok: ok,
        throughput_ops_sec: ok as f64 / wall,
        file_bytes: file_size(path),
        latency: summarize(&mut lat),
    }
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn seed(db: &Drevo, nodes: u64, edges: u64) -> Vec<u64> {
    let mut ids = Vec::with_capacity(nodes as usize);
    for i in 0..nodes {
        let node = db
            .create_node(NewNode {
                kind: format!("kind_{}", i % 5),
                title: format!("cc_node_{i:08}"),
                body: String::new(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .expect("seed node");
        ids.push(node.id);
    }
    let n = ids.len().max(1) as u64;
    for e in 0..edges {
        let from = ids[(e.wrapping_mul(2_654_435_761) % n) as usize];
        let to = ids[(e.wrapping_mul(40_503).wrapping_add(1) % n) as usize];
        let _ = db.create_edge(NewEdge {
            from_id: from,
            to_id: to,
            kind: "seed".to_string(),
            weight: 1.0,
            properties: Properties::default(),
        });
    }
    ids
}

/// Heavy churn in a **grow-then-shrink** shape, so the redb file first climbs
/// to a high-water mark and is then left with a large freed region for
/// `compact()` to reclaim (an interleaved create+delete never builds that peak
/// — the freelist just recycles in place). Returns the graph to ~its original
/// logical size: every inserted edge is deleted again; node bodies are
/// rewritten along the way to churn the node/FTS pages too.
fn churn(db: &Drevo, ids: &[u64], rounds: u64) {
    let n = ids.len().max(1) as u64;
    // Grow: insert `rounds` fresh edges (kept) and rewrite node bodies.
    let mut inserted = Vec::with_capacity(rounds as usize);
    for r in 0..rounds {
        let a = ids[(r.wrapping_mul(0x9E37_79B9) % n) as usize];
        let b = ids[(r.wrapping_mul(2_246_822_519).wrapping_add(1) % n) as usize];
        if let Ok(edge) = db.create_edge(NewEdge {
            from_id: a,
            to_id: b,
            kind: "churn".to_string(),
            weight: 1.0,
            properties: Properties::default(),
        }) {
            inserted.push(edge.id);
        }
        let _ = db.update_node(
            a,
            NodePatch {
                body: Some(format!("churned-{r}")),
                ..Default::default()
            },
        );
    }
    // Shrink: delete every inserted edge, freeing the pages they occupied.
    for id in inserted {
        let _ = db.delete_edge(id);
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let nodes = env_u64("NODES", 10_000);
    let edges = env_u64("EDGES", 20_000);
    let churn_rounds = env_u64("CHURN", 20_000);
    let probe_ops = env_u64("PROBE", 10_000);

    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("churn_compact.redb");

    let mut db = Drevo::open(&path).expect("open redb");
    let ids = seed(&db, nodes, edges);

    eprintln!(
        "churn_compact: nodes={nodes} edges={edges} churn={churn_rounds} probe={probe_ops} \
         (redb backend @ {})",
        path.display()
    );

    let steady = probe(&db, &ids, probe_ops, &path, "steady");
    churn(&db, &ids, churn_rounds);
    let degraded = probe(&db, &ids, probe_ops, &path, "degraded");
    let report = db.compact().expect("compact");
    let recovered = probe(&db, &ids, probe_ops, &path, "recovered");

    eprintln!(
        "{:>10}  {:>12}  {:>10}  {:>9}  {:>9}",
        "phase", "file_bytes", "ops/s", "rd_p50", "rd_p99"
    );
    for p in [&steady, &degraded, &recovered] {
        eprintln!(
            "{:>10}  {:>12}  {:>10.0}  {:>9}  {:>9}",
            p.name, p.file_bytes, p.throughput_ops_sec, p.latency.p50_us, p.latency.p99_us
        );
    }
    eprintln!(
        "compaction: {} -> {} bytes ({} reclaimed)",
        report.bytes_before.unwrap_or(0),
        report.bytes_after.unwrap_or(0),
        report.bytes_reclaimed
    );

    let out = serde_json::json!({
        "config": { "nodes": nodes, "edges": edges, "churn": churn_rounds, "probe_ops": probe_ops },
        "phases": [steady, degraded, recovered],
        "compaction": report,
    });
    match serde_json::to_string_pretty(&out) {
        Ok(json) => println!("{json}"),
        Err(e) => eprintln!("failed to serialize: {e}"),
    }
}
