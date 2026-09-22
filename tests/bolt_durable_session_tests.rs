//! Integration tests for the Bolt session layer over the **durable native**
//! store of record (epic #444 — KV retirement, PR-e1).
//!
//! The Bolt protocol state machine, the synchronous session driver, and the
//! authentication handshake are all engine-agnostic: they were historically
//! only ever constructed against the KV engine. These tests exercise the same
//! surfaces against `NativeService` through the durable constructors
//! ([`Session::new_durable`], [`Session::with_auth_durable`],
//! [`run_session_sync_durable`], [`run_session_sync_with_auth_durable`]), so the
//! capability is preserved and covered on the engine the shipping server
//! actually runs — the prerequisite for retiring the KV Bolt path.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::Arc;

use drevo::bolt::chunked::{read_message, write_message};
use drevo::bolt::packstream::{decode, encode, Value};
use drevo::bolt::session::{
    run_session_sync_durable, ClientMessage, ServerMessage, Session, State, RECORD, SUCCESS,
};
use drevo::native_service::NativeService;

fn service() -> Arc<NativeService> {
    Arc::new(NativeService::in_memory())
}

fn dict<I: IntoIterator<Item = (&'static str, Value)>>(entries: I) -> BTreeMap<String, Value> {
    entries
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

/// PackStream-encode + chunk-frame a list of client messages into one byte
/// stream, the way a real driver would write them back-to-back.
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
    use drevo::bolt::session::{GOODBYE, HELLO, PULL, RUN};
    match msg {
        ClientMessage::Hello { extra } => Value::Structure {
            tag: HELLO,
            fields: vec![Value::Dictionary(extra.clone())],
        },
        ClientMessage::Goodbye => Value::Structure {
            tag: GOODBYE,
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
        other => panic!("test harness does not encode {other:?}"),
    }
}

// --- Protocol state machine over the durable engine ------------------------

#[test]
fn durable_session_starts_connected_and_hello_reaches_ready() {
    let svc = service();
    let mut s = Session::new_durable(Arc::clone(&svc));
    assert_eq!(s.state(), State::Connected);
    let replies = s.handle(ClientMessage::Hello {
        extra: dict([("user_agent", Value::String("test/1".to_string()))]),
    });
    assert_eq!(replies.len(), 1);
    match &replies[0] {
        ServerMessage::Success { metadata } => {
            assert!(metadata.contains_key("server"));
            assert!(metadata.contains_key("connection_id"));
        }
        other => panic!("expected Success, got {other:?}"),
    }
    assert_eq!(s.state(), State::Ready);
}

#[test]
fn durable_session_run_return_literal_streams_a_record() {
    let svc = service();
    let mut s = Session::new_durable(Arc::clone(&svc));
    s.handle(ClientMessage::Hello { extra: dict([]) });
    let replies = s.handle(ClientMessage::Run {
        query: "RETURN 1 AS one, 2 AS two".to_string(),
        parameters: dict([]),
        extra: dict([]),
    });
    assert_eq!(replies.len(), 1);
    match &replies[0] {
        ServerMessage::Success { metadata } => {
            let fields = metadata.get("fields").expect("fields metadata");
            match fields {
                Value::List(items) => {
                    let names: Vec<String> = items
                        .iter()
                        .map(|v| match v {
                            Value::String(s) => s.clone(),
                            other => panic!("expected String field name, got {other:?}"),
                        })
                        .collect();
                    assert_eq!(names, vec!["one".to_string(), "two".to_string()]);
                }
                other => panic!("expected List of field names, got {other:?}"),
            }
        }
        other => panic!("expected Success, got {other:?}"),
    }
    assert_eq!(s.state(), State::Streaming);
}

#[test]
fn durable_session_run_before_hello_fails() {
    let svc = service();
    let mut s = Session::new_durable(Arc::clone(&svc));
    let replies = s.handle(ClientMessage::Run {
        query: "RETURN 1".to_string(),
        parameters: dict([]),
        extra: dict([]),
    });
    assert_eq!(replies.len(), 1);
    assert!(matches!(replies[0], ServerMessage::Failure { .. }));
}

// --- Synchronous driver end-to-end over the durable engine -----------------

#[test]
fn durable_sync_driver_runs_a_query_end_to_end() {
    let svc = service();
    let client = encode_client_stream(&[
        ClientMessage::Hello {
            extra: dict([("user_agent", Value::String("test/1".to_string()))]),
        },
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
    run_session_sync_durable(&mut reader, &mut writer, Arc::clone(&svc)).expect("session loop");

    let tags: Vec<u8> = decode_server_stream(writer)
        .iter()
        .map(|(t, _)| *t)
        .collect();
    assert_eq!(tags[0], SUCCESS, "HELLO ack");
    assert_eq!(tags[1], SUCCESS, "RUN ack");
    assert_eq!(tags[2], RECORD, "PULL record");
    assert_eq!(tags[3], SUCCESS, "PULL ack");
}

#[test]
fn durable_sync_driver_persists_writes_to_the_service() {
    let svc = service();
    let client = encode_client_stream(&[
        ClientMessage::Hello { extra: dict([]) },
        ClientMessage::Run {
            query: "CREATE (n:Note {title: 'from-bolt'})".to_string(),
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
    run_session_sync_durable(&mut reader, &mut writer, Arc::clone(&svc)).expect("session loop");

    // The write landed in the same service the session borrowed.
    let notes = svc.list_nodes_by_kind("Note", 10, 0);
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].title, "from-bolt");
}

// --- Authentication over the durable engine (gated on `bolt-auth`) ----------

#[cfg(feature = "bolt-auth")]
mod auth {
    use super::*;
    use drevo::bolt::auth::UserStore;
    use drevo::bolt::session::{run_session_sync_with_auth_durable, FAILURE};

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
    fn valid_basic_auth_runs_query_end_to_end_on_durable() {
        let svc = service();
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
        run_session_sync_with_auth_durable(&mut reader, &mut writer, Arc::clone(&svc), &store)
            .expect("session loop");

        let tags: Vec<u8> = decode_server_stream(writer)
            .iter()
            .map(|(t, _)| *t)
            .collect();
        assert_eq!(tags[0], SUCCESS, "HELLO ack");
        assert_eq!(tags[1], SUCCESS, "RUN ack");
        assert_eq!(tags[2], RECORD, "PULL record");
        assert_eq!(tags[3], SUCCESS, "PULL ack");
    }

    #[test]
    fn wrong_password_fails_and_blocks_queued_run_on_durable() {
        let svc = service();
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
        run_session_sync_with_auth_durable(&mut reader, &mut writer, Arc::clone(&svc), &store)
            .expect("session loop");

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
        // And the blocked CREATE must not have touched the store.
        assert!(svc.list_nodes_by_kind("Secret", 10, 0).is_empty());
    }
}

// --- Multi-database routing over the durable engine (issue #523) ------------

/// A Neo4j driver opening `session(database="b")` sends `db="b"` in the RUN
/// extra; the write must land in database `b` and leave the default database
/// untouched. Drives the full HELLO → RUN → PULL state machine through the
/// catalog-backed session, then inspects each catalog entry's own service to
/// prove the isolation is real and not just a per-query view.
#[cfg(feature = "http")]
#[test]
fn catalog_session_routes_autocommit_writes_to_the_named_database() {
    use drevo::database_registry::DatabaseRegistry;

    let default = service();
    let registry = Arc::new(DatabaseRegistry::new(Arc::clone(&default)));
    let b = registry.create_in_memory("b").expect("valid db name");

    let mut s = Session::new_durable_with_registry(Arc::clone(&registry));
    s.handle(ClientMessage::Hello { extra: dict([]) });
    assert_eq!(s.state(), State::Ready);

    // A real driver's `session(database="b").run("CREATE …")`.
    let run = s.handle(ClientMessage::Run {
        query: "CREATE (n:Note {title: 'in-b'})".to_string(),
        parameters: dict([]),
        extra: dict([("db", Value::String("b".to_string()))]),
    });
    assert!(matches!(run.last(), Some(ServerMessage::Success { .. })));
    s.handle(ClientMessage::Pull {
        extra: dict([("n", Value::Integer(-1))]),
    });
    assert_eq!(s.state(), State::Ready);

    // The row physically lives in database `b`, not the default — proving the
    // Bolt `db` selector routed through the shared catalog.
    let in_b = b.list_nodes_by_kind("Note", 10, 0);
    assert_eq!(in_b.len(), 1, "the write must land in database `b`");
    assert_eq!(in_b[0].title, "in-b");
    assert!(
        default.list_nodes_by_kind("Note", 10, 0).is_empty(),
        "the default database must stay untouched"
    );
}

/// A RUN naming a database the catalog does not hold fails with Neo4j's
/// unknown-database code and never touches the store.
#[cfg(feature = "http")]
#[test]
fn catalog_session_run_on_unknown_database_fails_with_database_not_found() {
    use drevo::database_registry::DatabaseRegistry;

    let default = service();
    let registry = Arc::new(DatabaseRegistry::new(Arc::clone(&default)));

    let mut s = Session::new_durable_with_registry(Arc::clone(&registry));
    s.handle(ClientMessage::Hello { extra: dict([]) });

    let run = s.handle(ClientMessage::Run {
        query: "CREATE (n:Note) RETURN n".to_string(),
        parameters: dict([]),
        extra: dict([("db", Value::String("ghost".to_string()))]),
    });
    match run.last() {
        Some(ServerMessage::Failure { metadata }) => assert_eq!(
            metadata.get("code"),
            Some(&Value::String(
                "Neo.ClientError.Database.DatabaseNotFound".to_string()
            )),
        ),
        other => panic!("expected a DatabaseNotFound FAILURE, got {other:?}"),
    }
    assert!(
        default.list_nodes_by_kind("Note", 10, 0).is_empty(),
        "a routed RUN to a missing database must not touch any store"
    );
}
