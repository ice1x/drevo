//! HTTP load harness — authoritative tested pieces (#241 slice 4).
//!
//! Validates the over-the-wire client + endpoint path that
//! `examples/http_load.rs` drives: the status-line parser (pure) and a small
//! end-to-end run that starts the real axum server on an ephemeral localhost
//! port and hits it with the same minimal HTTP/1.1 client. The full RPS sweep
//! is an on-demand measurement in the example; this keeps the flow honest on
//! the normal PR gate (localhost + in-memory backend is fast).
//!
//! The whole file is gated on `http` — without axum/tokio there is no server.

#![cfg(feature = "http")]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use drevo::api::{build_router, ApiState};
use drevo::db::Drevo;
use drevo::model::{NewNode, Properties};

// --- minimal HTTP/1.1 client (mirrors examples/http_load.rs) ----------------

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

fn wait_ready(addr: SocketAddr) -> bool {
    for _ in 0..100 {
        if let Ok(200) = http_request(addr, "GET", "/health", None) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn seed(db: &Drevo, n: u64) -> Vec<u64> {
    (0..n)
        .map(|i| {
            db.create_node(NewNode {
                kind: "note".to_string(),
                title: format!("http_node_{i:08}"),
                body: String::new(),
                body_html: String::new(),
                properties: Properties::default(),
            })
            .expect("seed node")
            .id
        })
        .collect()
}

/// Start the real axum server on an ephemeral port. The returned `Runtime`
/// must be kept alive for the server to keep serving.
fn start_server(db: Arc<Drevo>) -> (tokio::runtime::Runtime, SocketAddr) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let router = build_router(ApiState::new(db));
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .expect("bind ephemeral");
    let addr = listener.local_addr().expect("local addr");
    rt.spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (rt, addr)
}

// --- tests ------------------------------------------------------------------

#[test]
fn parse_status_extracts_code() {
    assert_eq!(parse_status(b"HTTP/1.1 201 Created\r\n\r\n"), 201);
    assert_eq!(
        parse_status(b"HTTP/1.1 200 OK\r\nX-Foo: y\r\n\r\nbody"),
        200
    );
    // No trailing CRLF still parses the status line.
    assert_eq!(parse_status(b"HTTP/1.1 404 Not Found"), 404);
    // Malformed / empty -> 0.
    assert_eq!(parse_status(b"garbage"), 0);
    assert_eq!(parse_status(b""), 0);
}

#[test]
fn http_load_end_to_end_small_scale() {
    let db = Arc::new(Drevo::open_in_memory().expect("open"));
    let ids = seed(&db, 10);
    let (_rt, addr) = start_server(Arc::clone(&db));
    assert!(wait_ready(addr), "server must become ready");

    // Fire a small read/write mix over the wire and assert every request is 2xx.
    let mut ok_reads = 0u32;
    let mut ok_writes = 0u32;
    for i in 0..20u64 {
        if i % 2 == 0 {
            let id = ids[(i as usize) % ids.len()];
            let code = http_request(addr, "GET", &format!("/nodes/{id}"), None).expect("GET");
            assert_eq!(code, 200, "GET /nodes/{id} must return 200");
            ok_reads += 1;
        } else {
            let code = http_request(addr, "POST", "/edges", Some(&edge_body(ids[0], ids[1])))
                .expect("POST");
            assert_eq!(code, 201, "POST /edges must return 201");
            ok_writes += 1;
        }
    }
    assert_eq!(ok_reads, 10);
    assert_eq!(ok_writes, 10);

    // A missing node is a clean 404 over the wire (error path also works).
    let code = http_request(addr, "GET", "/nodes/999999", None).expect("GET missing");
    assert_eq!(code, 404, "missing node must return 404");
}
