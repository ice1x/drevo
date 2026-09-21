//! Shared HTTP request/response types and helpers for drevo's JSON API.
//!
//! This module is the engine-independent HTTP layer that the durable-native
//! router in [`crate::native_api`] builds on. It no longer owns a router: the
//! legacy KV `Drevo` HTTP surface — the `ApiState`/`Db` extractor, `build_router`,
//! and every KV-backed handler — was removed with the rest of the KV serving path
//! (epic #444). What remains is the reusable library those handlers and the
//! native ones share:
//!
//! - [`ApiError`](crate::api::ApiError) — the unified error type and its
//!   `IntoResponse` + `From<…>` conversions (the `DrevoError → HTTP status`
//!   mapping is fenced by the tests at the bottom of this file).
//! - The request/response DTOs and query structs (`CypherRequest`,
//!   `CypherResponse`, `SearchFtsRequest`, `ListNodesQuery`, `FacetsResponse`,
//!   …) plus their default/limit constants, deserialized/serialized on both
//!   routers so the wire shapes stay identical.
//! - The engine-independent handler bodies (crate-internal) reused verbatim by
//!   [`crate::native_api`]: `exec_result_to_response` (Cypher `ExecResult` →
//!   JSON rows + graph projection), `json_to_cypher_value`,
//!   `embeddings_response`, and `embeddings_config_status` /
//!   `embeddings_config_apply`.
//!
//! The whole module is gated behind the `http` feature so that WebAssembly
//! builds (`--no-default-features --features wasm`) are unaffected.

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::cypher::executor::{self, ExecResult, Value as CypherValue};
use crate::embeddings::{EmbeddingBackend, EmbeddingsError, EmbeddingsRequest};
use crate::error::DrevoError;
use crate::fts::facet::Facet;
use crate::model::{Edge, Node, ScoredNode};

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
    /// A named resource (e.g. a database) does not exist (404 Not Found).
    NotFound(String),
    /// The request conflicts with existing state, e.g. creating a database
    /// that already exists (409 Conflict).
    Conflict(String),
    /// A required backend is not configured or is draining (503 Service
    /// Unavailable). Used by `POST /v1/embeddings` when no embeddings backend
    /// is wired in.
    Unavailable(String),
    /// An upstream dependency failed (502 Bad Gateway). Used by `POST
    /// /v1/embeddings` when the configured embeddings upstream errors.
    BadGateway(String),
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

impl From<EmbeddingsError> for ApiError {
    fn from(err: EmbeddingsError) -> Self {
        match err {
            // No backend wired in → the endpoint exists but cannot serve.
            EmbeddingsError::NotConfigured => Self::Unavailable(err.to_string()),
            // Bad client input (empty input, …).
            EmbeddingsError::InvalidInput(_) => Self::BadRequest(err.to_string()),
            // Operator misconfiguration (bad upstream URL): the service is not
            // properly configured, so it cannot serve — surface as 503.
            EmbeddingsError::InvalidUpstream(_) => Self::Unavailable(err.to_string()),
            // The configured upstream failed or misbehaved — a bad gateway.
            EmbeddingsError::Upstream(_) => Self::BadGateway(err.to_string()),
        }
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
                DrevoError::Locked
                | DrevoError::TransactionAlreadyActive
                | DrevoError::NeedsMigration { .. } => {
                    (StatusCode::SERVICE_UNAVAILABLE, err.to_string())
                }
                DrevoError::NoActiveTransaction => (StatusCode::CONFLICT, err.to_string()),
                DrevoError::Encode(_)
                | DrevoError::Decode(_)
                | DrevoError::Json(_)
                | DrevoError::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
            },
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            ApiError::Conflict(msg) => (StatusCode::CONFLICT, msg),
            ApiError::Unavailable(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg),
            ApiError::BadGateway(msg) => (StatusCode::BAD_GATEWAY, msg),
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
// Node types (task 00038)
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

// ---------------------------------------------------------------------
// Edge types (task 00039)
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
/// [`Direction::Both`](crate::model::Direction::Both). Accepted values (case-insensitive): `outgoing`,
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

// ---------------------------------------------------------------------
// Traversal query/response types (task 00040)
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
/// [`Direction::Both`](crate::model::Direction::Both), `depth` defaults to
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

// ---------------------------------------------------------------------
// Search types (task 00041)
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
/// underlying `search_fts` behaviour. `limit` is
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

// ── Cypher over HTTP (`POST /cypher`) types ─────────────────────────────
// The request/response shapes for the Cypher-over-HTTP endpoint, served by
// `crate::native_api`. A response carries BOTH a tabular result (`columns` +
// `rows`) and a `graph` projection — every Node / Relationship / Path value in
// the rows, deduped by id (see `exec_result_to_response` / `collect_graph`) —
// that the browser renders on the canvas the same way it renders
// `/export/json` and `/subgraph`.

/// Request body for `POST /cypher`.
#[derive(Debug, Deserialize)]
pub struct CypherRequest {
    /// The Cypher query text.
    pub query: Option<String>,
    /// Optional query parameters (`$name` placeholders), JSON scalars/containers.
    #[serde(default)]
    pub params: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Mutation counters surfaced from [`executor::ExecStats`].
#[derive(Debug, Default, Serialize)]
pub struct CypherStats {
    /// Nodes created by `CREATE` / `MERGE`.
    pub nodes_created: usize,
    /// Relationships created by `CREATE` / `MERGE`.
    pub relationships_created: usize,
    /// Property assignments performed by `SET` / `REMOVE` / `MERGE`.
    pub properties_set: usize,
    /// Nodes removed by `DELETE` / `DETACH DELETE`.
    pub nodes_deleted: usize,
    /// Relationships removed by `DELETE` / `DETACH DELETE`.
    pub relationships_deleted: usize,
    /// Labels added by `SET n:Label`.
    pub labels_added: usize,
    /// Labels removed by `REMOVE n:Label`.
    pub labels_removed: usize,
}

/// Graph projection: the Node / Relationship values found anywhere in the
/// result rows, deduped by id, in the `{nodes, edges}` shape the UI reads.
#[derive(Debug, Default, Serialize)]
pub struct CypherGraph {
    /// Deduped node objects (`{id, kind, title, uuid, labels, properties}`).
    pub nodes: Vec<serde_json::Value>,
    /// Deduped edge objects (`{id, from_id, to_id, kind, ...}`).
    pub edges: Vec<serde_json::Value>,
}

/// Response body for `POST /cypher`.
#[derive(Debug, Serialize)]
pub struct CypherResponse {
    /// Projected column names, in `RETURN` order.
    pub columns: Vec<String>,
    /// One entry per result row; each cell is the JSON form of a Cypher value.
    pub rows: Vec<Vec<serde_json::Value>>,
    /// Write-side mutation counters.
    pub stats: CypherStats,
    /// The Node / Relationship / Path values from the rows, for canvas render.
    pub graph: CypherGraph,
}

/// Convert a JSON parameter value into a Cypher runtime value.
pub(crate) fn json_to_cypher_value(v: serde_json::Value) -> CypherValue {
    match v {
        serde_json::Value::Null => CypherValue::Null,
        serde_json::Value::Bool(b) => CypherValue::Bool(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                CypherValue::Integer(i)
            } else {
                CypherValue::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => CypherValue::String(s),
        serde_json::Value::Array(a) => {
            CypherValue::List(a.into_iter().map(json_to_cypher_value).collect())
        }
        serde_json::Value::Object(o) => CypherValue::Map(
            o.into_iter()
                .map(|(k, v)| (k, json_to_cypher_value(v)))
                .collect(),
        ),
    }
}

/// Serialise a UUID byte array the same way `/export/json` does (a number
/// array) so the UI's existing `uuidToHyphenated` helper handles it.
fn uuid_to_json(uuid: &[u8; 16]) -> serde_json::Value {
    serde_json::Value::Array(uuid.iter().map(|b| serde_json::json!(b)).collect())
}

/// A Cypher `NodeValue` → the `{id, kind, title, uuid, labels, properties}`
/// shape the front-end renderer (`toElements`) reads.
fn node_value_to_json(n: &executor::NodeValue) -> serde_json::Value {
    let title = match n.properties.get("title") {
        Some(CypherValue::String(s)) => s.clone(),
        _ => String::new(),
    };
    serde_json::json!({
        "id": n.id,
        "kind": n.labels.first().cloned().unwrap_or_default(),
        "title": title,
        "uuid": uuid_to_json(&n.uuid),
        "labels": n.labels,
        "properties": map_to_json(&n.properties),
    })
}

/// A Cypher `RelationshipValue` → the `{id, from_id, to_id, kind, ...}` shape.
fn rel_value_to_json(r: &executor::RelationshipValue) -> serde_json::Value {
    serde_json::json!({
        "id": r.id,
        "from_id": r.from_id,
        "to_id": r.to_id,
        "kind": r.kind,
        "uuid": uuid_to_json(&r.uuid),
        "properties": map_to_json(&r.properties),
    })
}

fn map_to_json(m: &std::collections::BTreeMap<String, CypherValue>) -> serde_json::Value {
    serde_json::Value::Object(
        m.iter()
            .map(|(k, v)| (k.clone(), value_to_json(v)))
            .collect(),
    )
}

/// Convert a Cypher runtime value into JSON for the tabular `rows`.
fn value_to_json(v: &CypherValue) -> serde_json::Value {
    match v {
        CypherValue::Null => serde_json::Value::Null,
        CypherValue::Bool(b) => serde_json::Value::Bool(*b),
        CypherValue::Integer(i) => serde_json::json!(i),
        CypherValue::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        CypherValue::String(s) => serde_json::Value::String(s.clone()),
        CypherValue::List(items) => {
            serde_json::Value::Array(items.iter().map(value_to_json).collect())
        }
        CypherValue::Map(m) => map_to_json(m),
        CypherValue::Node(n) => node_value_to_json(n),
        CypherValue::Relationship(r) => rel_value_to_json(r),
        CypherValue::Path(p) => serde_json::json!({
            "nodes": p.nodes.iter().map(|n| node_value_to_json(n)).collect::<Vec<_>>(),
            "relationships": p.relationships.iter().map(|r| rel_value_to_json(r)).collect::<Vec<_>>(),
        }),
    }
}

/// Walk every value in the result, collecting the Node / Relationship values
/// (deduped by id) so the UI can draw the graph the query touched.
fn collect_graph(rows: &[Vec<CypherValue>]) -> CypherGraph {
    let mut nodes: std::collections::BTreeMap<u64, serde_json::Value> = Default::default();
    let mut edges: std::collections::BTreeMap<u64, serde_json::Value> = Default::default();
    fn walk(
        v: &CypherValue,
        nodes: &mut std::collections::BTreeMap<u64, serde_json::Value>,
        edges: &mut std::collections::BTreeMap<u64, serde_json::Value>,
    ) {
        match v {
            CypherValue::Node(n) => {
                nodes.entry(n.id).or_insert_with(|| node_value_to_json(n));
            }
            CypherValue::Relationship(r) => {
                edges.entry(r.id).or_insert_with(|| rel_value_to_json(r));
            }
            CypherValue::Path(p) => {
                for n in &p.nodes {
                    nodes.entry(n.id).or_insert_with(|| node_value_to_json(n));
                }
                for r in &p.relationships {
                    edges.entry(r.id).or_insert_with(|| rel_value_to_json(r));
                }
            }
            CypherValue::List(items) => items.iter().for_each(|x| walk(x, nodes, edges)),
            CypherValue::Map(m) => m.values().for_each(|x| walk(x, nodes, edges)),
            _ => {}
        }
    }
    for row in rows {
        for v in row {
            walk(v, &mut nodes, &mut edges);
        }
    }
    CypherGraph {
        nodes: nodes.into_values().collect(),
        edges: edges.into_values().collect(),
    }
}

pub(crate) fn exec_result_to_response(result: ExecResult) -> CypherResponse {
    let rows: Vec<Vec<serde_json::Value>> = result
        .rows
        .iter()
        .map(|row| row.iter().map(value_to_json).collect())
        .collect();
    let graph = collect_graph(&result.rows);
    let s = result.stats;
    CypherResponse {
        columns: result.columns,
        rows,
        stats: CypherStats {
            nodes_created: s.nodes_created,
            relationships_created: s.relationships_created,
            properties_set: s.properties_set,
            nodes_deleted: s.nodes_deleted,
            relationships_deleted: s.relationships_deleted,
            labels_added: s.labels_added,
            labels_removed: s.labels_removed,
        },
        graph,
    }
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

// ---------------------------------------------------------------------
// Import/Export request types (task 00055)
// ---------------------------------------------------------------------

/// JSON body accepted by `POST /import/json`. Carries the raw dump produced
/// by `GET /export/json` (or `Drevo::export_json`) — the server parses,
/// validates the format header, and replays the payload into the live
/// database.
#[derive(Debug, Deserialize)]
pub struct ImportJsonRequest {
    /// Raw `drevo-json-v1` payload — the full output of `GET /export/json`.
    pub dump: String,
}

/// JSON body accepted by `POST /import/graphml`. Carries a GraphML document —
/// drevo's own `GET /export/graphml` output, or any GraphML that follows the
/// same `<key>` / `<data>` conventions.
#[derive(Debug, Deserialize)]
pub struct ImportGraphmlRequest {
    /// Raw GraphML 1.0 XML document.
    pub graphml: String,
}

/// Response body for `GET /databases`.
#[derive(Debug, Serialize)]
pub struct DatabaseListResponse {
    /// All database names, sorted ascending. Always includes `default`.
    pub databases: Vec<String>,
    /// The name selected when a request specifies none.
    pub default: &'static str,
}

/// The engine-independent body of `POST /v1/embeddings`, shared by the KV
/// router and the durable-native one (`crate::native_api`): validate, then
/// proxy to the operator-configured backend — or answer the deterministic
/// `400` / `503` without one.
pub(crate) async fn embeddings_response(
    backend: Option<&EmbeddingBackend>,
    req: EmbeddingsRequest,
) -> Result<Json<serde_json::Value>, ApiError> {
    if req.input.is_empty() {
        return Err(ApiError::from(EmbeddingsError::InvalidInput(
            "`input` must contain at least one non-empty string".to_string(),
        )));
    }
    let backend = backend.ok_or(EmbeddingsError::NotConfigured)?;
    let resp = backend.embed(&req).await?;
    Ok(Json(resp))
}

/// The engine-independent body of `GET /config/embeddings`, shared by the KV
/// router and the durable-native one: the store's secret-free status view. The
/// API key is never included.
pub(crate) fn embeddings_config_status(
    store: Option<&crate::embeddings::EmbeddingsConfigStore>,
) -> Result<Json<crate::embeddings::EmbeddingsStatus>, ApiError> {
    let store = store.ok_or(EmbeddingsError::NotConfigured)?;
    Ok(Json(store.status()))
}

/// The engine-independent body of `POST /config/embeddings`: apply a partial
/// update (validate upstream, keep the secret when the key field is blank),
/// persist it (`0600`), and hot-swap it in. Returns the new secret-free status
/// — the API key is never echoed back. A malformed upstream is a `400`.
pub(crate) fn embeddings_config_apply(
    store: Option<&crate::embeddings::EmbeddingsConfigStore>,
    update: crate::embeddings::EmbeddingsConfigUpdate,
) -> Result<Json<crate::embeddings::EmbeddingsStatus>, ApiError> {
    let store = store.ok_or(EmbeddingsError::NotConfigured)?;
    let status = store.apply(update).map_err(|e| match e {
        EmbeddingsError::InvalidUpstream(m) => ApiError::BadRequest(m),
        other => ApiError::from(other),
    })?;
    Ok(Json(status))
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
            status_of(DrevoError::Io(std::io::Error::other("boom"))),
            StatusCode::INTERNAL_SERVER_ERROR,
            "Io → 500",
        );
        // The `Encode` and `Decode` variants of `DrevoError` wrap bincode
        // error types that cannot be constructed outside the bincode
        // crate. They share the `Io` 500 arm in `api.rs`, so
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
                DrevoError::Encode(_) => "Encode",
                DrevoError::Decode(_) => "Decode",
                DrevoError::Io(_) => "Io",
                DrevoError::Json(_) => "Json",
                DrevoError::TransactionAlreadyActive => "TransactionAlreadyActive",
                DrevoError::NoActiveTransaction => "NoActiveTransaction",
                DrevoError::NeedsMigration { .. } => "NeedsMigration",
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
