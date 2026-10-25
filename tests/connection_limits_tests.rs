//! Connection limits (issue #750): a server refuses Bolt connections beyond
//! `DREVO_MAX_CONNECTIONS` and HTTP requests beyond `DREVO_HTTP_MAX_IN_FLIGHT`
//! with a retryable error, keeps `/health` and `/metrics` answering when it is
//! full, and counts what it refused.

#![cfg(feature = "http")]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::bolt::handshake::MAGIC_PREAMBLE;
use drevo::bolt::packstream::{decode, encode, Value as BoltValue};
use drevo::bolt::session::HELLO;
use drevo::database_registry::DatabaseRegistry;
use drevo::limits::{DEFAULT_HTTP_MAX_IN_FLIGHT, DEFAULT_MAX_CONNECTIONS};
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use drevo::server::{Config, ConfigError};
use http_body_util::BodyExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tower::ServiceExt;

const SUCCESS: u8 = 0x70;
const FAILURE: u8 = 0x7F;
const NO_THREADS: &str = "Neo.TransientError.Request.NoThreadsAvailable";

fn cfg(key: &'static str, value: Option<&str>) -> Result<Config, ConfigError> {
    let value = value.map(str::to_string);
    Config::from_env(move |k| if k == key { value.clone() } else { None })
}

#[test]
fn the_limits_come_from_the_environment() {
    let c = cfg("DREVO_MAX_CONNECTIONS", None).unwrap();
    assert_eq!(c.max_connections, DEFAULT_MAX_CONNECTIONS);
    assert_eq!(c.http_max_in_flight, DEFAULT_HTTP_MAX_IN_FLIGHT);
    assert_eq!(
        cfg("DREVO_MAX_CONNECTIONS", Some("40"))
            .unwrap()
            .max_connections,
        40
    );
    assert_eq!(
        cfg("DREVO_MAX_CONNECTIONS", Some("0"))
            .unwrap()
            .max_connections,
        0,
        "0 means no limit"
    );
    assert_eq!(
        cfg("DREVO_HTTP_MAX_IN_FLIGHT", Some("8"))
            .unwrap()
            .http_max_in_flight,
        8
    );
    for key in ["DREVO_MAX_CONNECTIONS", "DREVO_HTTP_MAX_IN_FLIGHT"] {
        for bad in ["lots", "-1"] {
            let err = cfg(key, Some(bad)).unwrap_err();
            assert!(err.to_string().contains(key), "{key}={bad}: {err}");
        }
    }
}

// ── Bolt, end to end through the real server ───────────────────────────

fn frame(tag: u8, fields: Vec<BoltValue>) -> Vec<u8> {
    let mut buf = Vec::new();
    encode(&BoltValue::Structure { tag, fields }, &mut buf).unwrap();
    let mut framed = (buf.len() as u16).to_be_bytes().to_vec();
    framed.extend_from_slice(&buf);
    framed.extend_from_slice(&[0x00, 0x00]);
    framed
}

async fn read_one(client: &mut TcpStream) -> Option<BoltValue> {
    let mut payload = Vec::new();
    loop {
        let mut len = [0u8; 2];
        client.read_exact(&mut len).await.ok()?;
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

/// Connect, shake hands, say HELLO; the reply's tag and metadata.
async fn hello(addr: std::net::SocketAddr) -> (TcpStream, u8, BTreeMap<String, BoltValue>) {
    let mut client = TcpStream::connect(addr).await.unwrap();
    let mut handshake = MAGIC_PREAMBLE.to_vec();
    handshake.extend_from_slice(&[0, 0, 4, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    client.write_all(&handshake).await.unwrap();
    let mut version = [0u8; 4];
    client.read_exact(&mut version).await.unwrap();
    assert_eq!(
        version,
        [0, 0, 4, 4],
        "the handshake completes even when full"
    );
    let mut extra = BTreeMap::new();
    extra.insert(
        "user_agent".to_string(),
        BoltValue::String("limits-test/1".into()),
    );
    client
        .write_all(&frame(HELLO, vec![BoltValue::Dictionary(extra)]))
        .await
        .unwrap();
    match timeout(Duration::from_secs(5), read_one(&mut client)).await {
        Ok(Some(BoltValue::Structure { tag, fields })) => {
            let md = match fields.into_iter().next() {
                Some(BoltValue::Dictionary(md)) => md,
                _ => BTreeMap::new(),
            };
            (client, tag, md)
        }
        other => panic!("expected a reply to HELLO, got {other:?}"),
    }
}

async fn http_get(addr: std::net::SocketAddr, path: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).await.unwrap();
    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, raw)
}

fn sample(metrics: &str, name: &str, protocol: &str) -> f64 {
    let needle = format!("{name}{{protocol=\"{protocol}\"}}");
    metrics
        .lines()
        .find(|l| l.starts_with(&needle))
        .unwrap_or_else(|| panic!("{needle} missing from:\n{metrics}"))
        .rsplit(' ')
        .next()
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn bolt_connections_beyond_the_limit_are_refused_with_a_retryable_error() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().to_string_lossy().to_string();
    let mut cfg = Config::from_env(move |k| match k {
        "DREVO_HOST" => Some("127.0.0.1".to_string()),
        "DREVO_DATA_DIR" => Some(data_dir.clone()),
        "DREVO_AUTH" => Some("none".to_string()),
        "DREVO_MAX_CONNECTIONS" => Some("2".to_string()),
        _ => None,
    })
    .unwrap();
    cfg.port = 0;
    cfg.bolt_port = Some(0);
    let (listening, announced) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let _ = drevo::server::run_announcing(cfg, listening).await;
    });
    let addrs = timeout(Duration::from_secs(30), announced)
        .await
        .expect("the server started in time")
        .expect("the server did not start");
    let bolt = addrs.bolt.expect("bolt is served");

    let (first, tag, _) = hello(bolt).await;
    assert_eq!(tag, SUCCESS);
    let (_second, tag, _) = hello(bolt).await;
    assert_eq!(tag, SUCCESS);

    // The third is over the limit: refused at HELLO, then closed.
    let (mut third, tag, md) = hello(bolt).await;
    assert_eq!(tag, FAILURE);
    assert_eq!(
        md.get("code"),
        Some(&BoltValue::String(NO_THREADS.to_string())),
        "{md:?}"
    );
    let message = match md.get("message") {
        Some(BoltValue::String(m)) => m.clone(),
        other => panic!("no message: {other:?}"),
    };
    assert!(
        message.contains("DREVO_MAX_CONNECTIONS") && message.contains('2'),
        "the message names the limit: {message}"
    );
    assert!(
        timeout(Duration::from_secs(5), read_one(&mut third))
            .await
            .expect("the refused connection is closed")
            .is_none(),
        "nothing follows the refusal"
    );

    // Monitoring still works while Bolt is full.
    let (status, metrics) = http_get(addrs.http, "/metrics").await;
    assert_eq!(status, 200);
    assert_eq!(
        sample(&metrics, "drevo_connections_rejected_total", "bolt"),
        1.0
    );
    assert_eq!(sample(&metrics, "drevo_connections_active", "bolt"), 2.0);

    // A slot frees up when a client leaves.
    drop(first);
    let mut admitted = false;
    for _ in 0..50 {
        let (_c, tag, _) = hello(bolt).await;
        if tag == SUCCESS {
            admitted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(admitted, "a new connection is admitted once one closed");
    server.abort();
}

// ── HTTP, through the router ───────────────────────────────────────────

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Option<String>, String) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let retry = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, retry, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn http_requests_beyond_the_limit_get_503_but_health_and_metrics_answer() {
    let service = Arc::new(NativeService::in_memory());
    let registry = Arc::new(DatabaseRegistry::new(Arc::clone(&service)));
    registry.limits().http.set_max(1);
    let app = build_native_router(NativeApiState::with_registry(
        Arc::clone(&service),
        Arc::clone(&registry),
    ));

    // Within the limit: served.
    let (status, _, _) = get(&app, "/status").await;
    assert_eq!(status, StatusCode::OK);

    // Someone holds the only slot.
    let held = registry.limits().http.try_enter().expect("a free slot");
    let (status, retry, body) = get(&app, "/status").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        retry.as_deref(),
        Some("1"),
        "clients are told when to retry"
    );
    assert!(body.contains("DREVO_HTTP_MAX_IN_FLIGHT"), "{body}");

    let (status, _, _) = get(&app, "/health").await;
    assert_eq!(status, StatusCode::OK, "health is never refused");
    let (status, _, metrics) = get(&app, "/metrics").await;
    assert_eq!(status, StatusCode::OK, "metrics are never refused");
    assert_eq!(
        sample(&metrics, "drevo_connections_rejected_total", "http"),
        1.0
    );
    assert_eq!(sample(&metrics, "drevo_connections_active", "http"), 1.0);

    drop(held);
    let (status, _, _) = get(&app, "/status").await;
    assert_eq!(status, StatusCode::OK, "served again once the slot is free");
}

#[tokio::test]
async fn no_limit_means_every_request_is_served() {
    let service = Arc::new(NativeService::in_memory());
    let registry = Arc::new(DatabaseRegistry::new(Arc::clone(&service)));
    registry.limits().http.set_max(0);
    let app = build_native_router(NativeApiState::with_registry(
        Arc::clone(&service),
        Arc::clone(&registry),
    ));
    let _held: Vec<_> = (0..5)
        .map(|_| registry.limits().http.try_enter().expect("unlimited"))
        .collect();
    let (status, _, _) = get(&app, "/status").await;
    assert_eq!(status, StatusCode::OK);
}
