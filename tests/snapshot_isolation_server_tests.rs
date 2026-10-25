//! Snapshot isolation on the server (issue #749): `DREVO_TX_ISOLATION` sets
//! the default for every database, Bolt `BEGIN` can ask for one in
//! `tx_metadata` (`drevo.isolation`), explicit transactions on different
//! nodes both commit over the wire, and merged commits are counted on
//! `/metrics`.

#![cfg(feature = "http")]

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use drevo::bolt::handshake::MAGIC_PREAMBLE;
use drevo::bolt::listener::accept_and_run_session_durable_with_security;
use drevo::bolt::packstream::{decode, encode, Value as BoltValue};
use drevo::bolt::session::{BEGIN, COMMIT, HELLO, PULL, RUN};
use drevo::database_registry::DatabaseRegistry;
use drevo::native::Isolation;
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use drevo::server::{Config, ConfigError};
use http_body_util::BodyExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tower::ServiceExt;

fn cfg(value: Option<&str>) -> Result<Config, ConfigError> {
    let value = value.map(str::to_string);
    Config::from_env(move |k| {
        if k == "DREVO_TX_ISOLATION" {
            value.clone()
        } else {
            None
        }
    })
}

#[test]
fn the_default_isolation_comes_from_the_environment() {
    assert_eq!(cfg(None).unwrap().tx_isolation, Isolation::Snapshot);
    assert_eq!(
        cfg(Some("snapshot")).unwrap().tx_isolation,
        Isolation::Snapshot
    );
    assert_eq!(
        cfg(Some("Serializable")).unwrap().tx_isolation,
        Isolation::Serializable
    );
    let err = cfg(Some("read-committed")).unwrap_err();
    assert!(err.to_string().contains("DREVO_TX_ISOLATION"), "{err}");
}

#[test]
fn every_database_follows_the_server_settings_including_new_ones() {
    let registry = DatabaseRegistry::new(Arc::new(NativeService::in_memory()));
    registry.set_isolation(Isolation::Serializable);
    registry.set_conflict_history(33);
    let fresh = registry.create_in_memory("fresh").unwrap();
    for svc in [registry.default_service(), fresh] {
        assert_eq!(svc.graph().isolation(), Isolation::Serializable);
        assert_eq!(svc.graph().conflict_history_capacity(), 33);
    }
}

// ── Bolt over TCP ──────────────────────────────────────────────────────

fn frame(tag: u8, fields: Vec<BoltValue>) -> Vec<u8> {
    let mut buf = Vec::new();
    encode(&BoltValue::Structure { tag, fields }, &mut buf).unwrap();
    let mut framed = (buf.len() as u16).to_be_bytes().to_vec();
    framed.extend_from_slice(&buf);
    framed.extend_from_slice(&[0x00, 0x00]);
    framed
}

fn dict(entries: Vec<(&str, BoltValue)>) -> BoltValue {
    BoltValue::Dictionary(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
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

/// The tag and the `code` (for a FAILURE) of the next reply.
async fn reply(client: &mut TcpStream) -> (u8, Option<String>) {
    match timeout(Duration::from_secs(5), read_one(client)).await {
        Ok(Some(BoltValue::Structure { tag, fields })) => {
            let code = match fields.first() {
                Some(BoltValue::Dictionary(md)) => match md.get("code") {
                    Some(BoltValue::String(c)) => Some(c.clone()),
                    _ => None,
                },
                _ => None,
            };
            (tag, code)
        }
        other => panic!("expected a reply, got {other:?}"),
    }
}

const SUCCESS: u8 = 0x70;
const FAILURE: u8 = 0x7F;

async fn connect(registry: &Arc<DatabaseRegistry>) -> TcpStream {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let registry = Arc::clone(registry);
    tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let _ = accept_and_run_session_durable_with_security(
            socket,
            &registry,
            drevo::security::Security::default(),
        )
        .await;
    });
    let mut client = TcpStream::connect(addr).await.unwrap();
    let mut handshake = MAGIC_PREAMBLE.to_vec();
    handshake.extend_from_slice(&[0, 0, 4, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    client.write_all(&handshake).await.unwrap();
    let mut version = [0u8; 4];
    client.read_exact(&mut version).await.unwrap();
    client
        .write_all(&frame(HELLO, vec![dict(vec![])]))
        .await
        .unwrap();
    assert_eq!(reply(&mut client).await.0, SUCCESS);
    client
}

async fn send(client: &mut TcpStream, bytes: Vec<u8>) {
    client.write_all(&bytes).await.unwrap();
}

async fn begin(client: &mut TcpStream, isolation: Option<&str>) -> (u8, Option<String>) {
    let extra = match isolation {
        Some(level) => dict(vec![(
            "tx_metadata",
            dict(vec![("drevo.isolation", BoltValue::String(level.into()))]),
        )]),
        None => dict(vec![]),
    };
    send(client, frame(BEGIN, vec![extra])).await;
    reply(client).await
}

async fn run(client: &mut TcpStream, query: &str) {
    send(
        client,
        frame(
            RUN,
            vec![BoltValue::String(query.into()), dict(vec![]), dict(vec![])],
        ),
    )
    .await;
    assert_eq!(reply(client).await.0, SUCCESS, "RUN {query}");
    send(
        client,
        frame(PULL, vec![dict(vec![("n", BoltValue::Integer(-1))])]),
    )
    .await;
    assert_eq!(reply(client).await.0, SUCCESS, "PULL {query}");
}

async fn commit(client: &mut TcpStream) -> (u8, Option<String>) {
    send(client, frame(COMMIT, vec![])).await;
    reply(client).await
}

fn setup() -> (Arc<NativeService>, Arc<DatabaseRegistry>) {
    let svc = Arc::new(NativeService::in_memory());
    let registry = Arc::new(DatabaseRegistry::new(Arc::clone(&svc)));
    let q =
        drevo::cypher::parser::parse("CREATE (:Row {id: 1, v: 0}), (:Row {id: 3, v: 0})").unwrap();
    svc.execute(&q, std::collections::HashMap::new()).unwrap();
    (svc, registry)
}

#[tokio::test]
async fn two_bolt_transactions_on_different_nodes_both_commit() {
    let (svc, registry) = setup();
    let mut a = connect(&registry).await;
    let mut b = connect(&registry).await;
    assert_eq!(begin(&mut a, None).await.0, SUCCESS);
    assert_eq!(begin(&mut b, None).await.0, SUCCESS);
    run(&mut a, "MATCH (n:Row {id: 1}) SET n.v = n.v + 1").await;
    run(&mut b, "MATCH (n:Row {id: 3}) SET n.v = n.v + 1").await;
    assert_eq!(commit(&mut b).await, (SUCCESS, None));
    assert_eq!(
        commit(&mut a).await,
        (SUCCESS, None),
        "a was not rolled back"
    );
    let q = drevo::cypher::parser::parse("MATCH (n:Row) RETURN sum(n.v) AS total").unwrap();
    let out = svc.execute(&q, std::collections::HashMap::new()).unwrap();
    assert_eq!(
        format!("{:?}", out.rows),
        format!(
            "{:?}",
            vec![vec![drevo::cypher::executor::Value::Integer(2)]]
        )
    );
    assert_eq!(svc.graph().conflict_stats().merged, 1);
}

#[tokio::test]
async fn a_bolt_transaction_can_ask_for_serializable() {
    let (_svc, registry) = setup();
    let mut a = connect(&registry).await;
    let mut b = connect(&registry).await;
    assert_eq!(begin(&mut a, Some("serializable")).await.0, SUCCESS);
    assert_eq!(begin(&mut b, None).await.0, SUCCESS);
    run(&mut a, "MATCH (n:Row {id: 1}) SET n.v = n.v + 1").await;
    run(&mut b, "MATCH (n:Row {id: 3}) SET n.v = n.v + 1").await;
    assert_eq!(commit(&mut b).await.0, SUCCESS);
    assert_eq!(
        commit(&mut a).await,
        (
            FAILURE,
            Some("Neo.TransientError.Transaction.Outdated".to_string())
        )
    );
}

#[tokio::test]
async fn an_unknown_isolation_is_refused_at_begin() {
    let (_svc, registry) = setup();
    let mut a = connect(&registry).await;
    assert_eq!(
        begin(&mut a, Some("read-uncommitted")).await,
        (FAILURE, Some("Neo.ClientError.Request.Invalid".to_string()))
    );
}

#[tokio::test]
async fn merged_commits_are_counted_on_metrics() {
    let (svc, registry) = setup();
    let mut a = connect(&registry).await;
    let mut b = connect(&registry).await;
    begin(&mut a, None).await;
    begin(&mut b, None).await;
    run(&mut a, "MATCH (n:Row {id: 1}) SET n.v = 5").await;
    run(&mut b, "MATCH (n:Row {id: 3}) SET n.v = 6").await;
    commit(&mut b).await;
    commit(&mut a).await;
    let app = build_native_router(NativeApiState::with_registry(Arc::clone(&svc), registry));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let text = String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    let line = text
        .lines()
        .find(|l| l.starts_with("drevo_tx_merged_commits_total"))
        .unwrap_or_else(|| panic!("merged counter missing:\n{text}"));
    assert!(line.ends_with(" 1"), "{line}");
}
