//! Load / throughput harness — runnable baseline driver (first slice, #241).
//!
//! Drives a mixed read/write workload over the public `Drevo` API across a
//! concurrency sweep and prints a machine-readable JSON array of sweep points
//! (p50/p95/p99 + throughput) to stdout, plus a human-readable table to stderr.
//!
//! This is intentionally **not** a `cargo test` and is kept off `ci-fast`: it
//! is a measurement tool, run on demand. Its logic mirrors the authoritative,
//! unit-tested implementation in `tests/load_harness_tests.rs`.
//!
//! Run:
//! ```text
//! cargo run --release --example load_harness
//! NODES=20000 OPS=5000 READ_PCT=80 cargo run --release --example load_harness
//! ```
//!
//! Deferred to follow-up PRs (see #241): the churn→compact→recovery curve, the
//! redb-backed (on-disk, single-writer) variant, and the HTTP load path.

use std::sync::Arc;
use std::time::Instant;

use drevo::db::Drevo;
use drevo::model::{Direction, NewEdge, NewNode, Properties};
use serde::Serialize;
use tempfile::TempDir;

// --- latency summary --------------------------------------------------------

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

// --- workload mix -----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Read,
    Write,
}

fn pick_op(i: u64, read_pct: u8) -> Op {
    let pct = read_pct.min(100) as u64;
    if i % 100 < pct {
        Op::Read
    } else {
        Op::Write
    }
}

// --- driver -----------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct LoadConfig {
    threads: usize,
    ops_per_thread: u64,
    read_pct: u8,
    node_count: u64,
}

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

// --- backend selection ------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    /// Ephemeral in-memory backend (default) — measures the lock/contention
    /// ceiling without fsync cost.
    Memory,
    /// On-disk redb backend — the same sweep now pays redb's single-writer
    /// fsync + copy-on-write cost per commit.
    Redb,
}

impl Backend {
    fn label(self) -> &'static str {
        match self {
            Backend::Memory => "in-memory",
            Backend::Redb => "redb (on-disk)",
        }
    }
}

/// Parse the `BACKEND` env value. Anything other than a redb alias falls back
/// to the in-memory backend, so an unset/garbage value keeps the fast default.
fn resolve_backend(s: &str) -> Backend {
    match s.trim().to_ascii_lowercase().as_str() {
        "redb" | "disk" | "ondisk" | "on-disk" => Backend::Redb,
        _ => Backend::Memory,
    }
}

/// Open a fresh backend for one sweep point. For redb the returned `TempDir`
/// guard must outlive the returned handle — dropping it deletes the file.
fn open_backend(backend: Backend, idx: usize) -> (Option<TempDir>, Arc<Drevo>) {
    match backend {
        Backend::Memory => (
            None,
            Arc::new(Drevo::open_in_memory().expect("open in-memory drevo")),
        ),
        Backend::Redb => {
            let dir = TempDir::new().expect("temp dir");
            let path = dir.path().join(format!("load_{idx}.redb"));
            let db = Drevo::open(&path).expect("open redb drevo");
            (Some(dir), Arc::new(db))
        }
    }
}

// --- main -------------------------------------------------------------------

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let node_count = env_u64("NODES", 5_000);
    let ops_per_thread = env_u64("OPS", 2_000);
    let read_pct = env_u64("READ_PCT", 80).min(100) as u8;
    let backend = resolve_backend(&std::env::var("BACKEND").unwrap_or_default());
    let sweep = [1usize, 2, 4, 8, 16];

    eprintln!(
        "load_harness: nodes={node_count} ops/thread={ops_per_thread} read_pct={read_pct} \
         sweep={sweep:?} backend={}",
        backend.label()
    );
    if backend == Backend::Redb {
        eprintln!(
            "note: redb writes are fsync-bound — every create_edge commits to disk. \
             Lower NODES/OPS if this is slow on your host."
        );
    }
    eprintln!(
        "{:>7}  {:>10}  {:>10}  {:>9}  {:>9}  {:>9}  {:>9}",
        "threads", "ops/s", "wall_ms", "rd_p50", "rd_p99", "wr_p50", "wr_p99"
    );

    let mut points = Vec::with_capacity(sweep.len());
    for (idx, &threads) in sweep.iter().enumerate() {
        // Fresh graph per point so sweep points are comparable (writes don't
        // accumulate across higher thread counts). `_guard` keeps the redb
        // temp dir alive for the duration of this point.
        let (_guard, db) = open_backend(backend, idx);
        let ids = Arc::new(seed_graph(&db, node_count));
        let point = run_load(
            &db,
            &ids,
            LoadConfig {
                threads,
                ops_per_thread,
                read_pct,
                node_count,
            },
        );
        eprintln!(
            "{:>7}  {:>10.0}  {:>10}  {:>9}  {:>9}  {:>9}  {:>9}",
            point.threads,
            point.throughput_ops_sec,
            point.wall_ms,
            point.reads.p50_us,
            point.reads.p99_us,
            point.writes.p50_us,
            point.writes.p99_us
        );
        points.push(point);
    }

    match serde_json::to_string_pretty(&points) {
        Ok(json) => println!("{json}"),
        Err(e) => eprintln!("failed to serialize sweep: {e}"),
    }
}
