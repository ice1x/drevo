//! Load / throughput harness — first slice ([#241]).
//!
//! This file is the **authoritative, tested** implementation of the harness
//! logic: latency percentiles, the deterministic read/write op mix, and the
//! concurrent `run_load` driver. The runnable `examples/load_harness.rs` is a
//! thin driver that mirrors these same pieces to produce a baseline JSON sweep
//! on demand (kept off `ci-fast`); this test validates them at small scale so
//! CI protects the logic even though the full run never executes here.
//!
//! Scope of this slice: a mixed read/write workload over the public `Drevo`
//! API, a concurrency sweep, and p50/p95/p99 + throughput. The churn→compact
//! degradation curve and the HTTP path are deliberately deferred to follow-up
//! PRs (see [#241]).
//!
//! [#241]: https://github.com/ice1x/drevo/issues/241

use std::sync::Arc;
use std::time::Instant;

use drevo::db::Drevo;
use drevo::model::{Direction, NewEdge, NewNode, Properties};
use serde::Serialize;

// ---------------------------------------------------------------------------
// Latency summary
// ---------------------------------------------------------------------------

/// Aggregated latency statistics for one category of operation, in
/// microseconds. Percentiles use the nearest-rank method.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
struct LatencySummary {
    count: u64,
    min_us: u64,
    max_us: u64,
    mean_us: u64,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
}

/// Nearest-rank percentile over an already-sorted ascending slice.
///
/// `q` is in `[0.0, 100.0]`. Returns 0 for an empty slice. The rank is
/// `ceil(q/100 * n)` clamped to `[1, n]`, indexed as `rank - 1`.
fn percentile(sorted_us: &[u64], q: f64) -> u64 {
    if sorted_us.is_empty() {
        return 0;
    }
    let n = sorted_us.len();
    let rank = (q / 100.0 * n as f64).ceil() as usize;
    let idx = rank.clamp(1, n) - 1;
    sorted_us[idx]
}

/// Summarize a set of latency samples (consumes/sorts the buffer).
fn summarize(latencies_us: &mut [u64]) -> LatencySummary {
    if latencies_us.is_empty() {
        return LatencySummary::default();
    }
    latencies_us.sort_unstable();
    let count = latencies_us.len() as u64;
    let sum: u128 = latencies_us.iter().map(|&v| v as u128).sum();
    let mean_us = (sum / count as u128) as u64;
    LatencySummary {
        count,
        min_us: latencies_us[0],
        max_us: latencies_us[latencies_us.len() - 1],
        mean_us,
        p50_us: percentile(latencies_us, 50.0),
        p95_us: percentile(latencies_us, 95.0),
        p99_us: percentile(latencies_us, 99.0),
    }
}

// ---------------------------------------------------------------------------
// Workload mix
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Read,
    Write,
}

/// Deterministic op selection: exactly `read_pct` reads per 100 ops, so a
/// sweep is reproducible and the mix is exactly as configured. `read_pct` is
/// clamped to `0..=100`.
fn pick_op(i: u64, read_pct: u8) -> Op {
    let pct = read_pct.min(100) as u64;
    if i % 100 < pct {
        Op::Read
    } else {
        Op::Write
    }
}

// ---------------------------------------------------------------------------
// Backend selection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Memory,
    Redb,
}

/// Parse the `BACKEND` env value. Anything other than a redb alias falls back
/// to the in-memory backend, so an unset/garbage value keeps the fast default.
fn resolve_backend(s: &str) -> Backend {
    match s.trim().to_ascii_lowercase().as_str() {
        "redb" | "disk" | "ondisk" | "on-disk" => Backend::Redb,
        _ => Backend::Memory,
    }
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct LoadConfig {
    threads: usize,
    ops_per_thread: u64,
    read_pct: u8,
    /// Number of seed nodes the workload reads/links against.
    node_count: u64,
}

/// One point of the concurrency sweep — the machine-readable unit the baseline
/// table is built from.
#[derive(Debug, Clone, Serialize)]
struct SweepPoint {
    threads: usize,
    ops_per_thread: u64,
    read_pct: u8,
    total_ops: u64,
    errors: u64,
    wall_ms: u64,
    throughput_ops_sec: f64,
    reads: LatencySummary,
    writes: LatencySummary,
}

/// Seed `n` nodes and return their ids. Deterministic, globally-unique titles.
fn seed_graph(db: &Drevo, n: u64) -> Vec<u64> {
    let mut ids = Vec::with_capacity(n as usize);
    for i in 0..n {
        let node = db
            .create_node(NewNode {
                kind: format!("kind_{}", i % 5),
                title: format!("load_node_{i:08}"),
                body: String::new(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .expect("seed node");
        ids.push(node.id);
    }
    ids
}

/// Run one sweep point: `threads` workers, each issuing `ops_per_thread`
/// operations against the shared `Drevo`, timing every op and bucketing the
/// latency by read/write. Errors are counted, never panicked on.
fn run_load(db: &Arc<Drevo>, ids: &Arc<Vec<u64>>, cfg: LoadConfig) -> SweepPoint {
    let n = cfg.node_count.max(1);
    let start = Instant::now();

    let handles: Vec<_> = (0..cfg.threads)
        .map(|t| {
            let db = Arc::clone(db);
            let ids = Arc::clone(ids);
            let ops = cfg.ops_per_thread;
            let read_pct = cfg.read_pct;
            std::thread::spawn(move || {
                let mut reads = Vec::new();
                let mut writes = Vec::new();
                let mut errors: u64 = 0;
                for op_i in 0..ops {
                    // Distinct deterministic stream per thread.
                    let x = (t as u64).wrapping_mul(0x9E37_79B9).wrapping_add(op_i);
                    match pick_op(op_i, read_pct) {
                        Op::Read => {
                            let id = ids[(x % n) as usize];
                            let t0 = Instant::now();
                            let got = db
                                .get_node(id)
                                .and_then(|_| db.neighbors(id, Direction::Outgoing, None));
                            let dt = t0.elapsed().as_micros() as u64;
                            if got.is_ok() {
                                reads.push(dt);
                            } else {
                                errors += 1;
                            }
                        }
                        Op::Write => {
                            let from = ids[(x % n) as usize];
                            let to = ids[(x.wrapping_mul(7).wrapping_add(1) % n) as usize];
                            let t0 = Instant::now();
                            let res = db.create_edge(NewEdge {
                                from_id: from,
                                to_id: to,
                                kind: "load".to_string(),
                                weight: 1.0,
                                properties: Properties::default(),
                            });
                            let dt = t0.elapsed().as_micros() as u64;
                            if res.is_ok() {
                                writes.push(dt);
                            } else {
                                errors += 1;
                            }
                        }
                    }
                }
                (reads, writes, errors)
            })
        })
        .collect();

    let mut all_reads = Vec::new();
    let mut all_writes = Vec::new();
    let mut errors: u64 = 0;
    for h in handles {
        match h.join() {
            Ok((r, w, e)) => {
                all_reads.extend(r);
                all_writes.extend(w);
                errors += e;
            }
            // A panicked worker counts its whole slice as errored rather than
            // taking the harness down.
            Err(_) => errors += cfg.ops_per_thread,
        }
    }

    let wall = start.elapsed();
    let total_ops = cfg.threads as u64 * cfg.ops_per_thread;
    let wall_secs = wall.as_secs_f64().max(f64::MIN_POSITIVE);
    SweepPoint {
        threads: cfg.threads,
        ops_per_thread: cfg.ops_per_thread,
        read_pct: cfg.read_pct,
        total_ops,
        errors,
        wall_ms: wall.as_millis() as u64,
        throughput_ops_sec: (total_ops - errors) as f64 / wall_secs,
        reads: summarize(&mut all_reads),
        writes: summarize(&mut all_writes),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn percentile_nearest_rank_on_known_input() {
    // 1..=100 sorted; nearest-rank: p50 -> idx ceil(0.5*100)-1 = 49 -> 50,
    // p95 -> 95, p99 -> 99, p100 -> 100.
    let data: Vec<u64> = (1..=100).collect();
    assert_eq!(percentile(&data, 50.0), 50);
    assert_eq!(percentile(&data, 95.0), 95);
    assert_eq!(percentile(&data, 99.0), 99);
    assert_eq!(percentile(&data, 100.0), 100);
    assert_eq!(percentile(&data, 0.0), 1);
}

#[test]
fn percentile_empty_is_zero() {
    assert_eq!(percentile(&[], 50.0), 0);
    assert_eq!(percentile(&[], 99.0), 0);
}

#[test]
fn summarize_reports_ordered_stats() {
    let mut data: Vec<u64> = vec![5, 1, 4, 2, 3];
    let s = summarize(&mut data);
    assert_eq!(s.count, 5);
    assert_eq!(s.min_us, 1);
    assert_eq!(s.max_us, 5);
    assert_eq!(s.mean_us, 3);
    assert!(s.min_us <= s.p50_us);
    assert!(s.p50_us <= s.p95_us);
    assert!(s.p95_us <= s.p99_us);
    assert!(s.p99_us <= s.max_us);
}

#[test]
fn summarize_empty_is_default() {
    let mut empty: Vec<u64> = Vec::new();
    assert_eq!(summarize(&mut empty), LatencySummary::default());
}

#[test]
fn pick_op_honors_read_percentage() {
    let reads_at = |pct: u8| (0..100).filter(|&i| pick_op(i, pct) == Op::Read).count();
    assert_eq!(reads_at(100), 100);
    assert_eq!(reads_at(0), 0);
    assert_eq!(reads_at(50), 50);
    assert_eq!(reads_at(80), 80);
    // Clamped above 100.
    assert_eq!(reads_at(200), 100);
}

#[test]
fn resolve_backend_maps_aliases_and_defaults_to_memory() {
    assert_eq!(resolve_backend("redb"), Backend::Redb);
    assert_eq!(resolve_backend("  ReDB "), Backend::Redb);
    assert_eq!(resolve_backend("disk"), Backend::Redb);
    assert_eq!(resolve_backend("on-disk"), Backend::Redb);
    // Unset / unknown / garbage keeps the fast in-memory default.
    assert_eq!(resolve_backend(""), Backend::Memory);
    assert_eq!(resolve_backend("memory"), Backend::Memory);
    assert_eq!(resolve_backend("sqlite"), Backend::Memory);
}

#[test]
fn run_load_small_scale_is_consistent() {
    let db = Arc::new(Drevo::open_in_memory().expect("open"));
    let ids = Arc::new(seed_graph(&db, 200));
    let cfg = LoadConfig {
        threads: 4,
        ops_per_thread: 100,
        read_pct: 70,
        node_count: 200,
    };
    let point = run_load(&db, &ids, cfg);

    // Every op is accounted for exactly once.
    assert_eq!(point.total_ops, 400);
    assert_eq!(point.errors, 0, "no op should error at this scale");
    assert_eq!(point.reads.count + point.writes.count, point.total_ops);

    // 70% reads deterministically: 70 reads / 30 writes per 100 ops, ×4 threads.
    assert_eq!(point.reads.count, 280);
    assert_eq!(point.writes.count, 120);

    // Percentiles are ordered and throughput is positive.
    assert!(point.reads.p50_us <= point.reads.p99_us);
    assert!(point.throughput_ops_sec > 0.0);
}

#[test]
fn sweep_point_serializes_to_json() {
    let db = Arc::new(Drevo::open_in_memory().expect("open"));
    let ids = Arc::new(seed_graph(&db, 50));
    let point = run_load(
        &db,
        &ids,
        LoadConfig {
            threads: 2,
            ops_per_thread: 50,
            read_pct: 50,
            node_count: 50,
        },
    );
    let json = serde_json::to_value(&point).expect("serialize");
    for key in [
        "threads",
        "total_ops",
        "throughput_ops_sec",
        "reads",
        "writes",
    ] {
        assert!(json.get(key).is_some(), "missing key {key}");
    }
    assert!(json["reads"].get("p95_us").is_some());
}

#[test]
fn concurrency_sweep_covers_the_configured_thread_counts() {
    let db = Arc::new(Drevo::open_in_memory().expect("open"));
    let ids = Arc::new(seed_graph(&db, 100));
    let mut points = Vec::new();
    for &threads in &[1usize, 2, 4] {
        points.push(run_load(
            &db,
            &ids,
            LoadConfig {
                threads,
                ops_per_thread: 50,
                read_pct: 80,
                node_count: 100,
            },
        ));
    }
    assert_eq!(points.len(), 3);
    assert_eq!(
        points.iter().map(|p| p.threads).collect::<Vec<_>>(),
        vec![1, 2, 4]
    );
    assert!(points.iter().all(|p| p.errors == 0));
}
