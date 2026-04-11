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
//! Task 00039 added edge endpoints:
//!
//! - `POST /edges` — create an edge
//! - `GET /edges?kind=&limit=&offset=` — list edges filtered by kind
//! - `GET /edges/{id}` — fetch an edge by id
//! - `DELETE /edges/{id}` — delete an edge
//! - `GET /nodes/{id}/edges?direction=outgoing|incoming|both` —
//!   edges incident to a node (default: both)
//!
//! Task 00040 added graph traversal endpoints:
//!
//! - `GET /nodes/{id}/neighbors?direction=&kind=&depth=` — BFS-based
//!   neighbor discovery (default depth 1, direction both)
//! - `GET /paths/shortest?from=&to=` — Dijkstra shortest path as an
//!   ordered list of node ids
//! - `GET /nodes/{id}/subgraph?depth=` — bounded subgraph extraction
//!   (default depth 1)
//!
//! Task 00041 added the full-text search endpoint:
//!
//! - `POST /search/fts` — JSON body `{query, limit?}`, returns
//!   `{results: [ScoredNode]}` ranked by TF-IDF
//!
//! Admin endpoints arrive in task 00042. The whole module is gated
//! behind the `http` feature so that WebAssembly builds
//! (`--no-default-features --features wasm`) are unaffected.

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
use crate::model::{Direction, Edge, NewEdge, NewNode, Node, NodePatch, ScoredNode, SubGraph};

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

// ---------------------------------------------------------------------
// Edge endpoints (task 00039)
// ---------------------------------------------------------------------

/// Query parameters accepted by `GET /edges`.
///
/// Mirrors [`ListNodesQuery`]: `kind` is mandatory, `limit`/`offset`
/// are optional with the same defaults and cap.
#[derive(Debug, Deserialize)]
pub struct ListEdgesQuery {
    /// Edge kind to filter by (required).
    pub kind: Option<String>,
    /// Maximum number of edges to return. Defaults to 50, max 1000.
    pub limit: Option<usize>,
    /// Number of matching edges to skip for pagination. Defaults to 0.
    pub offset: Option<usize>,
}

/// Query parameters accepted by `GET /nodes/{id}/edges`.
///
/// `direction` is optional — when absent, the handler defaults to
/// [`Direction::Both`]. Accepted values (case-insensitive): `outgoing`,
/// `incoming`, `both`.
#[derive(Debug, Deserialize)]
pub struct NodeEdgesQuery {
    /// Traversal direction relative to the node. Optional.
    pub direction: Option<String>,
}

/// JSON envelope for edge list responses.
#[derive(Debug, Serialize)]
pub struct EdgeListResponse {
    /// The matched edges.
    pub edges: Vec<Edge>,
}

/// Handler for `POST /edges`. Creates a new edge from a JSON
/// [`NewEdge`] body and returns the stored edge. Returns 404 if either
/// endpoint node does not exist.
async fn create_edge(
    State(state): State<ApiState>,
    body: Result<Json<NewEdge>, JsonRejection>,
) -> Result<(StatusCode, Json<Edge>), ApiError> {
    let Json(new_edge) = body?;
    let edge = state.db.create_edge(new_edge)?;
    Ok((StatusCode::CREATED, Json(edge)))
}

/// Handler for `GET /edges/{id}`. Returns the edge or 404 if missing.
async fn get_edge(
    State(state): State<ApiState>,
    Path(id): Path<u64>,
) -> Result<Json<Edge>, ApiError> {
    let edge = state
        .db
        .get_edge(id)?
        .ok_or(GraphNoteError::EdgeNotFound(id))?;
    Ok(Json(edge))
}

/// Handler for `DELETE /edges/{id}`. Returns 204 on success or 404
/// if the edge does not exist.
async fn delete_edge(
    State(state): State<ApiState>,
    Path(id): Path<u64>,
) -> Result<StatusCode, ApiError> {
    state.db.delete_edge(id)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Handler for `GET /edges`. Lists edges filtered by `kind` with
/// pagination. A missing `kind` parameter yields 400 Bad Request.
async fn list_edges(
    State(state): State<ApiState>,
    query: Result<Query<ListEdgesQuery>, QueryRejection>,
) -> Result<Json<EdgeListResponse>, ApiError> {
    let Query(ListEdgesQuery {
        kind,
        limit,
        offset,
    }) = query?;
    let kind =
        kind.ok_or_else(|| ApiError::BadRequest("query parameter 'kind' is required".to_string()))?;
    let limit = limit.unwrap_or(50).min(1000);
    let offset = offset.unwrap_or(0);
    let edges = state.db.list_edges_by_kind(&kind, limit, offset)?;
    Ok(Json(EdgeListResponse { edges }))
}

/// Handler for `GET /nodes/{id}/edges`. Returns all edges incident to
/// the node in the given direction (default: both). Unknown direction
/// values yield 400 Bad Request.
async fn get_node_edges(
    State(state): State<ApiState>,
    Path(id): Path<u64>,
    query: Result<Query<NodeEdgesQuery>, QueryRejection>,
) -> Result<Json<EdgeListResponse>, ApiError> {
    let Query(NodeEdgesQuery { direction }) = query?;
    let direction = parse_direction(direction.as_deref())?;
    let edges = state.db.edges_of(id, direction)?;
    Ok(Json(EdgeListResponse { edges }))
}

// ---------------------------------------------------------------------
// Traversal endpoints (task 00040)
// ---------------------------------------------------------------------

/// Default depth used when `GET /nodes/{id}/neighbors` omits the
/// `depth` query parameter.
const DEFAULT_NEIGHBORS_DEPTH: u8 = 1;

/// Default depth used when `GET /nodes/{id}/subgraph` omits the
/// `depth` query parameter.
const DEFAULT_SUBGRAPH_DEPTH: u8 = 1;

/// Query parameters accepted by `GET /nodes/{id}/neighbors`.
///
/// All parameters are optional. `direction` defaults to
/// [`Direction::Both`], `depth` defaults to
/// [`DEFAULT_NEIGHBORS_DEPTH`], and `kind` is an optional edge kind
/// filter passed straight through to the traversal layer.
#[derive(Debug, Deserialize)]
pub struct NeighborsQuery {
    /// Traversal direction relative to the start node.
    pub direction: Option<String>,
    /// Optional edge kind filter.
    pub kind: Option<String>,
    /// BFS depth. Defaults to 1.
    pub depth: Option<u8>,
}

/// Query parameters accepted by `GET /paths/shortest`.
///
/// Both `from` and `to` are required node ids. A missing parameter
/// yields a 400 response.
#[derive(Debug, Deserialize)]
pub struct ShortestPathQuery {
    /// Source node id (required).
    pub from: Option<u64>,
    /// Target node id (required).
    pub to: Option<u64>,
}

/// Query parameters accepted by `GET /nodes/{id}/subgraph`.
///
/// Only `depth` is configurable. Defaults to
/// [`DEFAULT_SUBGRAPH_DEPTH`].
#[derive(Debug, Deserialize)]
pub struct SubgraphQuery {
    /// Traversal depth. Defaults to 1.
    pub depth: Option<u8>,
}

/// JSON envelope for the shortest-path endpoint. `path` is `null` when
/// the target is unreachable from the source.
#[derive(Debug, Serialize)]
pub struct ShortestPathResponse {
    /// The sequence of node ids from source to target, or `null` if
    /// unreachable.
    pub path: Option<Vec<u64>>,
}

/// Handler for `GET /nodes/{id}/neighbors`. Returns nodes reachable
/// from `id` via BFS with a configurable direction, edge-kind filter,
/// and depth. Returns 404 if the start node does not exist so that
/// callers can distinguish "node has no neighbors" from "node doesn't
/// exist".
async fn get_node_neighbors(
    State(state): State<ApiState>,
    Path(id): Path<u64>,
    query: Result<Query<NeighborsQuery>, QueryRejection>,
) -> Result<Json<NodeListResponse>, ApiError> {
    let Query(NeighborsQuery {
        direction,
        kind,
        depth,
    }) = query?;
    let direction = parse_direction(direction.as_deref())?;
    let depth = depth.unwrap_or(DEFAULT_NEIGHBORS_DEPTH);

    // Explicitly surface missing nodes as 404 — the underlying `bfs`
    // would otherwise silently return an empty list.
    if state.db.get_node(id)?.is_none() {
        return Err(GraphNoteError::NodeNotFound(id).into());
    }

    let nodes = state.db.bfs(id, depth, direction, kind.as_deref())?;
    Ok(Json(NodeListResponse { nodes }))
}

/// Handler for `GET /paths/shortest`. Runs Dijkstra over outgoing
/// edges. Both endpoints must exist — missing nodes yield 404. An
/// unreachable target produces a 200 response with `{"path": null}`
/// so that clients can distinguish "no such node" from "no route".
async fn get_shortest_path(
    State(state): State<ApiState>,
    query: Result<Query<ShortestPathQuery>, QueryRejection>,
) -> Result<Json<ShortestPathResponse>, ApiError> {
    let Query(ShortestPathQuery { from, to }) = query?;
    let from =
        from.ok_or_else(|| ApiError::BadRequest("query parameter 'from' is required".to_string()))?;
    let to =
        to.ok_or_else(|| ApiError::BadRequest("query parameter 'to' is required".to_string()))?;

    // Validate both endpoints up front so we can return 404 instead
    // of silently returning `None` (which means "unreachable").
    if state.db.get_node(from)?.is_none() {
        return Err(GraphNoteError::NodeNotFound(from).into());
    }
    if state.db.get_node(to)?.is_none() {
        return Err(GraphNoteError::NodeNotFound(to).into());
    }

    let path = state.db.shortest_path(from, to)?;
    Ok(Json(ShortestPathResponse { path }))
}

/// Handler for `GET /nodes/{id}/subgraph`. Extracts the subgraph of
/// all nodes and edges within `depth` hops of the root. Returns 404
/// if the root does not exist (the underlying traversal already maps
/// that case to `NodeNotFound`).
async fn get_node_subgraph(
    State(state): State<ApiState>,
    Path(id): Path<u64>,
    query: Result<Query<SubgraphQuery>, QueryRejection>,
) -> Result<Json<SubGraph>, ApiError> {
    let Query(SubgraphQuery { depth }) = query?;
    let depth = depth.unwrap_or(DEFAULT_SUBGRAPH_DEPTH);
    let sub = state.db.subgraph(id, depth)?;
    Ok(Json(sub))
}

// ---------------------------------------------------------------------
// Search endpoint (task 00041)
// ---------------------------------------------------------------------

/// Default `limit` applied to `POST /search/fts` when the client omits
/// it. Matches the node/edge list defaults to keep the API consistent.
const DEFAULT_SEARCH_LIMIT: usize = 10;

/// Maximum `limit` accepted by `POST /search/fts`. Requests above this
/// cap are silently clamped so that a pathological client cannot force
/// a huge scoring pass.
const MAX_SEARCH_LIMIT: usize = 1000;

/// JSON body for `POST /search/fts`.
///
/// `query` is required — a missing field yields 400 Bad Request. An
/// empty string is accepted but produces no results, mirroring the
/// underlying [`GraphNoteDb::search_fts`] behaviour. `limit` is
/// optional and defaults to [`DEFAULT_SEARCH_LIMIT`].
#[derive(Debug, Deserialize)]
pub struct SearchFtsRequest {
    /// Raw query text (required).
    pub query: Option<String>,
    /// Maximum number of results to return. Defaults to 10, capped at
    /// [`MAX_SEARCH_LIMIT`].
    pub limit: Option<usize>,
}

/// JSON envelope for `POST /search/fts` responses.
#[derive(Debug, Serialize)]
pub struct SearchFtsResponse {
    /// Scored nodes ranked by descending TF-IDF score.
    pub results: Vec<ScoredNode>,
}

/// Handler for `POST /search/fts`. Runs TF-IDF ranked full-text search
/// over the node title/body trigram index and returns up to `limit`
/// scored matches. A missing `query` field is rejected with 400.
async fn search_fts(
    State(state): State<ApiState>,
    body: Result<Json<SearchFtsRequest>, JsonRejection>,
) -> Result<Json<SearchFtsResponse>, ApiError> {
    let Json(SearchFtsRequest { query, limit }) = body?;
    let query =
        query.ok_or_else(|| ApiError::BadRequest("field 'query' is required".to_string()))?;
    let limit = limit.unwrap_or(DEFAULT_SEARCH_LIMIT).min(MAX_SEARCH_LIMIT);
    let results = state.db.search_fts(&query, limit)?;
    Ok(Json(SearchFtsResponse { results }))
}

/// Parse the `direction` query parameter into a [`Direction`].
///
/// Accepts `outgoing`, `incoming`, `both` (case-insensitive). `None`
/// defaults to [`Direction::Both`]. Any other value yields a
/// [`ApiError::BadRequest`].
fn parse_direction(value: Option<&str>) -> Result<Direction, ApiError> {
    let lowered = value.map(str::to_ascii_lowercase);
    match lowered.as_deref() {
        None | Some("both") => Ok(Direction::Both),
        Some("outgoing") => Ok(Direction::Outgoing),
        Some("incoming") => Ok(Direction::Incoming),
        Some(other) => Err(ApiError::BadRequest(format!(
            "invalid direction '{other}', expected one of: outgoing, incoming, both"
        ))),
    }
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
        .route("/nodes/{id}/edges", get(get_node_edges))
        .route("/nodes/{id}/neighbors", get(get_node_neighbors))
        .route("/nodes/{id}/subgraph", get(get_node_subgraph))
        .route("/edges", get(list_edges).post(create_edge))
        .route("/edges/{id}", get(get_edge).delete(delete_edge))
        .route("/paths/shortest", get(get_shortest_path))
        .route("/search/fts", axum::routing::post(search_fts))
        .with_state(state)
}
