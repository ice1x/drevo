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
    exec_result_to_response, json_to_cypher_value, ApiError, CypherRequest, CypherResponse,
};
use crate::cypher::parser;
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
}

impl NativeApiState {
    /// Wrap a service for serving.
    pub fn new(service: Arc<NativeService>) -> Self {
        Self {
            service,
            started_at: Instant::now(),
            shutting_down: Arc::new(AtomicBool::new(false)),
        }
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
        .route("/health", get(health))
        .route("/ready", get(health))
        .route("/status", get(status))
        .route("/cypher", post(cypher))
        .with_state(state)
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
