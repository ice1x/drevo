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
}

impl NativeApiState {
    /// Wrap a service for serving.
    pub fn new(service: Arc<NativeService>) -> Self {
        Self {
            service,
            started_at: Instant::now(),
            shutting_down: Arc::new(AtomicBool::new(false)),
            embeddings: None,
        }
    }

    /// Attach an embeddings backend, enabling `POST /v1/embeddings` —
    /// mirroring [`crate::api::ApiState::with_embeddings_backend`].
    #[must_use]
    pub fn with_embeddings_backend(mut self, backend: EmbeddingBackend) -> Self {
        self.embeddings = Some(Arc::new(backend));
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
        .route("/search/fts", post(search_fts))
        .route("/export/json", get(export_json))
        .route("/databases", get(list_databases))
        .route("/nodes/{id}", get(get_node))
        // The storage panel is redb-specific (bloat / keyspaces / shrink /
        // benchmark measure the KV file); on the WAL-backed engine these
        // answer 501 so the Web UI degrades loudly instead of lying.
        .route("/storage/bloat", get(storage_unsupported))
        .route("/storage/keyspaces", get(storage_unsupported))
        .route("/storage/shrink", post(storage_unsupported))
        .route("/storage/benchmark", post(storage_unsupported))
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

/// The redb storage panel has no meaning on the WAL store — a clear 501.
async fn storage_unsupported() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": "storage maintenance endpoints are redb-specific and not available on the native-durable engine",
            "status": 501,
        })),
    )
        .into_response()
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
