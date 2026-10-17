//! Signing in with a token from an identity provider (Keycloak, Kanidm,
//! Auth0, …): `Authorization: Bearer <JWT>` over HTTP, gRPC and Arrow Flight,
//! the `bearer` scheme over Bolt. The token's groups map to roles; the user
//! has no record in the server's catalog.
//!
//! The tokens are signed in the test with the RSA key made for drevo-auth's
//! tests (`drevo-auth/tests/fixtures`), and the provider's discovery document
//! and key set are served from 127.0.0.1.
#![cfg(feature = "oidc")]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::bolt::packstream::Value;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use drevo::oidc::HttpJwks;
use drevo::security::{Security, LOGIN_REFUSED};
use drevo_auth::audit::{AuditKind, MemorySink};
use drevo_auth::catalog::MemoryStore;
use drevo_auth::oidc::{
    parse_role_map, ClaimMapping, JwkSet, JwksSource, StaticJwks, TokenValidator, ValidatorConfig,
};
use drevo_auth::users::{AuthManager, NewUser};
use ring::rand::SystemRandom;
use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};
use tower::ServiceExt;

const ISSUER: &str = "https://login.example.test/realms/drevo";
const AUDIENCE: &str = "drevo";
const UNAUTHORIZED: &str = "Neo.ClientError.Security.Unauthorized";
const FORBIDDEN: &str = "Neo.ClientError.Security.Forbidden";

fn b64url(bytes: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..=chunk.len() {
            out.push(char::from(A[((n >> (18 - 6 * i)) & 63) as usize]));
        }
    }
    out
}

fn now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

/// The identity provider: its RSA signing key as `rsa-1`.
struct Provider {
    rsa: RsaKeyPair,
}

impl Provider {
    fn new() -> Self {
        let rsa = RsaKeyPair::from_pkcs8(include_bytes!(
            "../drevo-auth/tests/fixtures/test_rsa_2048.pk8"
        ))
        .unwrap();
        Self { rsa }
    }

    fn jwks(&self) -> String {
        let public: serde_json::Value = serde_json::from_str(include_str!(
            "../drevo-auth/tests/fixtures/test_rsa_2048.public.json"
        ))
        .unwrap();
        serde_json::json!({"keys": [{
            "kty": "RSA", "kid": "rsa-1", "use": "sig", "alg": "RS256",
            "n": public["n"], "e": public["e"]
        }]})
        .to_string()
    }

    fn sign_with(&self, alg: &str, claims: &serde_json::Value) -> String {
        let header = serde_json::json!({"alg": alg, "typ": "JWT", "kid": "rsa-1"});
        let input = format!(
            "{}.{}",
            b64url(header.to_string().as_bytes()),
            b64url(claims.to_string().as_bytes())
        );
        let signature = if alg == "RS256" {
            let mut sig = vec![0u8; self.rsa.public().modulus_len()];
            self.rsa
                .sign(
                    &RSA_PKCS1_SHA256,
                    &SystemRandom::new(),
                    input.as_bytes(),
                    &mut sig,
                )
                .unwrap();
            sig
        } else {
            Vec::new()
        };
        format!("{input}.{}", b64url(&signature))
    }

    fn sign(&self, claims: &serde_json::Value) -> String {
        self.sign_with("RS256", claims)
    }

    /// A token for `user` in `groups`, valid for five minutes.
    fn token(&self, user: &str, groups: &[&str]) -> String {
        self.sign(&claims(user, groups))
    }
}

fn claims(user: &str, groups: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "iss": ISSUER,
        "aud": AUDIENCE,
        "sub": format!("id-{user}"),
        "preferred_username": user,
        "groups": groups,
        "exp": now() + 300,
        "iat": now() - 5,
    })
}

/// Users named by `preferred_username`; group `devs` → `editor`, `staff` →
/// `reader`, `triage` → `triage` (a role the catalog may not have yet).
fn validator(provider: &Provider) -> Arc<TokenValidator> {
    let mapping = ClaimMapping {
        username_claim: "preferred_username".into(),
        roles_claim: Some("groups".into()),
        role_map: parse_role_map("devs=editor,staff=reader,triage=triage").unwrap(),
    };
    let keys = JwkSet::from_json(&provider.jwks()).unwrap();
    Arc::new(TokenValidator::new(
        ValidatorConfig::new(ISSUER, AUDIENCE),
        mapping,
        Arc::new(StaticJwks(keys)),
    ))
}

struct Server {
    provider: Provider,
    users: Arc<AuthManager>,
    security: Security,
    sink: Arc<MemorySink>,
    app: axum::Router,
}

impl Server {
    /// A local `admin` (password `long-password`) and a provider.
    fn new() -> Self {
        let users = Arc::new(AuthManager::open(Arc::new(MemoryStore::new())).unwrap());
        users
            .create_user(NewUser::new("admin", "long-password").change_required(false))
            .unwrap();
        users.grant_role("admin", "admin").unwrap();
        Self::with_security(users.clone(), Security::new(users))
    }

    fn with_security(users: Arc<AuthManager>, security: Security) -> Self {
        let provider = Provider::new();
        let sink = Arc::new(MemorySink::new());
        let security = security
            .with_oidc(validator(&provider))
            .with_audit(sink.clone(), false);
        let state = NativeApiState::new(Arc::new(NativeService::in_memory()))
            .with_security(security.clone());
        Self {
            provider,
            users,
            security,
            sink,
            app: build_native_router(state),
        }
    }

    async fn request(
        &self,
        method: &str,
        uri: &str,
        authorization: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(auth) = authorization {
            req = req.header("authorization", auth);
        }
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

    async fn cypher(&self, token: &str, query: &str) -> (StatusCode, serde_json::Value) {
        self.request(
            "POST",
            "/cypher",
            Some(&format!("Bearer {token}")),
            Some(serde_json::json!({ "query": query })),
        )
        .await
    }

    fn bolt(&self, scheme: &str, credentials: &str) -> (Session<'_>, Vec<ServerMessage>) {
        let mut session = Session::new_durable(Arc::new(NativeService::in_memory()))
            .with_security(self.security.clone());
        let replies = session.handle(ClientMessage::Hello {
            extra: dict(vec![
                ("scheme", Value::String(scheme.into())),
                ("credentials", Value::String(credentials.into())),
            ]),
        });
        (session, replies)
    }
}

fn dict(e: Vec<(&str, Value)>) -> BTreeMap<String, Value> {
    e.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

fn failure_code(replies: &[ServerMessage]) -> Option<Value> {
    replies.iter().find_map(|m| match m {
        ServerMessage::Failure { metadata } => metadata.get("code").cloned(),
        _ => None,
    })
}

fn run(session: &mut Session, query: &str) -> Vec<ServerMessage> {
    let mut replies = session.handle(ClientMessage::Run {
        query: query.into(),
        parameters: dict(vec![]),
        extra: dict(vec![]),
    });
    replies.extend(session.handle(ClientMessage::Pull {
        extra: dict(vec![("n", Value::Integer(-1))]),
    }));
    replies
}

// ── HTTP ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_provider_token_signs_in_with_the_roles_its_groups_map_to() {
    let s = Server::new();
    let token = s.provider.token("ann", &["devs", "unmapped"]);
    let (status, json) = s.cypher(&token, "CREATE (:Task {title: 'triage'})").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let (status, json) = s.cypher(&token, "SHOW CURRENT USER").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let row = &json["rows"][0];
    assert_eq!(row[0], "ann", "{json}");
    assert_eq!(row[1], serde_json::json!(["PUBLIC", "editor"]), "{json}");
    assert_eq!(row[2], false, "no password to change: {json}");
    // No local record was made for the provider's user.
    assert!(s.users.user("ann").is_none());
}

#[tokio::test]
async fn a_reader_token_reads_but_cannot_write() {
    let s = Server::new();
    let token = s.provider.token("rita", &["staff"]);
    let (status, json) = s.cypher(&token, "MATCH (n) RETURN count(n)").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let (status, json) = s.cypher(&token, "CREATE (:Task)").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(json["code"], FORBIDDEN);
    // The REST routes follow the same roles.
    let auth = format!("Bearer {token}");
    let (status, json) = s.request("GET", "/export/json", Some(&auth), None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let (status, json) = s
        .request(
            "POST",
            "/nodes",
            Some(&auth),
            Some(serde_json::json!({"title": "t", "body": "", "body_html": "", "properties": {}})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
}

#[tokio::test]
async fn a_token_without_mapped_groups_reaches_nothing() {
    let s = Server::new();
    s.cypher(&s.provider.token("ann", &["devs"]), "CREATE (:Task)")
        .await;
    let token = s.provider.token("guest", &["visitors"]);
    let (status, json) = s.cypher(&token, "MATCH (t:Task) RETURN count(t)").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["rows"][0][0], 0, "PUBLIC sees no nodes: {json}");
    let (status, _) = s.cypher(&token, "CREATE (:Task)").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_change_to_a_mapped_role_applies_at_once() {
    let s = Server::new();
    s.cypher(&s.provider.token("ann", &["devs"]), "CREATE (:Task)")
        .await;
    let token = s.provider.token("tom", &["triage"]);
    let count = "MATCH (t:Task) RETURN count(t)";
    assert_eq!(s.cypher(&token, count).await.1["rows"][0][0], 0);
    s.users.create_role("triage", false, None).unwrap();
    let grant = drevo_auth::privileges::parse("GRANT MATCH {*} ON GRAPH * TO triage")
        .unwrap()
        .privileges;
    s.users
        .grant(drevo_auth::privileges::Access::Grant, &grant, "triage")
        .unwrap();
    let (_, json) = s.cypher(&token, count).await;
    assert_eq!(json["rows"][0][0], 1, "{json}");
}

#[tokio::test]
async fn refused_tokens_get_the_same_answer_and_are_audited() {
    let s = Server::new();
    let p = &s.provider;
    let mut expired = claims("ann", &["devs"]);
    expired["exp"] = (now() - 3600).into();
    let mut wrong_audience = claims("ann", &["devs"]);
    wrong_audience["aud"] = "someone-else".into();
    let mut wrong_issuer = claims("ann", &["devs"]);
    wrong_issuer["iss"] = "https://evil.example.test".into();
    let good = p.token("ann", &["devs"]);
    let (head, _) = good.rsplit_once('.').unwrap();
    let tampered = format!("{head}.{}", b64url(b"not the signature"));
    let mut admin_claims = claims("ann", &["devs"]);
    admin_claims["groups"] = serde_json::json!(["admins"]);
    let (_, original_claims) = head.split_once('.').unwrap();
    let swapped = good.replacen(
        original_claims,
        &b64url(admin_claims.to_string().as_bytes()),
        1,
    );
    for token in [
        p.sign(&expired),
        p.sign(&wrong_audience),
        p.sign(&wrong_issuer),
        tampered,
        swapped,
        p.sign_with("none", &claims("ann", &["devs"])),
        "a.b.c".to_string(),
    ] {
        let (status, json) = s.cypher(&token, "RETURN 1").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{token}: {json}");
        assert_eq!(json["code"], UNAUTHORIZED);
        assert_eq!(json["error"], LOGIN_REFUSED, "the client learns nothing");
    }
    let failed: Vec<_> = s
        .sink
        .events()
        .into_iter()
        .filter(|e| e.kind == AuditKind::LoginFailed)
        .collect();
    assert_eq!(failed.len(), 7, "{failed:?}");
    let reasons: Vec<String> = failed.iter().filter_map(|e| e.reason.clone()).collect();
    assert!(reasons[0].contains("expired"), "{reasons:?}");
    assert!(reasons.iter().all(|r| !r.contains("eyJ")), "no token text");
}

#[tokio::test]
async fn a_token_cannot_borrow_a_local_users_privileges() {
    // The provider names a user `admin`, a reader there; the local `admin`
    // is an administrator. The token gets the provider's roles only.
    let s = Server::new();
    let token = s.provider.token("admin", &["staff"]);
    for q in [
        "GRANT ROLE admin TO admin",
        "CREATE USER mallory SET PASSWORD 'long-password'",
        "CREATE (:Task)",
    ] {
        let (status, json) = s.cypher(&token, q).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{q}: {json}");
    }
    let (status, json) = s
        .cypher(
            &token,
            "ALTER CURRENT USER SET PASSWORD FROM 'long-password' TO 'stolen-password'",
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert!(s.users.check_password("admin", "long-password"));
    for q in ["SHOW USER PRIVILEGES", "SHOW USER admin PRIVILEGES"] {
        let (status, json) = s.cypher(&token, q).await;
        assert_eq!(status, StatusCode::OK, "{q}: {json}");
        let roles: std::collections::BTreeSet<String> = json["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r[5].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            roles.into_iter().collect::<Vec<_>>(),
            ["PUBLIC", "reader"],
            "{q}: {json}"
        );
    }
}

#[tokio::test]
async fn local_passwords_and_sessions_still_work() {
    let s = Server::new();
    let mut encoded = b64url(b"admin:long-password")
        .replace('-', "+")
        .replace('_', "/");
    while encoded.len() % 4 != 0 {
        encoded.push('=');
    }
    let basic = format!("Basic {encoded}");
    let (status, json) = s
        .request(
            "POST",
            "/cypher",
            Some(&basic),
            Some(serde_json::json!({"query": "SHOW USERS"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let session = s.users.issue_session("admin").unwrap();
    let (status, _) = s.cypher(&session, "SHOW USERS").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_provider_makes_logins_required_even_without_a_first_user() {
    // `DREVO_AUTH=none` creates no first user; with a provider, its tokens
    // are still needed.
    let users = Arc::new(AuthManager::open(Arc::new(MemoryStore::new())).unwrap());
    let s = Server::with_security(users.clone(), Security::new(users).without_login());
    assert!(s.security.requires_login());
    let (status, _) = s
        .request(
            "POST",
            "/cypher",
            None,
            Some(serde_json::json!({"query": "RETURN 1"})),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let token = s.provider.token("ann", &["devs"]);
    assert_eq!(s.cypher(&token, "RETURN 1").await.0, StatusCode::OK);
}

#[test]
fn grpc_and_flight_read_the_same_header() {
    // gRPC and Arrow Flight log in through `login_http` with the request's
    // `authorization` metadata.
    let s = Server::new();
    let origin = drevo::security::Origin::new("grpc");
    let token = s.provider.token("ann", &["devs"]);
    let user = s
        .security
        .login_http(&origin, Some(&format!("Bearer {token}")), None)
        .unwrap();
    assert_eq!(user.name, "ann");
    assert_eq!(user.roles.as_deref(), Some(&["editor".to_string()][..]));
    let context = s.security.context_for(Some(&user), "drevo").unwrap();
    assert!(context.writes_everything("drevo"));
}

// ── Bolt ─────────────────────────────────────────────────────────────

#[test]
fn bolt_signs_in_with_the_bearer_scheme() {
    let s = Server::new();
    let (mut session, replies) = s.bolt("bearer", &s.provider.token("rita", &["staff"]));
    assert_eq!(failure_code(&replies), None, "{replies:?}");
    assert_eq!(failure_code(&run(&mut session, "MATCH (n) RETURN n")), None);
    assert_eq!(
        failure_code(&run(&mut session, "CREATE (:Task)")),
        Some(Value::String(FORBIDDEN.into()))
    );
}

#[test]
fn bolt_refuses_a_bad_token() {
    let s = Server::new();
    let mut expired = claims("ann", &["devs"]);
    expired["exp"] = (now() - 3600).into();
    let (_, replies) = s.bolt("bearer", &s.provider.sign(&expired));
    assert_eq!(
        failure_code(&replies),
        Some(Value::String(UNAUTHORIZED.into()))
    );
}

// ── Discovery ────────────────────────────────────────────────────────

/// A provider on 127.0.0.1 serving its discovery document and key set;
/// `issuer_override` makes the document name another issuer.
async fn serve_provider(
    jwks: String,
    issuer_override: Option<String>,
    jwks_uri_override: Option<String>,
) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let issuer = format!("{base}/realms/drevo");
    let fetches = Arc::new(AtomicUsize::new(0));
    let document = serde_json::json!({
        "issuer": issuer_override.unwrap_or_else(|| issuer.clone()),
        "jwks_uri": jwks_uri_override.unwrap_or_else(|| format!("{issuer}/certs")),
    })
    .to_string();
    let counted = fetches.clone();
    let app = axum::Router::new()
        .route(
            "/realms/drevo/.well-known/openid-configuration",
            axum::routing::get(move || {
                let document = document.clone();
                async move { document }
            }),
        )
        .route(
            "/realms/drevo/certs",
            axum::routing::get(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                let jwks = jwks.clone();
                async move { jwks }
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (issuer, fetches)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_key_set_is_found_through_the_providers_discovery_document() {
    let provider = Provider::new();
    let (issuer, fetches) = serve_provider(provider.jwks(), None, None).await;
    let source = HttpJwks::new(&issuer).unwrap();
    let mut claims = claims("ann", &["devs"]);
    claims["iss"] = issuer.clone().into();
    let token = provider.sign(&claims);
    let principal = tokio::task::spawn_blocking(move || {
        let validator = TokenValidator::new(
            ValidatorConfig::new(issuer, AUDIENCE),
            ClaimMapping {
                username_claim: "preferred_username".into(),
                ..ClaimMapping::default()
            },
            Arc::new(source),
        );
        let first = validator.validate(&token).unwrap();
        // The keys are cached: the second token needs no fetch.
        validator.validate(&token).unwrap();
        first
    })
    .await
    .unwrap();
    assert_eq!(principal.name, "ann");
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_discovery_document_for_another_issuer_is_refused() {
    let provider = Provider::new();
    let (issuer, _) = serve_provider(
        provider.jwks(),
        Some("https://evil.example.test".into()),
        None,
    )
    .await;
    let source = HttpJwks::new(&issuer).unwrap();
    let err = tokio::task::spawn_blocking(move || source.fetch().unwrap_err())
        .await
        .unwrap();
    assert!(err.to_string().contains("issuer"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_set_over_plain_http_elsewhere_is_refused() {
    let provider = Provider::new();
    let (issuer, _) = serve_provider(
        provider.jwks(),
        None,
        Some("http://keys.example.test/certs".into()),
    )
    .await;
    let source = HttpJwks::new(&issuer).unwrap();
    let err = tokio::task::spawn_blocking(move || source.fetch().unwrap_err())
        .await
        .unwrap();
    assert!(err.to_string().contains("https"), "{err}");
}

#[test]
fn a_provider_must_use_https_unless_it_is_this_machine() {
    for ok in [
        "https://login.example.test/realms/drevo",
        "http://localhost:8180/realms/drevo",
        "http://127.0.0.1:8180/realms/drevo",
        "http://[::1]:8180/realms/drevo",
    ] {
        assert!(HttpJwks::new(ok).is_ok(), "{ok}");
    }
    for bad in [
        "http://login.example.test/realms/drevo",
        "ftp://login.example.test",
        "not a url",
        "",
    ] {
        assert!(HttpJwks::new(bad).is_err(), "{bad}");
    }
}
