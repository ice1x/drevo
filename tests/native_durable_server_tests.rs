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
