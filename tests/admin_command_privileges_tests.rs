//! The user, role and privilege commands need Neo4j's DBMS privileges when the
//! server requires a login: `USER MANAGEMENT`, `ROLE MANAGEMENT` and
//! `PRIVILEGE MANAGEMENT`. Users may always look at and change themselves:
//! `SHOW CURRENT USER`, `ALTER CURRENT USER SET PASSWORD`, and their own
//! privileges.
#![cfg(all(feature = "http", feature = "auth"))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use drevo::security::Security;
use drevo_auth::catalog::MemoryStore;
use drevo_auth::users::{AuthManager, NewUser};
use tower::ServiceExt;

const FORBIDDEN: &str = "Neo.ClientError.Security.Forbidden";

struct Server {
    app: axum::Router,
    users: Arc<AuthManager>,
}

impl Server {
    /// `admin`; `ed` (editor); `una` who may manage users only; `rob` who may
    /// manage roles only.
    fn new() -> Self {
        let users = Arc::new(AuthManager::open(Arc::new(MemoryStore::new())).unwrap());
        for (name, role) in [
            ("admin", "admin"),
            ("ed", "editor"),
            ("una", "editor"),
            ("rob", "editor"),
        ] {
            users
                .create_user(NewUser::new(name, "long-password").change_required(false))
                .unwrap();
            users.grant_role(role, name).unwrap();
        }
        for (role, privilege, user) in [
            ("user_admin", "GRANT USER MANAGEMENT ON DBMS TO x", "una"),
            ("role_admin", "GRANT ROLE MANAGEMENT ON DBMS TO x", "rob"),
        ] {
            users.create_role(role, false, None).unwrap();
            let p = drevo_auth::privileges::parse(privilege).unwrap().privileges;
            users
                .grant(drevo_auth::privileges::Access::Grant, &p, role)
                .unwrap();
            users.grant_role(role, user).unwrap();
        }
        let state = NativeApiState::new(Arc::new(NativeService::in_memory()))
            .with_security(Security::new(Arc::clone(&users)));
        Self {
            app: build_native_router(state),
            users,
        }
    }

    async fn run(&self, user: &str, query: &str) -> (StatusCode, serde_json::Value) {
        let token = self.users.issue_session(user).unwrap();
        let body = serde_json::json!({ "query": query }).to_string();
        let req = Request::builder()
            .method("POST")
            .uri("/cypher")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    async fn ok(&self, user: &str, query: &str) {
        let (status, json) = self.run(user, query).await;
        assert_eq!(status, StatusCode::OK, "{user}: {query}: {json}");
    }

    async fn forbidden(&self, user: &str, query: &str) {
        let (status, json) = self.run(user, query).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{user}: {query}: {json}");
        assert_eq!(json["code"], FORBIDDEN, "{user}: {query}: {json}");
    }
}

#[tokio::test]
async fn an_editor_cannot_manage_users_roles_or_privileges() {
    let s = Server::new();
    for q in [
        "CREATE USER mallory SET PASSWORD 'long-password'",
        "ALTER USER admin SET PASSWORD 'stolen-password'",
        "ALTER USER admin SET STATUS SUSPENDED",
        "DROP USER admin",
        "SHOW USERS",
        "CREATE ROLE sneaky",
        "GRANT ROLE admin TO ed",
        "REVOKE ROLE admin FROM admin",
        "DROP ROLE admin",
        "SHOW ROLES",
        "GRANT ALL DBMS PRIVILEGES ON DBMS TO editor",
        "REVOKE DENY TRAVERSE ON GRAPH * FROM editor",
        "SHOW PRIVILEGES",
        "SHOW ROLE admin PRIVILEGES",
        "SHOW USER admin PRIVILEGES",
    ] {
        s.forbidden("ed", q).await;
    }
    // Nothing changed.
    assert_eq!(s.users.user_roles("ed").unwrap(), ["PUBLIC", "editor"]);
    assert!(s.users.user("mallory").is_none());
    assert!(s.users.check_password("admin", "long-password"));
}

#[tokio::test]
async fn everyone_may_look_at_and_change_themselves() {
    let s = Server::new();
    s.ok("ed", "SHOW CURRENT USER").await;
    s.ok("ed", "SHOW USER PRIVILEGES").await;
    s.ok("ed", "SHOW USER ed PRIVILEGES").await;
    s.ok("ed", "SHOW SUPPORTED PRIVILEGES").await;
    s.ok(
        "ed",
        "ALTER CURRENT USER SET PASSWORD FROM 'long-password' TO 'longer-password'",
    )
    .await;
    assert!(s.users.check_password("ed", "longer-password"));
}

#[tokio::test]
async fn each_management_privilege_covers_its_own_commands() {
    let s = Server::new();
    s.ok("una", "CREATE USER zoe SET PASSWORD 'long-password'")
        .await;
    s.ok("una", "SHOW USERS").await;
    s.forbidden("una", "CREATE ROLE triage").await;
    s.forbidden("una", "GRANT ROLE editor TO zoe").await;
    s.ok("rob", "CREATE ROLE triage").await;
    s.ok("rob", "GRANT ROLE triage TO zoe").await;
    s.ok("rob", "SHOW ROLES WITH USERS").await;
    s.forbidden("rob", "CREATE USER yan SET PASSWORD 'long-password'")
        .await;
    s.forbidden("rob", "GRANT TRAVERSE ON GRAPH * TO triage")
        .await;
}

#[tokio::test]
async fn an_admin_may_do_everything() {
    let s = Server::new();
    for q in [
        "CREATE USER zoe SET PASSWORD 'long-password'",
        "SHOW USERS",
        "CREATE ROLE triage",
        "GRANT ROLE triage TO zoe",
        "GRANT TRAVERSE ON GRAPH * TO triage",
        "SHOW PRIVILEGES",
        "SHOW USER zoe PRIVILEGES",
        "DROP USER zoe",
    ] {
        s.ok("admin", q).await;
    }
}

#[test]
fn bolt_refuses_with_the_neo4j_code() {
    use drevo::bolt::packstream::Value;
    use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
    use std::collections::BTreeMap;

    let s = Server::new();
    let dict = |e: Vec<(&str, Value)>| -> BTreeMap<String, Value> {
        e.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    };
    let mut session = Session::new_durable(Arc::new(NativeService::in_memory()))
        .with_security(Security::new(Arc::clone(&s.users)));
    session.handle(ClientMessage::Hello {
        extra: dict(vec![
            ("scheme", Value::String("basic".into())),
            ("principal", Value::String("ed".into())),
            ("credentials", Value::String("long-password".into())),
        ]),
    });
    let replies = session.handle(ClientMessage::Run {
        query: "GRANT ROLE admin TO ed".into(),
        parameters: dict(vec![]),
        extra: dict(vec![]),
    });
    let code = replies.iter().find_map(|m| match m {
        ServerMessage::Failure { metadata } => metadata.get("code").cloned(),
        _ => None,
    });
    assert_eq!(code, Some(Value::String(FORBIDDEN.into())));
    assert_eq!(s.users.user_roles("ed").unwrap(), ["PUBLIC", "editor"]);
}

#[tokio::test]
async fn without_logins_nothing_is_checked() {
    let users = Arc::new(AuthManager::open(Arc::new(MemoryStore::new())).unwrap());
    let state = NativeApiState::new(Arc::new(NativeService::in_memory()))
        .with_security(Security::new(users).without_login());
    let app = build_native_router(state);
    let body =
        serde_json::json!({ "query": "CREATE USER zoe SET PASSWORD 'long-password'" }).to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/cypher")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);
}

/// Preparing a two-phase-commit transaction pauses every write on the
/// database until it is resolved, so it is an administration procedure like
/// the resolvers: an editor may not, an admin may.
#[test]
fn preparing_a_transaction_needs_execute_admin_procedures() {
    use drevo::bolt::packstream::Value;
    use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
    use std::collections::BTreeMap;

    let s = Server::new();
    let dict = |e: Vec<(&str, Value)>| -> BTreeMap<String, Value> {
        e.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    };
    let prepare_as = |user: &str| -> Option<Value> {
        let mut session = Session::new_durable(Arc::new(NativeService::in_memory()))
            .with_security(Security::new(Arc::clone(&s.users)));
        session.handle(ClientMessage::Hello {
            extra: dict(vec![
                ("scheme", Value::String("basic".into())),
                ("principal", Value::String(user.into())),
                ("credentials", Value::String("long-password".into())),
            ]),
        });
        session.handle(ClientMessage::Begin {
            extra: dict(vec![]),
        });
        session.handle(ClientMessage::Run {
            query: "CREATE (:Order)".into(),
            parameters: dict(vec![]),
            extra: dict(vec![]),
        });
        session.handle(ClientMessage::Pull {
            extra: dict(vec![("n", Value::Integer(-1))]),
        });
        let replies = session.handle(ClientMessage::Run {
            query: "CALL drevo.tx.prepare('order-1')".into(),
            parameters: dict(vec![]),
            extra: dict(vec![]),
        });
        replies.iter().find_map(|m| match m {
            ServerMessage::Failure { metadata } => metadata.get("code").cloned(),
            _ => None,
        })
    };
    assert_eq!(prepare_as("ed"), Some(Value::String(FORBIDDEN.into())));
    assert_eq!(prepare_as("admin"), None);
}
