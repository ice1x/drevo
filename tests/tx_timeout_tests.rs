//! Per-transaction and idle timeouts.
//!
//! A client sets a time limit for its statement or transaction: Bolt
//! `tx_timeout` (milliseconds) on `BEGIN` or an autocommit `RUN`, HTTP
//! `timeout_ms` on `POST /cypher`. Past it the statement stops and an
//! explicit transaction is rolled back, with Neo4j's
//! `Neo.ClientError.Transaction.TransactionTimedOutClientConfiguration`. The
//! server's statement timeout caps the client's value.
//!
//! `DREVO_IDLE_SESSION_TIMEOUT_MS` closes a Bolt connection that holds an
//! open transaction and sends nothing, and rolls the transaction back.
//!
//! The scenario is an IT task manager: a team lead's report runs long, and a
//! script that opened a transaction to file tickets hangs half-way.

#![cfg(all(not(target_arch = "wasm32"), feature = "http"))]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use drevo::bolt::packstream::Value as BoltValue;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session, State};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

const CLIENT_TIMEOUT: &str = "Neo.ClientError.Transaction.TransactionTimedOutClientConfiguration";
const SERVER_TIMEOUT: &str = "Neo.ClientError.Transaction.TransactionTimedOut";

/// CPU-bound, constant-memory statement: `n²` additions via nested `reduce`.
fn slow_report(n: u32) -> String {
    format!(
        "RETURN reduce(s = 0, i IN range(1, {n}) | s + reduce(t = 0, j IN range(1, {n}) | t + j)) AS effort"
    )
}

/// Grow `n` until the report takes at least 800 ms here (no timeout set).
fn calibrate(svc: &NativeService) -> u32 {
    let mut n = 400;
    loop {
        let t = Instant::now();
        svc.execute(&parse(&slow_report(n)).unwrap(), Default::default())
            .expect("report");
        if t.elapsed() >= Duration::from_millis(800) || n >= 20_000 {
            return n;
        }
        n *= 2;
    }
}

fn tickets(svc: &NativeService) -> i64 {
    let r = svc
        .execute(
            &parse("MATCH (t:Ticket) RETURN count(t) AS c").unwrap(),
            Default::default(),
        )
        .unwrap();
    match &r.rows[0][0] {
        drevo::cypher::executor::Value::Integer(c) => *c,
        other => panic!("count: {other:?}"),
    }
}

fn open_transactions(svc: &NativeService) -> usize {
    svc.transactions()
        .list()
        .into_iter()
        .filter(|t| t.explicit)
        .count()
}

fn timeout_extra(ms: i64) -> BTreeMap<String, BoltValue> {
    BTreeMap::from([("tx_timeout".to_string(), BoltValue::Integer(ms))])
}

fn session(svc: &Arc<NativeService>) -> Session<'static> {
    let mut s = Session::new_durable(Arc::clone(svc));
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });
    s
}

fn run(s: &mut Session<'_>, query: &str, extra: BTreeMap<String, BoltValue>) -> ServerMessage {
    let mut replies = s.handle(ClientMessage::Run {
        query: query.to_string(),
        parameters: BTreeMap::new(),
        extra,
    });
    assert_eq!(replies.len(), 1, "{replies:?}");
    let reply = replies.remove(0);
    if matches!(reply, ServerMessage::Success { .. }) {
        let pulled = s.handle(ClientMessage::Pull {
            extra: BTreeMap::from([("n".to_string(), BoltValue::Integer(-1))]),
        });
        assert!(
            matches!(pulled.last(), Some(ServerMessage::Success { .. })),
            "{pulled:?}"
        );
    }
    reply
}

fn begin(s: &mut Session<'_>, extra: BTreeMap<String, BoltValue>) -> ServerMessage {
    let mut replies = s.handle(ClientMessage::Begin { extra });
    replies.remove(0)
}

fn one(mut replies: Vec<ServerMessage>) -> ServerMessage {
    assert_eq!(replies.len(), 1, "{replies:?}");
    replies.remove(0)
}

#[track_caller]
fn assert_success(reply: &ServerMessage) {
    assert!(matches!(reply, ServerMessage::Success { .. }), "{reply:?}");
}

#[track_caller]
fn assert_failure(reply: &ServerMessage, code: &str) -> String {
    match reply {
        ServerMessage::Failure { metadata } => {
            assert_eq!(
                metadata.get("code"),
                Some(&BoltValue::String(code.to_string())),
                "{metadata:?}"
            );
            match metadata.get("message") {
                Some(BoltValue::String(m)) => m.clone(),
                other => panic!("message: {other:?}"),
            }
        }
        other => panic!("expected FAILURE {code}, got {other:?}"),
    }
}

#[test]
fn an_autocommit_run_stops_at_its_tx_timeout() {
    let svc = Arc::new(NativeService::in_memory());
    let n = calibrate(&svc);
    let mut s = session(&svc);

    let started = Instant::now();
    let reply = run(&mut s, &slow_report(n), timeout_extra(100));
    let message = assert_failure(&reply, CLIENT_TIMEOUT);
    assert!(message.contains("100 ms"), "{message}");
    assert!(
        started.elapsed() < Duration::from_millis(700),
        "stopped after {:?}",
        started.elapsed()
    );

    // The limit was this statement's: the next one runs to the end.
    assert_success(&one(s.handle(ClientMessage::Reset)));
    assert_success(&run(&mut s, &slow_report(n), BTreeMap::new()));
}

#[test]
fn the_servers_statement_timeout_caps_a_longer_client_timeout() {
    let svc = Arc::new(NativeService::in_memory());
    let n = calibrate(&svc);
    svc.set_statement_timeout(Some(Duration::from_millis(100)));
    let mut s = session(&svc);

    let started = Instant::now();
    let reply = run(&mut s, &slow_report(n), timeout_extra(60_000));
    assert_failure(&reply, SERVER_TIMEOUT);
    assert!(started.elapsed() < Duration::from_millis(700));

    // A client limit under the server's is the client's.
    svc.set_statement_timeout(Some(Duration::from_secs(60)));
    assert_success(&one(s.handle(ClientMessage::Reset)));
    let reply = run(&mut s, &slow_report(n), timeout_extra(100));
    assert_failure(&reply, CLIENT_TIMEOUT);
}

#[test]
fn a_zero_tx_timeout_is_no_limit_and_a_bad_one_is_refused() {
    let svc = Arc::new(NativeService::in_memory());
    let mut s = session(&svc);
    assert_success(&run(
        &mut s,
        "CREATE (:Ticket {title: 'printer'})",
        timeout_extra(0),
    ));
    assert_eq!(tickets(&svc), 1);

    let reply = run(&mut s, "RETURN 1", timeout_extra(-5));
    assert_failure(&reply, "Neo.ClientError.Request.Invalid");
    assert_success(&one(s.handle(ClientMessage::Reset)));
    let reply = begin(
        &mut s,
        BTreeMap::from([(
            "tx_timeout".to_string(),
            BoltValue::String("soon".to_string()),
        )]),
    );
    assert_failure(&reply, "Neo.ClientError.Request.Invalid");
    assert_eq!(open_transactions(&svc), 0);
}

#[test]
fn an_explicit_transaction_past_its_tx_timeout_is_rolled_back() {
    let svc = Arc::new(NativeService::in_memory());
    let mut s = session(&svc);

    assert_success(&begin(&mut s, timeout_extra(100)));
    assert_success(&run(
        &mut s,
        "CREATE (:Ticket {title: 'vpn'})",
        BTreeMap::new(),
    ));
    std::thread::sleep(Duration::from_millis(150));

    // The next statement finds the transaction gone.
    let reply = run(&mut s, "CREATE (:Ticket {title: 'mail'})", BTreeMap::new());
    let message = assert_failure(&reply, CLIENT_TIMEOUT);
    assert!(message.contains("100 ms"), "{message}");
    assert_eq!(open_transactions(&svc), 0, "rolled back at once");
    assert_eq!(s.state(), State::Failed);
    assert_success(&one(s.handle(ClientMessage::Reset)));
    assert_eq!(s.state(), State::Ready);
    assert_eq!(tickets(&svc), 0);

    // Its COMMIT fails the same way, and nothing lands.
    assert_success(&begin(&mut s, timeout_extra(100)));
    assert_success(&run(
        &mut s,
        "CREATE (:Ticket {title: 'vpn'})",
        BTreeMap::new(),
    ));
    std::thread::sleep(Duration::from_millis(150));
    assert_failure(&one(s.handle(ClientMessage::Commit)), CLIENT_TIMEOUT);
    assert_success(&one(s.handle(ClientMessage::Reset)));
    assert_eq!(tickets(&svc), 0);
    assert_eq!(open_transactions(&svc), 0);

    // Within the limit it commits as usual.
    assert_success(&begin(&mut s, timeout_extra(10_000)));
    assert_success(&run(
        &mut s,
        "CREATE (:Ticket {title: 'vpn'})",
        BTreeMap::new(),
    ));
    assert_success(&one(s.handle(ClientMessage::Commit)));
    assert_eq!(tickets(&svc), 1);
}

#[test]
fn a_slow_statement_inside_a_transaction_stops_at_the_transactions_deadline() {
    let svc = Arc::new(NativeService::in_memory());
    let n = calibrate(&svc);
    let mut s = session(&svc);

    assert_success(&begin(&mut s, timeout_extra(150)));
    assert_success(&run(
        &mut s,
        "CREATE (:Ticket {title: 'disk'})",
        BTreeMap::new(),
    ));
    let started = Instant::now();
    let reply = run(&mut s, &slow_report(n), BTreeMap::new());
    assert_failure(&reply, CLIENT_TIMEOUT);
    assert!(started.elapsed() < Duration::from_millis(700));
    assert_eq!(open_transactions(&svc), 0, "rolled back with the statement");
    assert_success(&one(s.handle(ClientMessage::Reset)));
    assert_eq!(tickets(&svc), 0);
}

#[test]
fn a_silent_transaction_past_its_tx_timeout_is_rolled_back_without_the_client() {
    let svc = Arc::new(NativeService::in_memory());
    let mut s = session(&svc);
    assert_eq!(s.wake_in(), None, "no transaction, nothing to watch");

    assert_success(&begin(&mut s, timeout_extra(80)));
    assert_success(&run(
        &mut s,
        "CREATE (:Ticket {title: 'vpn'})",
        BTreeMap::new(),
    ));
    let wait = s.wake_in().expect("the deadline is watched");
    assert!(wait <= Duration::from_millis(80), "{wait:?}");
    assert!(s.on_quiet(), "not yet");
    assert_eq!(open_transactions(&svc), 1);

    std::thread::sleep(wait + Duration::from_millis(10));
    assert!(s.on_quiet(), "the connection stays open");
    assert_eq!(open_transactions(&svc), 0, "the snapshot is released");
    assert_eq!(s.wake_in(), None);

    // The client learns at its next message.
    assert_failure(&one(s.handle(ClientMessage::Commit)), CLIENT_TIMEOUT);
    assert_success(&one(s.handle(ClientMessage::Reset)));
    assert_eq!(tickets(&svc), 0);

    // A ROLLBACK of such a transaction just succeeds.
    assert_success(&begin(&mut s, timeout_extra(30)));
    std::thread::sleep(Duration::from_millis(40));
    assert!(s.on_quiet());
    assert_success(&one(s.handle(ClientMessage::Rollback)));
    assert_eq!(s.state(), State::Ready);
}

#[test]
fn an_idle_connection_holding_a_transaction_is_closed_and_rolled_back() {
    let svc = Arc::new(NativeService::in_memory());
    let mut s = session(&svc).with_idle_timeout(Some(Duration::from_millis(80)));
    assert_eq!(s.wake_in(), None, "idle without a transaction is fine");

    assert_success(&begin(&mut s, BTreeMap::new()));
    assert_success(&run(
        &mut s,
        "CREATE (:Ticket {title: 'vpn'})",
        BTreeMap::new(),
    ));
    let wait = s.wake_in().expect("idle limit watched");
    assert!(wait <= Duration::from_millis(80));
    std::thread::sleep(wait + Duration::from_millis(10));

    assert!(!s.on_quiet(), "the connection is closed");
    assert_eq!(s.state(), State::Defunct);
    assert_eq!(open_transactions(&svc), 0);
    assert_eq!(tickets(&svc), 0);
}

#[test]
fn a_busy_transaction_is_not_idle() {
    let svc = Arc::new(NativeService::in_memory());
    let mut s = session(&svc).with_idle_timeout(Some(Duration::from_millis(100)));

    assert_success(&begin(&mut s, BTreeMap::new()));
    for i in 0..5 {
        std::thread::sleep(Duration::from_millis(40));
        assert!(s.on_quiet());
        let q = format!("CREATE (:Ticket {{title: 'ticket {i}'}})");
        assert_success(&run(&mut s, &q, BTreeMap::new()));
    }
    assert_success(&one(s.handle(ClientMessage::Commit)));
    assert_eq!(tickets(&svc), 5);
}

mod over_tcp {
    use super::*;

    use drevo::bolt::handshake::MAGIC_PREAMBLE;
    use drevo::bolt::listener::accept_and_run_session_durable_with_security;
    use drevo::bolt::packstream::{decode, encode};
    use drevo::bolt::session::{BEGIN, COMMIT, GOODBYE, HELLO, PULL, RESET, RUN};
    use drevo::database_registry::DatabaseRegistry;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::timeout;

    fn frame(tag: u8, fields: Vec<BoltValue>) -> Vec<u8> {
        let mut buf = Vec::new();
        encode(&BoltValue::Structure { tag, fields }, &mut buf).unwrap();
        let mut framed = (buf.len() as u16).to_be_bytes().to_vec();
        framed.extend_from_slice(&buf);
        framed.extend_from_slice(&[0x00, 0x00]);
        framed
    }

    fn dict(map: BTreeMap<String, BoltValue>) -> BoltValue {
        BoltValue::Dictionary(map)
    }

    fn run_msg(query: &str) -> Vec<u8> {
        frame(
            RUN,
            vec![
                BoltValue::String(query.to_string()),
                dict(BTreeMap::new()),
                dict(BTreeMap::new()),
            ],
        )
    }

    fn pull_msg() -> Vec<u8> {
        frame(
            PULL,
            vec![dict(BTreeMap::from([(
                "n".to_string(),
                BoltValue::Integer(-1),
            )]))],
        )
    }

    /// One message, or `None` at end of stream.
    async fn read_one(client: &mut TcpStream) -> Option<BoltValue> {
        let mut payload = Vec::new();
        loop {
            let mut len = [0u8; 2];
            if client.read_exact(&mut len).await.is_err() {
                return None;
            }
            let l = u16::from_be_bytes(len) as usize;
            if l == 0 {
                break;
            }
            let start = payload.len();
            payload.resize(start + l, 0);
            client.read_exact(&mut payload[start..]).await.ok()?;
        }
        Some(decode(&payload).unwrap().0)
    }

    async fn tags(client: &mut TcpStream, count: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..count {
            match timeout(Duration::from_secs(5), read_one(client)).await {
                Ok(Some(BoltValue::Structure { tag, .. })) => out.push(tag),
                other => panic!("expected a message, got {other:?}"),
            }
        }
        out
    }

    async fn connect(
        registry: Arc<DatabaseRegistry>,
    ) -> (
        TcpStream,
        tokio::task::JoinHandle<drevo::bolt::error::BoltResult<()>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            accept_and_run_session_durable_with_security(
                socket,
                &registry,
                drevo::security::Security::default(),
            )
            .await
        });
        let mut client = TcpStream::connect(addr).await.unwrap();
        let mut handshake = MAGIC_PREAMBLE.to_vec();
        handshake.extend_from_slice(&[0, 0, 4, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        client.write_all(&handshake).await.unwrap();
        let mut version = [0u8; 4];
        client.read_exact(&mut version).await.unwrap();
        assert_eq!(version, [0, 0, 4, 4]);
        client
            .write_all(&frame(HELLO, vec![dict(BTreeMap::new())]))
            .await
            .unwrap();
        assert_eq!(tags(&mut client, 1).await, vec![0x70]);
        (client, server)
    }

    const SUCCESS: u8 = 0x70;
    const FAILURE: u8 = 0x7F;

    #[tokio::test]
    async fn the_server_closes_an_idle_connection_and_rolls_its_transaction_back() {
        let svc = Arc::new(NativeService::in_memory());
        let registry = Arc::new(DatabaseRegistry::new(Arc::clone(&svc)));
        registry.set_idle_session_timeout(Some(Duration::from_millis(150)));
        assert_eq!(
            registry.idle_session_timeout(),
            Some(Duration::from_millis(150))
        );
        let (mut client, server) = connect(registry).await;

        // A script opens a transaction, files a ticket, then hangs.
        let mut batch = frame(BEGIN, vec![dict(BTreeMap::new())]);
        batch.extend(run_msg("CREATE (:Ticket {title: 'vpn'})"));
        batch.extend(pull_msg());
        client.write_all(&batch).await.unwrap();
        assert_eq!(tags(&mut client, 3).await, vec![SUCCESS; 3]);
        assert_eq!(open_transactions(&svc), 1);

        let closed = timeout(Duration::from_secs(5), read_one(&mut client))
            .await
            .expect("closed within the limit");
        assert!(closed.is_none(), "{closed:?}");
        timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(open_transactions(&svc), 0);
        assert_eq!(tickets(&svc), 0);
    }

    #[tokio::test]
    async fn a_connection_without_a_transaction_may_stay_idle() {
        let svc = Arc::new(NativeService::in_memory());
        let registry = Arc::new(DatabaseRegistry::new(Arc::clone(&svc)));
        registry.set_idle_session_timeout(Some(Duration::from_millis(50)));
        let (mut client, server) = connect(registry).await;

        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut batch = run_msg("CREATE (:Ticket {title: 'printer'})");
        batch.extend(pull_msg());
        batch.extend(frame(GOODBYE, vec![]));
        client.write_all(&batch).await.unwrap();
        assert_eq!(tags(&mut client, 2).await, vec![SUCCESS; 2]);
        timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(tickets(&svc), 1);
    }

    #[tokio::test]
    async fn a_transaction_past_its_tx_timeout_is_rolled_back_while_the_connection_stays() {
        let svc = Arc::new(NativeService::in_memory());
        let registry = Arc::new(DatabaseRegistry::new(Arc::clone(&svc)));
        let (mut client, server) = connect(registry).await;

        let mut batch = frame(BEGIN, vec![dict(timeout_extra(100))]);
        batch.extend(run_msg("CREATE (:Ticket {title: 'vpn'})"));
        batch.extend(pull_msg());
        client.write_all(&batch).await.unwrap();
        assert_eq!(tags(&mut client, 3).await, vec![SUCCESS; 3]);

        // Rolled back while the client says nothing.
        let started = Instant::now();
        while open_transactions(&svc) > 0 {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "not rolled back"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // The connection is still there, and the COMMIT reports the timeout.
        let mut batch = frame(COMMIT, vec![]);
        batch.extend(frame(RESET, vec![]));
        batch.extend(run_msg("MATCH (t:Ticket) RETURN count(t) AS c"));
        batch.extend(pull_msg());
        batch.extend(frame(GOODBYE, vec![]));
        client.write_all(&batch).await.unwrap();
        let replies = tags(&mut client, 5).await;
        assert_eq!(replies, vec![FAILURE, SUCCESS, SUCCESS, 0x71, SUCCESS]);
        timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(tickets(&svc), 0);
    }
}

mod http {
    use super::*;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use drevo::native_api::{build_native_router, NativeApiState};
    use http_body_util::BodyExt;
    use serde_json::json;
    use tower::ServiceExt;

    async fn post(app: &axum::Router, body: serde_json::Value) -> (StatusCode, String) {
        let req = Request::builder()
            .method("POST")
            .uri("/cypher")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    #[tokio::test]
    async fn timeout_ms_stops_a_slow_cypher_request() {
        let svc = Arc::new(NativeService::in_memory());
        let n = calibrate(&svc);
        let app = build_native_router(NativeApiState::new(Arc::clone(&svc)));

        let started = Instant::now();
        let (status, body) = post(&app, json!({"query": slow_report(n), "timeout_ms": 100})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("100 ms timeout the client set"), "{body}");
        assert!(started.elapsed() < Duration::from_millis(700));

        // `0` is no limit; fast requests are untouched.
        let (status, body) = post(
            &app,
            json!({"query": "CREATE (:Ticket {title: 'vpn'}) RETURN 1 AS ok", "timeout_ms": 0}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = post(
            &app,
            json!({"query": "MATCH (t:Ticket) RETURN count(t) AS c", "timeout_ms": 5000}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(tickets(&svc), 1);
    }

    #[tokio::test]
    async fn the_server_limit_caps_timeout_ms() {
        let svc = Arc::new(NativeService::in_memory());
        let n = calibrate(&svc);
        svc.set_statement_timeout(Some(Duration::from_millis(100)));
        let app = build_native_router(NativeApiState::new(Arc::clone(&svc)));

        let (status, body) =
            post(&app, json!({"query": slow_report(n), "timeout_ms": 60_000})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("100 ms statement timeout"), "{body}");
    }
}

mod config {
    use std::time::Duration;

    use drevo::server::Config;

    fn cfg(value: Option<&str>) -> Result<Config, drevo::server::ConfigError> {
        Config::from_env(|key| match key {
            "DREVO_IDLE_SESSION_TIMEOUT_MS" => value.map(str::to_string),
            _ => None,
        })
    }

    #[test]
    fn idle_session_timeout_env_var_parses() {
        assert_eq!(cfg(None).unwrap().idle_session_timeout, None);
        assert_eq!(cfg(Some("0")).unwrap().idle_session_timeout, None);
        assert_eq!(
            cfg(Some("30000")).unwrap().idle_session_timeout,
            Some(Duration::from_secs(30))
        );
        for bad in ["soon", "-1"] {
            let err = cfg(Some(bad)).unwrap_err();
            assert!(
                err.to_string().contains("DREVO_IDLE_SESSION_TIMEOUT_MS"),
                "{err}"
            );
        }
    }
}
