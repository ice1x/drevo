//! Liveness while Cypher is busy — issue #547.
//!
//! On the live server a slow statement plus a handful of follow-up requests
//! parked every tokio worker (Cypher ran synchronously on the async worker
//! threads), so even `GET /health` — which takes no lock — stopped answering
//! until a container restart.
//!
//! The test starts the real axum server on a **two-worker** runtime, keeps
//! more slow statements in flight than there are workers, and requires
//! `/health` to answer while they are still running. The "still running" part
//! is checked, not assumed: every slow statement must finish *after* the
//! health reply, otherwise the test would pass vacuously.

#![cfg(feature = "http")]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;

/// One HTTP/1.1 request; returns the status code (0 if unparsable).
fn http(addr: SocketAddr, method: &str, path: &str, body: &str, timeout: Duration) -> u16 {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream.set_read_timeout(Some(timeout)).expect("timeout");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).expect("write");
    let mut buf = Vec::new();
    if stream.read_to_end(&mut buf).is_err() {
        return 0;
    }
    std::str::from_utf8(&buf)
        .ok()
        .and_then(|s| s.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

fn start_server(workers: usize) -> (tokio::runtime::Runtime, SocketAddr) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .expect("runtime");
    let router = build_native_router(NativeApiState::new(Arc::new(NativeService::in_memory())));
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    rt.spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (rt, addr)
}

/// CPU-bound, constant-memory statement: `n²` additions via nested `reduce`.
fn slow_body(n: u32) -> String {
    format!(
        r#"{{"query": "RETURN reduce(s = 0, i IN range(1, {n}) | s + reduce(t = 0, j IN range(1, {n}) | t + j)) AS v"}}"#
    )
}

#[test]
fn health_answers_while_more_slow_statements_than_workers_run() {
    let (_rt, addr) = start_server(2);
    let quick = Duration::from_secs(5);
    assert_eq!(http(addr, "GET", "/health", "", quick), 200, "server up");

    // Calibrate: one slow statement must take well over the health budget,
    // so "still running" below is meaningful on any machine.
    let mut n = 400;
    loop {
        let t = Instant::now();
        assert_eq!(
            http(
                addr,
                "POST",
                "/cypher",
                &slow_body(n),
                Duration::from_secs(120)
            ),
            200
        );
        if t.elapsed() >= Duration::from_millis(1500) || n >= 20_000 {
            break;
        }
        n *= 2;
    }

    let slow: Vec<_> = (0..4)
        .map(|_| {
            thread::spawn(move || {
                let code = http(
                    addr,
                    "POST",
                    "/cypher",
                    &slow_body(n),
                    Duration::from_secs(300),
                );
                (code, Instant::now())
            })
        })
        .collect();
    thread::sleep(Duration::from_millis(200)); // let them occupy the server

    let asked = Instant::now();
    let health = http(addr, "GET", "/health", "", quick);
    let answered = Instant::now();
    assert_eq!(health, 200, "/health must answer while Cypher is busy");
    assert!(
        answered - asked < Duration::from_secs(1),
        "/health took {:?} behind busy Cypher",
        answered - asked
    );

    for handle in slow {
        let (code, done) = handle.join().expect("slow statement thread");
        assert_eq!(code, 200, "slow statement completes normally");
        assert!(
            done > answered,
            "a slow statement finished before /health answered — the check was vacuous"
        );
    }
}
