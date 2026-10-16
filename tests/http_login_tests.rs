//! Logging in over HTTP when the server requires it: `Authorization: Basic`
//! or `Bearer <session token>`, or the `drevo_session` cookie that
//! `POST /auth/login` sets for the Web UI. Health, server info and the Web UI's
//! own files stay open so a client can find the server and sign in.
#![cfg(all(feature = "http", feature = "auth"))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use drevo::security::Security;
use drevo_auth::catalog::MemoryStore;
use drevo_auth::users::{AlterUser, AuthManager, NewUser, UserStatus};
use tower::ServiceExt;

const ANN_PW: &str = "ann-password";
const BOB_PW: &str = "bob-password";

fn app() -> (axum::Router, Arc<AuthManager>) {
    let users = Arc::new(AuthManager::open(Arc::new(MemoryStore::new())).unwrap());
    users
        .create_user(NewUser::new("ann", ANN_PW).change_required(false))
        .unwrap();
    users.create_user(NewUser::new("bob", BOB_PW)).unwrap();
    let state = NativeApiState::new(Arc::new(NativeService::in_memory()))
        .with_security(Security::new(Arc::clone(&users)));
    (build_native_router(state), users)
}

fn basic(user: &str, password: &str) -> String {
    // A minimal base64 encoder for the test (the server decodes it).
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
            if i <= chunk.len() {
                out.push(char::from(A[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    format!("Basic {out}")
}

struct Reply {
    status: StatusCode,
    json: serde_json::Value,
    text: String,
    set_cookie: Option<String>,
    www_authenticate: Option<String>,
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    auth: Option<(&str, String)>,
    body: Option<serde_json::Value>,
) -> Reply {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some((name, value)) = auth {
        req = req.header(name, value);
    }
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let header_str = |h: header::HeaderName| {
        resp.headers()
            .get(h)
            .map(|v| v.to_str().unwrap().to_string())
    };
    let set_cookie = header_str(header::SET_COOKIE);
    let www_authenticate = header_str(header::WWW_AUTHENTICATE);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    Reply {
        status,
        json,
        text,
        set_cookie,
        www_authenticate,
    }
}

fn cypher(query: &str) -> Option<serde_json::Value> {
    Some(serde_json::json!({ "query": query }))
}

async fn login(app: &axum::Router, user: &str, password: &str) -> Reply {
    call(
        app,
        "POST",
        "/auth/login",
        None,
        Some(serde_json::json!({ "username": user, "password": password })),
    )
    .await
}

// ── What stays open ──────────────────────────────────────────────────

#[tokio::test]
async fn health_info_and_the_web_ui_need_no_login() {
    let (app, _) = app();
    for uri in [
        "/health",
        "/ready",
        "/",
        "/status",
        "/ui",
        "/ui/app.js",
        "/ui/styles.css",
    ] {
        let r = call(&app, "GET", uri, None, None).await;
        assert_eq!(r.status, StatusCode::OK, "{uri}");
    }
}

#[tokio::test]
async fn everything_else_needs_a_login() {
    let (app, _) = app();
    for (method, uri, body) in [
        ("POST", "/cypher", cypher("RETURN 1")),
        ("GET", "/export/json", None),
        ("GET", "/nodes/1", None),
        ("GET", "/databases", None),
        ("GET", "/metrics", None),
        ("GET", "/problems", None),
        ("GET", "/report", None),
        (
            "POST",
            "/v1/embeddings",
            Some(serde_json::json!({"input": "x"})),
        ),
    ] {
        let r = call(&app, method, uri, None, body).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{method} {uri}");
        assert_eq!(
            r.json["code"], "Neo.ClientError.Security.Unauthorized",
            "{uri}"
        );
        // A Bearer challenge: browsers do not pop up their own login box.
        assert_eq!(
            r.www_authenticate.as_deref(),
            Some(r#"Bearer realm="drevo""#)
        );
    }
}

// ── Basic and Bearer ─────────────────────────────────────────────────

#[tokio::test]
async fn a_basic_header_with_the_right_password_is_let_through() {
    let (app, _) = app();
    let r = call(
        &app,
        "POST",
        "/cypher",
        Some(("authorization", basic("ann", ANN_PW))),
        cypher("RETURN 1 AS one"),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json["rows"], serde_json::json!([[1]]));
}

#[tokio::test]
async fn wrong_or_malformed_credentials_are_refused_alike() {
    let (app, users) = app();
    users
        .alter_user("bob", AlterUser::default().status(UserStatus::Suspended))
        .unwrap();
    let mut messages = Vec::new();
    for auth in [
        basic("ann", "wrong-password"),
        basic("zed", ANN_PW),
        basic("bob", BOB_PW),
        "Basic !!!".to_string(),
        "Bearer not-a-token".to_string(),
        "Digest x".to_string(),
    ] {
        let r = call(
            &app,
            "POST",
            "/cypher",
            Some(("authorization", auth.clone())),
            cypher("RETURN 1"),
        )
        .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{auth}");
        messages.push(r.json["error"].clone());
    }
    messages.dedup();
    assert_eq!(
        messages.len(),
        1,
        "one message for every refusal: {messages:?}"
    );
}

#[tokio::test]
async fn repeated_failures_answer_429() {
    let (app, _) = app();
    for _ in 0..3 {
        call(
            &app,
            "GET",
            "/databases",
            Some(("authorization", basic("ann", "wrong-password"))),
            None,
        )
        .await;
    }
    let r = call(
        &app,
        "GET",
        "/databases",
        Some(("authorization", basic("ann", ANN_PW))),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        r.json["code"],
        "Neo.ClientError.Security.AuthenticationRateLimit"
    );
}

// ── /auth/login, the cookie, /auth/me, /auth/logout ──────────────────

#[tokio::test]
async fn login_returns_a_token_and_sets_a_strict_http_only_cookie() {
    let (app, _) = app();
    let r = login(&app, "ann", ANN_PW).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json["user"], "ann");
    assert_eq!(r.json["passwordChangeRequired"], false);
    let token = r.json["token"].as_str().unwrap().to_string();
    assert_eq!(token.len(), 64);
    let cookie = r.set_cookie.expect("a session cookie");
    assert!(
        cookie.starts_with(&format!("drevo_session={token}")),
        "{cookie}"
    );
    for attr in ["HttpOnly", "SameSite=Strict", "Path=/"] {
        assert!(cookie.contains(attr), "{cookie} lacks {attr}");
    }

    // The token works as a Bearer header and as the cookie.
    let bearer = call(
        &app,
        "GET",
        "/databases",
        Some(("authorization", format!("Bearer {token}"))),
        None,
    )
    .await;
    assert_eq!(bearer.status, StatusCode::OK);
    let cookie = call(
        &app,
        "GET",
        "/databases",
        Some(("cookie", format!("other=1; drevo_session={token}"))),
        None,
    )
    .await;
    assert_eq!(cookie.status, StatusCode::OK);
}

#[tokio::test]
async fn a_failed_login_sets_no_cookie_and_echoes_no_password() {
    let (app, _) = app();
    let r = login(&app, "ann", "hunter2-guess").await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(r.set_cookie.is_none());
    assert!(!r.text.contains("hunter2"));
    let r = call(
        &app,
        "POST",
        "/auth/login",
        None,
        Some(serde_json::json!({"username": "ann"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "a password is required");
}

#[tokio::test]
async fn me_shows_the_signed_in_user() {
    let (app, _) = app();
    let token = login(&app, "ann", ANN_PW).await.json["token"]
        .as_str()
        .unwrap()
        .to_string();
    let r = call(
        &app,
        "GET",
        "/auth/me",
        Some(("cookie", format!("drevo_session={token}"))),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        r.json,
        serde_json::json!({ "user": "ann", "passwordChangeRequired": false })
    );
    let r = call(&app, "GET", "/auth/me", None, None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn logout_ends_the_session_and_clears_the_cookie() {
    let (app, _) = app();
    let token = login(&app, "ann", ANN_PW).await.json["token"]
        .as_str()
        .unwrap()
        .to_string();
    let r = call(
        &app,
        "POST",
        "/auth/logout",
        Some(("cookie", format!("drevo_session={token}"))),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let cleared = r.set_cookie.expect("the cookie is cleared");
    assert!(cleared.contains("Max-Age=0"), "{cleared}");
    let r = call(
        &app,
        "GET",
        "/databases",
        Some(("authorization", format!("Bearer {token}"))),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED, "the token is gone");
}

#[tokio::test]
async fn show_current_user_over_http_shows_the_caller() {
    let (app, _) = app();
    let r = call(
        &app,
        "POST",
        "/cypher",
        Some(("authorization", basic("ann", ANN_PW))),
        cypher("SHOW CURRENT USER"),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["rows"][0][0], "ann");
}

// ── A password that must be changed ──────────────────────────────────

#[tokio::test]
async fn a_user_who_must_change_the_password_can_only_change_it() {
    let (app, users) = app();
    let r = login(&app, "bob", BOB_PW).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["passwordChangeRequired"], true);
    let token = r.json["token"].as_str().unwrap().to_string();
    let as_bob = || Some(("authorization", format!("Bearer {token}")));

    let r = call(&app, "POST", "/cypher", as_bob(), cypher("RETURN 1")).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(
        r.json["code"],
        "Neo.ClientError.Security.CredentialsExpired"
    );
    let r = call(&app, "GET", "/databases", as_bob(), None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = call(&app, "GET", "/auth/me", as_bob(), None).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "the UI can still see who is signed in"
    );

    let r = call(
        &app,
        "POST",
        "/cypher",
        as_bob(),
        cypher(&format!(
            "ALTER CURRENT USER SET PASSWORD FROM '{BOB_PW}' TO 'bob-new-password'"
        )),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert!(users.check_password("bob", "bob-new-password"));
    // Changing the password ends the session; the new password works.
    let r = call(&app, "GET", "/databases", as_bob(), None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = call(
        &app,
        "GET",
        "/databases",
        Some(("authorization", basic("bob", "bob-new-password"))),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
}

// ── Without a login requirement ──────────────────────────────────────

#[tokio::test]
async fn without_a_login_requirement_nothing_asks_for_one() {
    let users = Arc::new(AuthManager::open(Arc::new(MemoryStore::new())).unwrap());
    let state = NativeApiState::new(Arc::new(NativeService::in_memory()))
        .with_security(Security::new(users).without_login());
    let app = build_native_router(state);
    let r = call(&app, "POST", "/cypher", None, cypher("RETURN 1")).await;
    assert_eq!(r.status, StatusCode::OK);
    let r = call(&app, "GET", "/auth/me", None, None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["user"], serde_json::Value::Null, "no login, no user");
}
