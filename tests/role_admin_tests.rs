//! Managing roles and privileges with Cypher over HTTP and Bolt: `CREATE` /
//! `DROP` / `RENAME ROLE`, `GRANT` / `REVOKE ROLE`, `SHOW ROLES`, `GRANT` /
//! `DENY` / `REVOKE` privileges, `SHOW PRIVILEGES [AS COMMANDS]` and `SHOW
//! SUPPORTED PRIVILEGES`.
//!
//! Roles and privileges live in the server's security catalog with its users.
//! These commands manage them; enforcing the privileges is a separate step.
#![cfg(all(feature = "http", feature = "auth"))]

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::bolt::packstream::Value;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::security_admin::parse;
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use drevo::security::{Security, SecurityErrorKind};
use drevo_auth::catalog::MemoryStore;
use drevo_auth::users::AuthManager;
use tower::ServiceExt;

const PW: &str = "first-password";

fn security() -> Security {
    Security::new(Arc::new(
        AuthManager::open(Arc::new(MemoryStore::new())).unwrap(),
    ))
}

/// An app on a server that needs no login: these tests are about the
/// commands.
fn app_with(security: Security) -> axum::Router {
    let state = NativeApiState::new(Arc::new(NativeService::in_memory()))
        .with_security(security.without_login());
    build_native_router(state)
}

/// POST /cypher; (status, body as JSON, raw text).
async fn cypher(app: &axum::Router, query: &str) -> (StatusCode, serde_json::Value, String) {
    let body = serde_json::json!({ "query": query, "params": {} }).to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/cypher")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    (status, json, text)
}

async fn ok(app: &axum::Router, query: &str) -> serde_json::Value {
    let (status, json, text) = cypher(app, query).await;
    assert_eq!(status, StatusCode::OK, "{query}: {text}");
    json
}

/// The rows of a result as maps from column to value.
fn rows(json: &serde_json::Value) -> Vec<BTreeMap<String, serde_json::Value>> {
    let columns: Vec<String> = serde_json::from_value(json["columns"].clone()).unwrap();
    json["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            columns
                .iter()
                .cloned()
                .zip(row.as_array().unwrap().iter().cloned())
                .collect()
        })
        .collect()
}

fn column(json: &serde_json::Value, name: &str) -> Vec<String> {
    rows(json)
        .iter()
        .map(|r| r[name].as_str().unwrap_or("").to_string())
        .collect()
}

/// A bug tracker's people: ann triages, bob develops.
async fn bug_tracker() -> axum::Router {
    let app = app_with(security());
    for q in [
        format!("CREATE USER ann SET PASSWORD '{PW}'"),
        format!("CREATE USER bob SET PASSWORD '{PW}'"),
        "CREATE ROLE triage".to_string(),
        "CREATE ROLE developer AS COPY OF editor".to_string(),
        "GRANT ROLE triage TO ann".to_string(),
        "GRANT ROLES developer, triage TO bob".to_string(),
    ] {
        ok(&app, &q).await;
    }
    app
}

// ── Roles ────────────────────────────────────────────────────────────

#[tokio::test]
async fn show_roles_lists_built_in_and_created_roles() {
    let app = bug_tracker().await;
    let json = ok(&app, "SHOW ROLES").await;
    assert_eq!(json["columns"], serde_json::json!(["role"]));
    assert_eq!(
        column(&json, "role"),
        [
            "PUBLIC",
            "admin",
            "architect",
            "developer",
            "editor",
            "publisher",
            "reader",
            "triage"
        ]
    );
}

#[tokio::test]
async fn show_roles_with_users_lists_each_member() {
    let app = bug_tracker().await;
    let json = ok(&app, "SHOW POPULATED ROLES WITH USERS").await;
    assert_eq!(json["columns"], serde_json::json!(["role", "member"]));
    let pairs: Vec<(String, String)> = rows(&json)
        .iter()
        .map(|r| {
            (
                r["role"].as_str().unwrap().to_string(),
                r["member"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let expected: Vec<(String, String)> = [
        ("PUBLIC", "ann"),
        ("PUBLIC", "bob"),
        ("developer", "bob"),
        ("triage", "ann"),
        ("triage", "bob"),
    ]
    .iter()
    .map(|(a, b)| (a.to_string(), b.to_string()))
    .collect();
    assert_eq!(pairs, expected);
    // Every role, members or not: a role without members has a null member.
    let all = ok(&app, "SHOW ROLES WITH USERS").await;
    let reader = rows(&all)
        .into_iter()
        .find(|r| r["role"] == "reader")
        .unwrap();
    assert_eq!(reader["member"], serde_json::Value::Null);
}

#[tokio::test]
async fn roles_show_up_in_show_users() {
    let app = bug_tracker().await;
    let users = ok(&app, "SHOW USERS").await;
    let bob = rows(&users)
        .into_iter()
        .find(|r| r["user"] == "bob")
        .unwrap();
    assert_eq!(
        bob["roles"],
        serde_json::json!(["PUBLIC", "developer", "triage"])
    );
    ok(&app, "REVOKE ROLE triage FROM bob").await;
    ok(&app, "RENAME ROLE developer TO dev").await;
    let users = ok(&app, "SHOW USERS").await;
    let bob = rows(&users)
        .into_iter()
        .find(|r| r["user"] == "bob")
        .unwrap();
    assert_eq!(bob["roles"], serde_json::json!(["PUBLIC", "dev"]));
    ok(&app, "DROP ROLE dev").await;
    let users = ok(&app, "SHOW USERS").await;
    let bob = rows(&users)
        .into_iter()
        .find(|r| r["user"] == "bob")
        .unwrap();
    assert_eq!(bob["roles"], serde_json::json!(["PUBLIC"]));
}

#[tokio::test]
async fn role_errors_use_neo4j_codes() {
    let app = bug_tracker().await;
    for (query, status) in [
        ("CREATE ROLE triage", StatusCode::CONFLICT),
        ("CREATE ROLE `bad name`", StatusCode::BAD_REQUEST),
        ("DROP ROLE ghost", StatusCode::NOT_FOUND),
        ("DROP ROLE PUBLIC", StatusCode::BAD_REQUEST),
        ("GRANT ROLE ghost TO ann", StatusCode::NOT_FOUND),
        ("GRANT ROLE reader TO zed", StatusCode::NOT_FOUND),
        ("GRANT TRAVERSE ON GRAPH * TO ghost", StatusCode::NOT_FOUND),
    ] {
        let (got, json, text) = cypher(&app, query).await;
        assert_eq!(got, status, "{query}: {text}");
        assert_eq!(
            json["code"], "Neo.ClientError.General.InvalidArguments",
            "{query}: {text}"
        );
    }
    let (got, json, _) = cypher(&app, "GRANT FLY ON GRAPH * TO triage").await;
    assert_eq!(got, StatusCode::BAD_REQUEST);
    assert_eq!(json["code"], "Neo.ClientError.Statement.SyntaxError");
    // IF [NOT] EXISTS turns the first three into no-ops.
    ok(&app, "CREATE ROLE triage IF NOT EXISTS").await;
    ok(&app, "DROP ROLE ghost IF EXISTS").await;
    ok(&app, "RENAME ROLE ghost IF EXISTS TO spirit").await;
}

// ── Privileges ───────────────────────────────────────────────────────

const PRIVILEGE_COLUMNS: [&str; 7] = [
    "access",
    "action",
    "resource",
    "graph",
    "segment",
    "role",
    "immutable",
];

#[tokio::test]
async fn granted_and_denied_privileges_show_as_rows() {
    let app = bug_tracker().await;
    ok(&app, "GRANT MATCH {*} ON GRAPH bugs NODES Bug TO triage").await;
    ok(
        &app,
        "DENY READ {reporter} ON GRAPH bugs NODES Bug TO triage",
    )
    .await;
    let json = ok(&app, "SHOW ROLE triage PRIVILEGES").await;
    assert_eq!(json["columns"], serde_json::json!(PRIVILEGE_COLUMNS));
    assert_eq!(
        rows(&json),
        [
            serde_json::json!({
                "access": "GRANTED", "action": "match", "resource": "all_properties",
                "graph": "bugs", "segment": "NODE(Bug)", "role": "triage", "immutable": false
            }),
            serde_json::json!({
                "access": "DENIED", "action": "read", "resource": "property(reporter)",
                "graph": "bugs", "segment": "NODE(Bug)", "role": "triage", "immutable": false
            }),
        ]
        .map(|v| serde_json::from_value::<BTreeMap<String, serde_json::Value>>(v).unwrap())
    );
}

#[tokio::test]
async fn privileges_show_as_commands_and_revoke_commands() {
    let app = bug_tracker().await;
    ok(&app, "GRANT MATCH {*} ON GRAPH bugs NODES Bug TO triage").await;
    ok(
        &app,
        "DENY READ {reporter} ON GRAPH bugs NODES Bug TO triage",
    )
    .await;
    let json = ok(&app, "SHOW ROLE triage PRIVILEGES AS COMMANDS").await;
    assert_eq!(json["columns"], serde_json::json!(["command"]));
    assert_eq!(
        column(&json, "command"),
        [
            "GRANT MATCH {*} ON GRAPH bugs NODE Bug TO `triage`",
            "DENY READ {reporter} ON GRAPH bugs NODE Bug TO `triage`",
        ]
    );
    let json = ok(&app, "SHOW ROLE triage PRIVILEGES AS REVOKE COMMANDS").await;
    let revokes = column(&json, "command");
    assert_eq!(
        revokes,
        [
            "REVOKE GRANT MATCH {*} ON GRAPH bugs NODE Bug FROM `triage`",
            "REVOKE DENY READ {reporter} ON GRAPH bugs NODE Bug FROM `triage`",
        ]
    );
    for command in &revokes {
        ok(&app, command).await;
    }
    let json = ok(&app, "SHOW ROLE triage PRIVILEGES").await;
    assert!(rows(&json).is_empty());
}

#[tokio::test]
async fn a_users_privileges_come_from_its_roles() {
    let app = bug_tracker().await;
    ok(&app, "GRANT TRAVERSE ON GRAPH bugs TO triage").await;
    let json = ok(&app, "SHOW USER ann PRIVILEGES").await;
    let mut columns = PRIVILEGE_COLUMNS.to_vec();
    columns.push("user");
    assert_eq!(json["columns"], serde_json::json!(columns));
    let rs = rows(&json);
    assert!(rs.iter().all(|r| r["user"] == "ann"));
    let roles: std::collections::BTreeSet<String> = rs
        .iter()
        .map(|r| r["role"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(roles.into_iter().collect::<Vec<_>>(), ["PUBLIC", "triage"]);
}

#[test]
fn show_user_privileges_without_a_name_is_the_signed_in_users() {
    let security = security();
    let users = security.users().unwrap();
    users
        .create_user(drevo_auth::users::NewUser::new("ann", PW))
        .unwrap();
    let cmd = parse("SHOW USER PRIVILEGES").unwrap().unwrap();
    let result = security.execute(&cmd, Some("ann"), &|_| None).unwrap();
    assert!(!result.rows.is_empty());
    let err = security.execute(&cmd, None, &|_| None).unwrap_err();
    assert_eq!(err.kind, SecurityErrorKind::Unauthenticated);
}

#[tokio::test]
async fn show_privileges_as_commands_recreates_the_same_privileges_on_an_empty_catalog() {
    // The source server: an ERP with its own roles.
    let source = bug_tracker().await;
    for q in [
        "CREATE ROLE accountant",
        "GRANT MATCH {amount, `due date`} ON GRAPH erp NODES Invoice TO accountant",
        "GRANT SET PROPERTY {status} ON GRAPH erp NODES Invoice TO accountant",
        "DENY DELETE ON GRAPH erp ELEMENTS * TO accountant",
        "GRANT EXECUTE PROCEDURE apoc.export.* ON DBMS TO accountant",
        "DENY ACCESS ON DATABASE payroll TO developer",
        "REVOKE GRANT NAME MANAGEMENT ON DATABASE * FROM publisher",
    ] {
        ok(&source, q).await;
    }
    let expected = ok(&source, "SHOW PRIVILEGES").await;
    let commands = column(&ok(&source, "SHOW PRIVILEGES AS COMMANDS").await, "command");
    assert_eq!(commands.len(), rows(&expected).len());

    // An empty catalog: no privileges at all, and the source's roles.
    let target = app_with(security());
    for revoke in column(
        &ok(&target, "SHOW PRIVILEGES AS REVOKE COMMANDS").await,
        "command",
    ) {
        ok(&target, &revoke).await;
    }
    assert!(rows(&ok(&target, "SHOW PRIVILEGES").await).is_empty());
    for role in column(&ok(&source, "SHOW ROLES").await, "role") {
        ok(&target, &format!("CREATE ROLE `{role}` IF NOT EXISTS")).await;
    }
    for command in &commands {
        ok(&target, command).await;
    }
    assert_eq!(
        ok(&target, "SHOW PRIVILEGES").await["rows"],
        expected["rows"]
    );
}

#[tokio::test]
async fn show_supported_privileges_lists_every_action() {
    let app = app_with(security());
    let json = ok(&app, "SHOW SUPPORTED PRIVILEGES").await;
    assert_eq!(
        json["columns"],
        serde_json::json!(["action", "qualifier", "target", "scope", "description"])
    );
    let actions = column(&json, "action");
    for a in [
        "traverse",
        "read",
        "match",
        "create",
        "delete",
        "set label",
        "remove label",
        "set property",
        "merge",
        "write",
        "all graph privileges",
        "access",
        "start",
        "stop",
        "index management",
        "constraint management",
        "name management",
        "transaction management",
        "all database privileges",
        "user management",
        "role management",
        "privilege management",
        "database management",
        "execute procedure",
        "execute function",
        "execute admin procedures",
        "all dbms privileges",
    ] {
        assert!(actions.contains(&a.to_string()), "{a}");
    }
}

// ── Bolt ─────────────────────────────────────────────────────────────

fn dict<I: IntoIterator<Item = (&'static str, Value)>>(entries: I) -> BTreeMap<String, Value> {
    entries
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

fn bolt_session(security: Security) -> Session<'static> {
    let mut s = Session::new_durable(Arc::new(NativeService::in_memory()))
        .with_security(security.without_login());
    s.handle(ClientMessage::Hello { extra: dict([]) });
    s
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

fn failure_code(replies: &[ServerMessage]) -> Option<String> {
    replies.iter().find_map(|m| match m {
        ServerMessage::Failure { metadata } => match metadata.get("code") {
            Some(Value::String(code)) => Some(code.clone()),
            _ => None,
        },
        _ => None,
    })
}

#[test]
fn bolt_manages_roles_and_privileges() {
    let security = security();
    let mut s = bolt_session(security.clone());
    for q in ["CREATE ROLE triage", "GRANT TRAVERSE ON GRAPH * TO triage"] {
        let replies = run(&mut s, q);
        assert!(failure_code(&replies).is_none(), "{q}: {replies:?}");
    }
    let replies = run(&mut s, "SHOW ROLE triage PRIVILEGES AS COMMANDS");
    let records: Vec<&Vec<Value>> = replies
        .iter()
        .filter_map(|m| match m {
            ServerMessage::Record { fields } => Some(fields),
            _ => None,
        })
        .collect();
    assert_eq!(records.len(), 2, "node and relationship");
    assert_eq!(
        records[0][0],
        Value::String("GRANT TRAVERSE ON GRAPH * NODE * TO `triage`".into())
    );
}

#[test]
fn bolt_refuses_role_and_privilege_commands_inside_an_explicit_transaction() {
    for q in [
        "CREATE ROLE triage",
        "GRANT TRAVERSE ON GRAPH * TO reader",
        "SHOW PRIVILEGES",
    ] {
        let mut s = bolt_session(security());
        s.handle(ClientMessage::Begin { extra: dict([]) });
        assert_eq!(
            failure_code(&run(&mut s, q)).as_deref(),
            Some("Neo.ClientError.Transaction.ForbiddenDueToTransactionType"),
            "{q}"
        );
    }
}
