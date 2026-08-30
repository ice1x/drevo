//! Guards for `DREVO_ENGINE=native-durable` (RFC #307, Phase 4/7): the
//! server mode where the WAL-backed native engine IS the store of record —
//! no KV store, no redb file — serving the minimal native HTTP surface.

#![cfg(all(
    not(target_arch = "wasm32"),
    feature = "http",
    feature = "redb-backend"
))]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use drevo::native_api::{build_native_router, NativeApiState};
use drevo::native_service::NativeService;
use drevo::server::{Config, EngineMode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder().method(method).uri(uri);
    let req = if let Some(ref v) = body {
        req.header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(v).unwrap()))
    } else {
        req.body(Body::empty())
    }
    .unwrap();
    let response = app.clone().oneshot(req).await.expect("router response");
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

async fn cypher(app: &axum::Router, query: &str) -> (StatusCode, Value) {
    send(app, "POST", "/cypher", Some(json!({ "query": query }))).await
}

#[test]
fn engine_parses_native_durable() {
    let cfg = Config::from_env(|k| match k {
        "DREVO_ENGINE" => Some("native-durable".to_string()),
        _ => None,
    })
    .unwrap();
    assert_eq!(cfg.engine, EngineMode::NativeDurable);
}

#[tokio::test]
async fn cypher_reads_writes_and_fts_flow_through_the_native_router() {
    let app = build_native_router(NativeApiState::new(Arc::new(NativeService::in_memory())));

    let (status, _) = cypher(
        &app,
        "CREATE (:Doc {title: 'notes', body: 'ownership and borrowing'})",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = cypher(&app, "MATCH (n) RETURN n.title").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["rows"], json!([["notes"]]));

    // Full-text is served natively in this mode.
    let (status, body) = cypher(&app, "CALL fts.search('borrowing', 5)").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["rows"].as_array().unwrap().len(), 1);

    // Health + status identify the mode.
    let (status, _) = send(&app, "GET", "/health", None).await;
    assert_eq!(status, StatusCode::OK);
    let (_, s) = send(&app, "GET", "/status", None).await;
    assert_eq!(s["engine"], "native-durable");

    // The KV REST surface is absent by design, not silently empty.
    let (status, _) = send(&app, "GET", "/nodes", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn parse_and_execution_errors_are_bad_requests() {
    let app = build_native_router(NativeApiState::new(Arc::new(NativeService::in_memory())));
    let (status, _) = cypher(&app, "MATCH (((").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = cypher(&app, "CALL drevo.semantic.status()").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "KV-only procedure surfaces as 400"
    );
}

#[tokio::test]
async fn durable_router_state_survives_a_service_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native.wal");
    {
        let app = build_native_router(NativeApiState::new(Arc::new(
            NativeService::open(&path).expect("open"),
        )));
        let (status, _) = cypher(&app, "CREATE (:Person {title: 'ada'})").await;
        assert_eq!(status, StatusCode::OK);
    }
    let app = build_native_router(NativeApiState::new(Arc::new(
        NativeService::open(&path).expect("reopen"),
    )));
    let (status, body) = cypher(&app, "MATCH (n) RETURN n.title").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["rows"], json!([["ada"]]));
}

#[tokio::test]
async fn run_boots_native_durable_without_creating_a_redb_file() {
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
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
        "DREVO_ENGINE" => Some("native-durable".to_string()),
        _ => None,
    })
    .unwrap();

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
    assert!(connected, "native-durable server did not start on {addr}");

    // The durable-native mode's store is the WAL; no redb file may appear.
    assert!(
        dir.path().join("native.wal").exists(),
        "the WAL must be created in the data dir"
    );
    assert!(
        !dir.path().join("drevo.redb").exists(),
        "native-durable must not open a KV store"
    );
    server.abort();
}

// ── Bolt over the durable engine ───────────────────────────────────────

mod bolt_durable {
    use super::*;
    use drevo::bolt::packstream::Value as BoltValue;
    use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
    use std::collections::BTreeMap;

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

    fn assert_success(messages: &[ServerMessage]) {
        assert!(
            matches!(messages.last(), Some(ServerMessage::Success { .. })),
            "expected SUCCESS, got {messages:?}"
        );
    }

    #[test]
    fn autocommit_write_and_read_flow_through_the_durable_session() {
        let service = Arc::new(NativeService::in_memory());
        let mut session = Session::new_durable(Arc::clone(&service));
        assert_success(&session.handle(ClientMessage::Hello { extra: dict([]) }));

        assert_success(&run_statement(
            &mut session,
            "CREATE (:Person {title: 'ada'})",
        ));
        assert_success(&pull_all(&mut session));

        assert_success(&run_statement(&mut session, "MATCH (n) RETURN n.title"));
        let records = pull_all(&mut session);
        let titles: Vec<&str> = records
            .iter()
            .filter_map(|m| match m {
                ServerMessage::Record { fields } => match &fields[0] {
                    BoltValue::String(s) => Some(s.as_str()),
                    other => panic!("expected string, got {other:?}"),
                },
                _ => None,
            })
            .collect();
        assert_eq!(titles, ["ada"]);
    }

    #[test]
    fn begin_is_refused_on_the_durable_engine() {
        let service = Arc::new(NativeService::in_memory());
        let mut session = Session::new_durable(service);
        assert_success(&session.handle(ClientMessage::Hello { extra: dict([]) }));
        let replies = session.handle(ClientMessage::Begin { extra: dict([]) });
        assert!(
            matches!(replies.last(), Some(ServerMessage::Failure { .. })),
            "BEGIN must fail on the durable engine, got {replies:?}"
        );
    }
}

// ── first-boot KV → WAL migration ──────────────────────────────────────

#[tokio::test]
async fn first_boot_migrates_existing_kv_data_and_leaves_redb_untouched() {
    use drevo::cypher::executor::execute;
    use drevo::cypher::parser::parse;
    use drevo::native_service::migrate_kv_into_wal_if_first_boot;
    use std::collections::HashMap;

    let dir = tempfile::tempdir().unwrap();
    let redb = dir.path().join("drevo.redb");
    let wal = dir.path().join("native.wal");

    // Seed a real KV store the way production data exists today.
    {
        let kv = drevo::db::Drevo::open(&redb).expect("open kv");
        for stmt in [
            "CREATE (:Person {title: 'ada', team: 'core'})",
            "CREATE (:Person {title: 'bob', team: 'infra'})",
            "MATCH (a {title: 'ada'}), (b {title: 'bob'}) CREATE (a)-[:KNOWS]->(b)",
        ] {
            execute(&parse(stmt).unwrap(), &kv, HashMap::new()).expect("seed");
        }
        kv.close().expect("close kv");
    }
    let redb_bytes_before = std::fs::metadata(&redb).unwrap().len();

    // First boot: the graph moves into the WAL.
    let report = migrate_kv_into_wal_if_first_boot(&redb, &wal)
        .expect("migrate")
        .expect("a first boot with KV data migrates");
    assert_eq!(report.nodes_imported, 2);
    assert_eq!(report.edges_imported, 1);
    assert!(wal.exists());

    // The durable service serves the migrated graph.
    let app = build_native_router(NativeApiState::new(Arc::new(
        NativeService::open(&wal).expect("open service"),
    )));
    let (status, body) = cypher(&app, "MATCH (a)-[:KNOWS]->(b) RETURN a.title, b.title").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["rows"], json!([["ada", "bob"]]));
    let (_, body) = cypher(&app, "MATCH (n {team: 'core'}) RETURN count(*)").await;
    assert_eq!(body["rows"], json!([[1]]));

    // The redb file is untouched (rollback stays possible), and a second
    // boot never re-migrates.
    assert_eq!(std::fs::metadata(&redb).unwrap().len(), redb_bytes_before);
    assert!(
        migrate_kv_into_wal_if_first_boot(&redb, &wal)
            .expect("second boot")
            .is_none(),
        "an existing WAL must never be touched"
    );
}

#[test]
fn migration_is_a_noop_without_kv_data() {
    use drevo::native_service::migrate_kv_into_wal_if_first_boot;
    let dir = tempfile::tempdir().unwrap();
    let report = migrate_kv_into_wal_if_first_boot(
        &dir.path().join("drevo.redb"),
        &dir.path().join("native.wal"),
    )
    .expect("noop");
    assert!(report.is_none());
    assert!(!dir.path().join("native.wal").exists());
}

// ── GraphML export / import (the backup path) ──────────────────────────

#[tokio::test]
async fn graphml_round_trip_through_the_native_router() {
    let app = build_native_router(NativeApiState::new(Arc::new(NativeService::in_memory())));
    let (status, _) = cypher(
        &app,
        "CREATE (:Person {title: 'ada', team: 'core'})-[:KNOWS]->(:Person {title: 'bob'})",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Export returns XML, not JSON.
    let req = Request::builder()
        .method("GET")
        .uri("/export/graphml")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("application/xml"));
    let xml = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(xml.contains("<graphml"));

    // Restore into a FRESH durable service; the property-indexed query
    // shape guards the feed-seeded index path.
    let app2 = build_native_router(NativeApiState::new(Arc::new(NativeService::in_memory())));
    let (status, report) = send(
        &app2,
        "POST",
        "/import/graphml",
        Some(json!({ "graphml": xml })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["nodes_imported"], 2);
    assert_eq!(report["edges_imported"], 1);
    let (_, body) = cypher(&app2, "MATCH (n {team: 'core'}) RETURN count(*)").await;
    assert_eq!(body["rows"], json!([[1]]));
    let (_, body) = cypher(&app2, "MATCH (a)-[:KNOWS]->(b) RETURN b.title").await;
    assert_eq!(body["rows"], json!([["bob"]]));

    // Re-importing drevo's own export is idempotent.
    let (status, report) = send(
        &app2,
        "POST",
        "/import/graphml",
        Some(json!({ "graphml": xml })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["nodes_imported"], 0);
    assert_eq!(report["nodes_skipped"], 2);
}

#[tokio::test]
async fn a_kv_backup_restores_into_the_durable_engine() {
    use drevo::cypher::executor::execute;
    use drevo::cypher::parser::parse;
    use std::collections::HashMap;

    // A backup taken from the KV engine (the live deployment's format)…
    let kv = drevo::db::Drevo::open_in_memory().expect("open kv");
    for stmt in [
        "CREATE (:Entity {title: 'kg-node', type: 'Trait'})",
        "CREATE (:Entity {title: 'kg-other'})",
    ] {
        execute(&parse(stmt).unwrap(), &kv, HashMap::new()).expect("seed");
    }
    let backup = kv.export_graphml().expect("kv export");

    // …restores into a zero-redb server.
    let app = build_native_router(NativeApiState::new(Arc::new(NativeService::in_memory())));
    let (status, report) = send(
        &app,
        "POST",
        "/import/graphml",
        Some(json!({ "graphml": backup })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["nodes_imported"], 2);
    let (_, body) = cypher(&app, "MATCH (n {type: 'Trait'}) RETURN n.title").await;
    assert_eq!(body["rows"], json!([["kg-node"]]));
}

#[tokio::test]
async fn import_accepts_bodies_beyond_the_default_axum_limit() {
    // The KV router's 2 MiB default body limit made restoring a real 71 MB
    // backup impossible; the native route raises it from day one. A >2 MiB
    // node body proves the raise without a slow test.
    // Build the oversized graph directly on a service (POST /cypher itself
    // keeps axum's default body limit — only the restore route is raised).
    let source = NativeService::in_memory();
    let big_body = "x".repeat(3 * 1024 * 1024);
    let q = drevo::cypher::parser::parse(&format!(
        "CREATE (:Blob {{title: 'big', body: '{big_body}'}})"
    ))
    .unwrap();
    source
        .execute(&q, std::collections::HashMap::new())
        .unwrap();
    let xml = source.export_graphml().unwrap();
    assert!(xml.len() > 3 * 1024 * 1024);

    let app2 = build_native_router(NativeApiState::new(Arc::new(NativeService::in_memory())));
    let (status, report) = send(
        &app2,
        "POST",
        "/import/graphml",
        Some(json!({ "graphml": xml })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a >2MiB restore must be accepted");
    assert_eq!(report["nodes_imported"], 1);
}
