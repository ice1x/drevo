//! Integration tests for Bolt authentication — Phase 11 task `00074`.
//!
//! These drive the argon2-backed [`UserStore`] end-to-end through the
//! synchronous session loop (`run_session_sync_with_auth`), the same
//! path the async listener entry point (`accept_and_run_session_with_auth`)
//! reuses over TCP/TLS. They prove that:
//!
//! * a correct basic-auth `HELLO` reaches `READY` and runs Cypher;
//! * a wrong password is answered with `Neo.ClientError.Security.Unauthorized`
//!   and the connection is closed *before* any queued `RUN` executes;
//! * an issued bearer **session token** re-authenticates without the
//!   password.
//!
//! Gated on the `bolt-auth` feature — the whole file compiles to nothing
//! in the default build (mirroring `tests/bolt_tls_tests.rs`).

#![cfg(all(
    not(target_arch = "wasm32"),
    feature = "redb-backend",
    feature = "bolt-auth"
))]

use std::collections::BTreeMap;
use std::io::Cursor;

use drevo::bolt::auth::UserStore;
use drevo::bolt::chunked::{read_message, write_message};
use drevo::bolt::packstream::{decode, encode, Value};
use drevo::bolt::session::{run_session_sync_with_auth, ClientMessage, FAILURE, RECORD, SUCCESS};
use drevo::db::Drevo;

fn open() -> Drevo {
    Drevo::open_in_memory().expect("open_in_memory")
}

fn dict<I: IntoIterator<Item = (&'static str, Value)>>(entries: I) -> BTreeMap<String, Value> {
    entries
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

/// PackStream-encode + chunk-frame a list of client messages into one
/// byte stream, the way a real driver would write them back-to-back.
fn encode_client_stream(msgs: &[ClientMessage]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for msg in msgs {
        let value = client_message_to_value(msg);
        let mut payload = Vec::new();
        encode(&value, &mut payload).unwrap();
        write_message(&payload, &mut bytes).unwrap();
    }
    bytes
}

/// Drain a chunked server reply stream into a flat list of decoded
/// `(tag, fields)` structures.
fn decode_server_stream(bytes: Vec<u8>) -> Vec<(u8, Vec<Value>)> {
    let mut out = Vec::new();
    let mut cur = Cursor::new(bytes);
    while let Ok(payload) = read_message(&mut cur) {
        let (val, rest) = decode(&payload).unwrap();
        assert!(rest.is_empty());
        match val {
            Value::Structure { tag, fields } => out.push((tag, fields)),
            other => panic!("expected Structure, got {other:?}"),
        }
    }
    out
}

fn client_message_to_value(msg: &ClientMessage) -> Value {
    use drevo::bolt::session::{DISCARD, GOODBYE, HELLO, PULL, RESET, RUN};
    match msg {
        ClientMessage::Hello { extra } => Value::Structure {
            tag: HELLO,
            fields: vec![Value::Dictionary(extra.clone())],
        },
        ClientMessage::Goodbye => Value::Structure {
            tag: GOODBYE,
            fields: vec![],
        },
        ClientMessage::Reset => Value::Structure {
            tag: RESET,
            fields: vec![],
        },
        ClientMessage::Run {
            query,
            parameters,
            extra,
        } => Value::Structure {
            tag: RUN,
            fields: vec![
                Value::String(query.clone()),
                Value::Dictionary(parameters.clone()),
                Value::Dictionary(extra.clone()),
            ],
        },
        ClientMessage::Pull { extra } => Value::Structure {
            tag: PULL,
            fields: vec![Value::Dictionary(extra.clone())],
        },
        ClientMessage::Discard { extra } => Value::Structure {
            tag: DISCARD,
            fields: vec![Value::Dictionary(extra.clone())],
        },
        other => panic!("test does not encode {other:?}"),
    }
}

fn hello_basic(principal: &str, credentials: &str) -> ClientMessage {
    ClientMessage::Hello {
        extra: dict([
            ("user_agent", Value::String("test-driver/1".to_string())),
            ("scheme", Value::String("basic".to_string())),
            ("principal", Value::String(principal.to_string())),
            ("credentials", Value::String(credentials.to_string())),
        ]),
    }
}

#[test]
fn valid_basic_auth_runs_query_end_to_end() {
    let drevo = open();
    let mut store = UserStore::new();
    store.add_user("neo4j", "s3cret").unwrap();

    let client = encode_client_stream(&[
        hello_basic("neo4j", "s3cret"),
        ClientMessage::Run {
            query: "RETURN 1 AS n".to_string(),
            parameters: dict([]),
            extra: dict([]),
        },
        ClientMessage::Pull {
            extra: dict([("n", Value::Integer(-1))]),
        },
        ClientMessage::Goodbye,
    ]);

    let mut reader = Cursor::new(client);
    let mut writer: Vec<u8> = Vec::new();
    run_session_sync_with_auth(&mut reader, &mut writer, &drevo, &store).expect("session loop");

    let replies = decode_server_stream(writer);
    let tags: Vec<u8> = replies.iter().map(|(t, _)| *t).collect();
    assert_eq!(tags[0], SUCCESS, "HELLO ack");
    assert_eq!(tags[1], SUCCESS, "RUN ack");
    assert_eq!(tags[2], RECORD, "PULL record");
    assert_eq!(tags[3], SUCCESS, "PULL ack");
}

#[test]
fn wrong_password_fails_and_blocks_queued_run() {
    let drevo = open();
    let mut store = UserStore::new();
    store.add_user("neo4j", "s3cret").unwrap();

    let client = encode_client_stream(&[
        hello_basic("neo4j", "wrong"),
        ClientMessage::Run {
            query: "CREATE (n:Secret) RETURN n".to_string(),
            parameters: dict([]),
            extra: dict([]),
        },
    ]);

    let mut reader = Cursor::new(client);
    let mut writer: Vec<u8> = Vec::new();
    run_session_sync_with_auth(&mut reader, &mut writer, &drevo, &store).expect("session loop");

    let replies = decode_server_stream(writer);
    assert_eq!(replies.len(), 1, "only the auth FAILURE; RUN never ran");
    let (tag, fields) = &replies[0];
    assert_eq!(*tag, FAILURE);
    let md = match &fields[0] {
        Value::Dictionary(m) => m,
        other => panic!("expected dict, got {other:?}"),
    };
    assert_eq!(
        md.get("code"),
        Some(&Value::String(
            "Neo.ClientError.Security.Unauthorized".to_string()
        ))
    );
    assert!(
        drevo.list_recent(10).unwrap().is_empty(),
        "CREATE must not have executed"
    );
}

#[test]
fn unknown_user_is_denied() {
    let drevo = open();
    let store = UserStore::new(); // no users provisioned

    let client = encode_client_stream(&[hello_basic("ghost", "whatever")]);
    let mut reader = Cursor::new(client);
    let mut writer: Vec<u8> = Vec::new();
    run_session_sync_with_auth(&mut reader, &mut writer, &drevo, &store).expect("session loop");

    let replies = decode_server_stream(writer);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].0, FAILURE);
}

#[test]
fn issued_bearer_token_authenticates() {
    let drevo = open();
    let mut store = UserStore::new();
    store.add_user("neo4j", "s3cret").unwrap();
    let token = store.issue_token("neo4j").expect("token");

    let client = encode_client_stream(&[
        ClientMessage::Hello {
            extra: dict([
                ("scheme", Value::String("bearer".to_string())),
                ("credentials", Value::String(token)),
            ]),
        },
        ClientMessage::Run {
            query: "RETURN 42 AS answer".to_string(),
            parameters: dict([]),
            extra: dict([]),
        },
        ClientMessage::Pull {
            extra: dict([("n", Value::Integer(-1))]),
        },
        ClientMessage::Goodbye,
    ]);

    let mut reader = Cursor::new(client);
    let mut writer: Vec<u8> = Vec::new();
    run_session_sync_with_auth(&mut reader, &mut writer, &drevo, &store).expect("session loop");

    let tags: Vec<u8> = decode_server_stream(writer)
        .iter()
        .map(|(t, _)| *t)
        .collect();
    assert_eq!(tags[0], SUCCESS, "bearer HELLO ack");
    assert_eq!(tags[1], SUCCESS, "RUN ack");
    assert_eq!(tags[2], RECORD);
    assert_eq!(tags[3], SUCCESS);
}
