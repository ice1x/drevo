//! The security audit log: the server records logins, logouts, password and
//! user / role / privilege changes, and refusals, with the protocol they came
//! over, and never a password.
#![cfg(all(feature = "http", feature = "auth"))]

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::bolt::packstream::Value;
use drevo::bolt::session::{ClientMessage, Session};
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use drevo::security::Security;
use drevo_auth::audit::{AuditEvent, AuditKind, MemorySink, Outcome};
use drevo_auth::catalog::MemoryStore;
use drevo_auth::users::{AuthManager, NewUser};
use tower::ServiceExt;

/// A password that must never reach the log.
const SECRET: &str = "Sup3r-Secret-Pw";

struct Server {
    app: axum::Router,
    users: Arc<AuthManager>,
    security: Security,
    sink: Arc<MemorySink>,
}

impl Server {
    /// `admin` and `rita` (reader), both with password [`SECRET`].
    fn new(queries: bool) -> Self {
        let users = Arc::new(AuthManager::open(Arc::new(MemoryStore::new())).unwrap());
        for (name, role) in [("admin", "admin"), ("rita", "reader")] {
            users
                .create_user(NewUser::new(name, SECRET).change_required(false))
                .unwrap();
            users.grant_role(role, name).unwrap();
        }
        let sink = Arc::new(MemorySink::new());
        let security = Security::new(Arc::clone(&users)).with_audit(sink.clone(), queries);
        let state = NativeApiState::new(Arc::new(NativeService::in_memory()))
            .with_security(security.clone());
        Self {
            app: build_native_router(state),
            users,
            security,
            sink,
        }
    }

    async fn http(
        &self,
        method: &str,
        uri: &str,
        authorization: Option<String>,
        body: Option<serde_json::Value>,
    ) -> StatusCode {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(a) = authorization {
            req = req.header("authorization", a);
        }
        let req = match body {
            Some(b) => req
                .header("content-type", "application/json")
                .body(Body::from(b.to_string())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        self.app.clone().oneshot(req).await.unwrap().status()
    }

    async fn cypher_as(&self, user: &str, query: &str) -> StatusCode {
        let token = self.users.issue_session(user).unwrap();
        self.http(
            "POST",
            "/cypher",
            Some(format!("Bearer {token}")),
            Some(serde_json::json!({ "query": query })),
        )
        .await
    }

    fn events(&self) -> Vec<AuditEvent> {
        self.sink.events()
    }

    fn of(&self, kind: AuditKind) -> Vec<AuditEvent> {
        self.events()
            .into_iter()
            .filter(|e| e.kind == kind)
            .collect()
    }
}

fn basic(user: &str, password: &str) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let raw = format!("{user}:{password}").into_bytes();
    let mut out = String::new();
    for chunk in raw.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                char::from(A[((n >> (18 - 6 * i)) & 63) as usize])
            } else {
                '='
            });
        }
    }
    format!("Basic {out}")
}

fn dict(entries: Vec<(&str, Value)>) -> BTreeMap<String, Value> {
    entries
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

fn bolt_hello(security: &Security, user: &str, password: &str) -> Session<'static> {
    let mut s =
        Session::new_durable(Arc::new(NativeService::in_memory())).with_security(security.clone());
    s.handle(ClientMessage::Hello {
        extra: dict(vec![
            ("scheme", Value::String("basic".into())),
            ("principal", Value::String(user.into())),
            ("credentials", Value::String(password.into())),
        ]),
    });
    s
}

// ── Logins ───────────────────────────────────────────────────────────

#[test]
fn bolt_logins_are_recorded() {
    let s = Server::new(false);
    bolt_hello(&s.security, "rita", SECRET);
    bolt_hello(&s.security, "rita", "wrong-password");
    let ok = s.of(AuditKind::LoginSucceeded);
    assert_eq!(ok.len(), 1);
    assert_eq!(ok[0].user.as_deref(), Some("rita"));
    assert_eq!(ok[0].surface.as_deref(), Some("bolt"));
    assert_eq!(ok[0].outcome, Outcome::Success);
    let failed = s.of(AuditKind::LoginFailed);
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].user.as_deref(), Some("rita"));
    assert_eq!(failed[0].outcome, Outcome::Failure);
}

#[test]
fn a_lockout_is_recorded() {
    let s = Server::new(false);
    for _ in 0..3 {
        bolt_hello(&s.security, "rita", "wrong-password");
    }
    bolt_hello(&s.security, "rita", SECRET);
    let locked = s.of(AuditKind::LoginLocked);
    assert_eq!(locked.len(), 1);
    assert_eq!(locked[0].user.as_deref(), Some("rita"));
}

#[tokio::test]
async fn http_logins_and_logouts_are_recorded_once_per_password_login() {
    let s = Server::new(false);
    // A Basic request is a password login.
    assert_eq!(
        s.http("GET", "/auth/me", Some(basic("rita", SECRET)), None)
            .await,
        StatusCode::OK
    );
    // A wrong password is a failed one.
    s.http("GET", "/auth/me", Some(basic("rita", "nope-nope")), None)
        .await;
    // A session token reused per request is not a new login.
    let token = s.users.issue_session("rita").unwrap();
    for _ in 0..3 {
        s.http("GET", "/auth/me", Some(format!("Bearer {token}")), None)
            .await;
    }
    s.http(
        "POST",
        "/auth/logout",
        Some(format!("Bearer {token}")),
        None,
    )
    .await;
    let ok = s.of(AuditKind::LoginSucceeded);
    assert_eq!(ok.len(), 1, "{ok:?}");
    assert_eq!(ok[0].surface.as_deref(), Some("http"));
    assert_eq!(s.of(AuditKind::LoginFailed).len(), 1);
    let out = s.of(AuditKind::Logout);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].user.as_deref(), Some("rita"));
}

#[tokio::test]
async fn the_sign_in_route_records_its_login() {
    let s = Server::new(false);
    s.http(
        "POST",
        "/auth/login",
        None,
        Some(serde_json::json!({"username": "rita", "password": SECRET})),
    )
    .await;
    s.http(
        "POST",
        "/auth/login",
        None,
        Some(serde_json::json!({"username": "rita", "password": "wrong-password"})),
    )
    .await;
    assert_eq!(s.of(AuditKind::LoginSucceeded).len(), 1);
    assert_eq!(s.of(AuditKind::LoginFailed).len(), 1);
}

// ── Changes ──────────────────────────────────────────────────────────

#[tokio::test]
async fn user_role_and_privilege_changes_are_recorded() {
    let s = Server::new(false);
    for q in [
        "CREATE USER zoe SET PASSWORD 'zoe-password-1'",
        "ALTER USER zoe SET PASSWORD 'zoe-password-2'",
        "ALTER USER zoe SET STATUS SUSPENDED",
        "CREATE ROLE triage",
        "GRANT ROLE triage TO zoe",
        "GRANT TRAVERSE ON GRAPH * TO triage",
        "DROP USER zoe",
    ] {
        assert_eq!(s.cypher_as("admin", q).await, StatusCode::OK, "{q}");
    }
    let kinds: Vec<(AuditKind, Option<String>)> = s
        .events()
        .into_iter()
        .filter(|e| e.outcome == Outcome::Success && e.kind != AuditKind::LoginSucceeded)
        .map(|e| (e.kind, e.target))
        .collect();
    assert_eq!(
        kinds,
        [
            (AuditKind::UserChanged, Some("zoe".to_string())),
            (AuditKind::PasswordChanged, Some("zoe".to_string())),
            (AuditKind::UserChanged, Some("zoe".to_string())),
            (AuditKind::RoleChanged, Some("triage".to_string())),
            (AuditKind::RoleChanged, Some("triage".to_string())),
            (AuditKind::PrivilegeChanged, Some("triage".to_string())),
            (AuditKind::UserChanged, Some("zoe".to_string())),
        ]
    );
    for e in s
        .events()
        .iter()
        .filter(|e| e.kind != AuditKind::LoginSucceeded)
    {
        assert_eq!(e.user.as_deref(), Some("admin"), "{e:?}");
        assert_eq!(e.surface.as_deref(), Some("http"), "{e:?}");
    }
}

#[tokio::test]
async fn a_users_own_password_change_is_recorded() {
    let s = Server::new(false);
    let q = format!("ALTER CURRENT USER SET PASSWORD FROM '{SECRET}' TO 'a-brand-new-pw'");
    assert_eq!(s.cypher_as("rita", &q).await, StatusCode::OK);
    let changed = s.of(AuditKind::PasswordChanged);
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].user.as_deref(), Some("rita"));
    assert_eq!(changed[0].target.as_deref(), Some("rita"));
}

// ── Refusals ─────────────────────────────────────────────────────────

#[tokio::test]
async fn refusals_are_recorded_without_the_query_by_default() {
    let s = Server::new(false);
    assert_eq!(
        s.cypher_as("rita", "CREATE (:Bug {title: 'x'})").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        s.cypher_as("rita", "CREATE USER eve SET PASSWORD 'eve-password-1'")
            .await,
        StatusCode::FORBIDDEN
    );
    let token = s.users.issue_session("rita").unwrap();
    assert_eq!(
        s.http(
            "GET",
            "/storage/bloat",
            Some(format!("Bearer {token}")),
            None
        )
        .await,
        StatusCode::FORBIDDEN
    );
    let refused = s.of(AuditKind::Forbidden);
    assert_eq!(refused.len(), 3, "{refused:?}");
    for e in &refused {
        assert_eq!(e.user.as_deref(), Some("rita"));
        assert_eq!(e.outcome, Outcome::Failure);
        assert!(e.reason.is_some());
        assert!(e.query.is_none(), "no query text unless asked for");
    }
}

#[tokio::test]
async fn refused_queries_can_be_recorded_when_asked_for() {
    let s = Server::new(true);
    s.cypher_as("rita", "CREATE (:Bug {title: 'x'})").await;
    let refused = s.of(AuditKind::Forbidden);
    assert_eq!(
        refused[0].query.as_deref(),
        Some("CREATE (:Bug {title: 'x'})")
    );
}

#[test]
fn bolt_refusals_are_recorded() {
    let s = Server::new(false);
    let mut session = bolt_hello(&s.security, "rita", SECRET);
    session.handle(ClientMessage::Run {
        query: "CREATE (:Bug)".into(),
        parameters: dict(vec![]),
        extra: dict(vec![]),
    });
    let refused = s.of(AuditKind::Forbidden);
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].surface.as_deref(), Some("bolt"));
}

// ── Never a password ─────────────────────────────────────────────────

#[tokio::test]
async fn no_password_ever_reaches_the_log() {
    // Query text on: the most a log line can carry.
    let s = Server::new(true);
    // Every way a password travels: Bolt and HTTP logins (right and wrong),
    // the sign-in route, user commands that set passwords, a user's own
    // change, and refused password commands.
    bolt_hello(&s.security, "rita", SECRET);
    bolt_hello(&s.security, "rita", &format!("{SECRET}x"));
    s.http("GET", "/auth/me", Some(basic("rita", SECRET)), None)
        .await;
    s.http(
        "GET",
        "/auth/me",
        Some(basic("admin", &format!("x{SECRET}"))),
        None,
    )
    .await;
    s.http(
        "POST",
        "/auth/login",
        None,
        Some(serde_json::json!({"username": "admin", "password": SECRET})),
    )
    .await;
    for (user, q) in [
        (
            "admin",
            format!("CREATE USER zoe SET PASSWORD '{SECRET}-zoe'"),
        ),
        (
            "admin",
            format!("ALTER USER zoe SET PASSWORD '{SECRET}-zoe2'"),
        ),
        (
            "rita",
            format!("CREATE USER eve SET PASSWORD '{SECRET}-eve'"),
        ),
        (
            "rita",
            format!("ALTER USER admin SET PASSWORD '{SECRET}-own'"),
        ),
        (
            "rita",
            format!("ALTER CURRENT USER SET PASSWORD FROM '{SECRET}' TO '{SECRET}-new'"),
        ),
    ] {
        s.cypher_as(user, &q).await;
    }
    let events = s.events();
    assert!(events.len() >= 10, "{events:?}");
    for e in &events {
        let line = e.to_json_line();
        assert!(!line.contains(SECRET), "password in the audit log: {line}");
    }
}

// ── The server's audit log file ──────────────────────────────────────

/// `server::run` with `DREVO_AUDIT_LOG` set writes JSON lines to the file,
/// with the client's address.
#[tokio::test]
async fn the_server_writes_the_audit_log_file() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let dir = tempfile::TempDir::new().unwrap();
    let log = dir.path().join("audit.log");
    let env: std::collections::HashMap<&str, String> = std::collections::HashMap::from([
        ("DREVO_HOST", "127.0.0.1".to_string()),
        ("DREVO_DATA_DIR", dir.path().to_string_lossy().to_string()),
        ("DREVO_AUTH", format!("admin/{SECRET}")),
        ("DREVO_AUDIT_LOG", log.to_string_lossy().to_string()),
    ]);
    let mut cfg = drevo::server::Config::from_env(move |k| env.get(k).cloned()).unwrap();
    // Port 0: the system picks a free port, the server reports it.
    cfg.port = 0;
    let (listening, announced) = tokio::sync::oneshot::channel();
    let server =
        tokio::spawn(async move { drevo::server::run_announcing(cfg, listening).await.unwrap() });
    let port = tokio::time::timeout(std::time::Duration::from_secs(30), announced)
        .await
        .expect("the server started in time")
        .expect("the server started")
        .http
        .port();
    let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let request = format!(
        "GET /auth/me HTTP/1.1\r\nHost: localhost\r\nAuthorization: {}\r\nConnection: close\r\n\r\n",
        basic("admin", "wrong-password")
    );
    tcp.write_all(request.as_bytes()).await.unwrap();
    let mut reply = Vec::new();
    let _ = tcp.read_to_end(&mut reply).await;
    assert!(String::from_utf8_lossy(&reply).starts_with("HTTP/1.1 401"));

    let text = std::fs::read_to_string(&log).unwrap();
    let failed: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|v| v["event"] == "login_failed")
        .collect();
    assert_eq!(failed.len(), 1, "{text}");
    assert_eq!(failed[0]["user"], "admin");
    assert_eq!(failed[0]["surface"], "http");
    assert!(
        failed[0]["client"]
            .as_str()
            .unwrap()
            .starts_with("127.0.0.1:"),
        "{text}"
    );
    assert!(!text.contains(SECRET));
    server.abort();
}
