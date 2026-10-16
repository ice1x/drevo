//! Managing users with Cypher over HTTP and Bolt: `CREATE USER`, `ALTER USER`,
//! `DROP USER`, `SHOW USERS`, `SHOW CURRENT USER`, `ALTER CURRENT USER`.
//!
//! Users live in the server's security catalog (`security.json` in the data
//! directory), run by the `drevo-auth` crate. These commands only manage
//! users; requiring a login is a separate step.
#![cfg(all(feature = "http", feature = "auth"))]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::bolt::packstream::Value;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session, State};
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use drevo::security::{open_server_security, Security};
use drevo_auth::catalog::MemoryStore;
use drevo_auth::users::{AuthManager, Bootstrap, BootstrapOutcome};
use tower::ServiceExt;

const PW: &str = "first-password";

fn security() -> Security {
    Security::new(Arc::new(
        AuthManager::open(Arc::new(MemoryStore::new())).unwrap(),
    ))
}

fn app_with(security: Security) -> axum::Router {
    let state = NativeApiState::new(Arc::new(NativeService::in_memory())).with_security(security);
    build_native_router(state)
}

/// POST /cypher; returns (status, body as JSON, raw body text).
async fn cypher(
    app: &axum::Router,
    query: &str,
    params: serde_json::Value,
) -> (StatusCode, serde_json::Value, String) {
    let body = serde_json::json!({ "query": query, "params": params }).to_string();
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
    let (status, json, text) = cypher(app, query, serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK, "{query}: {text}");
    json
}

/// SHOW USERS as `name → row` with the columns as keys.
async fn users(app: &axum::Router) -> BTreeMap<String, serde_json::Value> {
    let json = ok(app, "SHOW USERS").await;
    let columns: Vec<String> = serde_json::from_value(json["columns"].clone()).unwrap();
    assert_eq!(
        columns,
        [
            "user",
            "roles",
            "passwordChangeRequired",
            "suspended",
            "home"
        ]
    );
    json["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            let obj: serde_json::Map<String, serde_json::Value> = columns
                .iter()
                .cloned()
                .zip(row.as_array().unwrap().iter().cloned())
                .collect();
            (
                obj["user"].as_str().unwrap().to_string(),
                serde_json::Value::Object(obj),
            )
        })
        .collect()
}

// ── HTTP ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_then_show_users() {
    let app = app_with(security());
    ok(&app, &format!("CREATE USER ann SET PASSWORD '{PW}'")).await;
    ok(
        &app,
        &format!("CREATE USER bob SET PASSWORD '{PW}' CHANGE NOT REQUIRED SET STATUS SUSPENDED"),
    )
    .await;
    let users = users(&app).await;
    assert_eq!(users.keys().collect::<Vec<_>>(), ["ann", "bob"]);
    assert_eq!(users["ann"]["passwordChangeRequired"], true);
    assert_eq!(users["ann"]["suspended"], false);
    assert_eq!(users["ann"]["roles"], serde_json::json!([]));
    assert_eq!(users["ann"]["home"], serde_json::Value::Null);
    assert_eq!(users["bob"]["passwordChangeRequired"], false);
    assert_eq!(users["bob"]["suspended"], true);
}

#[tokio::test]
async fn creating_an_existing_user_is_a_conflict_unless_if_not_exists() {
    let app = app_with(security());
    ok(&app, &format!("CREATE USER ann SET PASSWORD '{PW}'")).await;
    let (status, json, _) = cypher(
        &app,
        &format!("CREATE USER ann SET PASSWORD '{PW}'"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["code"], "Neo.ClientError.General.InvalidArguments");
    assert!(json["error"].as_str().unwrap().contains("already exists"));
    ok(
        &app,
        &format!("CREATE USER ann IF NOT EXISTS SET PASSWORD '{PW}'"),
    )
    .await;
}

#[tokio::test]
async fn invalid_names_and_weak_passwords_are_bad_requests() {
    let app = app_with(security());
    for (query, says) in [
        ("CREATE USER ann SET PASSWORD 'short'", "at least 8"),
        (
            "CREATE USER `ann lee` SET PASSWORD 'long-enough'",
            "invalid user name",
        ),
    ] {
        let (status, json, _) = cypher(&app, query, serde_json::json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(json["code"], "Neo.ClientError.General.InvalidArguments");
        assert!(json["error"].as_str().unwrap().contains(says), "{json}");
    }
}

#[tokio::test]
async fn a_malformed_command_is_a_syntax_error() {
    let app = app_with(security());
    let (status, json, _) = cypher(&app, "CREATE USER ann", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["code"], "Neo.ClientError.Statement.SyntaxError");
}

#[tokio::test]
async fn the_password_can_be_a_parameter() {
    let app = app_with(security());
    let (status, _, text) = cypher(
        &app,
        "CREATE USER ann SET PASSWORD $pw",
        serde_json::json!({ "pw": PW }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(users(&app).await.contains_key("ann"));

    let (status, json, _) = cypher(
        &app,
        "CREATE USER bob SET PASSWORD $missing",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["code"], "Neo.ClientError.Statement.ParameterMissing");

    let (status, json, _) = cypher(
        &app,
        "CREATE USER bob SET PASSWORD $pw",
        serde_json::json!({ "pw": 12345678 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(json["error"].as_str().unwrap().contains("string"), "{json}");
}

#[tokio::test]
async fn alter_and_drop_users() {
    let app = app_with(security());
    ok(&app, &format!("CREATE USER ann SET PASSWORD '{PW}'")).await;
    ok(
        &app,
        "ALTER USER ann SET STATUS SUSPENDED SET PASSWORD CHANGE NOT REQUIRED",
    )
    .await;
    let ann = &users(&app).await["ann"];
    assert_eq!(ann["suspended"], true);
    assert_eq!(ann["passwordChangeRequired"], false);

    ok(&app, "ALTER USER ann SET PASSWORD 'second-password'").await;
    let (status, json, _) = cypher(
        &app,
        "ALTER USER ann SET PASSWORD 'second-password'",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "the same password again");
    assert!(json["error"].as_str().unwrap().contains("differ"));

    ok(&app, "DROP USER ann").await;
    assert!(users(&app).await.is_empty());
    let (status, json, _) = cypher(&app, "DROP USER ann", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["code"], "Neo.ClientError.General.InvalidArguments");
    ok(&app, "DROP USER ann IF EXISTS").await;
    ok(&app, "ALTER USER ann IF EXISTS SET STATUS ACTIVE").await;
}

#[tokio::test]
async fn current_user_commands_need_a_signed_in_user() {
    let app = app_with(security());
    let json = ok(&app, "SHOW CURRENT USER").await;
    assert_eq!(json["rows"], serde_json::json!([]), "nobody is signed in");
    let (status, json, _) = cypher(
        &app,
        &format!("ALTER CURRENT USER SET PASSWORD FROM '{PW}' TO 'second-password'"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json["code"], "Neo.ClientError.Security.Unauthorized");
}

#[tokio::test]
async fn no_response_ever_contains_a_password() {
    let app = app_with(security());
    let mut texts = Vec::new();
    for query in [
        "CREATE USER ann SET PASSWORD 'hunter2-first'",
        "CREATE USER ann SET PASSWORD 'hunter2-again'",
        "ALTER USER ann SET PASSWORD 'hunter2-first'",
        "CREATE USER `bad name` SET PASSWORD 'hunter2-third'",
        "CREATE USER bob SET PASSWORD 'hunter2-4' SET COLOUR 'x'",
        "ALTER CURRENT USER SET PASSWORD FROM 'hunter2-first' TO 'hunter2-five'",
        "SHOW USERS",
    ] {
        texts.push(cypher(&app, query, serde_json::json!({})).await.2);
    }
    for text in texts {
        assert!(!text.contains("hunter2"), "{text}");
        assert!(!text.contains("$argon2"), "{text}");
    }
}

#[tokio::test]
async fn without_user_management_the_commands_say_so() {
    let app = app_with(Security::default());
    let (status, json, _) = cypher(
        &app,
        &format!("CREATE USER ann SET PASSWORD '{PW}'"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        json["error"].as_str().unwrap().contains("not enabled"),
        "{json}"
    );
}

#[tokio::test]
async fn graph_queries_still_run_next_to_user_commands() {
    let app = app_with(security());
    ok(&app, "CREATE (:User {name: 'ann'})").await;
    let json = ok(&app, "MATCH (u:User) RETURN u.name AS name").await;
    assert_eq!(json["rows"], serde_json::json!([["ann"]]));
    assert!(
        users(&app).await.is_empty(),
        "a :User node is not a user account"
    );
}

// ── Storage and bootstrap ────────────────────────────────────────────

static NEXT: AtomicU64 = AtomicU64::new(0);
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "drevo_user_admin_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn users_are_kept_in_security_json_across_restarts() {
    let dir = TmpDir::new();
    let admin = Bootstrap::Admin {
        user: "admin".into(),
        password: "admin-password".into(),
    };
    {
        let (security, outcome) = open_server_security(Some(&dir.0), &admin).unwrap();
        assert_eq!(outcome, BootstrapOutcome::Created);
        let app = app_with(security);
        ok(&app, &format!("CREATE USER ann SET PASSWORD '{PW}'")).await;
    }
    assert!(dir.0.join("security.json").exists());
    let other = Bootstrap::Admin {
        user: "admin".into(),
        password: "changed-password".into(),
    };
    let (security, outcome) = open_server_security(Some(&dir.0), &other).unwrap();
    assert_eq!(outcome, BootstrapOutcome::AlreadyInitialised);
    let users = security.users().unwrap();
    assert!(
        users.check_password("admin", "admin-password"),
        "the first password holds"
    );
    assert!(users.check_password("ann", PW));
}

#[test]
fn an_in_memory_server_keeps_users_in_memory() {
    let (security, outcome) = open_server_security(None, &Bootstrap::Generated).unwrap();
    let BootstrapOutcome::Generated { user, password } = outcome else {
        panic!("expected a generated password");
    };
    assert_eq!(user, "neo4j");
    assert!(security.users().unwrap().check_password("neo4j", &password));
}

#[test]
fn a_damaged_catalog_stops_the_server() {
    let dir = TmpDir::new();
    std::fs::write(dir.0.join("security.json"), "{ damaged").unwrap();
    assert!(open_server_security(Some(&dir.0), &Bootstrap::Generated).is_err());
}

// ── Bolt ─────────────────────────────────────────────────────────────

fn dict<I: IntoIterator<Item = (&'static str, Value)>>(entries: I) -> BTreeMap<String, Value> {
    entries
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

/// A Bolt session on a server that needs no login (`DREVO_AUTH=none`): these
/// tests are about the commands; logging in is covered in bolt_login_tests.
fn bolt_session(security: Security) -> Session<'static> {
    let mut s = Session::new_durable(Arc::new(NativeService::in_memory()))
        .with_security(security.without_login());
    s.handle(ClientMessage::Hello { extra: dict([]) });
    s
}

fn run(s: &mut Session<'_>, query: &str, params: BTreeMap<String, Value>) -> Vec<ServerMessage> {
    let mut replies = s.handle(ClientMessage::Run {
        query: query.to_string(),
        parameters: params,
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
fn bolt_creates_and_lists_users() {
    let security = security();
    let mut s = bolt_session(security.clone());
    let replies = run(
        &mut s,
        "CREATE USER ann SET PASSWORD $pw",
        dict([("pw", Value::String(PW.to_string()))]),
    );
    assert!(failure_code(&replies).is_none(), "{replies:?}");
    assert_eq!(s.state(), State::Ready);

    let replies = run(&mut s, "SHOW USERS", dict([]));
    let records: Vec<&Vec<Value>> = replies
        .iter()
        .filter_map(|m| match m {
            ServerMessage::Record { fields } => Some(fields),
            _ => None,
        })
        .collect();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0][0], Value::String("ann".into()));
    assert!(security.users().unwrap().check_password("ann", PW));
}

#[test]
fn bolt_reports_user_errors_with_neo4j_codes() {
    let mut s = bolt_session(security());
    let replies = run(&mut s, "CREATE USER ann SET PASSWORD 'short'", dict([]));
    assert_eq!(
        failure_code(&replies).as_deref(),
        Some("Neo.ClientError.General.InvalidArguments")
    );
    assert_eq!(s.state(), State::Failed);
}

#[test]
fn bolt_refuses_user_commands_inside_an_explicit_transaction() {
    let security = security();
    let mut s = bolt_session(security.clone());
    s.handle(ClientMessage::Begin { extra: dict([]) });
    let replies = run(
        &mut s,
        &format!("CREATE USER ann SET PASSWORD '{PW}'"),
        dict([]),
    );
    assert_eq!(
        failure_code(&replies).as_deref(),
        Some("Neo.ClientError.Transaction.ForbiddenDueToTransactionType")
    );
    assert!(
        security.users().unwrap().user("ann").is_none(),
        "nothing was created"
    );
}
