//! Slice 1 of per-entity conflict detection (issue #749), server side: the
//! conflict history is sized by `DREVO_TX_CONFLICT_HISTORY`, and the engine's
//! classification of rejected commits reaches `/metrics` as
//! `drevo_tx_conflicts_total{reason="overlap"|"would_pass"|"history"}`.

#![cfg(feature = "http")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use drevo::server::{Config, ConfigError};
use drevo_core::conflict::DEFAULT_HISTORY;
use drevo_core::engine::GraphEngine;
use drevo_core::model::{NewNode, NodePatch, Properties};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn cfg(value: Option<&str>) -> Result<Config, ConfigError> {
    Config::from_env(|key| match key {
        "DREVO_TX_CONFLICT_HISTORY" => value.map(str::to_string),
        _ => None,
    })
}

#[test]
fn the_conflict_history_size_comes_from_the_environment() {
    assert_eq!(cfg(None).unwrap().tx_conflict_history, DEFAULT_HISTORY);
    assert_eq!(cfg(Some("250")).unwrap().tx_conflict_history, 250);
    for bad in ["0", "many", "-5"] {
        let err = cfg(Some(bad)).unwrap_err();
        assert!(
            err.to_string().contains("DREVO_TX_CONFLICT_HISTORY"),
            "{bad:?}: {err}"
        );
    }
}

#[test]
fn the_service_sizes_its_engine_history() {
    let service = NativeService::in_memory();
    assert_eq!(service.graph().conflict_history_capacity(), DEFAULT_HISTORY);
    service.set_conflict_history(7);
    assert_eq!(service.graph().conflict_history_capacity(), 7);
    assert_eq!(service.conflict_stats(), service.graph().conflict_stats());
}

fn note(title: &str) -> NewNode {
    NewNode {
        kind: "note".into(),
        title: title.into(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

async fn metrics(app: &axum::Router) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn sample(metrics: &str, reason: &str) -> f64 {
    let needle = format!("drevo_tx_conflicts_total{{reason=\"{reason}\"}}");
    metrics
        .lines()
        .find(|l| l.starts_with(&needle))
        .unwrap_or_else(|| panic!("{needle} missing from:\n{metrics}"))
        .rsplit(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn rejected_commits_reach_the_metrics_by_reason() {
    let service = Arc::new(NativeService::in_memory());
    let app = build_native_router(NativeApiState::new(Arc::clone(&service)));
    let text = metrics(&app).await;
    for reason in ["overlap", "would_pass", "history"] {
        assert_eq!(sample(&text, reason), 0.0, "{reason} starts at zero");
    }

    let g = service.graph();
    let a = g.create_node(note("a")).unwrap();
    let b = g.create_node(note("b")).unwrap();
    // A transaction on `a` loses to an auto-commit write of `b`: a per-entity
    // rule would have let it through.
    let tx = g.tx_begin();
    g.tx_engine(tx)
        .unwrap()
        .update_node(
            a.id,
            NodePatch {
                body: Some("tx".into()),
                ..Default::default()
            },
        )
        .unwrap();
    g.update_node(
        b.id,
        NodePatch {
            body: Some("direct".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(g.tx_commit(tx).is_err());
    // And one that loses to a write of the same node.
    let tx = g.tx_begin();
    g.tx_engine(tx)
        .unwrap()
        .update_node(
            a.id,
            NodePatch {
                body: Some("tx2".into()),
                ..Default::default()
            },
        )
        .unwrap();
    g.update_node(
        a.id,
        NodePatch {
            body: Some("direct2".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(g.tx_commit(tx).is_err());

    let text = metrics(&app).await;
    assert_eq!(sample(&text, "would_pass"), 1.0);
    assert_eq!(sample(&text, "overlap"), 1.0);
    assert_eq!(sample(&text, "history"), 0.0);
    assert!(
        text.contains("# TYPE drevo_tx_conflicts_total counter"),
        "declared as a counter"
    );
}
