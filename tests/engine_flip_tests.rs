//! Engine-flip slice B guards (RFC `docs/rfc-native-core.md` #307, Phase 6):
//! `DREVO_ENGINE` configuration, the HTTP `/cypher` routing through the
//! per-database mirror registry, and the Bolt session routing — autocommit
//! reads served natively, writes and in-transaction statements on KV.

#![cfg(all(
    not(target_arch = "wasm32"),
    feature = "http",
    feature = "redb-backend"
))]

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::api::{build_router, ApiState};
use drevo::bolt::packstream::Value as BoltValue;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::db::Drevo;
use drevo::native_mirror::MirrorRegistry;
use drevo::server::{Config, ConfigError, EngineMode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

// ── DREVO_ENGINE configuration ─────────────────────────────────────────

fn config_with_engine(engine: Option<&str>) -> Result<Config, ConfigError> {
    let engine = engine.map(str::to_string);
    Config::from_env(move |k| match k {
        "DREVO_ENGINE" => engine.clone(),
        _ => None,
    })
}

#[test]
fn engine_defaults_to_kv_when_unset() {
    assert_eq!(config_with_engine(None).unwrap().engine, EngineMode::Kv);
}

#[test]
fn engine_parses_kv_and_native_case_insensitively() {
    for raw in ["kv", "KV", " kv "] {
        assert_eq!(
            config_with_engine(Some(raw)).unwrap().engine,
            EngineMode::Kv,
            "raw: {raw:?}"
        );
    }
    for raw in ["native", "NATIVE", " Native "] {
        assert_eq!(
            config_with_engine(Some(raw)).unwrap().engine,
            EngineMode::Native,
            "raw: {raw:?}"
        );
    }
}

#[test]
fn engine_rejects_unknown_values() {
    match config_with_engine(Some("turbo")) {
        Err(ConfigError::InvalidEngine { value }) => assert_eq!(value, "turbo"),
        other => panic!("expected InvalidEngine, got {other:?}"),
    }
}

// ── HTTP /cypher routing ───────────────────────────────────────────────

async fn post_cypher(app: &axum::Router, query: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/cypher")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({ "query": query })).unwrap(),
        ))
        .unwrap();
    let response = app.clone().oneshot(req).await.expect("router response");
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&bytes).expect("json body");
    (status, value)
}

fn native_state() -> (ApiState, Arc<MirrorRegistry>, Arc<Drevo>) {
    let db = Arc::new(Drevo::open_in_memory().expect("open"));
    let registry = Arc::new(MirrorRegistry::new());
    let state = ApiState::new(Arc::clone(&db)).with_native_mirrors(Arc::clone(&registry));
    (state, registry, db)
}

#[tokio::test]
async fn http_cypher_serves_fresh_reads_natively() {
    let (state, registry, db) = native_state();
    let app = build_router(state);

    let (status, _) = post_cypher(&app, "CREATE (:Person {title: 'ada'})").await;
    assert_eq!(status, StatusCode::OK);
    registry.for_db(&db).rebuild_blocking(&db).expect("rebuild");

    let (status, body) = post_cypher(&app, "MATCH (n:Person) RETURN n.title").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["rows"], json!([["ada"]]));
    let stats = registry.for_db(&db).stats();
    assert_eq!(
        stats.native_hits, 1,
        "the fresh read must be served natively"
    );
}

#[tokio::test]
async fn http_cypher_write_then_read_stays_correct_via_kv_fallback() {
    let (state, registry, db) = native_state();
    let app = build_router(state);
    registry.for_db(&db).rebuild_blocking(&db).expect("rebuild");

    let (status, _) = post_cypher(&app, "CREATE (:Person {title: 'eve'})").await;
    assert_eq!(status, StatusCode::OK);

    // Read-your-writes: the stale mirror must fall back to KV, never serve
    // the old snapshot.
    let (status, body) =
        post_cypher(&app, "MATCH (n:Person) RETURN n.title ORDER BY n.title").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["rows"], json!([["eve"]]));
    let stats = registry.for_db(&db).stats();
    assert!(
        stats.kv_fallbacks >= 1,
        "stale read must fall back: {stats:?}"
    );
}

#[tokio::test]
async fn http_use_routes_through_the_named_databases_own_mirror() {
    let (state, registry, default_db) = native_state();
    let catalog = Arc::clone(&state.catalog);
    let app = build_router(state);

    let (status, _) = post_cypher(&app, "CREATE DATABASE second").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = post_cypher(&app, "USE second CREATE (:Thing {title: 'gadget'})").await;
    assert_eq!(status, StatusCode::OK);

    let second = catalog.get("second").expect("second db");
    registry
        .for_db(&second)
        .rebuild_blocking(&second)
        .expect("rebuild second");
    let (status, body) = post_cypher(&app, "USE second MATCH (n) RETURN n.title").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["rows"], json!([["gadget"]]));
    assert_eq!(
        registry.for_db(&second).stats().native_hits,
        1,
        "the USE read must be served by the second database's mirror"
    );
    assert_eq!(
        registry.for_db(&default_db).stats().native_hits,
        0,
        "the default database's mirror must not be involved"
    );
}

// ── Bolt session routing ───────────────────────────────────────────────

fn dict<I: IntoIterator<Item = (&'static str, BoltValue)>>(
    entries: I,
) -> BTreeMap<String, BoltValue> {
    entries
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

fn run_statement(session: &mut Session<'_>, query: &str) -> Vec<ServerMessage> {
    session.handle(ClientMessage::Run {
        query: query.to_string(),
        parameters: BTreeMap::new(),
        extra: BTreeMap::new(),
    })
}

fn pull_all(session: &mut Session<'_>) -> Vec<ServerMessage> {
    session.handle(ClientMessage::Pull {
        extra: dict([("n", BoltValue::Integer(-1))]),
    })
}

fn record_strings(messages: &[ServerMessage]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match m {
            ServerMessage::Record { fields } => Some(fields.clone()),
            _ => None,
        })
        .map(|fields| match &fields[0] {
            BoltValue::String(s) => s.clone(),
            other => panic!("expected string record, got {other:?}"),
        })
        .collect()
}

fn assert_success(messages: &[ServerMessage]) {
    assert!(
        matches!(messages.last(), Some(ServerMessage::Success { .. })),
        "expected SUCCESS, got {messages:?}"
    );
}

#[tokio::test]
async fn bolt_autocommit_reads_are_served_natively_and_tx_reads_are_not() {
    let db = Arc::new(Drevo::open_in_memory().expect("open"));
    let mirror = Arc::new(drevo::native_mirror::NativeMirror::new());

    let mut session = Session::new(&db).with_native_mirror(Arc::clone(&db), Arc::clone(&mirror));
    assert_success(&session.handle(ClientMessage::Hello { extra: dict([]) }));

    // Autocommit write routes to KV.
    assert_success(&run_statement(
        &mut session,
        "CREATE (:Person {title: 'ada'})",
    ));
    assert_success(&pull_all(&mut session));
    assert_eq!(mirror.stats().kv_routed, 1);

    mirror.rebuild_blocking(&db).expect("rebuild");

    // Autocommit read is served natively, with the right rows on the wire.
    assert_success(&run_statement(&mut session, "MATCH (n) RETURN n.title"));
    let records = pull_all(&mut session);
    assert_eq!(record_strings(&records), ["ada"]);
    assert_eq!(mirror.stats().native_hits, 1);

    // Inside an explicit transaction the mirror must be bypassed: the read
    // sees the transaction's own uncommitted write via the KV engine.
    assert_success(&session.handle(ClientMessage::Begin { extra: dict([]) }));
    assert_success(&run_statement(
        &mut session,
        "CREATE (:Person {title: 'bob'})",
    ));
    assert_success(&pull_all(&mut session));
    assert_success(&run_statement(
        &mut session,
        "MATCH (n:Person) RETURN n.title ORDER BY n.title",
    ));
    let records = pull_all(&mut session);
    assert_eq!(record_strings(&records), ["ada", "bob"]);
    assert_success(&session.handle(ClientMessage::Commit));
    assert_eq!(
        mirror.stats().native_hits,
        1,
        "in-transaction statements must not touch the mirror"
    );
}

// ── run() boot smoke with the flip active ──────────────────────────────

#[tokio::test]
async fn run_boots_and_serves_with_engine_native() {
    use std::time::Duration;
    use tempfile::TempDir;

    let dir = TempDir::new().unwrap();
    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let p = probe.local_addr().unwrap().port();
        drop(probe);
        p
    };
    let data_dir = dir.path().to_string_lossy().to_string();
    let port_str = port.to_string();
    let cfg = Config::from_env(move |k| match k {
        "DREVO_HOST" => Some("127.0.0.1".to_string()),
        "DREVO_PORT" => Some(port_str.clone()),
        "DREVO_DATA_DIR" => Some(data_dir.clone()),
        "DREVO_ENGINE" => Some("native".to_string()),
        _ => None,
    })
    .unwrap();
    assert_eq!(cfg.engine, EngineMode::Native);

    let server = tokio::spawn(async move {
        let _ = drevo::server::run(cfg).await;
    });

    let addr = format!("127.0.0.1:{port}");
    let mut connected = false;
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(&addr).await.is_ok() {
            connected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        connected,
        "server did not start listening on {addr} with DREVO_ENGINE=native"
    );
    server.abort();
}
