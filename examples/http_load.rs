//! HTTP load harness (#241 slice 4) — RPS/latency **over the wire**.
//!
//! Starts the drevo HTTP API on an ephemeral localhost port and drives a mixed
//! read/write workload against it with a minimal, dependency-free HTTP/1.1
//! client across a concurrency sweep. Unlike `load_harness` (which calls the
//! in-process API directly), this measures the full request path — TCP connect,
//! HTTP framing, axum routing, JSON (de)serialisation — so the numbers are
//! request-level RPS and p50/p95/p99 latency.
//!
//! Read op = `GET /nodes/{id}`; write op = `POST /edges` between two seed nodes.
//!
//! Measurement tool, not a `cargo test`; run on demand. The over-the-wire flow
//! is covered at small scale by `http_load_end_to_end_small_scale` in
//! `tests/http_load_tests.rs` (which runs on the normal PR gate — localhost +
//! in-memory backend is fast).
//!
//! Run:
//! ```text
//! cargo run --release --example http_load
//! NODES=5000 OPS=2000 READ_PCT=80 cargo run --release --example http_load
//! ```

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use drevo::api::{build_router, ApiState};
use drevo::db::Drevo;
use drevo::model::{NewNode, Properties};
use serde::Serialize;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Read,
    Write,
}

fn pick_op(i: u64, read_pct: u8) -> Op {
    if i % 100 < read_pct.min(100) as u64 {
        Op::Read
    } else {
        Op::Write
    }
}

// --- minimal HTTP/1.1 client ------------------------------------------------

/// Extract the status code from an HTTP response's first line
/// (`HTTP/1.1 201 Created` -> `201`). Returns 0 on a malformed head.
fn parse_status(resp: &[u8]) -> u16 {
    let head_end = resp
        .iter()
        .position(|&b| b == b'\r' || b == b'\n')
        .unwrap_or(resp.len());
    std::str::from_utf8(&resp[..head_end])
        .ok()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

/// One request over a fresh `Connection: close` TCP socket. Returns the HTTP
/// status code, or an io error if the socket fails.
fn http_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> std::io::Result<u16> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes())?;
    stream.flush()?;
    let mut buf = Vec::with_capacity(256);
    stream.read_to_end(&mut buf)?;
    Ok(parse_status(&buf))
}

fn edge_body(from: u64, to: u64) -> String {
    format!(r#"{{"from_id":{from},"to_id":{to},"kind":"http","weight":1.0,"properties":{{}}}}"#)
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
    requests_per_sec: f64,
    reads: LatencySummary,
    writes: LatencySummary,
}

/// Run one sweep point: `threads` client threads firing HTTP requests at the
/// server, timing each and bucketing by read/write. A non-2xx status or a
/// socket error counts as an error.
fn run_http_load(addr: SocketAddr, ids: &Arc<Vec<u64>>, cfg: LoadConfig) -> SweepPoint {
    let n = cfg.node_count.max(1);
    let start = Instant::now();
    let handles: Vec<_> = (0..cfg.threads)
        .map(|t| {
            let ids = Arc::clone(ids);
            let ops = cfg.ops_per_thread;
            let read_pct = cfg.read_pct;
            std::thread::spawn(move || {
                let mut reads = Vec::new();
                let mut writes = Vec::new();
                let mut errors: u64 = 0;
                for op_i in 0..ops {
                    let x = (t as u64).wrapping_mul(0x9E37_79B9).wrapping_add(op_i);
                    let t0 = Instant::now();
                    let (bucket, status) = match pick_op(op_i, read_pct) {
                        Op::Read => {
                            let id = ids[(x % n) as usize];
                            (
                                &mut reads,
                                http_request(addr, "GET", &format!("/nodes/{id}"), None),
                            )
                        }
                        Op::Write => {
                            let from = ids[(x % n) as usize];
                            let to = ids[(x.wrapping_mul(7).wrapping_add(1) % n) as usize];
                            (
                                &mut writes,
                                http_request(addr, "POST", "/edges", Some(&edge_body(from, to))),
                            )
                        }
                    };
                    let dt = t0.elapsed().as_micros() as u64;
                    match status {
                        Ok(code) if (200..300).contains(&code) => bucket.push(dt),
                        _ => errors += 1,
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
        requests_per_sec: (total_ops - errors) as f64 / wall_secs,
        reads: summarize(&mut all_reads),
        writes: summarize(&mut all_writes),
    }
}

fn seed(db: &Drevo, n: u64) -> Vec<u64> {
    let mut ids = Vec::with_capacity(n as usize);
    for i in 0..n {
        let node = db
            .create_node(NewNode {
                kind: format!("kind_{}", i % 5),
                title: format!("http_node_{i:08}"),
                body: String::new(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .expect("seed node");
        ids.push(node.id);
    }
    ids
}

/// Poll `GET /health` until the server accepts connections (or give up after
/// ~2 s). Returns whether the server became ready.
fn wait_ready(addr: SocketAddr) -> bool {
    for _ in 0..100 {
        if let Ok(code) = http_request(addr, "GET", "/health", None) {
            if code == 200 {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let node_count = env_u64("NODES", 2_000);
    let ops_per_thread = env_u64("OPS", 500);
    let read_pct = env_u64("READ_PCT", 80).min(100) as u8;
    let sweep = [1usize, 2, 4, 8, 16];

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let db = Arc::new(Drevo::open_in_memory().expect("open in-memory drevo"));
    let ids = Arc::new(seed(&db, node_count));
    let router = build_router(ApiState::new(Arc::clone(&db)));

    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    rt.spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    if !wait_ready(addr) {
        eprintln!("server did not become ready at {addr}");
        return;
    }

    eprintln!(
        "http_load: nodes={node_count} ops/thread={ops_per_thread} read_pct={read_pct} \
         sweep={sweep:?} serving http://{addr}"
    );
    eprintln!(
        "{:>7}  {:>10}  {:>10}  {:>9}  {:>9}  {:>9}  {:>9}",
        "threads", "req/s", "wall_ms", "rd_p50", "rd_p99", "wr_p50", "wr_p99"
    );

    let mut points = Vec::with_capacity(sweep.len());
    for &threads in &sweep {
        let point = run_http_load(
            addr,
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
            point.requests_per_sec,
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
