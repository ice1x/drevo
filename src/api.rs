//! HTTP API scaffold for GraphNote DB (task 00037).
//!
//! This module exposes a thin JSON adapter over [`GraphNoteDb`] built on
//! [`axum`] and [`tokio`]. Task 00037 only covers the server skeleton:
//! shared state, a unified error type, and a [`build_router`] entry
//! point with a single root endpoint returning server metadata.
//! Node, edge, traversal, search, and admin endpoints are added in
//! tasks 00038–00042.
//!
//! The module is gated behind the `http` feature so that WebAssembly
//! builds (`--no-default-features --features wasm`) are unaffected.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Json;
use axum::Router;
use serde::Serialize;

use crate::db::GraphNoteDb;
use crate::error::GraphNoteError;

/// Shared application state passed to every HTTP handler.
///
/// Wraps a reference-counted [`GraphNoteDb`] so the same database
/// instance is shared across all requests without locking at the
/// router level. The database itself is `Send + Sync` because the
/// underlying `StorageBackend` is.
#[derive(Clone)]
pub struct ApiState {
    /// The shared database handle.
    pub db: Arc<GraphNoteDb>,
}

impl ApiState {
    /// Create a new [`ApiState`] from an existing database handle.
    pub fn new(db: Arc<GraphNoteDb>) -> Self {
        Self { db }
    }
}

/// Wrapper around [`GraphNoteError`] that implements [`IntoResponse`].
///
/// Handlers return `Result<T, ApiError>` and the `?` operator converts
/// database errors into JSON error responses with an appropriate HTTP
/// status code.
pub struct ApiError(pub GraphNoteError);

impl From<GraphNoteError> for ApiError {
    fn from(err: GraphNoteError) -> Self {
        Self(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match &self.0 {
            GraphNoteError::NodeNotFound(_) | GraphNoteError::EdgeNotFound(_) => {
                (StatusCode::NOT_FOUND, self.0.to_string())
            }
            GraphNoteError::DuplicateTitle(_) => (StatusCode::CONFLICT, self.0.to_string()),
            GraphNoteError::Locked => (StatusCode::SERVICE_UNAVAILABLE, self.0.to_string()),
            GraphNoteError::Storage(_)
            | GraphNoteError::Serialization(_)
            | GraphNoteError::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string()),
        };
        let body = Json(serde_json::json!({ "error": message }));
        (status, body).into_response()
    }
}

/// Server metadata returned by `GET /`.
#[derive(Debug, Serialize)]
pub struct ServerInfo {
    /// Crate name.
    pub name: &'static str,
    /// Crate version (from `CARGO_PKG_VERSION`).
    pub version: &'static str,
}

/// Handler for `GET /` — returns a small JSON document describing
/// the running server. Acts as a smoke test for the scaffold and as
/// a default landing page for clients that hit the root URL.
async fn root(State(_state): State<ApiState>) -> Json<ServerInfo> {
    Json(ServerInfo {
        name: "graphnote-db",
        version: env!("CARGO_PKG_VERSION"),
    })
}

/// Build the HTTP [`Router`] for a given [`ApiState`].
///
/// Returned router can be served with `axum::serve` on a TCP listener
/// or driven directly in tests via [`tower::ServiceExt::oneshot`].
pub fn build_router(state: ApiState) -> Router {
    Router::new().route("/", get(root)).with_state(state)
}
