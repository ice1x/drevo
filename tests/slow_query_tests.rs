//! The slow-statement log: with `DREVO_SLOW_QUERY_MS` set, a statement that
//! runs longer is logged once at WARN (target `drevo::slow_query`) with its
//! duration, text, parameter names (never values), protocol, user and
//! database, and counted in `drevo_slow_statements_total`. Being WARN, it
//! lands in the problem feed the Web UI shows. The threshold can be changed
//! at runtime through `GET`/`POST /config/slow-query`.

#![cfg(feature = "http")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::cypher::executor::Value;
use drevo::cypher::parser::parse;
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use drevo::problems::{
    note_statement_duration, set_slow_query_threshold, slow_query_threshold, slow_statements,
    Problem, ProblemLayer, ProblemLog,
};
use drevo::server::{Config, ConfigError};
use drevo::transactions::{self, Caller};
use http_body_util::BodyExt;
use tower::ServiceExt;
use tracing_subscriber::layer::SubscriberExt;

/// The threshold is process-wide; tests that set it take turns.
static GLOBAL: Mutex<()> = Mutex::new(());

fn global() -> MutexGuard<'static, ()> {
    GLOBAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn cfg(value: Option<&str>) -> Result<Config, ConfigError> {
    let value = value.map(str::to_string);
    Config::from_env(move |k| {
        if k == "DREVO_SLOW_QUERY_MS" {
            value.clone()
        } else {
            None
        }
    })
}

#[test]
fn the_threshold_comes_from_the_environment() {
    assert_eq!(cfg(None).unwrap().slow_query, None, "off unless set");
    assert_eq!(
        cfg(Some("500")).unwrap().slow_query,
        Some(Duration::from_millis(500))
    );
    assert_eq!(cfg(Some("0")).unwrap().slow_query, None, "0 is off");
    let err = cfg(Some("soon")).unwrap_err();
    assert!(err.to_string().contains("DREVO_SLOW_QUERY_MS"), "{err}");
}

fn caller() -> Caller {
    Caller {
        surface: "bolt",
        client: Some("10.0.0.7:51234".into()),
        user: Some("ada".into()),
        connection: None,
    }
}

/// The problems `work` logs.
fn captured(work: impl FnOnce()) -> Vec<Problem> {
    let log = Arc::new(ProblemLog::new(32));
    let subscriber = tracing_subscriber::registry().with(ProblemLayer::new(Arc::clone(&log)));
    tracing::subscriber::with_default(subscriber, work);
    log.since(0).0
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn a_statement_over_the_threshold_is_logged_with_its_details() {
    let query = "MATCH (u:User {email: $email}) RETURN u";
    let problems = captured(|| {
        transactions::scope(caller(), query, || {
            note_statement_duration(
                Some(Duration::from_millis(100)),
                "sales",
                &names(&["email", "token"]),
                false,
                Duration::from_millis(1234),
            );
        });
    });
    assert_eq!(problems.len(), 1, "{problems:?}");
    let p = &problems[0];
    assert_eq!(p.level, "WARN");
    assert_eq!(p.target, "drevo::slow_query");
    assert_eq!(p.fields["duration_ms"], "1234");
    assert_eq!(p.fields["threshold_ms"], "100");
    assert_eq!(p.fields["query"], query);
    assert_eq!(p.fields["params"], "email, token");
    assert_eq!(p.fields["protocol"], "bolt");
    assert_eq!(p.fields["user"], "ada");
    assert_eq!(p.fields["database"], "sales");
    assert_eq!(p.fields["explicit"], "false");
}

#[test]
fn a_statement_under_the_threshold_or_with_it_off_is_not_logged() {
    let problems = captured(|| {
        transactions::scope(caller(), "RETURN 1", || {
            note_statement_duration(
                Some(Duration::from_millis(100)),
                "drevo",
                &[],
                false,
                Duration::from_millis(99),
            );
            note_statement_duration(None, "drevo", &[], false, Duration::from_secs(60));
        });
    });
    assert!(problems.is_empty(), "{problems:?}");
}

#[test]
fn a_long_statement_is_cut() {
    let query = format!("RETURN '{}'", "x".repeat(5000));
    let problems = captured(|| {
        transactions::scope(caller(), &query, || {
            note_statement_duration(
                Some(Duration::from_millis(1)),
                "drevo",
                &[],
                true,
                Duration::from_millis(5),
            );
        });
    });
    let q = &problems[0].fields["query"];
    assert!(q.chars().count() <= 2001, "{}", q.len());
    assert!(q.ends_with('…'));
    assert_eq!(problems[0].fields["explicit"], "true");
}

fn exec(svc: &NativeService, q: &str, params: HashMap<String, Value>) {
    svc.execute(&parse(q).unwrap(), params)
        .unwrap_or_else(|e| panic!("{q}: {e}"));
}

#[test]
fn the_service_logs_slow_statements_in_both_modes_without_parameter_values() {
    let _turn = global();
    let svc = NativeService::in_memory();
    let before = slow_statements();
    set_slow_query_threshold(Some(Duration::from_millis(1)));
    // Slow enough for a 1 ms threshold on any machine.
    let slow = "UNWIND range(1, 20000) AS i WITH i WHERE i % 7 = $k RETURN count(i)";
    let params = HashMap::from([("k".to_string(), Value::String("hunter2".into()))]);
    let problems = captured(|| {
        transactions::scope(caller(), slow, || exec(&svc, slow, params.clone()));
        let tx = svc.begin_tx();
        transactions::scope(caller(), slow, || {
            svc.execute_in_tx(tx, &parse(slow).unwrap(), params.clone())
                .unwrap();
        });
        svc.rollback_tx(tx);
    });
    set_slow_query_threshold(None);
    let slow_ones: Vec<&Problem> = problems
        .iter()
        .filter(|p| p.target == "drevo::slow_query")
        .collect();
    assert_eq!(slow_ones.len(), 2, "{problems:?}");
    assert_eq!(slow_ones[0].fields["explicit"], "false");
    assert_eq!(slow_ones[1].fields["explicit"], "true");
    for p in &slow_ones {
        assert_eq!(p.fields["params"], "k");
        assert_eq!(p.fields["database"], "drevo");
        let all = format!("{p:?}");
        assert!(!all.contains("hunter2"), "a parameter value leaked: {all}");
    }
    assert!(slow_statements() >= before + 2);
}

async fn call(app: &axum::Router, method: &str, uri: &str, body: &str) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[test]
fn the_threshold_is_changed_at_runtime_over_http() {
    let _turn = global();
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(changes_the_threshold_over_http());
}

async fn changes_the_threshold_over_http() {
    let app = build_native_router(NativeApiState::new(Arc::new(NativeService::in_memory())));
    set_slow_query_threshold(None);
    let (status, body) = call(&app, "GET", "/config/slow-query", "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["threshold_ms"], serde_json::Value::Null);

    let (status, body) = call(
        &app,
        "POST",
        "/config/slow-query",
        r#"{"threshold_ms": 250}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(slow_query_threshold(), Some(Duration::from_millis(250)));
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["threshold_ms"], 250);
    assert!(v["slow_statements"].is_u64(), "{body}");

    let (status, _) = call(
        &app,
        "POST",
        "/config/slow-query",
        r#"{"threshold_ms": null}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(slow_query_threshold(), None);

    let (status, _) = call(
        &app,
        "POST",
        "/config/slow-query",
        r#"{"threshold_ms": "fast"}"#,
    )
    .await;
    assert!(status.is_client_error(), "{status}");

    let (_, metrics) = call(&app, "GET", "/metrics", "").await;
    assert!(
        metrics
            .lines()
            .any(|l| l.starts_with("drevo_slow_statements_total")),
        "{metrics}"
    );
}
