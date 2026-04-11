//! HTTP API for GraphNote DB.
//!
//! This module exposes a thin JSON adapter over [`GraphNoteDb`] built on
//! [`axum`] and [`tokio`]. Task 00037 introduced the server skeleton
//! (shared state, unified error type, root endpoint) and task 00038
//! added node CRUD endpoints:
//!
//! - `GET /` — server metadata
//! - `POST /nodes` — create a node
//! - `GET /nodes?kind=&limit=&offset=` — list nodes filtered by kind
//! - `GET /nodes/{id}` — fetch a node by id
//! - `PATCH /nodes/{id}` — partial update
//! - `DELETE /nodes/{id}` — delete a node
//!
//! Edge, traversal, search, and admin endpoints arrive in tasks
//! 00039–00042. The whole module is gated behind the `http` feature
//! so that WebAssembly builds (`--no-default-features --features wasm`)
//! are unaffected.

use std::sync::Arc;

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Json;
use axum::Router;
use serde::{Deserialize, Serialize};

use crate::db::GraphNoteDb;
use crate::error::GraphNoteError;
use crate::model::{NewNode, Node, NodePatch};

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

/// Unified error type returned by every HTTP handler.
///
/// Wraps either a [`GraphNoteError`] (producing a status code based on
/// the underlying database error) or a bad-request variant for client
/// input problems (malformed JSON body, missing query parameter).
pub enum ApiError {
    /// A database operation failed.
    Db(GraphNoteError),
    /// The client sent an invalid request (400 Bad Request).
    BadRequest(String),
}

impl From<GraphNoteError> for ApiError {
    fn from(err: GraphNoteError) -> Self {
        Self::Db(err)
    }
}

impl From<JsonRejection> for ApiError {
    fn from(err: JsonRejection) -> Self {
        Self::BadRequest(err.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(err: QueryRejection) -> Self {
        Self::BadRequest(err.body_text())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::Db(err) => match &err {
                GraphNoteError::NodeNotFound(_) | GraphNoteError::EdgeNotFound(_) => {
                    (StatusCode::NOT_FOUND, err.to_string())
                }
                GraphNoteError::DuplicateTitle(_) => (StatusCode::CONFLICT, err.to_string()),
                GraphNoteError::Locked => (StatusCode::SERVICE_UNAVAILABLE, err.to_string()),
                GraphNoteError::Storage(_)
                | GraphNoteError::Serialization(_)
                | GraphNoteError::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
            },
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
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

// ---------------------------------------------------------------------
// Node CRUD handlers (task 00038)
// ---------------------------------------------------------------------

/// Query parameters accepted by `GET /nodes`.
///
/// `kind` is mandatory because list queries are always scoped to a
/// single node kind — the underlying index is `node_kind:{kind}:`.
#[derive(Debug, Deserialize)]
pub struct ListNodesQuery {
    /// Node kind to filter by (required).
    pub kind: Option<String>,
    /// Maximum number of nodes to return. Defaults to 50, max 1000.
    pub limit: Option<usize>,
    /// Number of matching nodes to skip for pagination. Defaults to 0.
    pub offset: Option<usize>,
}

/// JSON envelope for node list responses.
#[derive(Debug, Serialize)]
pub struct NodeListResponse {
    /// The matched nodes, at most `limit` items.
    pub nodes: Vec<Node>,
}

/// Handler for `POST /nodes`. Creates a new node from a JSON
/// [`NewNode`] body and returns the stored node with generated id,
/// uuid, and timestamps.
async fn create_node(
    State(state): State<ApiState>,
    body: Result<Json<NewNode>, JsonRejection>,
) -> Result<(StatusCode, Json<Node>), ApiError> {
    let Json(new_node) = body?;
    let node = state.db.create_node(new_node)?;
    Ok((StatusCode::CREATED, Json(node)))
}

/// Handler for `GET /nodes/{id}`. Returns the node or 404 if missing.
async fn get_node(
    State(state): State<ApiState>,
    Path(id): Path<u64>,
) -> Result<Json<Node>, ApiError> {
    let node = state
        .db
        .get_node(id)?
        .ok_or(GraphNoteError::NodeNotFound(id))?;
    Ok(Json(node))
}

/// Handler for `PATCH /nodes/{id}`. Applies a partial update via
/// [`NodePatch`] and returns the updated node.
async fn update_node(
    State(state): State<ApiState>,
    Path(id): Path<u64>,
    body: Result<Json<NodePatch>, JsonRejection>,
) -> Result<Json<Node>, ApiError> {
    let Json(patch) = body?;
    let node = state.db.update_node(id, patch)?;
    Ok(Json(node))
}

/// Handler for `DELETE /nodes/{id}`. Returns 204 on success or 404
/// if the node does not exist.
async fn delete_node(
    State(state): State<ApiState>,
    Path(id): Path<u64>,
) -> Result<StatusCode, ApiError> {
    state.db.delete_node(id)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Handler for `GET /nodes`. Lists nodes filtered by `kind` with
/// pagination. A missing `kind` parameter yields 400 Bad Request.
async fn list_nodes(
    State(state): State<ApiState>,
    query: Result<Query<ListNodesQuery>, QueryRejection>,
) -> Result<Json<NodeListResponse>, ApiError> {
    let Query(ListNodesQuery {
        kind,
        limit,
        offset,
    }) = query?;
    let kind =
        kind.ok_or_else(|| ApiError::BadRequest("query parameter 'kind' is required".to_string()))?;
    let limit = limit.unwrap_or(50).min(1000);
    let offset = offset.unwrap_or(0);
    let nodes = state.db.list_nodes_by_kind(&kind, limit, offset)?;
    Ok(Json(NodeListResponse { nodes }))
}

/// Build the HTTP [`Router`] for a given [`ApiState`].
///
/// Returned router can be served with `axum::serve` on a TCP listener
/// or driven directly in tests via [`tower::ServiceExt::oneshot`].
pub fn build_router(state: ApiState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/nodes", get(list_nodes).post(create_node))
        .route(
            "/nodes/{id}",
            get(get_node).patch(update_node).delete(delete_node),
        )
        .with_state(state)
}
