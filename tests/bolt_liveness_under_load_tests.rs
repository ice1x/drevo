//! Bolt liveness while Cypher is busy — issue #547.
//!
//! The Bolt session loop is async, but RUN/PULL execute Cypher synchronously.
//! Called straight from the loop, a slow statement pins its tokio worker; a
//! few of them in flight starve the runtime, and every other connection and
//! task — including the HTTP `/health` endpoint on the same runtime — stops
//! being served.
//!
//! On a **two-worker** runtime this test keeps three slow Bolt statements in
//! flight and requires an unrelated async task to be scheduled promptly while
//! they are still running (the probe must land sooner than one statement can
//! finish, so the test cannot pass vacuously).

#![cfg(all(not(target_arch = "wasm32"), feature = "http"))]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use drevo::bolt::handshake::MAGIC_PREAMBLE;
use drevo::bolt::listener::accept_and_run_session_durable;
use drevo::bolt::packstream::{decode, encode, Value};
use drevo::bolt::session::{HELLO, PULL, RUN};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

const SUCCESS: u8 = 0x70;

/// CPU-bound, constant-memory statement: `n²` additions via nested `reduce`.
fn slow_query(n: u32) -> String {
    format!("RETURN reduce(s = 0, i IN range(1, {n}) | s + reduce(t = 0, j IN range(1, {n}) | t + j)) AS v")
}

/// Grow `n` until one statement takes well over the liveness budget here;
/// returns `n` and how long one such statement took.
fn calibrate(svc: &NativeService) -> (u32, Duration) {
    let mut n = 400;
    loop {
        let q = parse(&slow_query(n)).expect("parse");
        let t = Instant::now();
        svc.execute(&q, HashMap::new()).expect("slow statement");
        let took = t.elapsed();
        if took >= Duration::from_millis(1500) || n >= 20_000 {
            return (n, took);
        }
        n *= 2;
    }
}

fn frame(tag: u8, fields: Vec<Value>) -> Vec<u8> {
    let mut buf = Vec::new();
    encode(&Value::Structure { tag, fields }, &mut buf).expect("encode");
    let mut framed = (buf.len() as u16).to_be_bytes().to_vec();
    framed.extend_from_slice(&buf);
    framed.extend_from_slice(&[0, 0]);
    framed
}

async fn read_one(client: &mut TcpStream) -> Value {
    let mut payload = Vec::new();
    loop {
        let mut len = [0u8; 2];
        client.read_exact(&mut len).await.expect("chunk length");
        let l = u16::from_be_bytes(len) as usize;
        if l == 0 {
            break;
        }
        let start = payload.len();
        payload.resize(start + l, 0);
        client
            .read_exact(&mut payload[start..])
            .await
            .expect("chunk");
    }
    decode(&payload).expect("decode").0
}

fn tag_of(v: &Value) -> u8 {
    match v {
        Value::Structure { tag, .. } => *tag,
        other => panic!("expected a structure, got {other:?}"),
    }
}

/// Connect and pipeline the Bolt 4.4 handshake, HELLO, RUN `query` and PULL
/// without waiting for any reply — so starting a statement never depends on
/// the server having a free worker (the replies are read afterwards).
async fn start_statement(addr: std::net::SocketAddr, query: String) -> TcpStream {
    let mut client = TcpStream::connect(addr).await.expect("connect");
    let mut bytes = MAGIC_PREAMBLE.to_vec();
    bytes.extend_from_slice(&[0, 0, 4, 4]);
    bytes.extend_from_slice(&[0; 12]);
    bytes.extend(frame(HELLO, vec![Value::Dictionary(BTreeMap::new())]));
    bytes.extend(frame(
        RUN,
        vec![
            Value::String(query),
            Value::Dictionary(BTreeMap::new()),
            Value::Dictionary(BTreeMap::new()),
        ],
    ));
    let mut pull = BTreeMap::new();
    pull.insert("n".to_string(), Value::Integer(-1));
    bytes.extend(frame(PULL, vec![Value::Dictionary(pull)]));
    client.write_all(&bytes).await.expect("pipeline");
    client
}

/// Read a pipelined statement's replies: the negotiated version, HELLO's
/// SUCCESS, RUN's SUCCESS, the record and PULL's terminal SUCCESS.
async fn finish_statement(client: &mut TcpStream) {
    let mut version = [0u8; 4];
    client.read_exact(&mut version).await.expect("version");
    assert_eq!(version, [0, 0, 4, 4]);
    let mut tags = Vec::new();
    for _ in 0..4 {
        tags.push(tag_of(&read_one(client).await));
    }
    assert_eq!(
        tags,
        vec![SUCCESS, SUCCESS, 0x71, SUCCESS],
        "statement streamed normally"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_stays_live_while_more_slow_bolt_statements_than_workers_run() {
    let service = Arc::new(NativeService::in_memory());
    let (n, one_statement) = calibrate(&service);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server_service = Arc::clone(&service);
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let svc = Arc::clone(&server_service);
            tokio::spawn(async move {
                let _ = accept_and_run_session_durable(socket, &svc).await;
            });
        }
    });

    let started = Instant::now();
    let mut clients = Vec::new();
    for _ in 0..3 {
        clients.push(start_statement(addr, slow_query(n)).await);
    }
    tokio::time::sleep(Duration::from_millis(200)).await; // let them occupy the runtime

    // An unrelated task must still get a worker promptly. Measured from the
    // start: a starved runtime also stalls the timer and I/O this test body
    // waits on above, not just the probe.
    let probe = timeout(
        Duration::from_secs(5),
        tokio::spawn(async { Instant::now() }),
    )
    .await
    .expect("runtime starved: no worker ran a trivial task while Bolt statements ran")
    .expect("probe task");
    // Each statement alone takes `one_statement` (three at once only take
    // longer), so a probe that ran sooner than that after they started ran
    // while all of them were still busy — the check is not vacuous.
    let budget = Duration::from_secs(2).min(one_statement);
    assert!(
        probe - started < budget,
        "runtime starved: a trivial task ran {:?} after the slow statements started \
         (budget {budget:?})",
        probe - started
    );

    for mut client in clients {
        finish_statement(&mut client).await;
    }
}
