//! HTTP API for drevo.
//!
//! This module exposes a thin JSON adapter over [`crate::db::Drevo`] built on
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
//! - `PATCH /edges/{id}` — partial update (task 00046)
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
//!   `{results: [ScoredNode]}` ranked by Okapi BM25 (task `00131`)
//!
//! Task 00055 (Phase 9 hardening) added JSON import / export endpoints
//! for backups and cross-deployment migration:
//!
//! - `GET /export/json` — full graph as `drevo-json-v1` document.
//! - `POST /import/json` — JSON body `{dump}`, returns
//!   `{nodes_imported, edges_imported, nodes_skipped, edges_skipped}`.
//!
//! Task 00056 (Phase 9 hardening) added the GraphML interop export:
//!
//! - `GET /export/graphml` — full graph as a GraphML 1.0 document
//!   (`application/xml`). Read-only — there is no `import_graphml`
//!   companion; `drevo-json-v1` remains the authoritative wire format.
//!
//! Task 00042 added the admin endpoints used by container liveness
//! probes and operators:
//!
//! - `GET /health` — cheap liveness probe, returns `{"status": "ok"}`
//!   while the process is serving and `{"status": "shutting_down"}`
//!   with HTTP 503 once graceful shutdown has been signalled (task
//!   00048).
//! - `GET /status` — server metadata including crate name, version,
//!   and process uptime in seconds since the [`crate::api::ApiState`] was built.
//!
//! Task 00048 added the readiness probe and made liveness shutdown-
//! aware so the standalone server binary cooperates correctly with
//! Kubernetes-style rolling restarts:
//!
//! - `GET /ready` — readiness probe that actively exercises the
//!   storage backend via [`crate::db::Drevo::health_check`]. Returns 200 with
//!   `{"status": "ready"}` while the DB is responsive and 503
//!   otherwise.
//!
//! The whole module is gated behind the `http` feature so that
//! WebAssembly builds (`--no-default-features --features wasm`) are
//! unaffected.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Json;
use axum::Router;
use serde::{Deserialize, Serialize};

use crate::db::Drevo;
use crate::dump::ImportReport;
use crate::error::DrevoError;
use crate::fts::facet::{Facet, FacetCollapse, DEFAULT_TRIGRAM_THRESHOLD};
use crate::model::{
    Direction, Edge, EdgePatch, NewEdge, NewNode, Node, NodePatch, ScoredNode, SubGraph,
};
use crate::observability::DrevoMetrics;

/// Shared application state passed to every HTTP handler.
///
/// Wraps a reference-counted [`Drevo`] so the same database
/// instance is shared across all requests without locking at the
/// router level. The database itself is `Send + Sync` because the
/// underlying `StorageBackend` is.
#[derive(Clone)]
pub struct ApiState {
    /// The shared database handle.
    pub db: Arc<Drevo>,
    /// Wall-clock instant at which this state was constructed. Used by
    /// `GET /status` to compute the process uptime without pulling in
    /// a system-time crate.
    pub started_at: Instant,
    /// Shared "graceful shutdown in progress" flag. Cloned `ApiState`
    /// instances (axum hands one per handler invocation) point at the
    /// same atomic so that flipping the flag from any task is visible
    /// to every other handler. `/health` and `/ready` consult it to
    /// return 503 once the process enters draining.
    shutting_down: Arc<AtomicBool>,
    /// Process metrics (Phase 15 task `00130`). Shared across every handler
    /// (and the request-instrumentation middleware) so request counts,
    /// latencies, and in-flight gauges accumulate into one registry that the
    /// `GET /metrics` route renders in the Prometheus exposition format.
    pub metrics: Arc<DrevoMetrics>,
}

impl ApiState {
    /// Create a new [`ApiState`] from an existing database handle. The
    /// `started_at` timestamp is captured at construction time so that
    /// `GET /status` can report how long this API instance has been
    /// serving traffic.
    pub fn new(db: Arc<Drevo>) -> Self {
        Self {
            db,
            started_at: Instant::now(),
            shutting_down: Arc::new(AtomicBool::new(false)),
            metrics: Arc::new(DrevoMetrics::new()),
        }
    }

    /// Mark the API as draining.
    ///
    /// Called once by the server entry point as soon as SIGTERM or
    /// Ctrl+C is observed, before `axum::serve` finishes the
    /// in-flight requests. Subsequent calls to `/health` and `/ready`
    /// return 503 so that the orchestrator (Kubernetes, Docker Swarm,
    /// Nomad) removes this pod from the load-balancer rotation
    /// before SIGKILL lands. Idempotent — multiple signals that land
    /// in quick succession are safe.
    pub fn signal_shutdown(&self) {
        // Release pairs with Acquire in `is_shutting_down` — that is
        // all we need: a single boolean transition that becomes
        // visible to every reader. No total order across other atomics
        // is required.
        self.shutting_down.store(true, Ordering::Release);
    }

    /// Returns `true` after [`signal_shutdown`](Self::signal_shutdown)
    /// has been called on this state (or any clone of it).
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }
}

/// Unified error type returned by every HTTP handler.
///
/// Wraps either a [`DrevoError`] (producing a status code based on
/// the underlying database error) or a bad-request variant for client
/// input problems (malformed JSON body, missing query parameter).
pub enum ApiError {
    /// A database operation failed.
    Db(DrevoError),
    /// The client sent an invalid request (400 Bad Request).
    BadRequest(String),
}

impl From<DrevoError> for ApiError {
    fn from(err: DrevoError) -> Self {
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
                DrevoError::NodeNotFound(_) | DrevoError::EdgeNotFound(_) => {
                    (StatusCode::NOT_FOUND, err.to_string())
                }
                DrevoError::DuplicateTitle(_) => (StatusCode::CONFLICT, err.to_string()),
                DrevoError::InvalidWeight(_) | DrevoError::Vector(_) => {
                    (StatusCode::BAD_REQUEST, err.to_string())
                }
                DrevoError::Locked | DrevoError::TransactionAlreadyActive => {
                    (StatusCode::SERVICE_UNAVAILABLE, err.to_string())
                }
                DrevoError::NoActiveTransaction => (StatusCode::CONFLICT, err.to_string()),
                DrevoError::Storage(_)
                | DrevoError::Encode(_)
                | DrevoError::Decode(_)
                | DrevoError::Json(_)
                | DrevoError::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
            },
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
        };
        json_error(status, &message)
    }
}

/// Build a unified JSON error response with both `error` (message) and
/// `status` (numeric HTTP status code) fields.  Every error response
/// produced by the API goes through this helper so that clients can
/// programmatically inspect the body without relying on HTTP status
/// alone.
fn json_error(status: StatusCode, message: &str) -> Response {
    let body = Json(serde_json::json!({
        "error": message,
        "status": status.as_u16(),
    }));
    (status, body).into_response()
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
        name: "drevo",
        version: env!("CARGO_PKG_VERSION"),
    })
}

// ---------------------------------------------------------------------
// Shared list-handler defaults (task 00109 audit fix — F3)
// ---------------------------------------------------------------------

/// Default `limit` applied to `GET /nodes` and `GET /edges` when the
/// client omits the query parameter. Kept in lockstep with
/// [`DEFAULT_SEARCH_LIMIT`] for predictable client behaviour across
/// endpoints.
pub const DEFAULT_LIST_LIMIT: usize = 50;

/// Maximum `limit` accepted by `GET /nodes` and `GET /edges`. Requests
/// above this cap are silently clamped so that a pathological client
/// cannot force an unbounded scan over the kind index — the same
/// rationale as [`MAX_SEARCH_LIMIT`] for the FTS endpoint.
pub const MAX_LIST_LIMIT: usize = 1000;

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
    /// Maximum number of nodes to return. Defaults to
    /// [`DEFAULT_LIST_LIMIT`], clamped at [`MAX_LIST_LIMIT`].
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
    let node = state.db.get_node(id)?.ok_or(DrevoError::NodeNotFound(id))?;
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
    let limit = limit.unwrap_or(DEFAULT_LIST_LIMIT).min(MAX_LIST_LIMIT);
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
    /// Maximum number of edges to return. Defaults to
    /// [`DEFAULT_LIST_LIMIT`], clamped at [`MAX_LIST_LIMIT`].
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
    let edge = state.db.get_edge(id)?.ok_or(DrevoError::EdgeNotFound(id))?;
    Ok(Json(edge))
}

/// Handler for `PATCH /edges/{id}`. Applies a partial update via
/// [`EdgePatch`] and returns the updated edge.
async fn update_edge(
    State(state): State<ApiState>,
    Path(id): Path<u64>,
    body: Result<Json<EdgePatch>, JsonRejection>,
) -> Result<Json<Edge>, ApiError> {
    let Json(patch) = body?;
    let edge = state.db.update_edge(id, patch)?;
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
    let limit = limit.unwrap_or(DEFAULT_LIST_LIMIT).min(MAX_LIST_LIMIT);
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
pub const DEFAULT_NEIGHBORS_DEPTH: u8 = 1;

/// Default depth used when `GET /nodes/{id}/subgraph` omits the
/// `depth` query parameter.
pub const DEFAULT_SUBGRAPH_DEPTH: u8 = 1;

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
        return Err(DrevoError::NodeNotFound(id).into());
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
        return Err(DrevoError::NodeNotFound(from).into());
    }
    if state.db.get_node(to)?.is_none() {
        return Err(DrevoError::NodeNotFound(to).into());
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
pub const DEFAULT_SEARCH_LIMIT: usize = 10;

/// Maximum `limit` accepted by `POST /search/fts`. Requests above this
/// cap are silently clamped so that a pathological client cannot force
/// a huge scoring pass.
pub const MAX_SEARCH_LIMIT: usize = 1000;

/// JSON body for `POST /search/fts`.
///
/// `query` is required — a missing field yields 400 Bad Request. An
/// empty string is accepted but produces no results, mirroring the
/// underlying [`Drevo::search_fts`] behaviour. `limit` is
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
    /// Scored nodes ranked by descending BM25 score.
    pub results: Vec<ScoredNode>,
}

/// Handler for `POST /search/fts`. Runs BM25-ranked full-text search
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

// ---------------------------------------------------------------------
// Keyword faceting (task 00133)
// ---------------------------------------------------------------------

/// Query parameters accepted by `GET /facets`.
///
/// `kind` is mandatory; everything else has a default. `collapse` selects
/// the keyword-similarity axis: `none` (default), `lexical`, or `semantic`.
/// Semantic collapse needs an embedder, which the HTTP server does not host
/// — it is rejected with 400; use the Rust/Python API with precomputed
/// embeddings for the semantic axis.
#[derive(Debug, Deserialize)]
pub struct FacetsQuery {
    /// Node classification to scan (required).
    pub kind: Option<String>,
    /// Source text field: `title`, `body` (default), or a property key.
    pub property: Option<String>,
    /// Keywords extracted per node before collapsing. Defaults to
    /// [`DEFAULT_FACET_KEYWORDS`], capped at [`MAX_FACET_KEYWORDS`].
    pub k: Option<usize>,
    /// Collapse axis: `none` (default) | `lexical` | `semantic`.
    pub collapse: Option<String>,
    /// Trigram-Jaccard threshold for `collapse=lexical`
    /// (default [`crate::fts::facet::DEFAULT_TRIGRAM_THRESHOLD`]).
    pub threshold: Option<f32>,
}

/// Default number of keywords extracted per node for faceting.
pub const DEFAULT_FACET_KEYWORDS: usize = 5;

/// Upper bound on the per-node keyword count for faceting.
pub const MAX_FACET_KEYWORDS: usize = 50;

/// JSON envelope for `GET /facets` responses.
#[derive(Debug, Serialize)]
pub struct FacetsResponse {
    /// Facets sorted by descending document count, then label.
    pub facets: Vec<Facet>,
}

/// Handler for `GET /facets?kind=&property=&k=&collapse=&threshold=`.
///
/// Groups every node of `kind` by the keywords extracted from `property`,
/// optionally collapsing near-duplicate keywords (lexical axis). Returns
/// `{facets: [{facet, members, count}]}`.
async fn facets(
    State(state): State<ApiState>,
    query: Result<Query<FacetsQuery>, QueryRejection>,
) -> Result<Json<FacetsResponse>, ApiError> {
    let Query(FacetsQuery {
        kind,
        property,
        k,
        collapse,
        threshold,
    }) = query?;
    let kind =
        kind.ok_or_else(|| ApiError::BadRequest("query parameter 'kind' is required".to_string()))?;
    let property = property.unwrap_or_else(|| "body".to_string());
    let k = k.unwrap_or(DEFAULT_FACET_KEYWORDS).min(MAX_FACET_KEYWORDS);

    let collapse = match collapse.as_deref().unwrap_or("none") {
        "none" => FacetCollapse::None,
        "lexical" => FacetCollapse::Lexical {
            trigram_threshold: threshold.unwrap_or(DEFAULT_TRIGRAM_THRESHOLD),
        },
        "semantic" => {
            return Err(ApiError::BadRequest(
                "collapse=semantic requires an embedder, which is not configured on the HTTP \
                 server; use the Rust/Python API with precomputed keyword embeddings"
                    .to_string(),
            ))
        }
        other => {
            return Err(ApiError::BadRequest(format!(
                "unknown collapse mode '{other}' (expected none|lexical|semantic)"
            )))
        }
    };

    let facets = state.db.facets(&kind, &property, k, &collapse)?;
    Ok(Json(FacetsResponse { facets }))
}

// ---------------------------------------------------------------------
// Admin endpoints (task 00042)
// ---------------------------------------------------------------------

/// Status values reported by `GET /health` and `GET /ready`. Serialized
/// as a lowercase string so the JSON payload stays the same as before
/// (`"ok"`, `"ready"`, `"shutting_down"`).
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    /// Liveness — the HTTP task is serving requests.
    Ok,
    /// Readiness — the HTTP task and the storage backend are both
    /// responsive.
    Ready,
    /// The process has received SIGTERM/Ctrl+C and is draining.
    ShuttingDown,
}

/// JSON body returned by `GET /health` and `GET /ready`. The numeric
/// HTTP status code conveys success (200) vs. drained/unhealthy (503).
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    /// Status marker — see [`HealthStatus`].
    pub status: HealthStatus,
}

/// JSON body returned by `GET /status`.
///
/// Carries the same `name`/`version` pair as `GET /` plus an
/// `uptime_seconds` field that reports how long the current
/// [`ApiState`] has been alive. Clients can use this for basic
/// observability and sanity checks after a restart.
#[derive(Debug, Serialize)]
pub struct StatusResponse {
    /// Crate name — same value as [`ServerInfo::name`].
    pub name: &'static str,
    /// Crate version — same value as [`ServerInfo::version`].
    pub version: &'static str,
    /// Seconds elapsed since the [`ApiState`] was constructed.
    pub uptime_seconds: u64,
}

/// 503 response emitted by both `/health` and `/ready` during
/// graceful shutdown.
fn shutting_down_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(HealthResponse {
            status: HealthStatus::ShuttingDown,
        }),
    )
        .into_response()
}

/// Handler for `GET /health` — Kubernetes-style liveness probe. Stays
/// dependency-free so DB contention does not trigger a pod restart.
async fn health(State(state): State<ApiState>) -> Response {
    if state.is_shutting_down() {
        return shutting_down_response();
    }
    Json(HealthResponse {
        status: HealthStatus::Ok,
    })
    .into_response()
}

/// Handler for `GET /ready` — Kubernetes-style readiness probe.
/// Actively exercises the storage backend via
/// [`Drevo::health_check`]; returns 503 if the probe fails or the
/// process is draining.
async fn ready(State(state): State<ApiState>) -> Response {
    if state.is_shutting_down() {
        return shutting_down_response();
    }
    match state.db.health_check() {
        Ok(()) => Json(HealthResponse {
            status: HealthStatus::Ready,
        })
        .into_response(),
        Err(err) => json_error(StatusCode::SERVICE_UNAVAILABLE, &err.to_string()),
    }
}

// ---------------------------------------------------------------------
// Export / Import endpoints (task 00055)
// ---------------------------------------------------------------------

/// JSON body accepted by `POST /import/json`. Carries the raw dump produced
/// by `GET /export/json` (or [`Drevo::export_json`]) — the server parses,
/// validates the format header, and replays the payload into the live
/// database.
#[derive(Debug, Deserialize)]
pub struct ImportJsonRequest {
    /// Raw `drevo-json-v1` payload — the full output of `GET /export/json`.
    pub dump: String,
}

/// Handler for `GET /export/json`. Streams the full graph as a pretty-printed
/// `drevo-json-v1` JSON document. Operators can curl this for backups or to
/// migrate data between deployments.
async fn export_json(State(state): State<ApiState>) -> Result<Response, ApiError> {
    let dump = state.db.export_json()?;
    Ok((StatusCode::OK, [("content-type", "application/json")], dump).into_response())
}

/// Handler for `POST /import/json`. Accepts an [`ImportJsonRequest`] body and
/// returns an [`ImportReport`] summarising newly-inserted vs. skipped rows.
/// Malformed payloads / unknown formats return 500 via [`DrevoError::Io`];
/// title collisions against existing nodes return 409.
async fn import_json(
    State(state): State<ApiState>,
    body: Result<Json<ImportJsonRequest>, JsonRejection>,
) -> Result<Json<ImportReport>, ApiError> {
    let Json(req) = body?;
    let report = state.db.import_json(&req.dump)?;
    Ok(Json(report))
}

/// Handler for `GET /export/graphml`. Returns the full graph as a GraphML 1.0
/// document (`application/xml`) — for interop with yEd, Gephi, NetworkX,
/// Cytoscape, igraph, and friends. Read-only — no inverse endpoint.
async fn export_graphml(State(state): State<ApiState>) -> Result<Response, ApiError> {
    let xml = state.db.export_graphml()?;
    Ok((
        StatusCode::OK,
        [("content-type", "application/xml; charset=utf-8")],
        xml,
    )
        .into_response())
}

/// Handler for `GET /status`. Returns server metadata and the current
/// process uptime derived from [`ApiState::started_at`].
async fn status(State(state): State<ApiState>) -> Json<StatusResponse> {
    let uptime_seconds = state.started_at.elapsed().as_secs();
    Json(StatusResponse {
        name: "drevo",
        version: env!("CARGO_PKG_VERSION"),
        uptime_seconds,
    })
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

/// Handler for unknown routes — returns a JSON 404 so that API
/// consumers always receive structured error responses rather than
/// axum's default empty body.
async fn fallback() -> Response {
    json_error(StatusCode::NOT_FOUND, "not found")
}

/// Method-not-allowed fallback for known paths. Axum invokes this
/// when a request reaches a registered path but uses an unregistered
/// HTTP method (e.g. `PUT /nodes`).
async fn method_not_allowed() -> Response {
    json_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
}

/// Helper macro to attach the 405 JSON fallback to a
/// [`MethodRouter`]. Each known path needs this so that unsupported
/// methods return a JSON body instead of axum's default empty 405.
fn with_405<S: Clone + Send + Sync + 'static>(
    mr: axum::routing::MethodRouter<S>,
) -> axum::routing::MethodRouter<S> {
    mr.fallback(method_not_allowed)
}

/// The Prometheus text exposition `Content-Type` (version `0.0.4`). Scrapers
/// key off this media type, so it must be exact.
const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// `GET /metrics` — render the process metrics in the Prometheus text
/// exposition format (Phase 15 task `00130`).
///
/// The process-uptime gauge is refreshed from [`ApiState::started_at`] just
/// before rendering so the scrape reflects the current uptime without a
/// background ticker. Always returns 200 with `text/plain; version=0.0.4`.
async fn metrics(State(state): State<ApiState>) -> Response {
    state
        .metrics
        .uptime_seconds
        .set(state.started_at.elapsed().as_secs() as i64);
    let body = state.metrics.render_prometheus();
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, PROMETHEUS_CONTENT_TYPE)],
        body,
    )
        .into_response()
}

/// Per-request instrumentation middleware (Phase 15 task `00130`).
///
/// Increments the in-flight gauge for the duration of the request, times the
/// downstream handler, and records the response status class + latency into the
/// shared [`DrevoMetrics`]. Runs for every route (including `/metrics` itself,
/// so scrapes are visible in the request totals — the conventional behaviour).
async fn track_metrics(
    State(state): State<ApiState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    state.metrics.request_started();
    let start = Instant::now();
    let response = next.run(request).await;
    let elapsed = start.elapsed().as_secs_f64();
    state
        .metrics
        .record_http(response.status().as_u16(), elapsed);
    state.metrics.request_finished();
    response
}

/// Build the HTTP [`Router`] for a given [`ApiState`].
///
/// Returned router can be served with `axum::serve` on a TCP listener
/// or driven directly in tests via [`tower::ServiceExt::oneshot`].
///
/// Unknown paths produce a JSON 404 via the fallback handler.
/// Unregistered methods on known paths produce a JSON 405 via per-route
/// fallbacks.
pub fn build_router(state: ApiState) -> Router {
    Router::new()
        .route("/", with_405(get(root)))
        .route("/nodes", with_405(get(list_nodes).post(create_node)))
        .route(
            "/nodes/{id}",
            with_405(get(get_node).patch(update_node).delete(delete_node)),
        )
        .route("/nodes/{id}/edges", with_405(get(get_node_edges)))
        .route("/nodes/{id}/neighbors", with_405(get(get_node_neighbors)))
        .route("/nodes/{id}/subgraph", with_405(get(get_node_subgraph)))
        .route("/edges", with_405(get(list_edges).post(create_edge)))
        .route(
            "/edges/{id}",
            with_405(get(get_edge).patch(update_edge).delete(delete_edge)),
        )
        .route("/paths/shortest", with_405(get(get_shortest_path)))
        .route("/search/fts", with_405(axum::routing::post(search_fts)))
        // ── Phase 17 task `00133` — keyword faceting endpoint ───────
        .route("/facets", with_405(get(facets)))
        .route("/export/json", with_405(get(export_json)))
        .route("/import/json", with_405(axum::routing::post(import_json)))
        .route("/export/graphml", with_405(get(export_graphml)))
        .route("/health", with_405(get(health)))
        .route("/ready", with_405(get(ready)))
        .route("/status", with_405(get(status)))
        // ── Phase 15 task `00130` — Prometheus metrics endpoint ─────
        .route("/metrics", with_405(get(metrics)))
        // ── Phase 15 task `00092` — embedded Web UI ─────────────────
        // Routes serve HTML / JS / CSS baked into the binary via
        // `include_str!` (see `crate::web_ui`). Same-origin with the
        // API above so the front-end's `fetch('/search/fts', …)` does
        // not need CORS.
        .route("/ui", get(crate::web_ui::serve_index))
        .route("/ui/", get(crate::web_ui::redirect_ui_slash))
        .route("/ui/app.js", get(crate::web_ui::serve_app_js))
        .route("/ui/styles.css", get(crate::web_ui::serve_styles_css))
        .fallback(fallback)
        // ── Phase 15 task `00130` — per-request metrics instrumentation.
        // Layered after the routes so it wraps every handler (including the
        // fallback) and before `with_state` so the middleware can extract the
        // shared `ApiState`.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            track_metrics,
        ))
        .with_state(state)
}

/// Regression test that locks in the `DrevoError → HTTP status` mapping
/// audited under task `00109`. Constructing the variants directly here
/// guarantees that adding a new [`DrevoError`] variant breaks **both** the
/// production match arm in [`ApiError::into_response`] **and** this test
/// at compile time — making the mapping a deliberate decision rather than
/// an accidental default.
#[cfg(test)]
mod error_mapping_tests {
    use super::*;
    use crate::storage::StorageError;

    fn status_of(err: DrevoError) -> StatusCode {
        let response = ApiError::Db(err).into_response();
        response.status()
    }

    #[test]
    fn apierror_maps_every_drevoerror_variant_to_expected_status() {
        assert_eq!(
            status_of(DrevoError::NodeNotFound(7)),
            StatusCode::NOT_FOUND,
            "NodeNotFound → 404",
        );
        assert_eq!(
            status_of(DrevoError::EdgeNotFound(7)),
            StatusCode::NOT_FOUND,
            "EdgeNotFound → 404",
        );
        assert_eq!(
            status_of(DrevoError::DuplicateTitle("dup".into())),
            StatusCode::CONFLICT,
            "DuplicateTitle → 409",
        );
        assert_eq!(
            status_of(DrevoError::InvalidWeight(f32::NAN)),
            StatusCode::BAD_REQUEST,
            "InvalidWeight → 400",
        );
        assert_eq!(
            status_of(DrevoError::Locked),
            StatusCode::SERVICE_UNAVAILABLE,
            "Locked → 503",
        );
        assert_eq!(
            status_of(DrevoError::Storage(StorageError::NotFound(b"k".to_vec()))),
            StatusCode::INTERNAL_SERVER_ERROR,
            "Storage → 500",
        );
        assert_eq!(
            status_of(DrevoError::Io(std::io::Error::other("boom"))),
            StatusCode::INTERNAL_SERVER_ERROR,
            "Io → 500",
        );
        // The `Encode` and `Decode` variants of `DrevoError` wrap bincode
        // error types that cannot be constructed outside the bincode
        // crate. They share the `Storage / Io` 500 arm in `api.rs`, so
        // the regression coverage above already exercises that arm — but
        // we still need a compile-time fence that fails when those
        // variants are removed or renamed. The `#[allow(dead_code)]`
        // match below is that fence: it has to mention each variant by
        // name, so adding/removing a variant breaks compilation here
        // **and** in `ApiError::into_response`.
        #[allow(dead_code)]
        fn variant_fence(err: &DrevoError) -> &'static str {
            match err {
                DrevoError::NodeNotFound(_) => "NodeNotFound",
                DrevoError::EdgeNotFound(_) => "EdgeNotFound",
                DrevoError::DuplicateTitle(_) => "DuplicateTitle",
                DrevoError::InvalidWeight(_) => "InvalidWeight",
                DrevoError::Locked => "Locked",
                DrevoError::Storage(_) => "Storage",
                DrevoError::Encode(_) => "Encode",
                DrevoError::Decode(_) => "Decode",
                DrevoError::Io(_) => "Io",
                DrevoError::Json(_) => "Json",
                DrevoError::TransactionAlreadyActive => "TransactionAlreadyActive",
                DrevoError::NoActiveTransaction => "NoActiveTransaction",
                DrevoError::Vector(_) => "Vector",
            }
        }
    }

    #[test]
    fn apierror_badrequest_returns_400_with_message() {
        let response = ApiError::BadRequest("missing query parameter".into()).into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
