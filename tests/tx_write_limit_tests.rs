//! The write limit of explicit transactions on the server:
//! `DREVO_MAX_TX_WRITES` caps how many writes one transaction may buffer
//! before it commits (1,000,000 unless set; 0 is no limit). The statement
//! that goes over it fails with Neo4j's
//! `Neo.ClientError.General.TransactionOutOfMemoryError`, the transaction is
//! rolled back, and `SHOW TRANSACTIONS` shows how many writes each open
//! transaction buffers.

#![cfg(feature = "http")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use drevo::bolt::handshake::MAGIC_PREAMBLE;
use drevo::bolt::listener::accept_and_run_session_durable_with_security;
use drevo::bolt::packstream::{decode, encode, Value as BoltValue};
use drevo::bolt::session::{BEGIN, COMMIT, HELLO, PULL, RUN};
use drevo::cypher::executor::{ExecError, Value};
use drevo::cypher::parser::parse;
use drevo::database_registry::DatabaseRegistry;
use drevo::error::DrevoError;
use drevo::native_service::NativeService;
use drevo::server::{Config, ConfigError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

const OUT_OF_MEMORY: &str = "Neo.ClientError.General.TransactionOutOfMemoryError";

fn cfg(value: Option<&str>) -> Result<Config, ConfigError> {
    let value = value.map(str::to_string);
    Config::from_env(move |k| {
        if k == "DREVO_MAX_TX_WRITES" {
            value.clone()
        } else {
            None
        }
    })
}

#[test]
fn the_limit_comes_from_the_environment() {
    assert_eq!(cfg(None).unwrap().max_tx_writes, Some(1_000_000));
    assert_eq!(cfg(Some("250")).unwrap().max_tx_writes, Some(250));
    assert_eq!(cfg(Some("0")).unwrap().max_tx_writes, None, "0 is no limit");
    let err = cfg(Some("lots")).unwrap_err();
    assert!(err.to_string().contains("DREVO_MAX_TX_WRITES"), "{err}");
}

#[test]
fn every_database_follows_the_server_limit_including_new_ones() {
    let registry = DatabaseRegistry::new(Arc::new(NativeService::in_memory()));
    registry.set_max_tx_writes(Some(7));
    let fresh = registry.create_in_memory("fresh").unwrap();
    for svc in [registry.default_service(), fresh] {
        assert_eq!(svc.graph().max_tx_writes(), Some(7));
    }
}

fn exec(svc: &NativeService, q: &str) -> Vec<Vec<Value>> {
    svc.execute(&parse(q).unwrap(), HashMap::new())
        .unwrap_or_else(|e| panic!("{q}: {e}"))
        .rows
}

fn count(svc: &NativeService) -> i64 {
    match exec(svc, "MATCH (n) RETURN count(n)")[0][0] {
        Value::Integer(n) => n,
        ref other => panic!("{other:?}"),
    }
}

#[test]
fn a_statement_over_the_limit_fails_and_rolls_the_transaction_back() {
    let svc = NativeService::in_memory();
    svc.graph().set_max_tx_writes(Some(5));
    let tx = svc.begin_tx();
    let q = parse("UNWIND range(1, 3) AS i CREATE (:N {i: i})").unwrap();
    svc.execute_in_tx(tx, &q, HashMap::new()).expect("3 of 5");
    let err = svc.execute_in_tx(tx, &q, HashMap::new()).unwrap_err();
    match &err {
        ExecError::Storage(DrevoError::TransactionTooLarge { limit: 5 }) => {}
        other => panic!("expected TransactionTooLarge, got {other:?}"),
    }
    assert!(err.to_string().contains('5'), "{err}");
    assert_eq!(svc.graph().tx_write_count(tx), None, "rolled back");
    assert!(svc.commit_tx(tx).is_err(), "nothing left to commit");
    assert_eq!(count(&svc), 0);
}

#[test]
fn a_transaction_within_the_limit_commits() {
    let svc = NativeService::in_memory();
    svc.graph().set_max_tx_writes(Some(5));
    let tx = svc.begin_tx();
    let q = parse("UNWIND range(1, 5) AS i CREATE (:N {i: i})").unwrap();
    svc.execute_in_tx(tx, &q, HashMap::new()).expect("5 of 5");
    svc.commit_tx(tx).expect("commit");
    assert_eq!(count(&svc), 5);
}

#[test]
fn an_auto_commit_statement_is_not_limited_even_when_run_as_a_transaction() {
    let svc = NativeService::in_memory();
    svc.graph().set_max_tx_writes(Some(2));
    // A NOT NULL constraint makes every writing statement run as one
    // transaction of its own.
    exec(
        &svc,
        "CREATE CONSTRAINT n_i FOR (n:N) REQUIRE n.i IS NOT NULL",
    );
    exec(&svc, "UNWIND range(1, 6) AS i CREATE (:N {i: i})");
    assert_eq!(count(&svc), 6);
}

#[test]
fn show_transactions_reports_the_buffered_writes() {
    let svc = NativeService::in_memory();
    let tx = svc.begin_tx();
    let q = parse("UNWIND range(1, 4) AS i CREATE (:N {i: i})").unwrap();
    svc.execute_in_tx(tx, &q, HashMap::new()).unwrap();
    let rows = exec(
        &svc,
        "SHOW TRANSACTIONS YIELD isExplicit, bufferedWrites RETURN isExplicit, bufferedWrites",
    );
    assert!(
        rows.contains(&vec![Value::Bool(true), Value::Integer(4)]),
        "{rows:?}"
    );
    assert!(
        rows.contains(&vec![Value::Bool(false), Value::Null]),
        "the SHOW statement itself has no buffer: {rows:?}"
    );
    svc.rollback_tx(tx);
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

async fn run(client: &mut TcpStream, query: &str) -> (u8, Option<String>) {
    send(
        client,
        frame(
            RUN,
            vec![BoltValue::String(query.into()), dict(vec![]), dict(vec![])],
        ),
    )
    .await;
    let (tag, code) = reply(client).await;
    if tag != SUCCESS {
        return (tag, code);
    }
    send(
        client,
        frame(PULL, vec![dict(vec![("n", BoltValue::Integer(-1))])]),
    )
    .await;
    reply(client).await
}

async fn commit(client: &mut TcpStream) -> (u8, Option<String>) {
    send(client, frame(COMMIT, vec![])).await;
    reply(client).await
}

#[tokio::test]
async fn bolt_reports_neo4js_code_and_the_transaction_is_gone() {
    let svc = Arc::new(NativeService::in_memory());
    let registry = Arc::new(DatabaseRegistry::new(Arc::clone(&svc)));
    registry.set_max_tx_writes(Some(4));
    let mut client = connect(&registry).await;
    send(&mut client, frame(BEGIN, vec![dict(vec![])])).await;
    assert_eq!(reply(&mut client).await.0, SUCCESS);
    assert_eq!(
        run(&mut client, "UNWIND range(1, 3) AS i CREATE (:N)").await,
        (SUCCESS, None)
    );
    assert_eq!(
        run(&mut client, "UNWIND range(1, 3) AS i CREATE (:N)").await,
        (FAILURE, Some(OUT_OF_MEMORY.to_string()))
    );
    assert_eq!(count(&svc), 0);
    // The session is FAILED as after any error; a fresh transaction works.
    send(&mut client, frame(drevo::bolt::session::RESET, vec![])).await;
    assert_eq!(reply(&mut client).await.0, SUCCESS);
    send(&mut client, frame(BEGIN, vec![dict(vec![])])).await;
    assert_eq!(reply(&mut client).await.0, SUCCESS);
    assert_eq!(run(&mut client, "CREATE (:N)").await, (SUCCESS, None));
    assert_eq!(commit(&mut client).await, (SUCCESS, None));
    assert_eq!(count(&svc), 1);
}
