//! HTTP surface for the durable-native server mode (RFC
//! `docs/rfc-native-core.md` #307, Phase 4/7 — `DREVO_ENGINE=native-durable`).
//!
//! A deliberately minimal router over a
//! [`crate::native_service::NativeService`]: liveness (`/health`, `/ready`),
//! identity (`/status`), and Cypher (`POST /cypher`) — the query surface the
//! native engine serves natively, full-text included. The KV REST surface
//! (nodes/edges CRUD, exports, vectors, semantic, Web UI) is **absent by
//! design** in this mode, not silently empty: those endpoints are the KV
//! store's, and this server runs without one. They return 404 until they are
//! ported to the native engine.
//!
//! Multi-database catalogs are also out of scope for this slice — the mode
//! serves the single durable graph the process was pointed at.

#![cfg(feature = "http")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;

use crate::api::{
    embeddings_response, exec_result_to_response, json_to_cypher_value, ApiError, CypherRequest,
    CypherResponse, DatabaseListResponse, ImportGraphmlRequest, SearchFtsRequest,
    SearchFtsResponse,
};
use crate::catalog::DEFAULT_DB;
use crate::cypher::parser;
use crate::embeddings::{EmbeddingBackend, EmbeddingsRequest};
use crate::native_service::NativeService;

/// Shared state of the durable-native HTTP surface.
#[derive(Clone)]
pub struct NativeApiState {
    /// The store of record.
    pub service: Arc<NativeService>,
    /// Construction instant, for `/status` uptime.
    started_at: Instant,
    /// Graceful-shutdown flag, mirroring [`crate::api::ApiState`]'s contract.
    shutting_down: Arc<AtomicBool>,
    /// Optional embeddings proxy backend — `POST /v1/embeddings` answers
    /// `503` ("not configured") without one, exactly like the KV router.
    embeddings: Option<Arc<EmbeddingBackend>>,
    /// Shared runtime embeddings config store backing `/config/embeddings`
    /// (Web-UI-settable API key/upstream/model); the same `Arc` the proxy
    /// reads, so a write takes effect live. Mirrors
    /// [`crate::api::ApiState::embeddings_config`].
    embeddings_config: Option<Arc<crate::embeddings::EmbeddingsConfigStore>>,
}

impl NativeApiState {
    /// Wrap a service for serving.
    pub fn new(service: Arc<NativeService>) -> Self {
        Self {
            service,
            started_at: Instant::now(),
            shutting_down: Arc::new(AtomicBool::new(false)),
            embeddings: None,
            embeddings_config: None,
        }
    }

    /// Attach an embeddings backend, enabling `POST /v1/embeddings` —
    /// mirroring [`crate::api::ApiState::with_embeddings_backend`].
    #[must_use]
    pub fn with_embeddings_backend(mut self, backend: EmbeddingBackend) -> Self {
        self.embeddings = Some(Arc::new(backend));
        self
    }

    /// Attach the shared embeddings config store, enabling
    /// `GET`/`POST /config/embeddings` — mirroring
    /// [`crate::api::ApiState::with_embeddings_config_store`].
    #[must_use]
    pub fn with_embeddings_config_store(
        mut self,
        store: Arc<crate::embeddings::EmbeddingsConfigStore>,
    ) -> Self {
        self.embeddings_config = Some(store);
        self
    }

    /// Mark the API as draining — `/health` and `/ready` answer 503 after.
    pub fn signal_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
    }

    fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }
}

/// Build the durable-native router. See the [module docs](self).
pub fn build_native_router(state: NativeApiState) -> Router {
    Router::new()
        // `GET /` — server info (name + version). The Web UI probes it on load
        // (`loadServerInfo`); without it the UI shows "Cannot reach drevo HTTP
        // API at /" even though every other endpoint is up. Serves the same
        // body as `/status`.
        .route("/", get(status))
        .route("/health", get(health))
        .route("/ready", get(health))
        .route("/status", get(status))
        .route("/cypher", post(cypher))
        .route("/export/graphml", get(export_graphml))
        .route("/v1/embeddings", post(embeddings))
        .route(
            "/config/embeddings",
            get(get_embeddings_config).post(set_embeddings_config),
        )
        .route("/search/fts", post(search_fts))
        .route("/export/json", get(export_json))
        .route("/databases", get(list_databases))
        .route("/nodes/{id}", get(get_node))
        // The storage panel is engine-agnostic: the same endpoints/contracts as
        // the KV router, implemented over the WAL store — bloat = physical WAL
        // vs compacted size, shrink = WAL compaction, benchmark = the same
        // throwaway probe with native FTS. Keyspaces are a redb-file concept
        // with no WAL analogue, so that one returns an empty breakdown.
        .route("/storage/bloat", get(storage_bloat))
        .route("/storage/keyspaces", get(storage_keyspaces))
        .route("/storage/shrink", post(storage_shrink))
        .route("/storage/benchmark", post(storage_benchmark))
        // The Web UI — the same embedded, same-origin assets as the KV
        // server (`crate::web_ui`), pointed at the same-shape endpoints.
        .route("/ui", get(crate::web_ui::serve_index))
        .route("/ui/", get(crate::web_ui::redirect_ui_slash))
        .route("/ui/app.js", get(crate::web_ui::serve_app_js))
        .route("/ui/styles.css", get(crate::web_ui::serve_styles_css))
        .route(
            "/ui/vendor/cytoscape.min.js",
            get(crate::web_ui::serve_vendor_cytoscape),
        )
        .route(
            "/ui/vendor/layout-base.js",
            get(crate::web_ui::serve_vendor_layout_base),
        )
        .route(
            "/ui/vendor/cose-base.js",
            get(crate::web_ui::serve_vendor_cose_base),
        )
        .route(
            "/ui/vendor/cytoscape-fcose.js",
            get(crate::web_ui::serve_vendor_fcose),
        )
        .route(
            "/ui/vendor/cola.min.js",
            get(crate::web_ui::serve_vendor_cola),
        )
        .route(
            "/ui/vendor/cytoscape-cola.js",
            get(crate::web_ui::serve_vendor_cytoscape_cola),
        )
        // Real backups are tens of megabytes; axum's default 2 MiB body
        // limit would make a restore of drevo's own export impossible, so
        // this route raises it (1 GiB) from day one.
        .route(
            "/import/graphml",
            post(import_graphml).layer(axum::extract::DefaultBodyLimit::max(1024 * 1024 * 1024)),
        )
        .with_state(state)
}

/// `POST /v1/embeddings` — the OpenAI-compatible embeddings proxy, identical
/// to the KV router's route (shared body); the restart tooling's key-check
/// depends on it being present in every server mode.
async fn embeddings(
    State(state): State<NativeApiState>,
    body: Result<Json<EmbeddingsRequest>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(req) = body?;
    embeddings_response(state.embeddings.as_deref(), req).await
}

/// `GET /config/embeddings` — secret-free view of the runtime embeddings
/// config (shared body with the KV router).
async fn get_embeddings_config(
    State(state): State<NativeApiState>,
) -> Result<Json<crate::embeddings::EmbeddingsStatus>, ApiError> {
    crate::api::embeddings_config_status(state.embeddings_config.as_deref())
}

/// `POST /config/embeddings` — validate + persist + hot-swap the runtime
/// embeddings config (shared body); the API key is never echoed back.
async fn set_embeddings_config(
    State(state): State<NativeApiState>,
    body: Result<Json<crate::embeddings::EmbeddingsConfigUpdate>, JsonRejection>,
) -> Result<Json<crate::embeddings::EmbeddingsStatus>, ApiError> {
    let Json(update) = body?;
    crate::api::embeddings_config_apply(state.embeddings_config.as_deref(), update)
}

/// `POST /search/fts` — BM25 full-text search over the native index, the
/// same request/response shape as the KV route.
async fn search_fts(
    State(state): State<NativeApiState>,
    body: Result<Json<SearchFtsRequest>, JsonRejection>,
) -> Result<Json<SearchFtsResponse>, ApiError> {
    let Json(SearchFtsRequest { query, limit }) = body?;
    let query =
        query.ok_or_else(|| ApiError::BadRequest("field 'query' is required".to_string()))?;
    let limit = limit.unwrap_or(10).min(100);
    let results = state.service.search_fts(&query, limit);
    Ok(Json(SearchFtsResponse { results }))
}

/// `GET /export/json` — the `drevo-json-v1` dump, same body as the KV route.
async fn export_json(State(state): State<NativeApiState>) -> Result<Response, ApiError> {
    let dump = state.service.export_json()?;
    Ok((StatusCode::OK, [("content-type", "application/json")], dump).into_response())
}

/// `GET /databases` — this mode serves the single durable graph, reported in
/// the KV route's response shape so the Web UI's selector keeps working.
async fn list_databases() -> Json<DatabaseListResponse> {
    Json(DatabaseListResponse {
        databases: vec![DEFAULT_DB.to_string()],
        default: DEFAULT_DB,
    })
}

/// `GET /nodes/{id}` — one node, the KV route's shape (the Web UI's detail
/// pane fetch).
async fn get_node(
    State(state): State<NativeApiState>,
    axum::extract::Path(id): axum::extract::Path<u64>,
) -> Result<Json<crate::model::Node>, ApiError> {
    Ok(Json(state.service.get_node(id)?))
}

/// `GET /storage/bloat` — physical WAL size vs compacted (logical) size, in the
/// same [`BloatReport`](crate::db::BloatReport) contract as the KV router, so the
/// Web UI's storage panel is engine-agnostic.
async fn storage_bloat(State(state): State<NativeApiState>) -> Json<crate::db::BloatReport> {
    Json(state.service.storage_bloat())
}

/// `POST /storage/shrink` — compact the append-only WAL to its current state and
/// report reclaimed bytes (same [`CompactReport`](crate::db::CompactReport)
/// contract as the KV router).
async fn storage_shrink(
    State(state): State<NativeApiState>,
) -> Result<Json<crate::db::CompactReport>, ApiError> {
    Ok(Json(state.service.compact()?))
}

/// `GET /storage/keyspaces` — the KV router breaks its redb file into per-prefix
/// keyspaces; the WAL store keeps its indexes in memory (rebuilt from the log on
/// open), so this reports the live in-memory index structures — records plus the
/// adjacency, title and kind indexes — in the same `{ "keyspaces": [...] }`
/// shape, keeping the panel's Keyspaces table populated and engine-agnostic.
async fn storage_keyspaces(State(state): State<NativeApiState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "keyspaces": state.service.keyspace_stats() }))
}

/// `POST /storage/benchmark` — the same self-contained probe as the KV router:
/// write throughput on a throwaway in-memory database (never touches the live
/// graph) plus median FTS latency read-only against this engine's live data.
async fn storage_benchmark(
    State(state): State<NativeApiState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    use crate::model::{NewNode, Properties};
    use std::time::Instant;

    const SHARED: &str =
        "anxious deadlines mentoring graph vectors embeddings semantic search relationships";
    let incr_n: usize = 500;
    let batch_n: usize = 500;
    let mk = |prefix: &str, i: usize| NewNode {
        kind: "bench".to_string(),
        title: format!("{prefix}-{i}"),
        body: format!("note {i} {SHARED}"),
        body_html: String::new(),
        properties: Properties::default(),
    };

    // Write throughput on throwaway in-memory databases — never the live graph.
    let scratch = crate::db::Drevo::open_in_memory()?;
    let t0 = Instant::now();
    for i in 0..incr_n {
        scratch.create_node(mk("bench-incr", i))?;
    }
    let incr_secs = t0.elapsed().as_secs_f64();

    let scratch2 = crate::db::Drevo::open_in_memory()?;
    let batch: Vec<NewNode> = (0..batch_n).map(|i| mk("bench-batch", i)).collect();
    let t1 = Instant::now();
    scratch2.create_nodes(batch)?;
    let batch_secs = t1.elapsed().as_secs_f64();

    // FTS latency read-only against this engine's live data (native index).
    let queries = ["graph", "error", "test", "the"];
    let reps: usize = 20;
    let mut samples: Vec<f64> = Vec::with_capacity(queries.len() * reps);
    for _ in 0..reps {
        for q in &queries {
            let t = Instant::now();
            let _ = state.service.search_fts(q, 10);
            samples.push(t.elapsed().as_secs_f64() * 1000.0);
        }
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let search_median_ms = samples.get(samples.len() / 2).copied().unwrap_or(0.0);

    let per_sec = |n: usize, secs: f64| if secs > 0.0 { n as f64 / secs } else { 0.0 };
    let round1 = |x: f64| (x * 10.0).round() / 10.0;
    let round3 = |x: f64| (x * 1000.0).round() / 1000.0;

    // Same field names/shape as the KV router's BenchmarkReport (agnostic UI).
    Ok(Json(serde_json::json!({
        "incr_write_nodes_per_sec": round1(per_sec(incr_n, incr_secs)),
        "batch_write_nodes_per_sec": round1(per_sec(batch_n, batch_secs)),
        "search_median_ms": round3(search_median_ms),
        "incr_n": incr_n,
        "batch_n": batch_n,
        "search_samples": samples.len(),
    })))
}

/// `GET /export/graphml` — the full graph as a GraphML 1.0 document, byte-
/// compatible with the KV server's export (the backup path).
async fn export_graphml(State(state): State<NativeApiState>) -> Result<Response, ApiError> {
    let xml = state.service.export_graphml()?;
    Ok((
        StatusCode::OK,
        [("content-type", "application/xml; charset=utf-8")],
        xml,
    )
        .into_response())
}

/// `POST /import/graphml` — restore a GraphML document (a drevo backup, or
/// interop GraphML) into the durable graph. Idempotent for drevo's own
/// exports; id collisions with different content are conflicts.
async fn import_graphml(
    State(state): State<NativeApiState>,
    body: Result<Json<ImportGraphmlRequest>, JsonRejection>,
) -> Result<Json<crate::dump::ImportReport>, ApiError> {
    let Json(req) = body?;
    let report = state.service.import_graphml(&req.graphml)?;
    Ok(Json(report))
}

async fn health(State(state): State<NativeApiState>) -> Response {
    if state.is_shutting_down() {
        (StatusCode::SERVICE_UNAVAILABLE, "draining").into_response()
    } else {
        (StatusCode::OK, "ok").into_response()
    }
}

/// `GET /status` — same shape as the KV server's, plus the engine marker.
async fn status(State(state): State<NativeApiState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "name": "drevo",
        "version": crate::VERSION,
        "engine": "native-durable",
        "uptime_seconds": state.started_at.elapsed().as_secs(),
        // Stable replica identity for the multi-writer/P2P substrate (issue
        // #389). A string so a u64 survives JSON clients that use f64 numbers.
        "origin": state.service.origin_id().0.to_string(),
    }))
}

/// `POST /cypher` — parse and execute on the durable service.
async fn cypher(
    State(state): State<NativeApiState>,
    body: Result<Json<CypherRequest>, JsonRejection>,
) -> Result<Json<CypherResponse>, ApiError> {
    let Json(CypherRequest { query, params }) = body?;
    let query =
        query.ok_or_else(|| ApiError::BadRequest("field 'query' is required".to_string()))?;
    let params = params
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| (k, json_to_cypher_value(v)))
        .collect();
    let ast = parser::parse(&query)
        .map_err(|e| ApiError::BadRequest(format!("Cypher parse error: {e}")))?;
    let result = state
        .service
        .execute(&ast, params)
        .map_err(|e| ApiError::BadRequest(format!("Cypher execution error: {e}")))?;
    Ok(Json(exec_result_to_response(result)))
}
