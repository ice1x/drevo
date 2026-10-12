//! `IS NOT NULL`, composite `IS UNIQUE` and `IS NODE KEY` constraints through
//! Cypher, Bolt and HTTP (issue #606).
//!
//! Uniqueness is checked on every write. Existence is checked at the end of a
//! statement (a writing statement runs as one transaction while such a
//! constraint is declared) and at the commit of an explicit transaction, so a
//! statement or transaction that leaves a required property missing writes
//! nothing.

#![cfg(feature = "http")]

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::bolt::packstream::Value as Bolt;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::error::DrevoError;
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use http_body_util::BodyExt;
use tower::ServiceExt;

static NEXT: AtomicU64 = AtomicU64::new(0);
const CODE: &str = "Neo.ClientError.Schema.ConstraintValidationFailed";

fn run(svc: &NativeService, q: &str) -> Result<ExecResult, ExecError> {
    svc.execute(
        &parse(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}")),
        HashMap::new(),
    )
}

fn run_ok(svc: &NativeService, q: &str) -> ExecResult {
    run(svc, q).unwrap_or_else(|e| panic!("execute `{q}`: {e:?}"))
}

fn count(svc: &NativeService, q: &str) -> i64 {
    match run_ok(svc, q).rows[0][0] {
        Value::Integer(n) => n,
        ref other => panic!("{other:?}"),
    }
}

fn is_violation(r: &Result<ExecResult, ExecError>) -> bool {
    matches!(
        r,
        Err(ExecError::Storage(DrevoError::ConstraintViolation { .. }))
    )
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

fn list(items: &[&str]) -> Value {
    Value::List(items.iter().map(|i| s(i)).collect())
}

/// Bug tracker: every ticket has a status; tickets are keyed by project and
/// number; a release name is unique per product.
fn tracker() -> NativeService {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CREATE CONSTRAINT ticket_status FOR (t:Ticket) REQUIRE t.status IS NOT NULL",
    );
    run_ok(
        &svc,
        "CREATE CONSTRAINT ticket_key FOR (t:Ticket) REQUIRE (t.project, t.number) IS NODE KEY",
    );
    run_ok(
        &svc,
        "CREATE CONSTRAINT release_name FOR (r:Release) REQUIRE (r.product, r.name) IS UNIQUE",
    );
    run_ok(
        &svc,
        "CREATE (:Ticket {title: 'Crash on start', project: 'APP', number: 1, status: 'open'})",
    );
    svc
}

// ── IS NOT NULL ─────────────────────────────────────────────────────────────

#[test]
fn a_node_without_the_required_property_is_refused() {
    let svc = tracker();
    assert!(is_violation(&run(
        &svc,
        "CREATE (:Ticket {title: 'No status', project: 'APP', number: 2})"
    )));
    assert!(is_violation(&run(
        &svc,
        "CREATE (:Ticket {title: 'Null status', project: 'APP', number: 2, status: null})"
    )));
    // A secondary label is covered too.
    assert!(is_violation(&run(
        &svc,
        "CREATE (:Note:Ticket {title: 'Tagged', project: 'APP', number: 2})"
    )));
    assert_eq!(count(&svc, "MATCH (t:Ticket) RETURN count(t)"), 1);
    // Other labels are free.
    run_ok(&svc, "CREATE (:Comment {title: 'no status needed'})");
}

#[test]
fn the_property_may_be_set_later_in_the_same_statement() {
    let svc = tracker();
    run_ok(
        &svc,
        "CREATE (t:Ticket {title: 'Slow search', project: 'APP', number: 2}) SET t.status = 'open'",
    );
    run_ok(
        &svc,
        "MERGE (t:Ticket {project: 'APP', number: 3}) \
         ON CREATE SET t.status = 'new', t.title = 'Typo'",
    );
    assert_eq!(count(&svc, "MATCH (t:Ticket) RETURN count(t)"), 3);
}

#[test]
fn a_statement_that_breaks_it_writes_nothing() {
    let svc = tracker();
    assert!(is_violation(&run(
        &svc,
        "CREATE (:Ticket {title: 'ok', project: 'APP', number: 2, status: 'open'}), \
                (:Ticket {title: 'bad', project: 'APP', number: 3})"
    )));
    assert!(is_violation(&run(
        &svc,
        "UNWIND [2, 3] AS n \
         CREATE (t:Ticket {title: 'bulk', project: 'APP', number: n}) \
         SET t.status = CASE n WHEN 2 THEN 'open' ELSE null END"
    )));
    assert_eq!(count(&svc, "MATCH (t:Ticket) RETURN count(t)"), 1);
}

#[test]
fn removing_or_nulling_the_property_is_refused() {
    let svc = tracker();
    assert!(is_violation(&run(
        &svc,
        "MATCH (t:Ticket {number: 1}) REMOVE t.status"
    )));
    assert!(is_violation(&run(
        &svc,
        "MATCH (t:Ticket {number: 1}) SET t.status = null"
    )));
    assert!(is_violation(&run(
        &svc,
        "MATCH (t:Ticket {number: 1}) SET t = {title: 'replaced'}"
    )));
    let res = run_ok(&svc, "MATCH (t:Ticket {number: 1}) RETURN t.status");
    assert_eq!(res.rows, vec![vec![s("open")]]);
    // Changing the value, and other writes, still work.
    run_ok(&svc, "MATCH (t:Ticket {number: 1}) SET t.status = 'closed'");
    run_ok(&svc, "MATCH (t:Ticket {number: 1}) DETACH DELETE t");
    assert_eq!(count(&svc, "MATCH (t:Ticket) RETURN count(t)"), 0);
}

#[test]
fn reads_and_other_writes_keep_working() {
    let svc = tracker();
    let res = run_ok(
        &svc,
        "MATCH (t:Ticket) WHERE t.status = 'open' RETURN t.title",
    );
    assert_eq!(res.rows, vec![vec![s("Crash on start")]]);
    run_ok(
        &svc,
        "MATCH (t:Ticket {number: 1}) CREATE (t)-[:HAS]->(:Comment {title: 'repro'})",
    );
    assert_eq!(
        count(&svc, "MATCH (:Ticket)-[:HAS]->(c:Comment) RETURN count(c)"),
        1
    );
}

#[test]
fn the_service_api_is_covered_too() {
    use drevo::engine::GraphEngine;
    use drevo::model::{NewNode, Properties};
    let svc = tracker();
    let bare = NewNode {
        kind: "Ticket".into(),
        title: "API".into(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    };
    assert!(svc.graph().create_node(bare).is_err());
}

// ── composite IS UNIQUE and NODE KEY ───────────────────────────────────────

#[test]
fn a_composite_unique_tuple_is_refused_only_when_repeated() {
    let svc = tracker();
    run_ok(
        &svc,
        "CREATE (:Release {product: 'app', name: '1.0'}), (:Release {product: 'web', name: '1.0'})",
    );
    assert!(is_violation(&run(
        &svc,
        "CREATE (:Release {product: 'app', name: '1.0'})"
    )));
    // Missing part of the tuple: not constrained.
    run_ok(
        &svc,
        "CREATE (:Release {name: '1.0'}), (:Release {name: '1.0'})",
    );
    assert!(is_violation(&run(
        &svc,
        "MATCH (r:Release {product: 'web'}) SET r.product = 'app'"
    )));
}

#[test]
fn a_node_key_needs_every_property_and_a_new_tuple() {
    let svc = tracker();
    assert!(is_violation(&run(
        &svc,
        "CREATE (:Ticket {title: 'no number', project: 'APP', status: 'open'})"
    )));
    assert!(is_violation(&run(
        &svc,
        "CREATE (:Ticket {title: 'dup', project: 'APP', number: 1, status: 'open'})"
    )));
    run_ok(
        &svc,
        "CREATE (:Ticket {title: 'other project', project: 'WEB', number: 1, status: 'open'})",
    );
    // MERGE on the key finds the existing ticket instead of failing.
    run_ok(
        &svc,
        "MERGE (t:Ticket {project: 'APP', number: 1}) ON MATCH SET t.status = 'triaged'",
    );
    assert_eq!(count(&svc, "MATCH (t:Ticket) RETURN count(t)"), 2);
}

// ── DDL ─────────────────────────────────────────────────────────────────────

#[test]
fn neo4j_4_and_5_spellings_are_accepted() {
    let svc = NativeService::in_memory();
    for q in [
        "CREATE CONSTRAINT ON (u:User) ASSERT exists(u.email)",
        "CREATE CONSTRAINT ON (u:User) ASSERT u.name IS NOT NULL",
        "CREATE CONSTRAINT ON (p:Person) ASSERT (p.first, p.last) IS NODE KEY",
        "CREATE CONSTRAINT FOR (d:Device) REQUIRE d.serial IS KEY",
        "CREATE CONSTRAINT IF NOT EXISTS FOR (b:Book) REQUIRE (b.title, b.year) IS UNIQUE \
         OPTIONS {indexProvider: 'range-1.0'}",
    ] {
        run_ok(&svc, q);
    }
    let res = run_ok(
        &svc,
        "SHOW CONSTRAINTS YIELD name, type, properties RETURN name, type, properties",
    );
    assert_eq!(
        res.rows,
        vec![
            vec![
                s("constraint_User_email_not_null"),
                s("NODE_PROPERTY_EXISTENCE"),
                list(&["email"])
            ],
            vec![
                s("constraint_User_name_not_null"),
                s("NODE_PROPERTY_EXISTENCE"),
                list(&["name"])
            ],
            vec![
                s("constraint_Person_first_last_node_key"),
                s("NODE_KEY"),
                list(&["first", "last"])
            ],
            vec![
                s("constraint_Device_serial_node_key"),
                s("NODE_KEY"),
                list(&["serial"])
            ],
            vec![
                s("constraint_Book_title_year"),
                s("UNIQUENESS"),
                list(&["title", "year"])
            ],
        ]
    );
    assert!(is_violation(&run(&svc, "CREATE (:User {name: 'ada'})")));
    assert!(is_violation(&run(&svc, "CREATE (:Device)")));
}

#[test]
fn show_constraints_filters_by_type() {
    let svc = tracker();
    let names = |q: &str| -> Vec<Value> {
        run_ok(&svc, &format!("{q} YIELD name RETURN name"))
            .rows
            .into_iter()
            .map(|mut r| r.remove(0))
            .collect()
    };
    assert_eq!(names("SHOW CONSTRAINTS").len(), 3);
    assert_eq!(names("SHOW UNIQUE CONSTRAINTS"), vec![s("release_name")]);
    assert_eq!(names("SHOW NODE KEY CONSTRAINTS"), vec![s("ticket_key")]);
    assert_eq!(names("SHOW KEY CONSTRAINTS"), vec![s("ticket_key")]);
    assert_eq!(
        names("SHOW NODE PROPERTY EXISTENCE CONSTRAINTS"),
        vec![s("ticket_status")]
    );
    assert_eq!(
        names("SHOW EXISTENCE CONSTRAINTS"),
        vec![s("ticket_status")]
    );
    assert!(names("SHOW RELATIONSHIP EXISTENCE CONSTRAINTS").is_empty());
    // An existence constraint owns no index.
    let res = run_ok(
        &svc,
        "SHOW EXISTENCE CONSTRAINTS YIELD ownedIndex RETURN ownedIndex",
    );
    assert_eq!(res.rows, vec![vec![Value::Null]]);
}

#[test]
fn kinds_on_one_property_are_separate_rules() {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CREATE CONSTRAINT FOR (u:User) REQUIRE u.email IS UNIQUE",
    );
    run_ok(
        &svc,
        "CREATE CONSTRAINT FOR (u:User) REQUIRE u.email IS NOT NULL",
    );
    let err = run(
        &svc,
        "CREATE CONSTRAINT again FOR (u:User) REQUIRE u.email IS NOT NULL",
    )
    .expect_err("equivalent");
    assert!(err.to_string().contains("equivalent"), "{err}");
    assert!(is_violation(&run(&svc, "CREATE (:User {title: 'x'})")));
    run_ok(&svc, "CREATE (:User {email: 'a@x'})");
    assert!(is_violation(&run(&svc, "CREATE (:User {email: 'a@x'})")));
    run_ok(&svc, "DROP CONSTRAINT constraint_User_email_not_null");
    run_ok(&svc, "CREATE (:User {title: 'x'})");
    assert!(is_violation(&run(&svc, "CREATE (:User {email: 'a@x'})")));
}

#[test]
fn creating_over_violating_data_fails_and_stores_nothing() {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CREATE (:Ticket {project: 'APP', number: 1}), (:Ticket {project: 'APP', number: 1})",
    );
    for q in [
        "CREATE CONSTRAINT FOR (t:Ticket) REQUIRE t.status IS NOT NULL",
        "CREATE CONSTRAINT FOR (t:Ticket) REQUIRE (t.project, t.number) IS UNIQUE",
        "CREATE CONSTRAINT FOR (t:Ticket) REQUIRE (t.project, t.number) IS NODE KEY",
    ] {
        let err = run(&svc, q).expect_err(q);
        assert!(err.to_string().contains("cannot create"), "{err}");
    }
    assert!(run_ok(&svc, "SHOW CONSTRAINTS").rows.is_empty());
}

#[test]
fn constraints_survive_a_restart() {
    let dir = std::env::temp_dir().join(format!(
        "drevo_schema_ddl_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let wal = dir.join("native.wal");
    {
        let svc = NativeService::open(&wal).unwrap();
        run_ok(
            &svc,
            "CREATE CONSTRAINT ticket_status FOR (t:Ticket) REQUIRE t.status IS NOT NULL",
        );
        run_ok(
            &svc,
            "CREATE CONSTRAINT ticket_key FOR (t:Ticket) REQUIRE (t.project, t.number) IS NODE KEY",
        );
        run_ok(
            &svc,
            "CREATE (:Ticket {project: 'APP', number: 1, status: 'open'})",
        );
    }
    let svc = NativeService::open(&wal).unwrap();
    assert_eq!(run_ok(&svc, "SHOW CONSTRAINTS").rows.len(), 2);
    assert!(is_violation(&run(
        &svc,
        "CREATE (:Ticket {project: 'APP', number: 2})"
    )));
    assert!(is_violation(&run(
        &svc,
        "CREATE (:Ticket {project: 'APP', number: 1, status: 'new'})"
    )));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unsupported_forms_stay_no_ops() {
    let svc = NativeService::in_memory();
    for q in [
        "CREATE CONSTRAINT FOR ()-[r:KNOWS]-() REQUIRE r.since IS NOT NULL",
        "CREATE CONSTRAINT FOR (p:Person) REQUIRE p.age IS :: INTEGER",
        "CREATE CONSTRAINT FOR (p:Person) REQUIRE (p.a, p.b) IS NOT NULL",
    ] {
        run_ok(&svc, q);
    }
    assert!(run_ok(&svc, "SHOW CONSTRAINTS").rows.is_empty());
    run_ok(&svc, "CREATE (:Person)");
}

// ── Bolt and HTTP ───────────────────────────────────────────────────────────

fn bolt_run(s: &mut Session, query: &str) -> Vec<ServerMessage> {
    let mut replies = s.handle(ClientMessage::Run {
        query: query.to_string(),
        parameters: BTreeMap::new(),
        extra: BTreeMap::new(),
    });
    if !replies
        .iter()
        .any(|m| matches!(m, ServerMessage::Failure { .. }))
    {
        replies.extend(s.handle(ClientMessage::Pull {
            extra: BTreeMap::new(),
        }));
    }
    replies
}

fn failure_code(replies: &[ServerMessage]) -> Option<String> {
    replies.iter().find_map(|m| match m {
        ServerMessage::Failure { metadata } => match metadata.get("code") {
            Some(Bolt::String(c)) => Some(c.clone()),
            _ => None,
        },
        _ => None,
    })
}

#[test]
fn bolt_autocommit_reports_the_neo4j_code() {
    let svc = Arc::new(tracker());
    let mut s = Session::new_durable(Arc::clone(&svc));
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });
    let replies = bolt_run(&mut s, "CREATE (:Ticket {project: 'APP', number: 9})");
    assert_eq!(failure_code(&replies).as_deref(), Some(CODE), "{replies:?}");
}

#[test]
fn bolt_transaction_is_checked_at_commit() {
    let svc = Arc::new(tracker());
    let mut s = Session::new_durable(Arc::clone(&svc));
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });

    // Create first, add the status in a later statement: commits.
    s.handle(ClientMessage::Begin {
        extra: BTreeMap::new(),
    });
    let r = bolt_run(&mut s, "CREATE (:Ticket {project: 'APP', number: 2})");
    assert_eq!(failure_code(&r), None, "{r:?}");
    let r = bolt_run(&mut s, "MATCH (t:Ticket {number: 2}) SET t.status = 'open'");
    assert_eq!(failure_code(&r), None, "{r:?}");
    let r = s.handle(ClientMessage::Commit);
    assert_eq!(failure_code(&r), None, "{r:?}");
    assert_eq!(count(&svc, "MATCH (t:Ticket) RETURN count(t)"), 2);

    // Still missing at commit: refused with the constraint code.
    s.handle(ClientMessage::Begin {
        extra: BTreeMap::new(),
    });
    bolt_run(&mut s, "CREATE (:Ticket {project: 'APP', number: 3})");
    let r = s.handle(ClientMessage::Commit);
    assert_eq!(failure_code(&r).as_deref(), Some(CODE), "{r:?}");
    assert_eq!(count(&svc, "MATCH (t:Ticket) RETURN count(t)"), 2);
}

async fn call(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let res = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn http_answers_409_with_the_code() {
    let svc = Arc::new(tracker());
    let app = build_native_router(NativeApiState::new(Arc::clone(&svc)));
    let (status, body) = call(
        app.clone(),
        "POST",
        "/cypher",
        serde_json::json!({"query": "CREATE (:Ticket {project: 'APP', number: 9})"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], CODE, "{body}");
    let (status, body) = call(
        app,
        "POST",
        "/nodes",
        serde_json::json!({
            "kind": "Ticket", "title": "rest", "body": "", "body_html": "",
            "properties": {"project": "APP", "number": 9}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}
