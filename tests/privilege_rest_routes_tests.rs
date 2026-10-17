//! The REST routes outside `/cypher` serve the graph unfiltered, so with
//! logins required they need the privileges that make filtering moot: reading
//! the whole graph, writing the whole graph, or administering the server.
//! Without them a route answers 403 with `Neo.ClientError.Security.Forbidden`.
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

struct Server {
    app: axum::Router,
    users: Arc<AuthManager>,
}

impl Server {
    /// `admin`; `rex` (reader); `ed` (editor); `rita` (reader who may not see
    /// `:Secret` nodes); `pat` (only `PUBLIC`).
    async fn new() -> Self {
        let users = Arc::new(AuthManager::open(Arc::new(MemoryStore::new())).unwrap());
        for (name, role) in [
            ("admin", Some("admin")),
            ("rex", Some("reader")),
            ("ed", Some("editor")),
            ("rita", Some("reader")),
            ("pat", None),
        ] {
            users
                .create_user(NewUser::new(name, "long-password").change_required(false))
                .unwrap();
            if let Some(role) = role {
                users.grant_role(role, name).unwrap();
            }
        }
        users.create_role("no_secrets", false, None).unwrap();
        let deny = drevo_auth::privileges::parse("DENY TRAVERSE ON GRAPH * NODES Secret TO x")
            .unwrap()
            .privileges;
        users
            .grant(drevo_auth::privileges::Access::Deny, &deny, "no_secrets")
            .unwrap();
        users.grant_role("no_secrets", "rita").unwrap();
        let state = NativeApiState::new(Arc::new(NativeService::in_memory()))
            .with_security(Security::new(Arc::clone(&users)));
        let s = Self {
            app: build_native_router(state),
            users,
        };
        let (status, _) = s
            .call(
                "admin",
                "POST",
                "/cypher",
                Some(serde_json::json!({"query": "CREATE (:Bug {title: 'b'}), (:Bug:Secret {title: 's'})"})),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        s
    }

    async fn call(
        &self,
        user: &str,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let token = self.users.issue_session(user).unwrap();
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"));
        let req = match body {
            Some(b) => req
                .header("content-type", "application/json")
                .body(Body::from(b.to_string())),
            None => req.body(Body::empty()),
        }
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

    async fn status(&self, user: &str, method: &str, uri: &str) -> StatusCode {
        let body = (method == "POST").then(|| serde_json::json!({}));
        self.call(user, method, uri, body).await.0
    }

    async fn forbidden(
        &self,
        user: &str,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) {
        let (status, json) = self.call(user, method, uri, body).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{user} {method} {uri}: {json}"
        );
        assert_eq!(json["code"], "Neo.ClientError.Security.Forbidden", "{json}");
    }
}

const GRAPH_READS: [&str; 6] = [
    "/export/json",
    "/export/graphml",
    "/nodes",
    "/edges",
    "/facets",
    "/nodes/1/neighbors",
];

#[tokio::test]
async fn graph_read_routes_need_to_read_the_whole_graph() {
    let s = Server::new().await;
    for uri in GRAPH_READS {
        assert_ne!(
            s.status("rex", "GET", uri).await,
            StatusCode::FORBIDDEN,
            "{uri}"
        );
        s.forbidden("rita", "GET", uri, None).await;
        s.forbidden("pat", "GET", uri, None).await;
    }
    s.forbidden(
        "rita",
        "POST",
        "/search/fts",
        Some(serde_json::json!({"query": "s"})),
    )
    .await;
}

#[tokio::test]
async fn graph_write_routes_need_to_write_the_whole_graph() {
    let s = Server::new().await;
    let node = serde_json::json!({"kind": "Bug", "title": "via rest", "body": "", "body_html": "", "properties": {}});
    s.forbidden("rex", "POST", "/nodes", Some(node.clone()))
        .await;
    let (status, json) = s.call("ed", "POST", "/nodes", Some(node)).await;
    assert!(status.is_success(), "{status}: {json}");
    s.forbidden("rex", "POST", "/import/json", Some(serde_json::json!({})))
        .await;
}

#[tokio::test]
async fn database_and_server_administration_need_admin() {
    let s = Server::new().await;
    s.forbidden(
        "ed",
        "POST",
        "/databases",
        Some(serde_json::json!({"name": "erp"})),
    )
    .await;
    let (status, json) = s
        .call(
            "admin",
            "POST",
            "/databases",
            Some(serde_json::json!({"name": "erp"})),
        )
        .await;
    assert!(status.is_success(), "{status}: {json}");
    s.forbidden("ed", "DELETE", "/databases/erp", None).await;
    for (method, uri) in [
        ("GET", "/problems"),
        ("GET", "/report"),
        ("GET", "/storage/bloat"),
        ("POST", "/storage/shrink"),
        ("GET", "/config/embeddings"),
        ("GET", "/transactions/prepared"),
    ] {
        let body = (method == "POST").then(|| serde_json::json!({}));
        s.forbidden("ed", method, uri, body).await;
        assert_ne!(
            s.status("admin", method, uri).await,
            StatusCode::FORBIDDEN,
            "{uri}"
        );
    }
}

#[tokio::test]
async fn routes_that_do_not_touch_the_graph_stay_open_to_every_user() {
    let s = Server::new().await;
    for uri in ["/status", "/auth/me", "/databases", "/metrics", "/ui"] {
        assert_eq!(s.status("pat", "GET", uri).await, StatusCode::OK, "{uri}");
    }
}

#[tokio::test]
async fn without_logins_every_route_is_open() {
    let users = Arc::new(AuthManager::open(Arc::new(MemoryStore::new())).unwrap());
    let state = NativeApiState::new(Arc::new(NativeService::in_memory()))
        .with_security(Security::new(users).without_login());
    let app = build_native_router(state);
    for uri in ["/export/json", "/nodes", "/storage/bloat", "/problems"] {
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let status = app.clone().oneshot(req).await.unwrap().status();
        assert_ne!(status, StatusCode::FORBIDDEN, "{uri}");
    }
}
