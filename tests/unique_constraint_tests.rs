//! `CREATE CONSTRAINT … IS UNIQUE` through Cypher, Bolt and HTTP (issue #604).
//!
//! A declared UNIQUE constraint is enforced on every node write, listed by
//! `SHOW CONSTRAINTS`, removed with `DROP CONSTRAINT` and kept across a
//! restart. Relationship and property-type constraints stay accepted no-ops.

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

/// ERP: customers are identified by email.
fn erp() -> NativeService {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CREATE CONSTRAINT customer_email FOR (c:Customer) REQUIRE c.email IS UNIQUE",
    );
    run_ok(
        &svc,
        "CREATE (:Customer {title: 'Ann', email: 'ann@x'}), (:Customer {title: 'Bob', email: 'bob@x'})",
    );
    svc
}

// ── enforcement ─────────────────────────────────────────────────────────────

#[test]
fn a_duplicate_value_is_refused_on_create_and_set() {
    let svc = erp();
    assert!(is_violation(&run(
        &svc,
        "CREATE (:Customer {title: 'Ann 2', email: 'ann@x'})"
    )));
    assert!(is_violation(&run(
        &svc,
        "MATCH (c:Customer {title: 'Bob'}) SET c.email = 'ann@x'"
    )));
    assert!(is_violation(&run(
        &svc,
        "MATCH (c:Customer {title: 'Bob'}) SET c += {email: 'ann@x'}"
    )));
    assert_eq!(count(&svc, "MATCH (c:Customer) RETURN count(c)"), 2);
    assert_eq!(
        count(&svc, "MATCH (c:Customer {email: 'bob@x'}) RETURN count(c)"),
        1,
        "the refused SET changed nothing"
    );
}

#[test]
fn merge_finds_the_existing_node_instead_of_failing() {
    let svc = erp();
    run_ok(&svc, "MERGE (c:Customer {email: 'ann@x'}) SET c.vip = true");
    assert_eq!(count(&svc, "MATCH (c:Customer) RETURN count(c)"), 2);
    assert_eq!(
        count(&svc, "MATCH (c:Customer {vip: true}) RETURN count(c)"),
        1
    );
}

#[test]
fn other_labels_missing_values_and_secondary_labels() {
    let svc = erp();
    // Another label may reuse the value; nodes without it are unconstrained.
    run_ok(&svc, "CREATE (:Supplier {email: 'ann@x'})");
    run_ok(
        &svc,
        "CREATE (:Customer {title: 'Anon 1'}), (:Customer {title: 'Anon 2'})",
    );
    // Adding the label to a node with a taken value is refused.
    assert!(is_violation(&run(
        &svc,
        "MATCH (s:Supplier) SET s:Customer"
    )));
}

#[test]
fn a_title_constraint_covers_the_title_field() {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CREATE CONSTRAINT FOR (t:Tag) REQUIRE t.title IS UNIQUE",
    );
    run_ok(&svc, "CREATE (:Tag {title: 'rust'})");
    assert!(is_violation(&run(&svc, "CREATE (:Tag {title: 'rust'})")));
    // Notes may share the name (#595); so may untitled tags.
    run_ok(&svc, "CREATE (:Note {title: 'rust'})");
    run_ok(&svc, "CREATE (:Tag {x: 1}), (:Tag {x: 2})");
}

#[test]
fn the_service_api_is_covered_too() {
    let svc = erp();
    let err = svc
        .create_node(drevo::model::NewNode {
            kind: "Customer".into(),
            title: "Ann again".into(),
            body: String::new(),
            body_html: String::new(),
            properties: drevo::model::Properties::from_iter([(
                "email".to_string(),
                serde_json::json!("ann@x"),
            )]),
        })
        .expect_err("violation");
    assert!(
        matches!(err, DrevoError::ConstraintViolation { .. }),
        "{err:?}"
    );
}

// ── DDL ─────────────────────────────────────────────────────────────────────

#[test]
fn ddl_names_if_not_exists_and_equivalents() {
    let svc = erp();
    // Same name again.
    let err = run(
        &svc,
        "CREATE CONSTRAINT customer_email FOR (c:Customer) REQUIRE c.title IS UNIQUE",
    )
    .expect_err("name taken");
    assert!(err.to_string().contains("already exists"), "{err}");
    // The same rule under another name.
    let err = run(
        &svc,
        "CREATE CONSTRAINT other FOR (c:Customer) REQUIRE c.email IS UNIQUE",
    )
    .expect_err("equivalent");
    assert!(err.to_string().contains("customer_email"), "{err}");
    run_ok(
        &svc,
        "CREATE CONSTRAINT other IF NOT EXISTS FOR (c:Customer) REQUIRE c.email IS UNIQUE",
    );
    // Names are shared with indexes.
    run_ok(
        &svc,
        "CREATE TEXT INDEX shared FOR (c:Customer) ON (c.title)",
    );
    let err = run(
        &svc,
        "CREATE CONSTRAINT shared FOR (c:Customer) REQUIRE c.code IS UNIQUE",
    )
    .expect_err("name taken by an index");
    assert!(err.to_string().contains("already exists"), "{err}");
    // A generated name.
    run_ok(
        &svc,
        "CREATE CONSTRAINT IF NOT EXISTS FOR (c:Customer) REQUIRE (c.code) IS UNIQUE",
    );
    let names: Vec<Value> = run_ok(
        &svc,
        "SHOW CONSTRAINTS YIELD name RETURN name ORDER BY name",
    )
    .rows
    .into_iter()
    .map(|r| r[0].clone())
    .collect();
    assert_eq!(
        names,
        vec![s("constraint_Customer_code"), s("customer_email")]
    );
}

#[test]
fn creating_over_violating_data_fails_and_stores_nothing() {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CREATE (:User {email: 'a@x'}), (:User {email: 'a@x'})",
    );
    let err = run(
        &svc,
        "CREATE CONSTRAINT FOR (u:User) REQUIRE u.email IS UNIQUE",
    )
    .expect_err("data violates it");
    assert!(err.to_string().contains("duplicate"), "{err}");
    assert!(run_ok(&svc, "SHOW CONSTRAINTS").rows.is_empty());
    run_ok(&svc, "CREATE (:User {email: 'a@x'})");
}

#[test]
fn neo4j_4_syntax_and_options_are_accepted() {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CREATE CONSTRAINT legacy IF NOT EXISTS ON (u:User) ASSERT u.email IS UNIQUE",
    );
    run_ok(
        &svc,
        "CREATE CONSTRAINT modern FOR (p:Person) REQUIRE p.id IS UNIQUE OPTIONS {}",
    );
    run_ok(&svc, "CREATE (:User {email: 'a@x'}), (:Person {id: 1})");
    assert!(is_violation(&run(&svc, "CREATE (:User {email: 'a@x'})")));
    assert!(is_violation(&run(&svc, "CREATE (:Person {id: 1})")));
}

#[test]
fn other_constraint_forms_stay_no_ops() {
    let svc = NativeService::in_memory();
    for q in [
        "CREATE CONSTRAINT FOR ()-[r:KNOWS]-() REQUIRE r.since IS UNIQUE",
        "CREATE CONSTRAINT FOR (p:Person) REQUIRE p.age IS :: INTEGER",
        "DROP CONSTRAINT ON (p:Person) ASSERT p.name IS UNIQUE",
    ] {
        run_ok(&svc, q);
    }
    assert!(run_ok(&svc, "SHOW CONSTRAINTS").rows.is_empty());
    run_ok(
        &svc,
        "CREATE (:Person {a: 1, b: 2}), (:Person {a: 1, b: 2})",
    );
}

#[test]
fn drop_constraint_lifts_the_rule() {
    let svc = erp();
    run_ok(&svc, "DROP CONSTRAINT customer_email");
    run_ok(&svc, "CREATE (:Customer {title: 'Ann 2', email: 'ann@x'})");
    let err = run(&svc, "DROP CONSTRAINT customer_email").expect_err("gone");
    assert!(err.to_string().contains("no constraint named"), "{err}");
    run_ok(&svc, "DROP CONSTRAINT customer_email IF EXISTS");
}

#[test]
fn show_constraints_lists_unique_constraints() {
    let svc = erp();
    let res = run_ok(
        &svc,
        "SHOW CONSTRAINTS YIELD name, type, entityType, labelsOrTypes, properties, ownedIndex \
         RETURN name, type, entityType, labelsOrTypes, properties, ownedIndex",
    );
    assert_eq!(
        res.rows,
        vec![vec![
            s("customer_email"),
            s("UNIQUENESS"),
            s("NODE"),
            Value::List(vec![s("Customer")]),
            Value::List(vec![s("email")]),
            s("customer_email"),
        ]]
    );
    assert_eq!(run_ok(&svc, "SHOW UNIQUE CONSTRAINTS").rows.len(), 1);
    assert!(run_ok(&svc, "SHOW EXISTENCE CONSTRAINTS").rows.is_empty());
}

#[test]
fn constraints_survive_a_restart() {
    let dir = std::env::temp_dir().join(format!(
        "drevo_unique_ddl_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let wal = dir.join("native.wal");
    {
        let svc = NativeService::open(&wal).unwrap();
        run_ok(
            &svc,
            "CREATE CONSTRAINT customer_email FOR (c:Customer) REQUIRE c.email IS UNIQUE",
        );
        run_ok(&svc, "CREATE (:Customer {email: 'ann@x'})");
    }
    let svc = NativeService::open(&wal).unwrap();
    assert_eq!(run_ok(&svc, "SHOW CONSTRAINTS").rows.len(), 1);
    assert!(is_violation(&run(
        &svc,
        "CREATE (:Customer {email: 'ann@x'})"
    )));
    let _ = std::fs::remove_dir_all(&dir);
}

// ── Bolt and HTTP ───────────────────────────────────────────────────────────

#[test]
fn bolt_reports_the_neo4j_constraint_code() {
    let svc = Arc::new(erp());
    let mut s = Session::new_durable(Arc::clone(&svc));
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });
    let replies = s.handle(ClientMessage::Run {
        query: "CREATE (:Customer {email: 'ann@x'})".to_string(),
        parameters: BTreeMap::new(),
        extra: BTreeMap::new(),
    });
    let code = replies.iter().find_map(|m| match m {
        ServerMessage::Failure { metadata } => match metadata.get("code") {
            Some(Bolt::String(c)) => Some(c.clone()),
            _ => None,
        },
        _ => None,
    });
    assert_eq!(code.as_deref(), Some(CODE), "{replies:?}");
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request");
    let response = app.clone().oneshot(req).await.expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, serde_json::from_slice(&bytes).expect("json"))
}

#[tokio::test]
async fn http_answers_409_with_the_code() {
    let app = build_native_router(NativeApiState::new(Arc::new(erp())));
    let (status, body) = call(
        &app,
        "POST",
        "/cypher",
        serde_json::json!({ "query": "CREATE (:Customer {email: 'bob@x'})" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], CODE, "{body}");

    let (status, body) = call(
        &app,
        "POST",
        "/nodes",
        serde_json::json!({
            "kind": "Customer", "title": "Bob again", "body": "", "body_html": "",
            "properties": { "email": "bob@x" }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], CODE, "{body}");
}
