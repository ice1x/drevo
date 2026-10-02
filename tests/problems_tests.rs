//! Server problem feed for the Web UI — #552 slice A.
//!
//! Server-side problems (a statement timeout, a storage failure, any WARN /
//! ERROR the server logs) are captured into a bounded in-memory ring buffer by
//! a `tracing` layer, redacted, and served from `GET /problems?since=<seq>` so
//! the Web UI can notify the user and build a problem report.
//!
//! These tests lock:
//! - the ring buffer: bounded, ordered, `since` returns only newer entries;
//! - redaction of key/token-looking fields and `sk-…` values;
//! - the layer captures WARN and ERROR, never INFO and below;
//! - a statement timeout is logged at ERROR with the query and counted;
//!   ordinary client errors (syntax, semantics) are not server problems;
//! - `GET /problems` and the `drevo_statement_timeouts_total` metric;
//! - a Bolt client hanging up is a disconnect, not a problem.

#![cfg(feature = "http")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::bolt::error::BoltError;
use drevo::cypher::executor::ExecError;
use drevo::cypher::parser::parse;
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use drevo::problems::{note_exec_error, statement_timeouts, ProblemLayer, ProblemLog};
use http_body_util::BodyExt;
use tower::ServiceExt;
use tracing_subscriber::layer::SubscriberExt;

fn fields(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn ring_buffer_is_bounded_ordered_and_incremental() {
    let log = ProblemLog::new(3);
    for i in 0..5 {
        log.record("ERROR", "t", &format!("m{i}"), fields(&[]));
    }
    let (all, next) = log.since(0);
    let msgs: Vec<_> = all.iter().map(|p| p.message.as_str()).collect();
    assert_eq!(msgs, vec!["m2", "m3", "m4"], "oldest dropped first");
    assert_eq!(next, 5);
    assert!(all.windows(2).all(|w| w[0].seq < w[1].seq));

    log.record("WARN", "t", "m5", fields(&[]));
    let (newer, next2) = log.since(next);
    assert_eq!(newer.len(), 1);
    assert_eq!(newer[0].message, "m5");
    assert_eq!(newer[0].level, "WARN");
    assert_eq!(next2, 6);
    assert!(log.since(next2).0.is_empty());
}

#[test]
fn secrets_are_redacted() {
    let log = ProblemLog::new(8);
    log.record(
        "ERROR",
        "t",
        "upstream rejected key sk-abcdefghijklmnop1234",
        fields(&[
            ("api_key", "plain-secret"),
            ("authorization", "Bearer xyz"),
            ("detail", "used sk-ZZZZZZZZZZZZZZZZ here"),
            ("query", "MATCH (n) RETURN n"),
        ]),
    );
    let (all, _) = log.since(0);
    let p = &all[0];
    assert!(
        !p.message.contains("sk-abcdefghijklmnop1234"),
        "{}",
        p.message
    );
    assert_eq!(p.fields["api_key"], "<redacted>");
    assert_eq!(p.fields["authorization"], "<redacted>");
    assert!(
        !p.fields["detail"].contains("sk-ZZZZ"),
        "{}",
        p.fields["detail"]
    );
    assert_eq!(p.fields["query"], "MATCH (n) RETURN n");
}

#[test]
fn the_layer_captures_warn_and_error_only() {
    let log = Arc::new(ProblemLog::new(16));
    let subscriber = tracing_subscriber::registry().with(ProblemLayer::new(Arc::clone(&log)));
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!("routine");
        tracing::debug!("chatter");
        tracing::warn!(peer = "x", "something odd");
        tracing::error!(code = 7, "something broke");
    });
    let (all, _) = log.since(0);
    let got: Vec<_> = all
        .iter()
        .map(|p| (p.level.as_str(), p.message.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![("WARN", "something odd"), ("ERROR", "something broke")]
    );
    assert_eq!(all[0].fields["peer"], "x");
    assert_eq!(all[1].fields["code"], "7");
    assert!(
        all[1].at.ends_with('Z'),
        "ISO-8601 UTC timestamp: {}",
        all[1].at
    );
}

#[test]
fn a_statement_timeout_is_an_error_problem_and_is_counted() {
    let log = Arc::new(ProblemLog::new(16));
    let subscriber = tracing_subscriber::registry().with(ProblemLayer::new(Arc::clone(&log)));
    let before = statement_timeouts();
    let long_query = format!("MATCH (n) WHERE n.x = '{}' RETURN n", "y".repeat(5000));
    tracing::subscriber::with_default(subscriber, || {
        note_exec_error(
            "http",
            "drevo",
            &long_query,
            &ExecError::Timeout { limit_ms: 30 },
        );
        // Client-side mistakes are not server problems.
        note_exec_error(
            "http",
            "drevo",
            "RETURN $missing",
            &ExecError::MissingParameter("missing".into()),
        );
    });
    assert!(statement_timeouts() > before);
    let (all, _) = log.since(0);
    assert_eq!(all.len(), 1, "{all:?}");
    let p = &all[0];
    assert_eq!(p.level, "ERROR");
    assert_eq!(p.target, "drevo::query");
    assert_eq!(p.fields["limit_ms"], "30");
    assert_eq!(p.fields["database"], "drevo");
    assert_eq!(p.fields["protocol"], "http");
    assert!(p.fields["query"].starts_with("MATCH (n) WHERE n.x = 'yyy"));
    assert!(p.fields["query"].chars().count() <= 2001, "query truncated");
}

#[test]
fn bolt_hang_ups_are_disconnects_not_problems() {
    assert!(BoltError::Eof.is_client_disconnect());
    for kind in [
        std::io::ErrorKind::UnexpectedEof,
        std::io::ErrorKind::ConnectionReset,
        std::io::ErrorKind::BrokenPipe,
    ] {
        assert!(BoltError::Io(std::io::Error::new(kind, "early eof")).is_client_disconnect());
    }
    assert!(!BoltError::Io(std::io::Error::other("disk on fire")).is_client_disconnect());
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    let response = app.clone().oneshot(req).await.expect("router response");
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn problems_endpoint_serves_the_global_feed_incrementally() {
    let app = build_native_router(NativeApiState::new(Arc::new(NativeService::in_memory())));
    let marker = format!("problem-endpoint-marker-{}", std::process::id());
    ProblemLog::global().record("ERROR", "drevo::test", &marker, fields(&[("k", "v")]));

    let (status, body) = get(&app, "/problems?since=0").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    let problems = json["problems"].as_array().expect("problems array");
    let ours = problems
        .iter()
        .find(|p| p["message"] == marker.as_str())
        .expect("our problem is listed");
    assert_eq!(ours["level"], "ERROR");
    assert_eq!(ours["fields"]["k"], "v");
    let next = json["next"].as_u64().expect("next cursor");

    let (_, body) = get(&app, &format!("/problems?since={next}")).await;
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert!(json["problems"]
        .as_array()
        .expect("array")
        .iter()
        .all(|p| p["message"] != marker.as_str()));

    let (status, _) = get(&app, "/problems").await;
    assert_eq!(status, StatusCode::OK, "since defaults to 0");
}

#[tokio::test]
async fn an_http_timeout_lands_in_the_metrics() {
    let service = Arc::new(NativeService::in_memory());
    service.set_statement_timeout(Some(Duration::from_millis(20)));
    let app = build_native_router(NativeApiState::new(Arc::clone(&service)));
    let before = statement_timeouts();

    let query = "RETURN reduce(s = 0, i IN range(1, 4000) | s + reduce(t = 0, j IN range(1, 4000) | t + j)) AS v";
    // Sanity: the statement really is over the limit.
    let direct = service.execute(&parse(query).expect("parse"), HashMap::new());
    assert!(
        matches!(direct, Err(ExecError::Timeout { .. })),
        "{direct:?}"
    );

    let body = serde_json::json!({ "query": query }).to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/cypher")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("request");
    let response = app.clone().oneshot(req).await.expect("response");
    assert!(response.status().is_client_error() || response.status().is_server_error());
    assert!(statement_timeouts() > before);

    let (_, metrics) = get(&app, "/metrics").await;
    let line = metrics
        .lines()
        .find(|l| l.starts_with("drevo_statement_timeouts_total"))
        .expect("timeout counter exported");
    let value: f64 = line
        .rsplit(' ')
        .next()
        .expect("value")
        .parse()
        .expect("number");
    assert!(value >= 1.0, "{line}");
}
