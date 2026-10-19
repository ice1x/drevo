//! Seeing and stopping what runs: `SHOW TRANSACTIONS` lists the running
//! statements and open transactions of every database, and `TERMINATE
//! TRANSACTIONS` stops them, as in Neo4j.
//!
//! - A terminated statement stops at its next cancellation check with
//!   `Neo.ClientError.Transaction.Terminated`.
//! - A terminated explicit transaction is rolled back: its next statement and
//!   its commit are refused.
#![cfg(feature = "http")]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use drevo::bolt::packstream::Value as Bolt;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::executor::{ExecError, Value};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;
use drevo::transactions::{self, Caller, TransactionInfo};

const TERMINATED: &str = "Neo.ClientError.Transaction.Terminated";

fn run(svc: &NativeService, q: &str) -> Vec<Vec<Value>> {
    svc.execute(&parse(q).unwrap(), HashMap::new())
        .unwrap_or_else(|e| panic!("{q}: {e}"))
        .rows
}

fn run_as(svc: &NativeService, caller: Caller, q: &str) -> Result<Vec<Vec<Value>>, ExecError> {
    transactions::scope(caller, q, || {
        svc.execute(&parse(q).unwrap(), HashMap::new())
            .map(|r| r.rows)
    })
}

fn caller(user: &str) -> Caller {
    Caller {
        surface: "bolt",
        client: Some("10.0.0.7:51234".into()),
        user: Some(user.into()),
        connection: None,
    }
}

/// A graph big enough that a three-way cartesian product with a filter runs
/// for many seconds.
fn slow_graph() -> Arc<NativeService> {
    let svc = Arc::new(NativeService::in_memory());
    run(&svc, "UNWIND range(1, 400) AS x CREATE (:Item {x: x})");
    svc
}

const SLOW: &str = "MATCH (a:Item), (b:Item), (c:Item) WHERE a.x + b.x + c.x < 0 RETURN count(*)";

/// Wait until a statement whose query is `query` is listed.
fn wait_for(svc: &NativeService, query: &str) -> TransactionInfo {
    let start = Instant::now();
    loop {
        if let Some(t) = svc
            .transactions()
            .list()
            .into_iter()
            .find(|t| t.query.as_deref() == Some(query))
        {
            return t;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{query} never ran"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn column(rows: &[Vec<Value>], columns: &[String], name: &str) -> Vec<Value> {
    let i = columns.iter().position(|c| c == name).unwrap_or_else(|| {
        panic!("no column {name} in {columns:?}");
    });
    rows.iter().map(|r| r[i].clone()).collect()
}

fn show(svc: &NativeService, q: &str) -> (Vec<String>, Vec<Vec<Value>>) {
    let r = svc.execute(&parse(q).unwrap(), HashMap::new()).unwrap();
    (r.columns, r.rows)
}

// ── The registry ─────────────────────────────────────────────────────

#[test]
fn a_running_statement_is_listed_with_who_runs_it() {
    let svc = slow_graph();
    let worker = {
        let svc = Arc::clone(&svc);
        std::thread::spawn(move || run_as(&svc, caller("ann"), SLOW))
    };
    let t = wait_for(&svc, SLOW);
    assert!(t.id.starts_with("drevo-transaction-"), "{}", t.id);
    assert_eq!(t.database, "drevo");
    assert_eq!(t.user.as_deref(), Some("ann"));
    assert_eq!(t.client.as_deref(), Some("10.0.0.7:51234"));
    assert_eq!(t.surface, "bolt");
    assert!(!t.explicit);
    assert!(svc.transactions().terminate(&t.id).is_some());
    let result = worker.join().unwrap();
    assert!(matches!(result, Err(ExecError::Terminated)), "{result:?}");
    // Finished statements leave the list.
    assert!(svc.transactions().list().is_empty());
}

#[test]
fn termination_stops_a_statement_quickly() {
    let svc = slow_graph();
    let worker = {
        let svc = Arc::clone(&svc);
        std::thread::spawn(move || {
            let started = Instant::now();
            let r = run_as(&svc, caller("ann"), SLOW);
            (r, started)
        })
    };
    let t = wait_for(&svc, SLOW);
    let asked = Instant::now();
    svc.transactions().terminate(&t.id);
    let (result, _) = worker.join().unwrap();
    assert!(matches!(result, Err(ExecError::Terminated)));
    assert!(
        asked.elapsed() < Duration::from_secs(2),
        "stopped after {:?}",
        asked.elapsed()
    );
}

#[test]
fn a_long_read_right_after_a_write_does_not_hold_up_other_statements() {
    // The long statement is the first to see the indexes behind the write:
    // it brings them up to date, then must run without holding them.
    let svc = slow_graph();
    let worker = {
        let svc = Arc::clone(&svc);
        std::thread::spawn(move || run_as(&svc, caller("ann"), SLOW))
    };
    let t = wait_for(&svc, SLOW);
    let started = Instant::now();
    assert_eq!(run(&svc, "RETURN 1"), [vec![Value::Integer(1)]]);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "waited {:?} for the long statement",
        started.elapsed()
    );
    assert!(!worker.is_finished());
    svc.transactions().terminate(&t.id);
    assert!(matches!(worker.join().unwrap(), Err(ExecError::Terminated)));
}

#[test]
fn an_unknown_id_terminates_nothing() {
    let svc = NativeService::in_memory();
    assert!(svc
        .transactions()
        .terminate("drevo-transaction-999")
        .is_none());
    assert!(svc.transactions().terminate("garbage").is_none());
}

#[test]
fn every_database_of_a_server_shares_one_list() {
    let registry =
        drevo::database_registry::DatabaseRegistry::new(Arc::new(NativeService::in_memory()));
    let erp = registry.create_in_memory("erp").unwrap();
    run(&erp, "UNWIND range(1, 400) AS x CREATE (:Item {x: x})");
    let worker = {
        let erp = Arc::clone(&erp);
        std::thread::spawn(move || run_as(&erp, caller("ann"), SLOW))
    };
    let default = registry.get(registry.default_name()).unwrap();
    let t = wait_for(&default, SLOW);
    assert_eq!(t.database, "erp");
    assert!(t.id.starts_with("erp-transaction-"), "{}", t.id);
    default.transactions().terminate(&t.id);
    assert!(matches!(worker.join().unwrap(), Err(ExecError::Terminated)));
}

// ── Cypher ───────────────────────────────────────────────────────────

#[test]
fn show_transactions_lists_itself_in_neo4js_columns() {
    let svc = NativeService::in_memory();
    let (columns, rows) = transactions::scope(caller("ann"), "SHOW TRANSACTIONS", || {
        show(&svc, "SHOW TRANSACTIONS")
    });
    for c in [
        "database",
        "transactionId",
        "currentQueryId",
        "connectionId",
        "clientAddress",
        "username",
        "currentQuery",
        "startTime",
        "status",
        "elapsedTime",
        "protocol",
    ] {
        assert!(columns.iter().any(|x| x == c), "{c} missing: {columns:?}");
    }
    assert_eq!(rows.len(), 1, "the SHOW itself: {rows:?}");
    assert_eq!(
        column(&rows, &columns, "currentQuery"),
        [Value::String("SHOW TRANSACTIONS".into())]
    );
    assert_eq!(
        column(&rows, &columns, "username"),
        [Value::String("ann".into())]
    );
    assert_eq!(
        column(&rows, &columns, "status"),
        [Value::String("Running".into())]
    );
    let Value::String(start) = &column(&rows, &columns, "startTime")[0] else {
        panic!("startTime is a string");
    };
    assert!(start.ends_with('Z') && start.contains('T'), "{start}");
}

#[test]
fn show_transactions_takes_ids_yield_and_where() {
    let svc = slow_graph();
    let worker = {
        let svc = Arc::clone(&svc);
        std::thread::spawn(move || run_as(&svc, caller("ann"), SLOW))
    };
    let t = wait_for(&svc, SLOW);
    let (_, rows) = show(&svc, &format!("SHOW TRANSACTION '{}'", t.id));
    assert_eq!(rows.len(), 1);
    let (columns, rows) = show(
        &svc,
        "SHOW TRANSACTIONS YIELD transactionId AS id, username WHERE username = 'ann' RETURN id",
    );
    assert_eq!(columns, ["id"]);
    assert_eq!(rows, [vec![Value::String(t.id.clone())]]);
    let (_, rows) = show(&svc, "SHOW TRANSACTIONS 'nope-transaction-1', 'x'");
    assert!(rows.is_empty());
    svc.transactions().terminate(&t.id);
    worker.join().unwrap().unwrap_err();
}

#[test]
fn terminate_transactions_stops_them_and_says_what_happened() {
    let svc = slow_graph();
    let worker = {
        let svc = Arc::clone(&svc);
        std::thread::spawn(move || run_as(&svc, caller("ann"), SLOW))
    };
    let t = wait_for(&svc, SLOW);
    let mut params = HashMap::new();
    params.insert(
        "ids".to_string(),
        Value::List(vec![
            Value::String(t.id.clone()),
            Value::String("drevo-transaction-424242".into()),
        ]),
    );
    let r = svc
        .execute(&parse("TERMINATE TRANSACTIONS $ids").unwrap(), params)
        .unwrap();
    assert_eq!(r.columns, ["transactionId", "username", "message"]);
    assert_eq!(
        r.rows,
        [
            vec![
                Value::String(t.id.clone()),
                Value::String("ann".into()),
                Value::String("Transaction terminated.".into()),
            ],
            vec![
                Value::String("drevo-transaction-424242".into()),
                Value::Null,
                Value::String("Transaction not found.".into()),
            ],
        ]
    );
    assert!(matches!(worker.join().unwrap(), Err(ExecError::Terminated)));
}

#[test]
fn transaction_commands_parse_as_neo4j_writes_them() {
    for q in [
        "SHOW TRANSACTIONS",
        "SHOW TRANSACTION",
        "SHOW TRANSACTIONS 'neo4j-transaction-1', 'neo4j-transaction-2'",
        "SHOW TRANSACTIONS $ids",
        "SHOW TRANSACTIONS YIELD *",
        "SHOW TRANSACTIONS WHERE username = 'ann'",
        "TERMINATE TRANSACTION 'neo4j-transaction-1'",
        "TERMINATE TRANSACTIONS 'a', 'b'",
        "TERMINATE TRANSACTIONS $ids",
        "TERMINATE TRANSACTIONS 'a' YIELD message",
    ] {
        parse(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    for q in [
        "TERMINATE",
        "TERMINATE TRANSACTIONS",
        "SHOW TRANSACTIONS 1 2",
    ] {
        assert!(parse(q).is_err(), "{q} should not parse");
    }
}

// ── Bolt ─────────────────────────────────────────────────────────────

fn dict(e: Vec<(&str, Bolt)>) -> BTreeMap<String, Bolt> {
    e.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

fn failure_code(replies: &[ServerMessage]) -> Option<Bolt> {
    replies.iter().find_map(|m| match m {
        ServerMessage::Failure { metadata } => metadata.get("code").cloned(),
        _ => None,
    })
}

fn bolt_run(session: &mut Session<'_>, query: &str) -> Vec<ServerMessage> {
    let mut replies = session.handle(ClientMessage::Run {
        query: query.into(),
        parameters: dict(vec![]),
        extra: dict(vec![]),
    });
    replies.extend(session.handle(ClientMessage::Pull {
        extra: dict(vec![("n", Bolt::Integer(-1))]),
    }));
    replies
}

fn open_bolt_tx(svc: &Arc<NativeService>) -> Session<'static> {
    let mut session = Session::new_durable(Arc::clone(svc));
    session.handle(ClientMessage::Hello {
        extra: dict(vec![]),
    });
    session.handle(ClientMessage::Begin {
        extra: dict(vec![]),
    });
    assert_eq!(
        failure_code(&bolt_run(&mut session, "CREATE (:Order)")),
        None
    );
    session
}

fn explicit(svc: &NativeService) -> TransactionInfo {
    svc.transactions()
        .list()
        .into_iter()
        .find(|t| t.explicit)
        .expect("an explicit transaction is open")
}

#[test]
fn an_open_explicit_transaction_is_listed_between_statements() {
    let svc = Arc::new(NativeService::in_memory());
    let mut session = open_bolt_tx(&svc);
    let t = explicit(&svc);
    assert_eq!(t.surface, "bolt");
    assert_eq!(t.query, None, "idle between statements");
    session.handle(ClientMessage::Rollback);
    assert!(svc.transactions().list().is_empty(), "rolled back: gone");
}

#[test]
fn a_terminated_explicit_transaction_refuses_its_next_statement_and_keeps_nothing() {
    let svc = Arc::new(NativeService::in_memory());
    let mut session = open_bolt_tx(&svc);
    svc.transactions().terminate(&explicit(&svc).id).unwrap();
    assert_eq!(
        failure_code(&bolt_run(&mut session, "CREATE (:Order)")),
        Some(Bolt::String(TERMINATED.into()))
    );
    session.handle(ClientMessage::Reset);
    assert_eq!(
        run(&svc, "MATCH (o:Order) RETURN count(o)"),
        [vec![Value::Integer(0)]]
    );
    assert!(svc.transactions().list().is_empty());
}

#[test]
fn a_terminated_explicit_transaction_cannot_commit() {
    let svc = Arc::new(NativeService::in_memory());
    let mut session = open_bolt_tx(&svc);
    svc.transactions().terminate(&explicit(&svc).id).unwrap();
    assert_eq!(
        failure_code(&session.handle(ClientMessage::Commit)),
        Some(Bolt::String(TERMINATED.into()))
    );
    assert_eq!(
        run(&svc, "MATCH (o:Order) RETURN count(o)"),
        [vec![Value::Integer(0)]]
    );
    assert!(svc.transactions().list().is_empty());
}

#[test]
fn a_bolt_statement_terminated_from_another_connection_fails_with_terminated() {
    let svc = slow_graph();
    let worker = {
        let svc = Arc::clone(&svc);
        std::thread::spawn(move || {
            let mut session = Session::new_durable(svc);
            session.handle(ClientMessage::Hello {
                extra: dict(vec![]),
            });
            failure_code(&bolt_run(&mut session, SLOW))
        })
    };
    let t = wait_for(&svc, SLOW);
    assert_eq!(t.surface, "bolt");
    let mut other = Session::new_durable(Arc::clone(&svc));
    other.handle(ClientMessage::Hello {
        extra: dict(vec![]),
    });
    let replies = bolt_run(&mut other, &format!("TERMINATE TRANSACTION '{}'", t.id));
    assert_eq!(failure_code(&replies), None, "{replies:?}");
    assert_eq!(
        worker.join().unwrap(),
        Some(Bolt::String(TERMINATED.into()))
    );
}

// ── HTTP ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn http_statements_are_listed_with_their_protocol() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    let state = drevo::native_api::NativeApiState::new(Arc::new(NativeService::in_memory()));
    let app = drevo::native_api::build_native_router(state);
    let body = serde_json::json!({
        "query": "SHOW TRANSACTIONS YIELD protocol, currentQuery RETURN protocol, currentQuery"
    })
    .to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/cypher")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["rows"][0][0], "http", "{json}");
    assert_eq!(
        json["rows"][0][1],
        "SHOW TRANSACTIONS YIELD protocol, currentQuery RETURN protocol, currentQuery"
    );
}

// ── Privileges ───────────────────────────────────────────────────────

#[cfg(feature = "auth")]
mod privileges {
    use super::*;
    use drevo::cypher::executor::Restriction;
    use drevo_auth::catalog::MemoryStore;
    use drevo_auth::users::{AuthManager, NewUser};

    fn context(users: &AuthManager, name: &str) -> drevo_auth::privileges::SecurityContext {
        users.security_context(name, "drevo")
    }

    fn users() -> AuthManager {
        let users = AuthManager::open(Arc::new(MemoryStore::new())).unwrap();
        for (name, role) in [("ann", "reader"), ("bob", "editor"), ("root", "admin")] {
            users
                .create_user(NewUser::new(name, "long-password"))
                .unwrap();
            users.grant_role(role, name).unwrap();
        }
        users
    }

    fn show_as(svc: &NativeService, users: &AuthManager, who: &str, q: &str) -> Vec<Vec<Value>> {
        let context = context(users, who);
        transactions::scope(caller(who), q, || {
            svc.execute_as(
                &parse(q).unwrap(),
                HashMap::new(),
                Some(Restriction {
                    context: &context,
                    database: "drevo",
                }),
            )
            .unwrap()
            .rows
        })
    }

    #[test]
    fn users_see_and_stop_their_own_and_admins_everyones() {
        let svc = slow_graph();
        let users = users();
        let worker = {
            let svc = Arc::clone(&svc);
            std::thread::spawn(move || run_as(&svc, caller("bob"), SLOW))
        };
        let t = wait_for(&svc, SLOW);
        let ids = "SHOW TRANSACTIONS YIELD transactionId RETURN transactionId";
        // ann sees only her own SHOW, and cannot stop bob's statement.
        assert_eq!(show_as(&svc, &users, "ann", ids).len(), 1);
        let rows = show_as(
            &svc,
            &users,
            "ann",
            &format!("TERMINATE TRANSACTION '{}'", t.id),
        );
        assert_eq!(rows[0][2], Value::String("Transaction not found.".into()));
        assert!(!worker.is_finished());
        // An admin holds TRANSACTION MANAGEMENT on every database.
        assert_eq!(show_as(&svc, &users, "root", ids).len(), 2);
        let rows = show_as(
            &svc,
            &users,
            "root",
            &format!("TERMINATE TRANSACTION '{}'", t.id),
        );
        assert_eq!(rows[0][2], Value::String("Transaction terminated.".into()));
        assert!(matches!(worker.join().unwrap(), Err(ExecError::Terminated)));
    }

    #[test]
    fn a_user_may_stop_their_own_statement() {
        let svc = slow_graph();
        let users = users();
        let worker = {
            let svc = Arc::clone(&svc);
            std::thread::spawn(move || run_as(&svc, caller("ann"), SLOW))
        };
        let t = wait_for(&svc, SLOW);
        let rows = show_as(
            &svc,
            &users,
            "ann",
            &format!("TERMINATE TRANSACTION '{}'", t.id),
        );
        assert_eq!(rows[0][2], Value::String("Transaction terminated.".into()));
        assert!(matches!(worker.join().unwrap(), Err(ExecError::Terminated)));
    }
}
