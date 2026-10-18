//! A write to a read-only database fails with Neo4j's code.
//!
//! - Bolt `FAILURE.code` is `Neo.ClientError.General.ForbiddenOnReadOnlyDatabase`,
//!   for an auto-commit write and for the commit of an explicit transaction
//!   that wrote; reads keep working.
//! - HTTP answers 403 with the same string in the body's `code`, on
//!   `POST /cypher` and on the REST write routes.

#![cfg(feature = "http")]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::bolt::packstream::Value as Bolt;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::parser::parse;
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use http_body_util::BodyExt;
use tower::ServiceExt;

const READ_ONLY: &str = "Neo.ClientError.General.ForbiddenOnReadOnlyDatabase";

/// A database holding one bug, then switched read-only.
fn read_only_service() -> Arc<NativeService> {
    let svc = Arc::new(NativeService::in_memory());
    svc.execute(
        &parse("CREATE (:Bug {title: 'BUG-1 login fails'})").unwrap(),
        HashMap::new(),
    )
    .unwrap();
    svc.graph().set_read_only(true).unwrap();
    svc
}

// ── Bolt ────────────────────────────────────────────────────────────────

fn hello(svc: &Arc<NativeService>) -> Session<'static> {
    let mut s = Session::new_durable(Arc::clone(svc));
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });
    s
}

fn run(s: &mut Session<'_>, q: &str) -> Vec<ServerMessage> {
    let mut replies = s.handle(ClientMessage::Run {
        query: q.to_string(),
        parameters: BTreeMap::new(),
        extra: BTreeMap::new(),
    });
    if matches!(replies.first(), Some(ServerMessage::Success { .. })) {
        replies.extend(s.handle(ClientMessage::Pull {
            extra: BTreeMap::from([("n".to_string(), Bolt::Integer(-1))]),
        }));
    }
    replies
}

fn code_of(replies: &[ServerMessage]) -> Option<String> {
    replies.iter().find_map(|m| match m {
        ServerMessage::Failure { metadata } => match metadata.get("code") {
            Some(Bolt::String(c)) => Some(c.clone()),
            _ => None,
        },
        _ => None,
    })
}

#[test]
fn bolt_auto_commit_write_fails_with_the_read_only_code() {
    let svc = read_only_service();
    let mut s = hello(&svc);
    let replies = run(&mut s, "CREATE (:Bug {title: 'BUG-2 logout fails'})");
    assert_eq!(code_of(&replies).as_deref(), Some(READ_ONLY), "{replies:?}");
    s.handle(ClientMessage::Reset);

    let replies = run(&mut s, "MATCH (b:Bug) RETURN b.title");
    assert_eq!(code_of(&replies), None, "reads still work: {replies:?}");
    assert!(replies
        .iter()
        .any(|m| matches!(m, ServerMessage::Record { .. })));
}

#[test]
fn bolt_explicit_transaction_that_wrote_cannot_commit() {
    let svc = read_only_service();
    let mut s = hello(&svc);
    s.handle(ClientMessage::Begin {
        extra: BTreeMap::new(),
    });
    let _ = run(&mut s, "CREATE (:Bug {title: 'BUG-3'})");
    let replies = s.handle(ClientMessage::Commit);
    assert_eq!(code_of(&replies).as_deref(), Some(READ_ONLY), "{replies:?}");
    assert_eq!(svc.graph().node_count(), 1);
}

#[test]
fn bolt_explicit_transaction_that_only_read_commits() {
    let svc = read_only_service();
    let mut s = hello(&svc);
    s.handle(ClientMessage::Begin {
        extra: BTreeMap::new(),
    });
    let replies = run(&mut s, "MATCH (b:Bug) RETURN count(b)");
    assert_eq!(code_of(&replies), None, "{replies:?}");
    let replies = s.handle(ClientMessage::Commit);
    assert!(
        matches!(replies.first(), Some(ServerMessage::Success { .. })),
        "{replies:?}"
    );
}

// ── HTTP ────────────────────────────────────────────────────────────────

async fn call(app: &axum::Router, uri: &str, body: &str) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("POST")
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

#[tokio::test]
async fn http_cypher_write_is_403_with_the_code() {
    let app = build_native_router(NativeApiState::new(read_only_service()));
    let (status, body) = call(
        &app,
        "/cypher",
        r#"{"query": "CREATE (:Bug {title: 'BUG-2'})"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], READ_ONLY, "{body}");

    let (status, body) = call(
        &app,
        "/cypher",
        r#"{"query": "MATCH (b:Bug) RETURN b.title"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rows"][0][0], "BUG-1 login fails");
}

#[tokio::test]
async fn http_rest_write_is_403_with_the_code() {
    let app = build_native_router(NativeApiState::new(read_only_service()));
    let (status, body) = call(
        &app,
        "/nodes",
        r#"{"kind": "Bug", "title": "BUG-2", "body": "", "body_html": "", "properties": {}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], READ_ONLY, "{body}");
}
