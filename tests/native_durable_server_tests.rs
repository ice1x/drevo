//! Guards for `DREVO_ENGINE=native-durable` (RFC #307, Phase 4/7): the
//! server mode where the WAL-backed native engine IS the store of record —
//! no KV store, no redb file — serving the minimal native HTTP surface.

#![cfg(all(not(target_arch = "wasm32"), feature = "http"))]

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

#[test]
fn default_engine_is_native_durable_and_kv_still_parses() {
    // The KV serving mode was removed (epic #444): native-durable is the
    // default and the only served engine.
    assert_eq!(EngineMode::default(), EngineMode::NativeDurable);
    let cfg = Config::from_env(|_| None).unwrap();
    assert_eq!(
        cfg.engine,
        EngineMode::NativeDurable,
        "no DREVO_ENGINE must default to native-durable"
    );
    // `kv` still parses (the KV code compiles for the test corpus); `run()`
    // warns and serves native for it, but the value is not rejected.
    let kv = Config::from_env(|k| match k {
        "DREVO_ENGINE" => Some("kv".to_string()),
        _ => None,
    })
    .unwrap();
    assert_eq!(kv.engine, EngineMode::Kv);
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

    // The raw-REST surface is now served on the native engine too (REST-CRUD
    // parity): `GET /nodes` without a `kind` is a 400, not a 404 — the route
    // exists and validates its query, exactly like the KV router.
    let (status, _) = send(&app, "GET", "/nodes", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn root_serves_server_info_for_the_web_ui() {
    // The Web UI probes `GET /` on load (`loadServerInfo`) for `{name, version}`
    // and shows "Cannot reach drevo HTTP API at /" when it 404s. The durable
    // router must serve it, like the KV router does.
    let app = build_native_router(NativeApiState::new(Arc::new(NativeService::in_memory())));
    let (status, info) = send(&app, "GET", "/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(info["name"], "drevo");
    assert!(
        info["version"].is_string(),
        "root must report a version string, got {info}"
    );
}

#[tokio::test]
async fn parse_and_execution_errors_are_bad_requests() {
    let app = build_native_router(NativeApiState::new(Arc::new(NativeService::in_memory())));
    let (status, _) = cypher(&app, "MATCH (((").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // A still-KV-only procedure (engine.status) surfaces as 400 on native.
    // (semantic.status now runs on native — see native_service_tests, #447.)
    let (status, _) = cypher(&app, "CALL drevo.engine.status()").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "KV-only procedure surfaces as 400"
    );
}

/// Raw-REST CRUD + traversal + faceting + metrics now have parity with the KV
/// router on the durable-native engine, so a non-Cypher client migrates
/// unchanged (issue: native-router REST-CRUD parity).
#[tokio::test]
async fn rest_crud_surface_has_parity_with_the_kv_router() {
    let app = build_native_router(NativeApiState::new(Arc::new(NativeService::in_memory())));

    // POST /nodes → 201 with a generated id.
    let (status, a) = send(
        &app,
        "POST",
        "/nodes",
        Some(
            json!({ "kind": "note", "title": "A", "body": "graph database engine",
                     "body_html": "", "properties": {} }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let a_id = a["id"].as_u64().expect("node id");
    let (status, b) = send(
        &app,
        "POST",
        "/nodes",
        Some(
            json!({ "kind": "note", "title": "B", "body": "graph database theory",
                     "body_html": "", "properties": {} }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let b_id = b["id"].as_u64().expect("node id");

    // GET /nodes?kind= lists them; a missing kind is a 400.
    let (status, list) = send(&app, "GET", "/nodes?kind=note", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["nodes"].as_array().unwrap().len(), 2);
    let (status, _) = send(&app, "GET", "/nodes", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // POST /edges → 201; GET /edges?kind= lists it.
    let (status, _) = send(
        &app,
        "POST",
        "/edges",
        Some(json!({ "from_id": a_id, "to_id": b_id, "kind": "link",
                     "weight": 1.0, "properties": {} })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, edges) = send(&app, "GET", "/edges?kind=link", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(edges["edges"].as_array().unwrap().len(), 1);
    // An edge to a missing endpoint is a 404, like the KV router.
    let (status, _) = send(
        &app,
        "POST",
        "/edges",
        Some(json!({ "from_id": a_id, "to_id": 999_999, "kind": "link",
                     "weight": 1.0, "properties": {} })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // GET /paths/shortest: a→b reachable, unreachable target is 200 {path:null},
    // a missing endpoint is 404.
    let (status, path) = send(
        &app,
        "GET",
        &format!("/paths/shortest?from={a_id}&to={b_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(path["path"], json!([a_id, b_id]));
    let (status, path) = send(
        &app,
        "GET",
        &format!("/paths/shortest?from={b_id}&to={a_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(path["path"], Value::Null, "b→a has no outgoing route");
    let (status, _) = send(&app, "GET", "/paths/shortest?from=1&to=999999", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // GET /facets groups the two notes by the shared keywords of their body.
    let (status, facets) = send(&app, "GET", "/facets?kind=note&property=body&k=5", None).await;
    assert_eq!(status, StatusCode::OK);
    let labels: Vec<&str> = facets["facets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["facet"].as_str().unwrap())
        .collect();
    assert!(
        labels.contains(&"graph") && labels.contains(&"database"),
        "shared keywords should surface as facets, got {labels:?}"
    );

    // GET /metrics renders Prometheus text with the standard gauges.
    let req = Request::builder()
        .method("GET")
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(ct.starts_with("text/plain; version=0.0.4"), "got {ct}");
    let text = String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(
        text.contains("drevo_process_uptime_seconds"),
        "metrics body missing uptime gauge: {text}"
    );
}

/// Per-node traversal (`/nodes/{id}/edges|neighbors|subgraph`) and JSON import
/// (`POST /import/json`) round out the raw-REST parity on the native engine.
#[tokio::test]
async fn traversal_and_import_have_parity_with_the_kv_router() {
    let app = build_native_router(NativeApiState::new(Arc::new(NativeService::in_memory())));

    // a -link-> b
    let (_, a) = send(
        &app,
        "POST",
        "/nodes",
        Some(json!({ "kind": "note", "title": "A", "body": "",
                     "body_html": "", "properties": {} })),
    )
    .await;
    let a_id = a["id"].as_u64().unwrap();
    let (_, b) = send(
        &app,
        "POST",
        "/nodes",
        Some(json!({ "kind": "note", "title": "B", "body": "",
                     "body_html": "", "properties": {} })),
    )
    .await;
    let b_id = b["id"].as_u64().unwrap();
    let (st, _) = send(
        &app,
        "POST",
        "/edges",
        Some(json!({ "from_id": a_id, "to_id": b_id, "kind": "link",
                     "weight": 1.0, "properties": {} })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    // GET /nodes/{id}/edges — the one incident edge (default direction=both).
    let (st, edges) = send(&app, "GET", &format!("/nodes/{a_id}/edges"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(edges["edges"].as_array().unwrap().len(), 1);
    // An invalid direction is a 400.
    let (st, _) = send(
        &app,
        "GET",
        &format!("/nodes/{a_id}/edges?direction=sideways"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // GET /nodes/{id}/neighbors — b; a missing node is 404.
    let (st, neigh) = send(&app, "GET", &format!("/nodes/{a_id}/neighbors"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(neigh["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(neigh["nodes"][0]["id"].as_u64().unwrap(), b_id);
    let (st, _) = send(&app, "GET", "/nodes/999999/neighbors", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // GET /nodes/{id}/subgraph — a + b within 1 hop; missing root is 404.
    let (st, sub) = send(
        &app,
        "GET",
        &format!("/nodes/{a_id}/subgraph?depth=1"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(sub["nodes"].as_array().unwrap().len(), 2);
    let (st, _) = send(&app, "GET", "/nodes/999999/subgraph", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // POST /import/json — export this graph, replay it into a fresh engine.
    let (st, dump) = send(&app, "GET", "/export/json", None).await;
    assert_eq!(st, StatusCode::OK);
    // /export/json returns the raw dump document; feed it back verbatim.
    let dump_str = serde_json::to_string(&dump).unwrap();
    let fresh = build_native_router(NativeApiState::new(Arc::new(NativeService::in_memory())));
    let (st, report) = send(
        &fresh,
        "POST",
        "/import/json",
        Some(json!({ "dump": dump_str })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "import report: {report}");
    assert_eq!(report["nodes_imported"].as_u64().unwrap(), 2);
    assert_eq!(report["edges_imported"].as_u64().unwrap(), 1);
    // The imported graph is queryable on the fresh engine.
    let (st, list) = send(&fresh, "GET", "/nodes?kind=note", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(list["nodes"].as_array().unwrap().len(), 2);
    // A malformed / unknown-format dump is rejected.
    let (st, _) = send(
        &fresh,
        "POST",
        "/import/json",
        Some(json!({ "dump": "{\"format\":\"nope\"}" })),
    )
    .await;
    assert_ne!(st, StatusCode::OK, "unknown dump format must not import");
}

#[tokio::test]
async fn storage_panel_is_engine_agnostic_on_the_wal_store() {
    // The storage panel (bloat / shrink / benchmark / keyspaces) used to 501 on
    // the WAL engine; it now serves the same contracts as the KV router.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native.wal");
    let app = build_native_router(NativeApiState::new(Arc::new(
        NativeService::open(&path).expect("open"),
    )));

    // Create + churn so the append-only WAL accumulates superseded records.
    for i in 0..20 {
        let (st, _) = cypher(&app, &format!("CREATE (:Bench {{i: {i}}})")).await;
        assert_eq!(st, StatusCode::OK);
    }
    for r in 0..6 {
        let (st, _) = cypher(&app, &format!("MATCH (n:Bench) SET n.r = {r}")).await;
        assert_eq!(st, StatusCode::OK);
    }

    // bloat: physical WAL > compacted logical size → a ratio above 1.
    let (st, bloat) = send(&app, "GET", "/storage/bloat", None).await;
    assert_eq!(st, StatusCode::OK, "bloat must not 501");
    let file = bloat["file_bytes"].as_u64().expect("file_bytes");
    let logical = bloat["logical_bytes"].as_u64().expect("logical_bytes");
    assert!(
        file > logical,
        "WAL bloated: file {file} > logical {logical}"
    );
    assert!(bloat["bloat_ratio"].as_f64().unwrap() > 1.0);
    assert_eq!(bloat["node_count"].as_u64().unwrap(), 20);

    // shrink: compaction reclaims the bloat, same CompactReport contract.
    let (st, rep) = send(&app, "POST", "/storage/shrink", None).await;
    assert_eq!(st, StatusCode::OK, "shrink must not 501");
    let before = rep["bytes_before"].as_u64().unwrap();
    let after = rep["bytes_after"].as_u64().unwrap();
    assert!(after <= before, "after {after} <= before {before}");
    assert_eq!(rep["bytes_reclaimed"].as_u64().unwrap(), before - after);
    assert!(after < file, "shrink actually reduced the file");

    // After compaction the file is ~ the logical size.
    let (_, bloat2) = send(&app, "GET", "/storage/bloat", None).await;
    assert!(bloat2["bloat_ratio"].as_f64().unwrap() < 1.5);
    assert_eq!(
        bloat2["node_count"].as_u64().unwrap(),
        20,
        "data intact after shrink"
    );

    // benchmark: a report, not a 501.
    let (st, bench) = send(&app, "POST", "/storage/benchmark", None).await;
    assert_eq!(st, StatusCode::OK, "benchmark must not 501");
    assert!(bench["incr_write_nodes_per_sec"].as_f64().unwrap() > 0.0);
    assert_eq!(bench["incr_n"].as_u64().unwrap(), 500);

    // keyspaces: a real per-structure breakdown of the in-memory index stack
    // (records + adjacency + title + kind), not an empty list. The WAL stores
    // only records on disk, but the panel's Keyspaces table is populated from
    // the live indexes so it matches the KV router's richness.
    let (st, ks) = send(&app, "GET", "/storage/keyspaces", None).await;
    assert_eq!(st, StatusCode::OK, "keyspaces must not 501");
    let rows = ks["keyspaces"].as_array().expect("keyspaces array");
    assert!(!rows.is_empty(), "keyspaces breakdown must not be empty");
    let by = |name: &str| {
        rows.iter()
            .find(|r| r["prefix"] == name)
            .unwrap_or_else(|| panic!("keyspace `{name}` present"))
    };
    // 20 live nodes → the `node` keyspace reports 20 rows with a real footprint.
    assert_eq!(by("node")["entries"].as_u64().unwrap(), 20);
    assert!(by("node")["content_bytes"].as_u64().unwrap() > 0);
    // The index keyspaces are present (no on-disk redb tables, but live in RAM).
    for name in ["edge", "out", "in", "title", "kind"] {
        by(name); // panics if the row is missing
    }
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

    fn failure_code(messages: &[ServerMessage]) -> String {
        match messages.last() {
            Some(ServerMessage::Failure { metadata }) => match metadata.get("code") {
                Some(BoltValue::String(code)) => code.clone(),
                other => panic!("failure without a code: {other:?}"),
            },
            other => panic!("expected FAILURE, got {other:?}"),
        }
    }

    #[test]
    fn managed_transaction_commits_atomically_on_the_durable_engine() {
        let service = Arc::new(NativeService::in_memory());
        let mut session = Session::new_durable(Arc::clone(&service));
        assert_success(&session.handle(ClientMessage::Hello { extra: dict([]) }));

        assert_success(&session.handle(ClientMessage::Begin { extra: dict([]) }));
        assert_success(&run_statement(
            &mut session,
            "CREATE (:Person {title: 'ada'})",
        ));
        assert_success(&pull_all(&mut session));

        // Read-your-writes inside the transaction…
        assert_success(&run_statement(
            &mut session,
            "MATCH (n:Person) RETURN count(*)",
        ));
        let records = pull_all(&mut session);
        assert!(
            records
                .iter()
                .any(|m| matches!(m, ServerMessage::Record { fields } if fields[0] == BoltValue::Integer(1))),
            "the transaction must see its own write: {records:?}"
        );
        // …while a concurrent autocommit reader on the service sees nothing.
        let q = drevo::cypher::parser::parse("MATCH (n) RETURN count(*)").unwrap();
        let live = service
            .execute(&q, std::collections::HashMap::new())
            .unwrap();
        assert_eq!(
            live.rows,
            vec![vec![drevo::cypher::executor::Value::Integer(0)]]
        );

        assert_success(&session.handle(ClientMessage::Commit));
        let live = service
            .execute(&q, std::collections::HashMap::new())
            .unwrap();
        assert_eq!(
            live.rows,
            vec![vec![drevo::cypher::executor::Value::Integer(1)]]
        );
    }

    #[test]
    fn managed_transaction_rollback_and_reset_discard() {
        let service = Arc::new(NativeService::in_memory());
        let q = drevo::cypher::parser::parse("MATCH (n) RETURN count(*)").unwrap();

        let mut session = Session::new_durable(Arc::clone(&service));
        assert_success(&session.handle(ClientMessage::Hello { extra: dict([]) }));
        assert_success(&session.handle(ClientMessage::Begin { extra: dict([]) }));
        assert_success(&run_statement(&mut session, "CREATE (:G {title: 'g1'})"));
        assert_success(&pull_all(&mut session));
        assert_success(&session.handle(ClientMessage::Rollback));
        let live = service
            .execute(&q, std::collections::HashMap::new())
            .unwrap();
        assert_eq!(
            live.rows,
            vec![vec![drevo::cypher::executor::Value::Integer(0)]]
        );

        // RESET inside a transaction rolls it back too.
        assert_success(&session.handle(ClientMessage::Begin { extra: dict([]) }));
        assert_success(&run_statement(&mut session, "CREATE (:G {title: 'g2'})"));
        assert_success(&pull_all(&mut session));
        session.handle(ClientMessage::Reset);
        let live = service
            .execute(&q, std::collections::HashMap::new())
            .unwrap();
        assert_eq!(
            live.rows,
            vec![vec![drevo::cypher::executor::Value::Integer(0)]]
        );
    }

    #[test]
    fn conflicting_commit_surfaces_the_transient_code() {
        let service = Arc::new(NativeService::in_memory());
        let mut session = Session::new_durable(Arc::clone(&service));
        assert_success(&session.handle(ClientMessage::Hello { extra: dict([]) }));
        assert_success(&session.handle(ClientMessage::Begin { extra: dict([]) }));
        assert_success(&run_statement(&mut session, "CREATE (:T {title: 't'})"));
        assert_success(&pull_all(&mut session));

        // A concurrent autocommit write advances the live graph, so the
        // transaction's commit loses the first-committer race.
        let w = drevo::cypher::parser::parse("CREATE (:Race {title: 'r'})").unwrap();
        service
            .execute(&w, std::collections::HashMap::new())
            .unwrap();

        let replies = session.handle(ClientMessage::Commit);
        assert_eq!(
            failure_code(&replies),
            "Neo.TransientError.Transaction.Outdated",
            "drivers must see the retryable class"
        );
    }
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

// ── /v1/embeddings parity ──────────────────────────────────────────────

#[tokio::test]
async fn embeddings_route_exists_with_kv_identical_unconfigured_semantics() {
    // The restart tooling probes POST /v1/embeddings after boot, so the
    // route must exist in every server mode with the same contract:
    // deterministic 400 on empty input, 503 when no backend is configured.
    let app = build_native_router(NativeApiState::new(Arc::new(NativeService::in_memory())));

    let (status, _) = send(&app, "POST", "/v1/embeddings", Some(json!({ "input": [] }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "empty input is a 400");

    let (status, _) = send(
        &app,
        "POST",
        "/v1/embeddings",
        Some(json!({ "input": "ping" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "no backend configured is a 503"
    );
}

// ── Web-UI surface parity ──────────────────────────────────────────────

#[tokio::test]
async fn web_ui_surface_is_served_on_the_native_router() {
    let app = build_native_router(NativeApiState::new(Arc::new(NativeService::in_memory())));
    let (status, _) = cypher(
        &app,
        "CREATE (:Doc {title: 'guide', body: 'ownership and borrowing', team: 'core'})",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Static assets.
    for uri in [
        "/ui",
        "/ui/app.js",
        "/ui/styles.css",
        "/ui/vendor/cytoscape.min.js",
    ] {
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
    }

    // FTS search, KV-shaped.
    let (status, body) = send(
        &app,
        "POST",
        "/search/fts",
        Some(json!({ "query": "borrowing", "limit": 5 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["results"].as_array().unwrap().len(), 1);
    assert_eq!(body["results"][0]["node"]["title"], "guide");

    // Node detail fetch + 404.
    let id = body["results"][0]["node"]["id"].as_u64().unwrap();
    let (status, node) = send(&app, "GET", &format!("/nodes/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(node["title"], "guide");
    let (status, _) = send(&app, "GET", "/nodes/99999", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // JSON dump.
    let (status, dump) = send(&app, "GET", "/export/json", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dump["nodes"].as_array().unwrap().len(), 1);

    // Database selector sees the single durable graph.
    let (status, dbs) = send(&app, "GET", "/databases", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dbs["databases"], json!(["drevo"]));

    // The storage panel is engine-agnostic on the WAL store (its bloat/shrink/
    // benchmark/keyspaces semantics are covered by
    // `storage_panel_is_engine_agnostic_on_the_wal_store`); here just confirm the
    // endpoints are served, not 501.
    let (status, _) = send(&app, "GET", "/storage/bloat", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(&app, "POST", "/storage/shrink", None).await;
    assert_eq!(status, StatusCode::OK);
}
