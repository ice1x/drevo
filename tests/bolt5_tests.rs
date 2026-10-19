//! Bolt 5.0–5.4 next to 4.4 / 4.3.
//!
//! - The handshake takes version ranges and picks the highest version drevo
//!   speaks (5.4 down to 4.3).
//! - From 5.0 nodes and relationships carry `element_id` strings.
//! - From 5.1 `HELLO` carries no credentials: `LOGON` logs in and `LOGOFF`
//!   logs out, so a driver can re-authenticate a pooled connection.
//! - 5.4 adds `TELEMETRY`, acknowledged with `SUCCESS`.
//! - 4.x clients keep logging in with `HELLO` and get the 4.x shapes.
//!
//! The scenario is a bug tracker: a `Bug` reported by a `User`.

#![cfg(all(not(target_arch = "wasm32"), feature = "http"))]

use std::collections::BTreeMap;
use std::sync::Arc;

use drevo::bolt::handshake::BoltVersion;
use drevo::bolt::packstream::Value;
use drevo::bolt::session::{decode_client, ClientMessage, ServerMessage, Session, State};
use drevo::native_service::NativeService;

const V44: BoltVersion = BoltVersion { major: 4, minor: 4 };
const V50: BoltVersion = BoltVersion { major: 5, minor: 0 };
const V53: BoltVersion = BoltVersion { major: 5, minor: 3 };
const V54: BoltVersion = BoltVersion { major: 5, minor: 4 };

fn dict<I: IntoIterator<Item = (&'static str, Value)>>(entries: I) -> BTreeMap<String, Value> {
    entries
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

fn basic(user: &str, password: &str) -> BTreeMap<String, Value> {
    dict([
        ("scheme", Value::String("basic".into())),
        ("principal", Value::String(user.into())),
        ("credentials", Value::String(password.into())),
    ])
}

fn tracker() -> Arc<NativeService> {
    let svc = Arc::new(NativeService::in_memory());
    svc.execute(
        &drevo::cypher::parser::parse(
            "CREATE (:User {title: 'ann'})-[:REPORTED {at: 1}]->(:Bug {title: 'crash on save'})",
        )
        .unwrap(),
        Default::default(),
    )
    .unwrap();
    svc
}

fn run(s: &mut Session<'_>, query: &str) -> Vec<ServerMessage> {
    let mut replies = s.handle(ClientMessage::Run {
        query: query.to_string(),
        parameters: dict([]),
        extra: dict([]),
    });
    if matches!(replies.first(), Some(ServerMessage::Success { .. })) {
        replies.extend(s.handle(ClientMessage::Pull {
            extra: dict([("n", Value::Integer(-1))]),
        }));
    }
    replies
}

fn records(replies: &[ServerMessage]) -> Vec<Vec<Value>> {
    replies
        .iter()
        .filter_map(|m| match m {
            ServerMessage::Record { fields } => Some(fields.clone()),
            _ => None,
        })
        .collect()
}

#[track_caller]
fn success(replies: &[ServerMessage]) {
    assert!(
        matches!(replies, [ServerMessage::Success { .. }]),
        "{replies:?}"
    );
}

#[track_caller]
fn failure_code(replies: &[ServerMessage]) -> String {
    match replies {
        [ServerMessage::Failure { metadata }] => match metadata.get("code") {
            Some(Value::String(code)) => code.clone(),
            other => panic!("code: {other:?}"),
        },
        other => panic!("expected one FAILURE, got {other:?}"),
    }
}

fn fields(v: &Value) -> (u8, &[Value]) {
    match v {
        Value::Structure { tag, fields } => (*tag, fields),
        other => panic!("expected a structure, got {other:?}"),
    }
}

/// A logged-in session on `version` over the bug tracker.
fn ready(version: BoltVersion) -> Session<'static> {
    let mut s = Session::new_durable(tracker()).with_version(version);
    success(&s.handle(ClientMessage::Hello {
        extra: dict([("user_agent", Value::String("test/1".into()))]),
    }));
    if version >= (BoltVersion { major: 5, minor: 1 }) {
        assert_eq!(s.state(), State::Authentication);
        success(&s.handle(ClientMessage::Logon {
            auth: dict([("scheme", Value::String("none".into()))]),
        }));
    }
    assert_eq!(s.state(), State::Ready);
    s
}

#[test]
fn the_new_messages_decode() {
    use drevo::bolt::session::{LOGOFF, LOGON, TELEMETRY};
    let logon = Value::Structure {
        tag: LOGON,
        fields: vec![Value::Dictionary(basic("ann", "pw"))],
    };
    assert_eq!(
        decode_client(&logon).unwrap(),
        ClientMessage::Logon {
            auth: basic("ann", "pw")
        }
    );
    let logoff = Value::Structure {
        tag: LOGOFF,
        fields: vec![],
    };
    assert_eq!(decode_client(&logoff).unwrap(), ClientMessage::Logoff);
    let telemetry = Value::Structure {
        tag: TELEMETRY,
        fields: vec![Value::Integer(2)],
    };
    assert_eq!(
        decode_client(&telemetry).unwrap(),
        ClientMessage::Telemetry {
            api: Value::Integer(2)
        }
    );
    for bad in [
        Value::Structure {
            tag: LOGON,
            fields: vec![],
        },
        Value::Structure {
            tag: LOGOFF,
            fields: vec![Value::Null],
        },
        Value::Structure {
            tag: TELEMETRY,
            fields: vec![],
        },
    ] {
        assert!(decode_client(&bad).is_err(), "{bad:?}");
    }
}

#[test]
fn bolt_5_nodes_and_relationships_carry_element_ids() {
    let mut s = ready(V54);
    let rows = records(&run(
        &mut s,
        "MATCH p = (u:User)-[r:REPORTED]->(b:Bug) RETURN u, r, p",
    ));
    assert_eq!(rows.len(), 1);

    let (tag, node) = fields(&rows[0][0]);
    assert_eq!(tag, 0x4E);
    assert_eq!(node.len(), 4, "id, labels, properties, element_id");
    let Value::Integer(user_id) = node[0] else {
        panic!("{node:?}")
    };
    assert_eq!(node[3], Value::String(user_id.to_string()));

    let (tag, rel) = fields(&rows[0][1]);
    assert_eq!(tag, 0x52);
    assert_eq!(rel.len(), 8, "… element_id, start and end element ids");
    let ids: Vec<String> = rel[..3]
        .iter()
        .map(|v| match v {
            Value::Integer(i) => i.to_string(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        rel[5..].to_vec(),
        ids.into_iter().map(Value::String).collect::<Vec<_>>()
    );

    let (tag, path) = fields(&rows[0][2]);
    assert_eq!(tag, 0x50);
    let Value::List(nodes) = &path[0] else {
        panic!("{path:?}")
    };
    assert!(nodes.iter().all(|n| fields(n).1.len() == 4));
    let Value::List(rels) = &path[1] else {
        panic!("{path:?}")
    };
    let (tag, unbound) = fields(&rels[0]);
    assert_eq!(tag, 0x72);
    assert_eq!(unbound.len(), 4, "id, type, properties, element_id");

    // Nested in lists and maps as well.
    let rows = records(&run(
        &mut s,
        "MATCH (b:Bug) RETURN [b] AS list, {bug: b} AS map",
    ));
    let Value::List(list) = &rows[0][0] else {
        panic!()
    };
    assert_eq!(fields(&list[0]).1.len(), 4);
    let Value::Dictionary(map) = &rows[0][1] else {
        panic!()
    };
    assert_eq!(fields(&map["bug"]).1.len(), 4);

    // Plain values are untouched.
    let rows = records(&run(&mut s, "RETURN 1 AS n, 'x' AS s"));
    assert_eq!(rows[0], vec![Value::Integer(1), Value::String("x".into())]);
}

#[test]
fn bolt_4_keeps_the_4_x_shapes() {
    let mut s = ready(V44);
    let rows = records(&run(&mut s, "MATCH (u:User)-[r]->(b) RETURN u, r"));
    assert_eq!(fields(&rows[0][0]).1.len(), 3);
    assert_eq!(fields(&rows[0][1]).1.len(), 5);
}

#[test]
fn bolt_5_0_still_logs_in_with_hello() {
    let mut s = Session::new_durable(tracker()).with_version(V50);
    assert_eq!(s.version(), V50);
    success(&s.handle(ClientMessage::Hello { extra: dict([]) }));
    assert_eq!(s.state(), State::Ready);
    let rows = records(&run(&mut s, "MATCH (b:Bug) RETURN b"));
    assert_eq!(fields(&rows[0][0]).1.len(), 4, "5.0 has element ids");
}

#[test]
fn before_logon_only_logon_or_goodbye_is_accepted() {
    let mut s = Session::new_durable(tracker()).with_version(V54);
    success(&s.handle(ClientMessage::Hello { extra: dict([]) }));
    assert_eq!(s.state(), State::Authentication);
    let code = failure_code(&run(&mut s, "MATCH (b:Bug) RETURN b"));
    assert_eq!(code, "Neo.ClientError.Request.Invalid");
    assert_eq!(s.state(), State::Defunct);

    let mut s = Session::new_durable(tracker()).with_version(V54);
    s.handle(ClientMessage::Hello { extra: dict([]) });
    assert!(s.handle(ClientMessage::Goodbye).is_empty());
    assert_eq!(s.state(), State::Defunct);
}

#[test]
fn logoff_returns_to_authentication_and_logon_again_works() {
    let mut s = ready(V54);
    success(&s.handle(ClientMessage::Logoff));
    assert_eq!(s.state(), State::Authentication);
    success(&s.handle(ClientMessage::Logon {
        auth: dict([("scheme", Value::String("none".into()))]),
    }));
    assert_eq!(
        records(&run(&mut s, "MATCH (b:Bug) RETURN b.title")).len(),
        1
    );

    // Not inside a transaction, and LOGON only after HELLO / LOGOFF.
    success(&s.handle(ClientMessage::Begin { extra: dict([]) }));
    let code = failure_code(&s.handle(ClientMessage::Logoff));
    assert_eq!(code, "Neo.ClientError.Request.Invalid");
    success(&s.handle(ClientMessage::Reset));
    let code = failure_code(&s.handle(ClientMessage::Logon {
        auth: dict([("scheme", Value::String("none".into()))]),
    }));
    assert_eq!(code, "Neo.ClientError.Request.Invalid");
}

#[test]
fn bolt_4_refuses_logon_logoff_and_telemetry() {
    for msg in [
        ClientMessage::Logon {
            auth: dict([("scheme", Value::String("none".into()))]),
        },
        ClientMessage::Logoff,
        ClientMessage::Telemetry {
            api: Value::Integer(0),
        },
    ] {
        let mut s = ready(V44);
        let code = failure_code(&s.handle(msg));
        assert_eq!(code, "Neo.ClientError.Request.Invalid");
    }
}

#[test]
fn telemetry_is_acknowledged_from_bolt_5_4() {
    let mut s = ready(V54);
    success(&s.handle(ClientMessage::Telemetry {
        api: Value::Integer(0),
    }));
    assert_eq!(s.state(), State::Ready);
    let mut s = ready(V53);
    let code = failure_code(&s.handle(ClientMessage::Telemetry {
        api: Value::Integer(0),
    }));
    assert_eq!(code, "Neo.ClientError.Request.Invalid");
}

mod over_tcp {
    use super::*;

    use drevo::bolt::handshake::MAGIC_PREAMBLE;
    use drevo::bolt::listener::accept_and_run_session_durable;
    use drevo::bolt::packstream::{decode, encode};
    use drevo::bolt::session::{GOODBYE, HELLO, LOGON, PULL, RUN};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::timeout;

    fn frame(tag: u8, fields: Vec<Value>) -> Vec<u8> {
        let mut buf = Vec::new();
        encode(&Value::Structure { tag, fields }, &mut buf).unwrap();
        let mut framed = (buf.len() as u16).to_be_bytes().to_vec();
        framed.extend_from_slice(&buf);
        framed.extend_from_slice(&[0x00, 0x00]);
        framed
    }

    async fn read_one(client: &mut TcpStream) -> Value {
        let mut payload = Vec::new();
        loop {
            let mut len = [0u8; 2];
            client.read_exact(&mut len).await.unwrap();
            let l = u16::from_be_bytes(len) as usize;
            if l == 0 {
                break;
            }
            let start = payload.len();
            payload.resize(start + l, 0);
            client.read_exact(&mut payload[start..]).await.unwrap();
        }
        decode(&payload).unwrap().0
    }

    /// Handshake with `slots`; the server's 4-byte answer.
    async fn negotiate(slots: [[u8; 4]; 4]) -> (TcpStream, [u8; 4]) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let svc = tracker();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            accept_and_run_session_durable(socket, &svc).await
        });
        let mut client = TcpStream::connect(addr).await.unwrap();
        let mut handshake = MAGIC_PREAMBLE.to_vec();
        for slot in slots {
            handshake.extend_from_slice(&slot);
        }
        client.write_all(&handshake).await.unwrap();
        let mut version = [0u8; 4];
        timeout(Duration::from_secs(5), client.read_exact(&mut version))
            .await
            .unwrap()
            .unwrap();
        (client, version)
    }

    #[tokio::test]
    async fn a_5_x_driver_negotiates_5_4_and_logs_in_with_logon() {
        // What the neo4j 5.x drivers offer: 5.8–5.0, 4.4–4.2, 4.1, 3.0.
        let (mut client, version) =
            negotiate([[0, 8, 8, 5], [0, 2, 4, 4], [0, 0, 1, 4], [0, 0, 0, 3]]).await;
        assert_eq!(version, [0, 0, 4, 5]);

        let mut batch = frame(HELLO, vec![Value::Dictionary(dict([]))]);
        batch.extend(frame(
            LOGON,
            vec![Value::Dictionary(dict([(
                "scheme",
                Value::String("none".into()),
            )]))],
        ));
        batch.extend(frame(
            RUN,
            vec![
                Value::String("MATCH (b:Bug) RETURN b".into()),
                Value::Dictionary(dict([])),
                Value::Dictionary(dict([])),
            ],
        ));
        batch.extend(frame(
            PULL,
            vec![Value::Dictionary(dict([("n", Value::Integer(-1))]))],
        ));
        batch.extend(frame(GOODBYE, vec![]));
        client.write_all(&batch).await.unwrap();

        let mut tags = Vec::new();
        let mut node_fields = 0;
        for _ in 0..5 {
            let msg = timeout(Duration::from_secs(5), read_one(&mut client))
                .await
                .unwrap();
            let (tag, f) = fields(&msg);
            if tag == 0x71 {
                let Value::List(row) = &f[0] else { panic!() };
                node_fields = fields(&row[0]).1.len();
            }
            tags.push(tag);
        }
        assert_eq!(tags, vec![0x70, 0x70, 0x70, 0x71, 0x70]);
        assert_eq!(node_fields, 4);
    }

    #[tokio::test]
    async fn a_4_4_driver_still_gets_4_4() {
        let (_client, version) = negotiate([[0, 2, 4, 4], [0, 0, 1, 4], [0; 4], [0; 4]]).await;
        assert_eq!(version, [0, 0, 4, 4]);
        let (_client, version) = negotiate([[0, 3, 9, 5], [0, 0, 4, 4], [0; 4], [0; 4]]).await;
        assert_eq!(version, [0, 0, 4, 4], "5.9–5.6 covers nothing drevo speaks");
    }
}

#[cfg(feature = "auth")]
mod logins {
    use super::*;

    use drevo::security::Security;
    use drevo_auth::catalog::MemoryStore;
    use drevo_auth::users::{AuthManager, NewUser};

    fn security() -> Security {
        let users = Arc::new(AuthManager::open(Arc::new(MemoryStore::new())).unwrap());
        for (name, pw) in [("ann", "ann-password"), ("bob", "bob-password")] {
            users
                .create_user(NewUser::new(name, pw).change_required(false))
                .unwrap();
        }
        Security::new(users)
    }

    fn current_user(s: &mut Session<'_>) -> Value {
        records(&run(s, "SHOW CURRENT USER"))[0][0].clone()
    }

    #[test]
    fn logon_checks_the_password_and_logoff_lets_another_user_in() {
        let mut s = Session::new_durable(tracker())
            .with_security(security())
            .with_version(V54);
        // A 5.1+ HELLO has no credentials and does not log in.
        success(&s.handle(ClientMessage::Hello { extra: dict([]) }));
        assert_eq!(s.state(), State::Authentication);

        success(&s.handle(ClientMessage::Logon {
            auth: basic("ann", "ann-password"),
        }));
        assert_eq!(current_user(&mut s), Value::String("ann".into()));

        // Re-authenticate the pooled connection as bob.
        success(&s.handle(ClientMessage::Logoff));
        success(&s.handle(ClientMessage::Logon {
            auth: basic("bob", "bob-password"),
        }));
        assert_eq!(current_user(&mut s), Value::String("bob".into()));
    }

    #[test]
    fn a_wrong_logon_is_unauthorized_and_closes_the_connection() {
        let mut s = Session::new_durable(tracker())
            .with_security(security())
            .with_version(V54);
        s.handle(ClientMessage::Hello { extra: dict([]) });
        let code = failure_code(&s.handle(ClientMessage::Logon {
            auth: basic("ann", "wrong"),
        }));
        assert_eq!(code, "Neo.ClientError.Security.Unauthorized");
        assert_eq!(s.state(), State::Defunct);
    }

    #[test]
    fn bolt_4_and_5_0_log_in_with_hello() {
        for version in [V44, V50] {
            let mut s = Session::new_durable(tracker())
                .with_security(security())
                .with_version(version);
            success(&s.handle(ClientMessage::Hello {
                extra: basic("ann", "ann-password"),
            }));
            assert_eq!(s.state(), State::Ready);
            assert_eq!(current_user(&mut s), Value::String("ann".into()));

            let mut s = Session::new_durable(tracker())
                .with_security(security())
                .with_version(version);
            let code = failure_code(&s.handle(ClientMessage::Hello {
                extra: basic("ann", "wrong"),
            }));
            assert_eq!(code, "Neo.ClientError.Security.Unauthorized");
        }
    }
}
