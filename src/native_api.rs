//! HTTP surface for the durable-native server mode (RFC
//! `docs/rfc-native-core.md` #307, Phase 4/7 — `DREVO_ENGINE=native-durable`).
//!
//! A router over a [`crate::native_service::NativeService`]: liveness
//! (`/health`, `/ready`), identity (`/status`), Cypher (`POST /cypher`,
//! full-text included), the storage panel, the Web UI, and the full raw-REST
//! graph surface — node/edge CRUD (`/nodes`, `/edges`), per-node traversal
//! (`/nodes/{id}/edges` / `/neighbors` / `/subgraph`), shortest path
//! (`/paths/shortest`), keyword faceting (`/facets`), JSON import
//! (`POST /import/json`), and Prometheus metrics (`/metrics`) — all on the
//! **same routes and contracts as the KV router** ([`crate::api`]), so a
//! non-Cypher client migrates from KV to native-durable unchanged.
//!
//! Multi-database catalogs are the one remaining KV-router feature out of scope
//! for this mode — it serves the single durable graph the process was pointed
//! at.

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
    CypherResponse, DatabaseListResponse, EdgeListResponse, FacetsQuery, FacetsResponse,
    ImportGraphmlRequest, ImportJsonRequest, ListEdgesQuery, ListNodesQuery, NeighborsQuery,
    NodeEdgesQuery, NodeListResponse, SearchFtsRequest, SearchFtsResponse, ShortestPathQuery,
    ShortestPathResponse, SubgraphQuery, DEFAULT_FACET_KEYWORDS, DEFAULT_LIST_LIMIT,
    DEFAULT_NEIGHBORS_DEPTH, DEFAULT_SUBGRAPH_DEPTH, MAX_FACET_KEYWORDS, MAX_LIST_LIMIT,
};
use crate::catalog::DEFAULT_DB;
use crate::cypher::parser;
use crate::embeddings::{EmbeddingBackend, EmbeddingsRequest};
use crate::fts::facet::{FacetCollapse, DEFAULT_TRIGRAM_THRESHOLD};
use crate::model::Direction;
use crate::native_service::NativeService;
use crate::observability::DrevoMetrics;

/// Shared state of the durable-native HTTP surface.
#[derive(Clone)]
pub struct NativeApiState {
    /// The store of record.
    pub service: Arc<NativeService>,
    /// Construction instant, for `/status` uptime.
    started_at: Instant,
    /// Graceful-shutdown flag: flipped once on SIGTERM/Ctrl+C so `/health`
    /// and `/ready` report draining.
    shutting_down: Arc<AtomicBool>,
    /// Optional embeddings proxy backend — `POST /v1/embeddings` answers
    /// `503` ("not configured") without one, exactly like the KV router.
    embeddings: Option<Arc<EmbeddingBackend>>,
    /// Shared runtime embeddings config store backing `/config/embeddings`
    /// (Web-UI-settable API key/upstream/model); the same `Arc` the proxy
    /// reads, so a write takes effect live.
    embeddings_config: Option<Arc<crate::embeddings::EmbeddingsConfigStore>>,
    /// Prometheus metrics registry backing `GET /metrics` and the per-request
    /// instrumentation middleware — a [`DrevoMetrics`] registry, so a scrape
    /// looks identical on either engine.
    metrics: Arc<DrevoMetrics>,
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
            metrics: Arc::new(DrevoMetrics::new()),
        }
    }

    /// Attach an embeddings backend, enabling `POST /v1/embeddings`.
    #[must_use]
    pub fn with_embeddings_backend(mut self, backend: EmbeddingBackend) -> Self {
        self.embeddings = Some(Arc::new(backend));
        self
    }

    /// Attach the shared embeddings config store, enabling
    /// `GET`/`POST /config/embeddings`.
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
        // Raw-REST graph CRUD + traversal + faceting — the same routes/contracts
        // as the KV router (src/api.rs), served over the native engine so a
        // non-Cypher client migrates unchanged. Cypher/Bolt clients never needed
        // these; a raw-REST client did (issue: native-router REST-CRUD parity).
        .route("/nodes", get(list_nodes).post(create_node))
        .route("/nodes/{id}/edges", get(get_node_edges))
        .route("/nodes/{id}/neighbors", get(get_node_neighbors))
        .route("/nodes/{id}/subgraph", get(get_node_subgraph))
        .route("/edges", get(list_edges).post(create_edge))
        .route("/facets", get(facets))
        .route("/paths/shortest", get(get_shortest_path))
        .route("/import/json", post(import_json))
        // Prometheus scrape — same exposition format and gauges as the KV
        // router, refreshed from the WAL store's physical size + uptime.
        .route("/metrics", get(metrics))
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
        .route("/ui/graph_math.js", get(crate::web_ui::serve_graph_math_js))
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
        // Per-request metrics instrumentation — layered after the routes so it
        // wraps every handler and before `with_state` so it can extract the
        // shared state, exactly as the KV router does.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            track_metrics,
        ))
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

/// `POST /nodes` — create a node from a JSON [`NewNode`](crate::model::NewNode)
/// body; returns the stored node (201). KV-router parity.
async fn create_node(
    State(state): State<NativeApiState>,
    body: Result<Json<crate::model::NewNode>, JsonRejection>,
) -> Result<(StatusCode, Json<crate::model::Node>), ApiError> {
    let Json(new_node) = body?;
    let node = state.service.create_node(new_node)?;
    Ok((StatusCode::CREATED, Json(node)))
}

/// `GET /nodes?kind=&limit=&offset=` — list nodes of a kind, paginated. A
/// missing `kind` is a 400, exactly as the KV router reports it.
async fn list_nodes(
    State(state): State<NativeApiState>,
    query: Result<axum::extract::Query<ListNodesQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<NodeListResponse>, ApiError> {
    let axum::extract::Query(ListNodesQuery {
        kind,
        limit,
        offset,
    }) = query?;
    let kind =
        kind.ok_or_else(|| ApiError::BadRequest("query parameter 'kind' is required".to_string()))?;
    let limit = limit.unwrap_or(DEFAULT_LIST_LIMIT).min(MAX_LIST_LIMIT);
    let offset = offset.unwrap_or(0);
    let nodes = state.service.list_nodes_by_kind(&kind, limit, offset);
    Ok(Json(NodeListResponse { nodes }))
}

/// `POST /edges` — create an edge from a JSON [`NewEdge`](crate::model::NewEdge)
/// body; returns the stored edge (201). A missing endpoint node is a 404.
async fn create_edge(
    State(state): State<NativeApiState>,
    body: Result<Json<crate::model::NewEdge>, JsonRejection>,
) -> Result<(StatusCode, Json<crate::model::Edge>), ApiError> {
    let Json(new_edge) = body?;
    let edge = state.service.create_edge(new_edge)?;
    Ok((StatusCode::CREATED, Json(edge)))
}

/// `GET /edges?kind=&limit=&offset=` — list edges of a kind, paginated. A
/// missing `kind` is a 400, matching the KV router.
async fn list_edges(
    State(state): State<NativeApiState>,
    query: Result<axum::extract::Query<ListEdgesQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<EdgeListResponse>, ApiError> {
    let axum::extract::Query(ListEdgesQuery {
        kind,
        limit,
        offset,
    }) = query?;
    let kind =
        kind.ok_or_else(|| ApiError::BadRequest("query parameter 'kind' is required".to_string()))?;
    let limit = limit.unwrap_or(DEFAULT_LIST_LIMIT).min(MAX_LIST_LIMIT);
    let offset = offset.unwrap_or(0);
    let edges = state.service.list_edges_by_kind(&kind, limit, offset);
    Ok(Json(EdgeListResponse { edges }))
}

/// `GET /facets?kind=&property=&k=&collapse=&threshold=` — keyword faceting over
/// nodes of `kind`, keywords scored with the native FTS corpus statistics. Same
/// contract as the KV router; `collapse=semantic` is rejected (no HTTP-hosted
/// embedder), exactly as there.
async fn facets(
    State(state): State<NativeApiState>,
    query: Result<axum::extract::Query<FacetsQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<FacetsResponse>, ApiError> {
    let axum::extract::Query(FacetsQuery {
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

    let facets = state.service.facets(&kind, &property, k, &collapse)?;
    Ok(Json(FacetsResponse { facets }))
}

/// `GET /paths/shortest?from=&to=` — Dijkstra over the native graph. Both
/// endpoints must exist (404 otherwise); an unreachable target is a 200 with
/// `{"path": null}`, so a client can tell "no such node" from "no route" —
/// identical to the KV router.
async fn get_shortest_path(
    State(state): State<NativeApiState>,
    query: Result<
        axum::extract::Query<ShortestPathQuery>,
        axum::extract::rejection::QueryRejection,
    >,
) -> Result<Json<ShortestPathResponse>, ApiError> {
    let axum::extract::Query(ShortestPathQuery { from, to }) = query?;
    let from =
        from.ok_or_else(|| ApiError::BadRequest("query parameter 'from' is required".to_string()))?;
    let to =
        to.ok_or_else(|| ApiError::BadRequest("query parameter 'to' is required".to_string()))?;

    // Validate both endpoints up front — `get_node` surfaces a missing node as
    // 404, distinguishing it from an unreachable target (`{"path": null}`).
    state.service.get_node(from)?;
    state.service.get_node(to)?;

    let path = state.service.shortest_path(from, to)?;
    Ok(Json(ShortestPathResponse { path }))
}

/// Parse the `direction` query parameter into a [`Direction`] — `outgoing` /
/// `incoming` / `both` (case-insensitive), defaulting to `Both`. Local copy of
/// the KV router's helper so the native surface owns it once KV is removed.
fn parse_direction(value: Option<&str>) -> Result<Direction, ApiError> {
    match value.map(str::to_ascii_lowercase).as_deref() {
        None | Some("both") => Ok(Direction::Both),
        Some("outgoing") => Ok(Direction::Outgoing),
        Some("incoming") => Ok(Direction::Incoming),
        Some(other) => Err(ApiError::BadRequest(format!(
            "invalid direction '{other}', expected one of: outgoing, incoming, both"
        ))),
    }
}

/// `GET /nodes/{id}/edges?direction=` — edges incident to the node. Like the KV
/// router, a missing node yields an empty list, not a 404.
async fn get_node_edges(
    State(state): State<NativeApiState>,
    axum::extract::Path(id): axum::extract::Path<u64>,
    query: Result<axum::extract::Query<NodeEdgesQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<EdgeListResponse>, ApiError> {
    let axum::extract::Query(NodeEdgesQuery { direction }) = query?;
    let direction = parse_direction(direction.as_deref())?;
    let edges = state.service.edges_of(id, direction);
    Ok(Json(EdgeListResponse { edges }))
}

/// `GET /nodes/{id}/neighbors?direction=&depth=&kind=` — BFS reachable nodes.
/// A missing start node is a 404 (distinguishing "no neighbours" from "no
/// node"), exactly as the KV router.
async fn get_node_neighbors(
    State(state): State<NativeApiState>,
    axum::extract::Path(id): axum::extract::Path<u64>,
    query: Result<axum::extract::Query<NeighborsQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<NodeListResponse>, ApiError> {
    let axum::extract::Query(NeighborsQuery {
        direction,
        kind,
        depth,
    }) = query?;
    let direction = parse_direction(direction.as_deref())?;
    let depth = depth.unwrap_or(DEFAULT_NEIGHBORS_DEPTH);
    // Surface a missing node as 404 — `bfs` would otherwise return empty.
    state.service.get_node(id)?;
    let nodes = state.service.bfs(id, depth, direction, kind.as_deref())?;
    Ok(Json(NodeListResponse { nodes }))
}

/// `GET /nodes/{id}/subgraph?depth=` — the subgraph within `depth` hops of the
/// root. 404 if the root does not exist (the shared traversal maps that to
/// `NodeNotFound`), matching the KV router.
async fn get_node_subgraph(
    State(state): State<NativeApiState>,
    axum::extract::Path(id): axum::extract::Path<u64>,
    query: Result<axum::extract::Query<SubgraphQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<crate::model::SubGraph>, ApiError> {
    let axum::extract::Query(SubgraphQuery { depth }) = query?;
    let depth = depth.unwrap_or(DEFAULT_SUBGRAPH_DEPTH);
    let sub = state.service.subgraph_filtered(id, depth, None)?;
    Ok(Json(sub))
}

/// `POST /import/json` — replay a `drevo-json-v1` dump (a `GET /export/json`
/// body) into the durable store; returns an
/// [`ImportReport`](crate::dump::ImportReport). Same contract as the KV router.
async fn import_json(
    State(state): State<NativeApiState>,
    body: Result<Json<ImportJsonRequest>, JsonRejection>,
) -> Result<Json<crate::dump::ImportReport>, ApiError> {
    let Json(req) = body?;
    Ok(Json(state.service.import_json(&req.dump)?))
}

/// Prometheus content type — must be exact; scrapers key off it.
const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// `GET /metrics` — render process metrics in the Prometheus text format, the
/// same registry/gauges as the KV router. The uptime gauge is refreshed from
/// `started_at` and the physical-size gauge from the WAL's on-disk size just
/// before rendering, so a scrape needs no background ticker.
async fn metrics(State(state): State<NativeApiState>) -> Response {
    state
        .metrics
        .uptime_seconds
        .set(state.started_at.elapsed().as_secs() as i64);
    // Physical WAL size (O(1) stat). A probe failure leaves the previous value
    // rather than faking a zero.
    if let Some(bytes) = state.service.graph().wal_bytes() {
        state.metrics.storage_file_bytes.set(bytes as i64);
    }
    let body = state.metrics.render_prometheus();
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, PROMETHEUS_CONTENT_TYPE)],
        body,
    )
        .into_response()
}

/// Per-request instrumentation middleware — the native counterpart of the KV
/// router's `track_metrics`: count in-flight, time the handler, record the
/// status class + latency into the shared [`DrevoMetrics`].
async fn track_metrics(
    State(state): State<NativeApiState>,
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
