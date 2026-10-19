//! `SHOW CONNECTIONS` / `TERMINATE CONNECTIONS`: the server's open Bolt
//! connections, listed from the handshake until they close with the
//! driver's user agent and the user they logged in as, and the Web UI's
//! signed-in sessions. Terminating a Bolt connection closes it and rolls
//! back its transaction.
//!
//! The scenario is an IT task manager whose connection pool leaks: an idle
//! connection with no transaction does not show in `SHOW TRANSACTIONS`, but
//! it does here.

#![cfg(all(not(target_arch = "wasm32"), feature = "http"))]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use drevo::bolt::packstream::Value as BoltValue;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::executor::Value;
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

fn rows(svc: &NativeService, q: &str) -> Vec<Vec<Value>> {
    svc.execute(&parse(q).unwrap(), HashMap::new())
        .unwrap_or_else(|e| panic!("`{q}`: {e}"))
        .rows
}

fn string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => panic!("expected a string, got {other:?}"),
    }
}

fn hello(agent: &str) -> ClientMessage {
    ClientMessage::Hello {
        extra: BTreeMap::from([(
            "user_agent".to_string(),
            BoltValue::String(agent.to_string()),
        )]),
    }
}

fn run(s: &mut Session<'_>, q: &str) -> Vec<ServerMessage> {
    let mut replies = s.handle(ClientMessage::Run {
        query: q.to_string(),
        parameters: BTreeMap::new(),
        extra: BTreeMap::new(),
    });
    if matches!(replies.first(), Some(ServerMessage::Success { .. })) {
        replies.extend(s.handle(ClientMessage::Pull {
            extra: BTreeMap::from([("n".to_string(), BoltValue::Integer(-1))]),
        }));
    }
    replies
}

#[test]
fn two_bolt_connections_are_listed_then_one_closes() {
    let svc = Arc::new(NativeService::in_memory());
    let mut worker = Session::new_durable(Arc::clone(&svc)).with_client(Some("10.0.0.7:5100"));
    worker.handle(hello("neo4j-python/5.28.0 Python/3.13"));
    let mut ui = Session::new_durable(Arc::clone(&svc));
    ui.handle(hello("neo4j-javascript/5.26.0"));

    let listed = rows(
        &svc,
        "SHOW CONNECTIONS YIELD connectionId, connector, userAgent, clientAddress, connectTime",
    );
    assert_eq!(listed.len(), 2, "{listed:?}");
    assert_eq!(string(&listed[0][0]), worker.connection_id());
    assert_eq!(string(&listed[1][0]), ui.connection_id());
    assert!(listed.iter().all(|r| string(&r[1]) == "bolt"));
    assert_eq!(string(&listed[0][2]), "neo4j-python/5.28.0 Python/3.13");
    assert_eq!(string(&listed[1][2]), "neo4j-javascript/5.26.0");
    assert_eq!(string(&listed[0][3]), "10.0.0.7:5100");
    assert_eq!(listed[1][3], Value::Null);
    assert!(string(&listed[0][4]).ends_with('Z'), "an ISO-8601 time");

    drop(ui);
    let listed = rows(&svc, "SHOW CONNECTIONS");
    assert_eq!(listed.len(), 1);
    assert_eq!(string(&listed[0][0]), worker.connection_id());
    // The Bolt id is what HELLO reported.
    assert!(worker.connection_id().starts_with("bolt-"));
}

#[test]
fn show_connections_takes_yield_and_where() {
    let svc = Arc::new(NativeService::in_memory());
    let mut a = Session::new_durable(Arc::clone(&svc));
    a.handle(hello("neo4j-python/5.28.0"));
    let mut b = Session::new_durable(Arc::clone(&svc));
    b.handle(hello("cypher-shell/5.26"));
    let listed = rows(
        &svc,
        "SHOW CONNECTIONS YIELD connectionId, userAgent WHERE userAgent STARTS WITH 'cypher-shell'",
    );
    assert_eq!(listed.len(), 1);
    assert_eq!(string(&listed[0][0]), b.connection_id());
}

#[test]
fn show_transactions_names_the_real_connection() {
    let svc = Arc::new(NativeService::in_memory());
    let mut s = Session::new_durable(Arc::clone(&svc));
    s.handle(hello("neo4j-python/5.28.0"));
    s.handle(ClientMessage::Begin {
        extra: BTreeMap::new(),
    });
    run(&mut s, "CREATE (:Task {title: 'patch the VPN'})");
    let listed = rows(
        &svc,
        "SHOW TRANSACTIONS YIELD connectionId, isExplicit WHERE isExplicit",
    );
    assert_eq!(listed.len(), 1);
    assert_eq!(string(&listed[0][0]), s.connection_id());
}

#[test]
fn terminate_connections_reports_unknown_ids_and_parses() {
    let svc = Arc::new(NativeService::in_memory());
    let out = rows(&svc, "TERMINATE CONNECTIONS 'bolt-999999', 'http-77'");
    assert_eq!(out.len(), 2);
    assert!(out.iter().all(|r| string(&r[2]) == "No connection found"));
    assert!(parse("TERMINATE CONNECTIONS").is_err());
    let err = parse("TERMINATE SESSIONS 'x'").unwrap_err().to_string();
    assert!(err.contains("TRANSACTIONS or CONNECTIONS"), "{err}");
}

mod over_tcp {
    use super::*;

    use drevo::bolt::handshake::MAGIC_PREAMBLE;
    use drevo::bolt::listener::accept_and_run_session_durable_with_security;
    use drevo::bolt::packstream::{decode, encode};
    use drevo::bolt::session::{BEGIN, HELLO, PULL, RUN};
    use drevo::database_registry::DatabaseRegistry;
    use std::time::Duration;
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

    #[tokio::test]
    async fn terminating_a_bolt_connection_closes_it_and_rolls_back() {
        let svc = Arc::new(NativeService::in_memory());
        let registry = Arc::new(DatabaseRegistry::new(Arc::clone(&svc)));
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

        let dict = |m: BTreeMap<String, BoltValue>| BoltValue::Dictionary(m);
        let mut batch = frame(
            HELLO,
            vec![dict(BTreeMap::from([(
                "user_agent".to_string(),
                BoltValue::String("leaky-pool/1.0".into()),
            )]))],
        );
        batch.extend(frame(BEGIN, vec![dict(BTreeMap::new())]));
        batch.extend(frame(
            RUN,
            vec![
                BoltValue::String("CREATE (:Task {title: 'half-filed'})".into()),
                dict(BTreeMap::new()),
                dict(BTreeMap::new()),
            ],
        ));
        batch.extend(frame(
            PULL,
            vec![dict(BTreeMap::from([(
                "n".to_string(),
                BoltValue::Integer(-1),
            )]))],
        ));
        client.write_all(&batch).await.unwrap();
        for _ in 0..4 {
            timeout(Duration::from_secs(5), read_one(&mut client))
                .await
                .unwrap()
                .expect("a reply");
        }

        let listed = rows(
            &svc,
            "SHOW CONNECTIONS YIELD connectionId, userAgent WHERE userAgent = 'leaky-pool/1.0'",
        );
        assert_eq!(listed.len(), 1);
        let id = string(&listed[0][0]);
        let out = rows(&svc, &format!("TERMINATE CONNECTIONS '{id}'"));
        assert_eq!(string(&out[0][2]), "Connection found");

        let closed = timeout(Duration::from_secs(5), read_one(&mut client))
            .await
            .expect("closed promptly");
        assert!(closed.is_none(), "{closed:?}");
        timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(rows(&svc, "SHOW CONNECTIONS").is_empty());
        let open = rows(&svc, "SHOW TRANSACTIONS YIELD isExplicit WHERE isExplicit");
        assert!(open.is_empty(), "rolled back: {open:?}");
        let tasks = rows(&svc, "MATCH (t:Task) RETURN count(t)");
        assert_eq!(tasks[0][0], Value::Integer(0));
    }
}

#[cfg(feature = "auth")]
mod privileges {
    use super::*;

    use drevo::security::Security;
    use drevo_auth::catalog::MemoryStore;
    use drevo_auth::users::{AuthManager, NewUser};

    const PW: &str = "task-manager-pw";

    fn records(replies: &[ServerMessage]) -> Vec<Vec<BoltValue>> {
        replies
            .iter()
            .filter_map(|m| match m {
                ServerMessage::Record { fields } => Some(fields.clone()),
                _ => None,
            })
            .collect()
    }

    fn users() -> Arc<AuthManager> {
        let users = Arc::new(AuthManager::open(Arc::new(MemoryStore::new())).unwrap());
        for (name, role) in [("ann", "editor"), ("root", "admin")] {
            users
                .create_user(NewUser::new(name, PW).change_required(false))
                .unwrap();
            users.grant_role(role, name).unwrap();
        }
        users
    }

    fn logged_in(svc: &Arc<NativeService>, security: &Security, user: &str) -> Session<'static> {
        let mut s = Session::new_durable(Arc::clone(svc)).with_security(security.clone());
        let replies = s.handle(ClientMessage::Hello {
            extra: BTreeMap::from([
                (
                    "user_agent".to_string(),
                    BoltValue::String(format!("{user}-app")),
                ),
                ("scheme".to_string(), BoltValue::String("basic".into())),
                ("principal".to_string(), BoltValue::String(user.into())),
                ("credentials".to_string(), BoltValue::String(PW.into())),
            ]),
        });
        assert!(
            matches!(replies.as_slice(), [ServerMessage::Success { .. }]),
            "{replies:?}"
        );
        s
    }

    fn column(replies: &[ServerMessage], i: usize) -> Vec<BoltValue> {
        records(replies).into_iter().map(|r| r[i].clone()).collect()
    }

    #[test]
    fn users_see_their_own_connections_and_admins_all() {
        let svc = Arc::new(NativeService::in_memory());
        let security = Security::new(users());
        let mut ann = logged_in(&svc, &security, "ann");
        let mut root = logged_in(&svc, &security, "root");

        let mine = column(&run(&mut ann, "SHOW CONNECTIONS YIELD username"), 0);
        assert_eq!(mine, vec![BoltValue::String("ann".into())]);
        let all = column(&run(&mut root, "SHOW CONNECTIONS YIELD username"), 0);
        assert_eq!(all.len(), 2);

        // Someone else's connection reads as not found.
        let q = format!("TERMINATE CONNECTIONS '{}'", root.connection_id());
        let out = column(&run(&mut ann, &q), 2);
        assert_eq!(out, vec![BoltValue::String("No connection found".into())]);
        let q = format!("TERMINATE CONNECTIONS '{}'", ann.connection_id());
        let out = column(&run(&mut root, &q), 2);
        assert_eq!(out, vec![BoltValue::String("Connection found".into())]);
    }

    #[test]
    fn web_ui_sessions_are_listed_and_ended() {
        let svc = Arc::new(NativeService::in_memory());
        let users = users();
        let security = Security::new(Arc::clone(&users));
        svc.transactions()
            .set_http_sessions(Arc::new(security.clone()));
        let token = users.issue_session("ann").unwrap();

        let listed = rows(
            &svc,
            "SHOW CONNECTIONS YIELD connectionId, connector, username WHERE connector = 'http'",
        );
        assert_eq!(listed.len(), 1);
        let id = string(&listed[0][0]);
        assert!(id.starts_with("http-"), "{id}");
        assert_eq!(string(&listed[0][2]), "ann");
        assert!(
            !format!("{listed:?}").contains(&token),
            "the token stays hidden"
        );

        let out = rows(&svc, &format!("TERMINATE CONNECTIONS '{id}'"));
        assert_eq!(string(&out[0][1]), "ann");
        assert_eq!(string(&out[0][2]), "Connection found");
        assert!(users
            .authenticate(&drevo_auth::authn::Credentials::Token(token))
            .is_err());
        assert!(rows(&svc, "SHOW CONNECTIONS").is_empty());
    }
}
