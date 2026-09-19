//! Native-engine coverage for the semantic subsystem (issue #447, epic #444).
//!
//! The semantic auto-embedding state machine, `semantic.*` procedures, and the
//! `drevo.semantic.status` health accounting were originally KV-only (the KV
//! suites `semantic_autoembed_tests` / `semantic_rel_tests` /
//! `semantic_status_health_tests` / `semantic_reindex_tests`). This suite ports
//! that behaviour onto the durable-native serving layer
//! ([`drevo::native_service::NativeService`]) so `DREVO_ENGINE=native-durable`
//! and the native embedded handle serve semantic queries **without** a KV
//! `secondary`.
//!
//! What it locks (native parity with the KV acceptance bullets):
//! - registering an `Auto`-mode target then `CREATE`ing a matching node/edge
//!   embeds its `text_property` into `embedding_property` server-side, so
//!   `drevo.semantic.query` / `.queryRel` retrieve it with no client round-trip;
//! - the double no-op (no embedder, unregistered label, `Manual` mode, no text);
//! - the reindex backfill drains a pre-existing un-embedded target;
//! - `drevo.semantic.status` surfaces the real backlog: `pending_count`,
//!   `failed_count`, `last_error`, and the derived `degraded` state — including
//!   an upstream outage during ingest and a pre-existing un-embedded backlog.
//!
//! Gated on `embeddings-proxy` like the KV suites: it exercises the real
//! [`drevo::embeddings::SyncEmbedder`] against a local in-process axum stub — no
//! external network, deterministic. Run with:
//!
//! ```text
//! cargo test --features embeddings-proxy --test semantic_native_tests
//! ```

#![cfg(feature = "embeddings-proxy")]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value as JsonValue};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;

use drevo::cypher::executor::Value;
use drevo::cypher::parser::parse;
use drevo::embeddings::{EmbeddingsConfig, SyncEmbedder};
use drevo::model::{NewNode, Properties};
use drevo::native_service::NativeService;
use drevo::semantic_index::IndexMode;

/// A stub `/v1/embeddings` whose health is flipped by shared state: when
/// `healthy` is true it returns the fixed vector `[1.0, 0.0]`; when false it
/// answers `500`, exercising the fail-open write path and the degraded backlog.
#[derive(Clone, Default)]
struct StubState {
    healthy: Arc<AtomicBool>,
}

async fn stub_embed(
    State(state): State<StubState>,
    Json(_body): Json<JsonValue>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if state.healthy.load(Ordering::SeqCst) {
        Json(json!({
            "object": "list",
            "data": [{"object": "embedding", "index": 0, "embedding": [1.0, 0.0]}],
            "model": "stub-embed",
            "usage": {"total_tokens": 1}
        }))
        .into_response()
    } else {
        (StatusCode::INTERNAL_SERVER_ERROR, "stub outage").into_response()
    }
}

/// Spawn the stub on `rt`, returning its address and the health toggle.
fn spawn_stub(rt: &Runtime) -> (SocketAddr, Arc<AtomicBool>) {
    let healthy = Arc::new(AtomicBool::new(true));
    let state = StubState {
        healthy: healthy.clone(),
    };
    let addr = rt.block_on(async move {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let router = Router::new()
            .route("/v1/embeddings", post(stub_embed))
            .with_state(state);
        tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serve");
        });
        addr
    });
    (addr, healthy)
}

/// A `NativeService` (in-memory) with a real [`SyncEmbedder`] pointed at `addr`.
fn service_with_embedder(addr: SocketAddr) -> NativeService {
    let cfg = EmbeddingsConfig {
        upstream: format!("http://{addr}/v1/embeddings"),
        api_key: None,
        model: Some("stub-embed".to_string()),
    };
    let svc = NativeService::in_memory();
    svc.set_embedder(Arc::new(SyncEmbedder::from_config(cfg).expect("embedder")));
    svc
}

fn new_node(kind: &str, title: &str, text: Option<&str>) -> NewNode {
    let mut props = HashMap::new();
    if let Some(text) = text {
        props.insert("text".to_string(), json!(text));
    }
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties(props),
    }
}

/// The `embedding` property of the node with `title`, if any.
fn embedding_of(svc: &NativeService, title: &str) -> Option<JsonValue> {
    let node = svc.get_node_by_title(title).expect("node exists");
    node.properties.0.get("embedding").cloned()
}

fn run(svc: &NativeService, src: &str) -> Vec<Vec<Value>> {
    let q = parse(src).expect("parse");
    svc.execute(&q, HashMap::new()).expect("execute").rows
}

/// One `drevo.semantic.status` row for `label`, keyed by column name.
fn status_row(svc: &NativeService, label: &str) -> HashMap<String, Value> {
    let rows = run(
        svc,
        "CALL drevo.semantic.status() \
         YIELD label, target_kind, state, pending_count, failed_count, last_error \
         RETURN label, target_kind, state, pending_count, failed_count, last_error",
    );
    let cols = [
        "label",
        "target_kind",
        "state",
        "pending_count",
        "failed_count",
        "last_error",
    ];
    for row in rows {
        if row[0] == Value::String(label.to_string()) {
            return cols
                .iter()
                .zip(row)
                .map(|(c, v)| ((*c).to_string(), v))
                .collect();
        }
    }
    panic!("no status row for label {label}");
}

// ── Node auto-embed on write ─────────────────────────────────────────

#[test]
fn auto_embed_on_create_then_semantic_query_finds_it() {
    let rt = Runtime::new().expect("rt");
    let (addr, _healthy) = spawn_stub(&rt);
    let svc = service_with_embedder(addr);

    svc.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");
    svc.create_node(new_node("Doc", "d1", Some("hello world")))
        .expect("create");

    // Embedded server-side on ingest — no client round-trip.
    assert_eq!(embedding_of(&svc, "d1"), Some(json!([1.0, 0.0])));

    // Full loop: query text embedded server-side retrieves the node.
    let rows = run(
        &svc,
        "CALL drevo.semantic.query('Doc', 'embedding', 'anything', 5) \
         YIELD node, score RETURN node.title AS t ORDER BY score DESC",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::String("d1".to_string()));
}

#[test]
fn auto_embed_via_cypher_create() {
    let rt = Runtime::new().expect("rt");
    let (addr, _healthy) = spawn_stub(&rt);
    let svc = service_with_embedder(addr);

    svc.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");
    run(&svc, "CREATE (:Doc {title: 'd1', text: 'via cypher'})");

    assert_eq!(embedding_of(&svc, "d1"), Some(json!([1.0, 0.0])));
}

#[test]
fn auto_embed_skips_unregistered_label() {
    let rt = Runtime::new().expect("rt");
    let (addr, _healthy) = spawn_stub(&rt);
    let svc = service_with_embedder(addr);

    svc.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");
    svc.create_node(new_node("Other", "o1", Some("not a doc")))
        .expect("create");

    assert_eq!(embedding_of(&svc, "o1"), None);
}

#[test]
fn manual_mode_is_not_auto_embedded() {
    let rt = Runtime::new().expect("rt");
    let (addr, _healthy) = spawn_stub(&rt);
    let svc = service_with_embedder(addr);

    svc.semantic_register("Doc", "text", "embedding", IndexMode::Manual, None)
        .expect("register");
    svc.create_node(new_node("Doc", "d1", Some("manual")))
        .expect("create");

    assert_eq!(embedding_of(&svc, "d1"), None);
}

#[test]
fn no_embedder_means_no_auto_embed() {
    let svc = NativeService::in_memory();
    svc.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");
    svc.create_node(new_node("Doc", "d1", Some("no embedder")))
        .expect("create");

    assert_eq!(embedding_of(&svc, "d1"), None);
}

#[test]
fn node_without_text_property_is_left_alone() {
    let rt = Runtime::new().expect("rt");
    let (addr, _healthy) = spawn_stub(&rt);
    let svc = service_with_embedder(addr);

    svc.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");
    svc.create_node(new_node("Doc", "d1", None))
        .expect("create");

    assert_eq!(embedding_of(&svc, "d1"), None);
}

// Re-embed on SET/update is a separate slice (KV threads it through several
// `update_node` call sites; the native funnel differs) — this PR covers the
// core ingest path (CREATE) plus the reindex backfill, which together let
// native serve semantic queries without a KV secondary.

// ── Relationship auto-embed ──────────────────────────────────────────

#[test]
fn rel_auto_embed_on_create_then_query_finds_it() {
    let rt = Runtime::new().expect("rt");
    let (addr, _healthy) = spawn_stub(&rt);
    let svc = service_with_embedder(addr);

    svc.semantic_register_rel(
        "RELATES_TO",
        "fact",
        "fact_embedding",
        IndexMode::Auto,
        None,
    )
    .expect("register rel");
    run(
        &svc,
        "CREATE (a:P {title: 'a'})-[:RELATES_TO {fact: 'a mentors b'}]->(b:P {title: 'b'})",
    );

    let rows = run(
        &svc,
        "MATCH ()-[r:RELATES_TO]->() RETURN r.fact_embedding AS e",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0][0],
        Value::List(vec![Value::Float(1.0), Value::Float(0.0)])
    );

    let rows = run(
        &svc,
        "CALL drevo.semantic.queryRel('RELATES_TO', 'fact_embedding', 'who mentors whom', 5) \
         YIELD rel, score RETURN score ORDER BY score DESC",
    );
    assert_eq!(rows.len(), 1);
}

#[test]
fn rel_manual_target_is_not_auto_embedded() {
    let rt = Runtime::new().expect("rt");
    let (addr, _healthy) = spawn_stub(&rt);
    let svc = service_with_embedder(addr);

    svc.semantic_register_rel(
        "RELATES_TO",
        "fact",
        "fact_embedding",
        IndexMode::Manual,
        None,
    )
    .expect("register rel");
    run(
        &svc,
        "CREATE (a:P {title: 'a'})-[:RELATES_TO {fact: 'manual'}]->(b:P {title: 'b'})",
    );

    let rows = run(
        &svc,
        "MATCH ()-[r:RELATES_TO]->() RETURN r.fact_embedding AS e",
    );
    assert_eq!(rows[0][0], Value::Null);
}

// ── Health accounting: drevo.semantic.status ─────────────────────────

#[test]
fn healthy_target_reads_clean() {
    let rt = Runtime::new().expect("rt");
    let (addr, _healthy) = spawn_stub(&rt);
    let svc = service_with_embedder(addr);

    svc.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");
    svc.create_node(new_node("Doc", "d1", Some("all good")))
        .expect("create");

    let row = status_row(&svc, "Doc");
    assert_eq!(row["pending_count"], Value::Integer(0));
    assert_eq!(row["failed_count"], Value::Integer(0));
    assert_eq!(row["state"], Value::String("enabled".to_string()));
    assert_eq!(row["last_error"], Value::Null);
}

#[test]
fn outage_during_ingest_is_surfaced_as_degraded() {
    let rt = Runtime::new().expect("rt");
    let (addr, healthy) = spawn_stub(&rt);
    let svc = service_with_embedder(addr);

    svc.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");

    // Upstream is down during the write — the write still succeeds (fail-open),
    // but the node lands un-embedded and the failure is tallied.
    healthy.store(false, Ordering::SeqCst);
    svc.create_node(new_node("Doc", "d1", Some("during outage")))
        .expect("create still succeeds");

    assert_eq!(embedding_of(&svc, "d1"), None);
    let row = status_row(&svc, "Doc");
    assert_eq!(row["state"], Value::String("degraded".to_string()));
    assert_eq!(row["pending_count"], Value::Integer(1));
    assert_eq!(row["failed_count"], Value::Integer(1));
    assert!(matches!(row["last_error"], Value::String(_)));
}

#[test]
fn failed_count_accumulates_across_writes() {
    let rt = Runtime::new().expect("rt");
    let (addr, healthy) = spawn_stub(&rt);
    let svc = service_with_embedder(addr);

    svc.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");
    healthy.store(false, Ordering::SeqCst);
    for i in 0..3 {
        svc.create_node(new_node("Doc", &format!("d{i}"), Some("x")))
            .expect("create");
    }

    let row = status_row(&svc, "Doc");
    assert_eq!(row["pending_count"], Value::Integer(3));
    assert_eq!(row["failed_count"], Value::Integer(3));
    assert_eq!(row["state"], Value::String("degraded".to_string()));
}

#[test]
fn preexisting_unembedded_nodes_show_as_pending() {
    let rt = Runtime::new().expect("rt");
    let (addr, _healthy) = spawn_stub(&rt);
    let svc = service_with_embedder(addr);

    // Node created before the rule → un-embedded → pending, failed stays 0.
    svc.create_node(new_node("Doc", "d1", Some("older")))
        .expect("create");
    svc.semantic_register("Doc", "text", "embedding", IndexMode::Auto, None)
        .expect("register");

    let row = status_row(&svc, "Doc");
    assert_eq!(row["pending_count"], Value::Integer(1));
    assert_eq!(row["failed_count"], Value::Integer(0));
    assert_eq!(row["state"], Value::String("degraded".to_string()));

    // Draining the backlog (reindex) clears the degraded state.
    svc.semantic_reindex("Doc", "text", "embedding", 100)
        .expect("reindex");
    let row = status_row(&svc, "Doc");
    assert_eq!(row["pending_count"], Value::Integer(0));
    assert_eq!(row["state"], Value::String("enabled".to_string()));
}
