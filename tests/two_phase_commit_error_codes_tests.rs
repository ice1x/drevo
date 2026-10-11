//! Stable error codes for resolving a prepared transaction (issue #590).
//!
//! An external two-phase-commit coordinator must tell "this gid is unknown or
//! already resolved" and "this gid was rolled back heuristically" apart from
//! every other failure, by code alone:
//!
//! - Bolt `FAILURE.code` is `Neo.ClientError.Transaction.UnknownPreparedTransaction`
//!   or `Neo.ClientError.Transaction.HeuristicRollback`;
//! - HTTP answers 404 / 409 with the same string in the body's `code`, on the
//!   `/transactions/prepared/{gid}/…` routes and on `POST /cypher`;
//! - the message text is unchanged, so matching on it keeps working.

#![cfg(feature = "http")]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::bolt::packstream::Value as Bolt;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::executor::{ExecError, UnresolvedPrepared};
use drevo::cypher::parser::parse;
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use http_body_util::BodyExt;
use tower::ServiceExt;

const UNKNOWN: &str = "Neo.ClientError.Transaction.UnknownPreparedTransaction";
const HEURISTIC: &str = "Neo.ClientError.Transaction.HeuristicRollback";

fn prepared(svc: &NativeService, gid: &str) {
    let tx = svc.begin_tx();
    svc.execute_in_tx(tx, &parse("CREATE (:T)").unwrap(), HashMap::new())
        .unwrap();
    svc.prepare_tx(tx, gid).unwrap();
}

fn heuristically_rolled_back(svc: &NativeService, gid: &str) {
    prepared(svc, gid);
    svc.heuristic_rollback_prepared(gid).unwrap();
}

// ── the codes themselves ─────────────────────────────────────────────────

#[test]
fn codes_are_stable_strings() {
    assert_eq!(UnresolvedPrepared::UnknownGid.code(), UNKNOWN);
    assert_eq!(UnresolvedPrepared::HeuristicRollback.code(), HEURISTIC);
}

#[test]
fn cypher_errors_carry_the_reason_and_keep_the_message() {
    let svc = NativeService::in_memory();
    heuristically_rolled_back(&svc, "late");
    for (q, reason, text) in [
        (
            "CALL drevo.tx.commitPrepared('nope')",
            UnresolvedPrepared::UnknownGid,
            "no transaction is prepared as \"nope\"",
        ),
        (
            "CALL drevo.tx.rollbackPrepared('nope')",
            UnresolvedPrepared::UnknownGid,
            "no transaction is prepared as \"nope\"",
        ),
        (
            "CALL drevo.tx.heuristicRollback('nope')",
            UnresolvedPrepared::UnknownGid,
            "no transaction is prepared as \"nope\"",
        ),
        (
            "CALL drevo.tx.commitPrepared('late')",
            UnresolvedPrepared::HeuristicRollback,
            "rolled back heuristically",
        ),
    ] {
        let err = svc
            .execute(&parse(q).unwrap(), HashMap::new())
            .expect_err(q);
        match &err {
            ExecError::PreparedTransactionUnresolved { reason: got, .. } => {
                assert_eq!(*got, reason, "{q}");
            }
            other => panic!("{q}: expected PreparedTransactionUnresolved, got {other:?}"),
        }
        let message = err.to_string();
        assert!(
            message.starts_with("invalid procedure call `drevo.tx."),
            "{message}"
        );
        assert!(message.contains(text), "{q}: {message}");
    }
}

// ── Bolt ────────────────────────────────────────────────────────────────

fn hello(svc: &Arc<NativeService>) -> Session<'static> {
    let mut s = Session::new_durable(Arc::clone(svc));
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });
    s
}

fn failure_code(s: &mut Session<'_>, q: &str) -> String {
    let replies = s.handle(ClientMessage::Run {
        query: q.to_string(),
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
    // Leave the session usable for the next statement.
    s.handle(ClientMessage::Reset);
    code.unwrap_or_else(|| panic!("`{q}` should fail, got {replies:?}"))
}

#[test]
fn bolt_classifies_resolve_failures_by_code() {
    let svc = Arc::new(NativeService::in_memory());
    heuristically_rolled_back(&svc, "late");
    let mut s = hello(&svc);
    assert_eq!(
        failure_code(&mut s, "CALL drevo.tx.commitPrepared('dat:nope')"),
        UNKNOWN
    );
    assert_eq!(
        failure_code(&mut s, "CALL drevo.tx.rollbackPrepared('dat:nope')"),
        UNKNOWN
    );
    assert_eq!(
        failure_code(&mut s, "CALL drevo.tx.commitPrepared('late')"),
        HEURISTIC
    );
    // Any other failure of the same procedures keeps its usual code.
    assert_eq!(
        failure_code(&mut s, "CALL drevo.tx.commitPrepared(42)"),
        "Neo.ClientError.Statement.SemanticError"
    );
}

#[test]
fn a_resolved_gid_is_unknown_the_second_time() {
    let svc = Arc::new(NativeService::in_memory());
    prepared(&svc, "once");
    let mut s = hello(&svc);
    let replies = s.handle(ClientMessage::Run {
        query: "CALL drevo.tx.commitPrepared('once')".to_string(),
        parameters: BTreeMap::new(),
        extra: BTreeMap::new(),
    });
    assert!(matches!(replies[0], ServerMessage::Success { .. }));
    s.handle(ClientMessage::Pull {
        extra: BTreeMap::from([("n".to_string(), Bolt::Integer(-1))]),
    });
    assert_eq!(
        failure_code(&mut s, "CALL drevo.tx.commitPrepared('once')"),
        UNKNOWN
    );
}

// ── HTTP ────────────────────────────────────────────────────────────────

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: &str,
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
    (status, serde_json::from_slice(&bytes).expect("json body"))
}

fn app() -> axum::Router {
    let svc = Arc::new(NativeService::in_memory());
    heuristically_rolled_back(&svc, "late");
    build_native_router(NativeApiState::new(svc))
}

#[tokio::test]
async fn http_routes_carry_the_code() {
    let app = app();
    for (uri, status, code) in [
        (
            "/transactions/prepared/nope/commit",
            StatusCode::NOT_FOUND,
            UNKNOWN,
        ),
        (
            "/transactions/prepared/nope/rollback",
            StatusCode::NOT_FOUND,
            UNKNOWN,
        ),
        (
            "/transactions/prepared/nope/heuristic-rollback",
            StatusCode::NOT_FOUND,
            UNKNOWN,
        ),
        (
            "/transactions/prepared/late/commit",
            StatusCode::CONFLICT,
            HEURISTIC,
        ),
    ] {
        let (got, body) = call(&app, "POST", uri, "").await;
        assert_eq!(got, status, "{uri}: {body}");
        assert_eq!(body["code"], code, "{uri}: {body}");
        assert_eq!(body["status"], status.as_u16(), "{uri}: {body}");
    }
}

#[tokio::test]
async fn http_cypher_carries_the_code() {
    let app = app();
    for (query, status, code) in [
        (
            "CALL drevo.tx.commitPrepared('nope')",
            StatusCode::NOT_FOUND,
            UNKNOWN,
        ),
        (
            "CALL drevo.tx.commitPrepared('late')",
            StatusCode::CONFLICT,
            HEURISTIC,
        ),
    ] {
        let body = serde_json::json!({ "query": query }).to_string();
        let (got, json) = call(&app, "POST", "/cypher", &body).await;
        assert_eq!(got, status, "{query}: {json}");
        assert_eq!(json["code"], code, "{query}: {json}");
    }
    // Other Cypher errors keep their shape: 400 and no code.
    let body = serde_json::json!({ "query": "RETURN 1 / 0" }).to_string();
    let (got, json) = call(&app, "POST", "/cypher", &body).await;
    assert_eq!(got, StatusCode::BAD_REQUEST);
    assert!(json.get("code").is_none(), "{json}");
}
