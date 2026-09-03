//! Native-engine load harness — concurrency, throughput, tail latency, and
//! **deep traversal + durable writes** for the native graph engine (RFC #307).
//!
//! The `http_load` / `load_harness` examples measure the KV engine over the API
//! at shallow depth (point read + single edge insert). This one targets the
//! engine the live deployment now runs on — `native-durable` — and the two
//! things a graph engine actually has to survive that a micro-benchmark hides:
//!
//! * **concurrency**: a thread sweep hammering a *shared* engine, so read
//!   scaling and write serialization show up as real numbers (throughput +
//!   p50/p95/p99), not single-threaded op latency;
//! * **depth**: a 3-hop breadth-first traversal, where the index-free adjacency
//!   thesis is supposed to pay off (each hop is a pointer-chase in RAM for the
//!   native engine vs a fresh store lookup for KV);
//! * **durable writes**: `create_edge` against the WAL-backed native store, so
//!   the per-write `fsync` cost under contention is measured, not assumed.
//!
//! Both engines (`native-durable` with a real WAL, and the KV `Drevo`) are
//! loaded from the **same real-data GraphML snapshot** and driven through the
//! identical `GraphEngine` seam, so the comparison is apples-to-apples.
//!
//! # Running
//!
//! ```text
//! DREVO_BASELINE_GRAPHML=$HOME/drevo_backups/<snapshot>.graphml \
//!     cargo run --release --example native_load
//! # tunables: THREADS=1,2,4,8,16  OPS=2000  HOPS=3  WRITE_OPS=500
//! ```
//!
//! Without `DREVO_BASELINE_GRAPHML` it prints how to enable itself and exits 0,
//! so `clippy --all-targets` still compile-checks it. A tiny synthetic run is
//! exercised on the normal test gate by `tests/native_load_tests.rs`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use drevo::db::Drevo;
use drevo::engine::GraphEngine;
use drevo::migrate::migrate;
use drevo::model::{Direction, NewEdge, Properties};
use drevo::native::NativeGraph;
use serde::Serialize;

// --- latency summary (shared shape with examples/http_load.rs) --------------

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
    sorted_us[rank.clamp(1, n) - 1]
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

// --- workloads --------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Workload {
    PointRead,
    OneHop,
    ThreeHop,
    WriteEdge,
}

impl Workload {
    fn label(self) -> &'static str {
        match self {
            Workload::PointRead => "point_read",
            Workload::OneHop => "one_hop",
            Workload::ThreeHop => "three_hop_bfs",
            Workload::WriteEdge => "write_edge_fsync",
        }
    }
    fn mutates(self) -> bool {
        matches!(self, Workload::WriteEdge)
    }
}

/// A cheap, allocation-light deterministic mixer so each op touches a different
/// part of the graph without a real RNG (keeps the driver dependency-free).
fn mix(a: u64, b: u64) -> u64 {
    let mut x = a
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(b.wrapping_mul(0xD1B5_4A32_D192_ED03));
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 33;
    x
}

/// Breadth-first traversal to `depth` hops from `start`, returning how many
/// distinct nodes were reached — the deep-traversal workload.
fn bfs_reach<E: GraphEngine>(engine: &E, start: u64, depth: u32) -> usize {
    let mut seen = HashSet::from([start]);
    let mut frontier = vec![start];
    for _ in 0..depth {
        let mut next = Vec::new();
        for &node in &frontier {
            if let Ok(neighbours) = engine.neighbor_ids(node, Direction::Outgoing, None) {
                for nb in neighbours {
                    if seen.insert(nb) {
                        next.push(nb);
                    }
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    seen.len()
}

/// The read-only inputs a workload op needs, bundled so the driver stays under
/// the argument-count lint. `Copy` (two slice refs + a small int), so scoped
/// worker closures capture it by value.
#[derive(Clone, Copy)]
struct Workspace<'a> {
    ids: &'a [u64],
    seeds: &'a [u64],
    hops: u32,
}

/// One workload op; returns whether it succeeded (an engine error counts as an
/// error, not a timed sample).
fn do_op<E: GraphEngine>(engine: &E, wl: Workload, ws: Workspace, x: u64) -> bool {
    let n = ws.ids.len() as u64;
    let s = ws.seeds.len() as u64;
    match wl {
        Workload::PointRead => engine.get_node(ws.ids[(x % n) as usize]).is_ok(),
        Workload::OneHop => engine
            .neighbor_ids(ws.seeds[(x % s) as usize], Direction::Outgoing, None)
            .is_ok(),
        Workload::ThreeHop => {
            let _ = bfs_reach(engine, ws.seeds[(x % s) as usize], ws.hops);
            true
        }
        Workload::WriteEdge => {
            let from = ws.ids[(x % n) as usize];
            let to = ws.ids[(mix(x, 7) % n) as usize];
            engine
                .create_edge(NewEdge {
                    from_id: from,
                    to_id: to,
                    kind: "load".to_string(),
                    weight: 1.0,
                    properties: Properties::default(),
                })
                .is_ok()
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct Point {
    engine: String,
    workload: String,
    threads: usize,
    total_ops: u64,
    errors: u64,
    wall_ms: u64,
    ops_per_sec: f64,
    latency: LatencySummary,
}

/// Drive `threads` scoped threads, each running `ops_per_thread` iterations of
/// `wl` against the shared `engine`, and fold their per-op latencies into one
/// summary. Scoped threads borrow `&engine` directly (`GraphEngine: Sync`), so
/// there is no per-op cloning.
fn run_point<E: GraphEngine + Sync>(
    engine: &E,
    engine_name: &str,
    wl: Workload,
    ws: Workspace,
    threads: usize,
    ops_per_thread: u64,
) -> Point {
    let errors = AtomicU64::new(0);
    let start = Instant::now();
    let per_thread: Vec<Vec<u64>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let errors = &errors;
                scope.spawn(move || {
                    let mut lat = Vec::with_capacity(ops_per_thread as usize);
                    for op_i in 0..ops_per_thread {
                        let x = mix(t as u64, op_i);
                        let t0 = Instant::now();
                        let ok = do_op(engine, wl, ws, x);
                        let dt = t0.elapsed().as_micros() as u64;
                        if ok {
                            lat.push(dt);
                        } else {
                            errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    lat
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let wall = start.elapsed();

    let mut all: Vec<u64> = per_thread.into_iter().flatten().collect();
    let total_ops = threads as u64 * ops_per_thread;
    let errs = errors.load(Ordering::Relaxed);
    let wall_secs = wall.as_secs_f64().max(f64::MIN_POSITIVE);
    Point {
        engine: engine_name.to_string(),
        workload: wl.label().to_string(),
        threads,
        total_ops,
        errors: errs,
        wall_ms: wall.as_millis() as u64,
        ops_per_sec: (total_ops - errs) as f64 / wall_secs,
        latency: summarize(&mut all),
    }
}

// --- setup ------------------------------------------------------------------

/// The highest-out-degree nodes, which make the 1-hop / 3-hop traversals do
/// real fan-out work rather than dead-ending immediately.
fn pick_seeds<E: GraphEngine>(engine: &E, ids: &[u64], k: usize) -> Vec<u64> {
    let mut by_deg: Vec<(u64, usize)> = ids
        .iter()
        .map(|&id| {
            let deg = engine
                .neighbor_ids(id, Direction::Outgoing, None)
                .map(|v| v.len())
                .unwrap_or(0);
            (id, deg)
        })
        .collect();
    by_deg.sort_unstable_by_key(|&(_, deg)| std::cmp::Reverse(deg));
    by_deg.truncate(k.max(1));
    by_deg.into_iter().map(|(id, _)| id).collect()
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let Ok(path) = std::env::var("DREVO_BASELINE_GRAPHML") else {
        eprintln!(
            "native_load skipped. Set DREVO_BASELINE_GRAPHML to a GraphML export, e.g.:\n  \
             DREVO_BASELINE_GRAPHML=$HOME/drevo_backups/<snapshot>.graphml \
             cargo run --release --example native_load"
        );
        return;
    };

    let ops = env_u64("OPS", 2_000);
    let write_ops = env_u64("WRITE_OPS", 500);
    let hops = env_u64("HOPS", 3) as u32;
    let sweep: Vec<usize> = std::env::var("THREADS")
        .ok()
        .map(|raw| {
            raw.split(',')
                .filter_map(|s| s.trim().parse().ok())
                .collect::<Vec<_>>()
        })
        .filter(|v: &Vec<usize>| !v.is_empty())
        .unwrap_or_else(|| vec![1, 2, 4, 8]);

    // Load the KV engine from the snapshot, then mirror it into a durable native
    // store (a real on-disk WAL in a temp dir, so writes fsync for real).
    let kv = Drevo::open_in_memory().expect("open kv");
    let report = kv
        .import_graphml_from_path(std::path::Path::new(&path))
        .expect("import graphml");
    let tmp = std::env::temp_dir().join(format!("drevo_native_load_{}.wal", std::process::id()));
    let native = NativeGraph::open_durable(&tmp).expect("open durable native");
    let mig = migrate(&kv, &native).expect("migrate kv->native");
    eprintln!(
        "native_load: loaded {} nodes / {} edges (native durable seeded {} nodes / {} edges), \
         wal={}",
        report.nodes_imported,
        report.edges_imported,
        mig.nodes_imported,
        mig.edges_imported,
        tmp.display()
    );

    let ids: Vec<u64> = GraphEngine::all_nodes(&native)
        .expect("all_nodes")
        .iter()
        .map(|n| n.id)
        .collect();
    let seeds = pick_seeds(&native, &ids, 32);
    eprintln!(
        "workload: OPS/thread={ops} WRITE_OPS/thread={write_ops} HOPS={hops} \
         seeds={} (top out-degree {}), thread sweep {sweep:?}",
        seeds.len(),
        native
            .neighbor_ids(seeds[0], Direction::Outgoing, None)
            .map(|v| v.len())
            .unwrap_or(0),
    );
    eprintln!(
        "{:>14}  {:>7}  {:>7}  {:>12}  {:>9}  {:>9}  {:>9}",
        "workload", "engine", "threads", "ops/s", "p50_us", "p95_us", "p99_us"
    );

    let mut points: Vec<Point> = Vec::new();
    // Read workloads first (immutable), each across the whole thread sweep, on
    // both engines; the mutating write workload runs last so it never perturbs a
    // read measurement.
    let ws = Workspace {
        ids: &ids,
        seeds: &seeds,
        hops,
    };
    let read_workloads = [Workload::PointRead, Workload::OneHop, Workload::ThreeHop];
    for &wl in read_workloads.iter().chain([Workload::WriteEdge].iter()) {
        let per_thread_ops = if wl.mutates() { write_ops } else { ops };
        for &threads in &sweep {
            let kv_point = run_point(&kv, "kv", wl, ws, threads, per_thread_ops);
            let native_point = run_point(&native, "native", wl, ws, threads, per_thread_ops);
            for p in [&kv_point, &native_point] {
                eprintln!(
                    "{:>14}  {:>7}  {:>7}  {:>12.0}  {:>9}  {:>9}  {:>9}",
                    p.workload,
                    p.engine,
                    p.threads,
                    p.ops_per_sec,
                    p.latency.p50_us,
                    p.latency.p95_us,
                    p.latency.p99_us
                );
            }
            points.push(kv_point);
            points.push(native_point);
        }
    }

    let _ = std::fs::remove_file(&tmp);
    match serde_json::to_string_pretty(&points) {
        Ok(json) => println!("{json}"),
        Err(e) => eprintln!("serialize failed: {e}"),
    }
}
