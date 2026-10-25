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
//! The multi-database catalog (`GET`/`POST /databases`, `DELETE
//! /databases/{name}`, issue #523) is served here over a
//! [`crate::database_registry::DatabaseRegistry`]: the always-present default
//! is the durable graph the process was pointed at, and additional named
//! databases can be created, listed, dropped, and — via Cypher `USE` over
//! `POST /cypher` and the Bolt `db` selector — queried; in the durable server
//! they persist across restarts (issue #523). A client that names no database
//! still sees exactly the single-graph behaviour.

#![cfg(feature = "http")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use axum::Router;

use crate::api::{
    embeddings_response, exec_result_to_response, json_to_cypher_value, ApiError,
    CreateDatabaseRequest, CypherRequest, CypherResponse, DatabaseListResponse, EdgeListResponse,
    FacetsQuery, FacetsResponse, ImportGraphmlRequest, ImportJsonRequest, ListEdgesQuery,
    ListNodesQuery, NeighborsQuery, NodeEdgesQuery, NodeListResponse, SearchFtsRequest,
    SearchFtsResponse, ShortestPathQuery, ShortestPathResponse, SubgraphQuery,
    DEFAULT_FACET_KEYWORDS, DEFAULT_LIST_LIMIT, DEFAULT_NEIGHBORS_DEPTH, DEFAULT_SUBGRAPH_DEPTH,
    MAX_FACET_KEYWORDS, MAX_LIST_LIMIT,
};
use crate::cypher::parser;
use crate::database_registry::{DatabaseRegistry, RegistryError};
use crate::embeddings::{EmbeddingBackend, EmbeddingsRequest};
use crate::fts::facet::{FacetCollapse, DEFAULT_TRIGRAM_THRESHOLD};
use crate::model::Direction;
use crate::native_service::NativeService;
use crate::observability::DrevoMetrics;

/// The name of the single database this server serves. Historically the
/// multi-database catalog's always-present default; the native server serves
/// exactly this one durable graph, and `/databases` advertises it by this name.
pub const DEFAULT_DB: &str = "drevo";

/// Shared state of the durable-native HTTP surface.
#[derive(Clone)]
pub struct NativeApiState {
    /// The store of record — the default database's service.
    pub service: Arc<NativeService>,
    /// The multi-database catalog (issue #523). Its default entry is the same
    /// [`Arc`] as [`service`](Self::service), so existing single-database
    /// handlers are unaffected; `/databases` enumerates this registry and the
    /// create / drop lifecycle mutates it.
    registry: Arc<DatabaseRegistry>,
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
    /// The text-to-Cypher provider behind `drevo.cypher.fromText`, settable
    /// through `GET`/`POST /config/text2cypher`.
    text2cypher_config: Option<Arc<crate::text2cypher::Text2CypherConfigStore>>,
    /// The server's users, for the Cypher user-management commands.
    security: crate::security::Security,
    /// Prometheus metrics registry backing `GET /metrics` and the per-request
    /// instrumentation middleware — a [`DrevoMetrics`] registry, so a scrape
    /// looks identical on either engine.
    metrics: Arc<DrevoMetrics>,
    /// Body formats offered next to JSON (issue #581); JSON only by default.
    formats: Arc<crate::wire_format::WireFormats>,
    /// Whether the session cookie is marked `Secure` (sent over HTTPS only):
    /// set when the server serves HTTPS.
    secure_cookies: bool,
    /// Where `POST /admin/backup` writes backups; `<data_dir>/backups` when
    /// unset.
    backup_dir: Option<std::path::PathBuf>,
}

impl NativeApiState {
    /// Wrap a service for serving. Builds a fresh single-entry catalog whose
    /// default is `service`.
    pub fn new(service: Arc<NativeService>) -> Self {
        let registry = Arc::new(DatabaseRegistry::new(Arc::clone(&service)));
        Self::with_registry(service, registry)
    }

    /// Wrap a service for serving atop an existing multi-database catalog.
    ///
    /// Used when the catalog must be shared with another surface — the Bolt
    /// listener threads the same [`DatabaseRegistry`] so a `CREATE DATABASE`
    /// over HTTP is visible to Bolt sessions and vice versa (issue #523). The
    /// registry's default service and `service` are expected to be the same
    /// handle.
    pub fn with_registry(service: Arc<NativeService>, registry: Arc<DatabaseRegistry>) -> Self {
        Self {
            service,
            registry,
            started_at: Instant::now(),
            shutting_down: Arc::new(AtomicBool::new(false)),
            embeddings: None,
            embeddings_config: None,
            text2cypher_config: None,
            security: crate::security::Security::default(),
            metrics: Arc::new(DrevoMetrics::new()),
            formats: Arc::new(crate::wire_format::WireFormats::default()),
            secure_cookies: false,
            backup_dir: None,
        }
    }

    /// Write `POST /admin/backup` backups under `dir` (`DREVO_BACKUP_DIR`)
    /// instead of `<data_dir>/backups`.
    #[must_use]
    pub fn with_backup_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.backup_dir = Some(dir);
        self
    }

    /// Where backups go: the configured directory, else `<data_dir>/backups`;
    /// `None` for a server that keeps its data in memory.
    fn backup_root(&self) -> Option<std::path::PathBuf> {
        let data_dir = self.registry.data_dir()?;
        Some(
            self.backup_dir
                .clone()
                .unwrap_or_else(|| data_dir.join("backups")),
        )
    }

    /// Mark the `drevo_session` cookie `Secure`, so browsers send it over
    /// HTTPS only. The server sets this when it serves HTTPS.
    #[must_use]
    pub fn with_secure_cookies(mut self, secure: bool) -> Self {
        self.secure_cookies = secure;
        self
    }

    /// Offer `formats` (from `DREVO_HTTP_FORMATS`) next to JSON for request
    /// and response bodies (issue #581).
    #[must_use]
    pub fn with_wire_formats(mut self, formats: crate::wire_format::WireFormats) -> Self {
        self.formats = Arc::new(formats);
        self
    }

    /// Run the Cypher user-management commands (`CREATE USER`, `SHOW USERS`,
    /// …) against `security`.
    #[must_use]
    pub fn with_security(mut self, security: crate::security::Security) -> Self {
        self.security = security;
        self
    }

    /// The shared multi-database catalog backing `/databases` and Cypher `USE`.
    ///
    /// Exposed so the serving layer can hand the *same* registry to the Bolt
    /// listener, keeping the two protocols' database views in lock-step
    /// (issue #523).
    pub fn registry(&self) -> Arc<DatabaseRegistry> {
        Arc::clone(&self.registry)
    }

    /// The server's users and their privileges.
    #[must_use]
    pub fn security(&self) -> &crate::security::Security {
        &self.security
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

    /// Attach the text-to-Cypher provider store, enabling
    /// `GET`/`POST /config/text2cypher`.
    #[must_use]
    pub fn with_text2cypher_config_store(
        mut self,
        store: Arc<crate::text2cypher::Text2CypherConfigStore>,
    ) -> Self {
        self.text2cypher_config = Some(store);
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
        // Sign-in for the Web UI and API clients (issue #634).
        .route("/auth/login", post(auth_login))
        .route("/auth/logout", post(auth_logout))
        .route("/auth/me", get(auth_me))
        .route("/export/graphml", get(export_graphml))
        .route("/v1/embeddings", post(embeddings))
        .route(
            "/config/embeddings",
            get(get_embeddings_config).post(set_embeddings_config),
        )
        .route(
            "/config/text2cypher",
            get(get_text2cypher_config).post(set_text2cypher_config),
        )
        .route("/search/fts", post(search_fts))
        .route("/export/json", get(export_json))
        .route("/databases", get(list_databases).post(create_database))
        .route("/databases/{name}", delete(drop_database))
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
        .route("/problems", get(problems))
        .route("/transactions/prepared", get(list_prepared_http))
        .route(
            "/transactions/prepared/{gid}/commit",
            post(commit_prepared_http),
        )
        .route(
            "/transactions/prepared/{gid}/rollback",
            post(rollback_prepared_http),
        )
        .route(
            "/transactions/prepared/{gid}/heuristic-rollback",
            post(heuristic_rollback_prepared_http),
        )
        .route("/report", get(problem_report))
        // The storage panel is engine-agnostic: the same endpoints/contracts as
        // the KV router, implemented over the WAL store — bloat = physical WAL
        // vs compacted size, shrink = WAL compaction, benchmark = the same
        // throwaway probe with native FTS. Keyspaces are a redb-file concept
        // with no WAL analogue, so that one returns an empty breakdown.
        .route("/storage/bloat", get(storage_bloat))
        .route("/storage/keyspaces", get(storage_keyspaces))
        .route("/storage/shrink", post(storage_shrink))
        .route("/storage/benchmark", post(storage_benchmark))
        // Physical backups of every database and the server's files.
        .route("/admin/backup", post(admin_backup))
        .route("/admin/backups", get(admin_backups))
        // The Web UI — the same embedded, same-origin assets as the KV
        // server (`crate::web_ui`), pointed at the same-shape endpoints.
        // Committed changes as Server-Sent Events.
        .route("/events", get(crate::events::stream))
        .route("/ui", get(crate::web_ui::serve_index))
        .route("/ui/", get(crate::web_ui::redirect_ui_slash))
        .route("/ui/app.js", get(crate::web_ui::serve_app_js))
        .route("/ui/graph_math.js", get(crate::web_ui::serve_graph_math_js))
        .route("/ui/report.js", get(crate::web_ui::serve_report_js))
        .route(
            "/ui/transactions_view.js",
            get(crate::web_ui::serve_transactions_view_js),
        )
        .route(
            "/ui/backups_view.js",
            get(crate::web_ui::serve_backups_view_js),
        )
        .route("/ui/db_access.js", get(crate::web_ui::serve_db_access_js))
        .route(
            "/ui/live_refresh.js",
            get(crate::web_ui::serve_live_refresh_js),
        )
        .route(
            "/ui/results_view.js",
            get(crate::web_ui::serve_results_view_js),
        )
        .route(
            "/ui/query_history.js",
            get(crate::web_ui::serve_query_history_js),
        )
        .route("/ui/auth.js", get(crate::web_ui::serve_auth_js))
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
        // Login check (issue #634): innermost, so the handlers see the signed-in
        // user and the metrics layer still counts refused requests.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_login,
        ))
        // Per-request metrics instrumentation — layered after the routes so it
        // wraps every handler and before `with_state` so it can extract the
        // shared state, exactly as the KV router does.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            track_metrics,
        ))
        // Outermost: CBOR / MessagePack bodies are transcoded to and from JSON
        // here, so every handler (and the metrics layer) only sees JSON.
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state.formats),
            crate::wire_format::negotiate,
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

/// `GET /config/text2cypher` — the text-to-Cypher provider without its key.
async fn get_text2cypher_config(
    State(state): State<NativeApiState>,
) -> Result<Json<crate::text2cypher::Text2CypherStatus>, ApiError> {
    crate::api::text2cypher_config_status(state.text2cypher_config.as_deref())
}

/// `POST /config/text2cypher` — change the provider: persisted, installed at
/// once; the key is never echoed back.
async fn set_text2cypher_config(
    State(state): State<NativeApiState>,
    body: Result<Json<crate::text2cypher::Text2CypherConfigUpdate>, JsonRejection>,
) -> Result<Json<crate::text2cypher::Text2CypherStatus>, ApiError> {
    let Json(update) = body?;
    crate::api::text2cypher_config_apply(state.text2cypher_config.as_deref(), update)
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

/// Lift a [`RegistryError`] into the HTTP error envelope: invalid name → 400,
/// already-exists → 409, not-found → 404, protected-default → 409.
impl From<RegistryError> for ApiError {
    fn from(err: RegistryError) -> Self {
        match err {
            RegistryError::InvalidName(_) => ApiError::BadRequest(err.to_string()),
            RegistryError::AlreadyExists(_) | RegistryError::ProtectedDefault(_) => {
                ApiError::Conflict(err.to_string())
            }
            RegistryError::NotFound(_) => ApiError::NotFound(err.to_string()),
            // A durable-WAL open/delete failure is a server-side I/O fault —
            // route it through the existing `Db(Io)` arm so it surfaces as 500
            // without adding an `ApiError` variant.
            RegistryError::Storage { .. } => ApiError::Db(crate::error::DrevoError::Io(
                std::io::Error::other(err.to_string()),
            )),
            // A read-only server (#642): 403 with Neo4j's read-only code.
            RegistryError::ReadOnly => ApiError::Coded {
                status: StatusCode::FORBIDDEN,
                code: crate::api::FORBIDDEN_ON_READ_ONLY_DATABASE,
                message: err.to_string(),
            },
        }
    }
}

/// `GET /databases` — every database in the catalog (issue #523), in the KV
/// route's response shape so the Web UI's selector keeps working. With only the
/// default database present this is `{ "databases": ["drevo"], "default":
/// "drevo" }`, exactly as before.
async fn list_databases(State(state): State<NativeApiState>) -> Json<DatabaseListResponse> {
    Json(database_list(&state.registry))
}

/// `POST /databases` — create a new named database (issue #523). The body is
/// `{ "name": "<db>" }`; the name is validated (400 on a bad name, 409 if it
/// already exists). Returns `201 Created` with the updated database list.
///
/// The new database is durable when the server is durable (its WAL directory is
/// created under the data dir, so it survives a restart) and in-memory
/// otherwise, per the registry's mode. It is immediately listable, droppable,
/// and a query target via `USE` / the Bolt `db` selector.
async fn create_database(
    State(state): State<NativeApiState>,
    body: Result<Json<CreateDatabaseRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<DatabaseListResponse>), ApiError> {
    let Json(CreateDatabaseRequest { name }) = body?;
    state.registry.create(&name)?;
    Ok((StatusCode::CREATED, Json(database_list(&state.registry))))
}

/// `DELETE /databases/{name}` — drop a named database (issue #523). Dropping the
/// default database is a 409; an unknown name is a 404. Returns `200 OK` with
/// the updated database list.
async fn drop_database(
    State(state): State<NativeApiState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<Json<DatabaseListResponse>, ApiError> {
    state.registry.remove(&name)?;
    Ok(Json(database_list(&state.registry)))
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
    state
        .metrics
        .sync_statement_timeouts(crate::problems::statement_timeouts());
    // Rejected explicit-transaction commits, by what a per-entity rule would
    // have said (issue #749).
    state
        .metrics
        .sync_tx_conflicts(state.service.conflict_stats());
    // Two-phase commit (#556): a prepared transaction blocks every write, so
    // its count and age are first-class operational signals.
    let prepared = state.service.list_prepared();
    state
        .metrics
        .prepared_transactions
        .set(prepared.len() as i64);
    let now = crate::model::now_ms();
    let oldest_age_ms = prepared
        .iter()
        .map(|p| now.saturating_sub(p.prepared_at_ms))
        .max()
        .unwrap_or(0);
    state
        .metrics
        .prepared_oldest_age_seconds
        .set(oldest_age_ms / 1000);
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
/// same [`BloatReport`](crate::report::BloatReport) contract as the KV router, so the
/// Web UI's storage panel is engine-agnostic.
async fn storage_bloat(State(state): State<NativeApiState>) -> Json<crate::report::BloatReport> {
    Json(state.service.storage_bloat())
}

/// `POST /storage/shrink` — compact the append-only WAL to its current state and
/// report reclaimed bytes (same [`CompactReport`](crate::report::CompactReport)
/// contract as the KV router).
async fn storage_shrink(
    State(state): State<NativeApiState>,
) -> Result<Json<crate::report::CompactReport>, ApiError> {
    Ok(Json(state.service.compact()?))
}

/// The body of `POST /admin/backup`.
#[derive(Debug, Default, serde::Deserialize)]
struct BackupRequest {
    /// The backup's name in the backup directory; made from the time when
    /// absent.
    name: Option<String>,
    /// The name of an earlier backup to continue incrementally.
    since: Option<String>,
}

/// `POST /admin/backup` — back up every database and the server's files
/// into the backup directory while the server keeps serving:
/// `{"name": "monday", "since": "sunday"}`, both optional. Answers with the
/// backup's summary (`server.json`) plus its `name` and `path`.
async fn admin_backup(
    State(state): State<NativeApiState>,
    body: Option<Json<BackupRequest>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    use crate::backup::{self, ServerBackupError};
    let request = body.map(|Json(b)| b).unwrap_or_default();
    let root = state
        .backup_root()
        .ok_or_else(|| ApiError::Conflict(ServerBackupError::InMemory.to_string()))?;
    let name = request
        .name
        .unwrap_or_else(|| backup::timestamp_name(crate::model::now_ms()));
    if !backup::valid_backup_name(&name) {
        return Err(ApiError::BadRequest(format!(
            "`{name}` is not a backup name: use letters, digits, `_`, `-` and `.`"
        )));
    }
    let since = match request.since {
        Some(since) if !backup::valid_backup_name(&since) => {
            return Err(ApiError::BadRequest(format!(
                "`{since}` is not a backup name"
            )));
        }
        Some(since) => Some(root.join(since)),
        None => None,
    };
    let dir = root.join(&name);
    let registry = state.registry();
    let data_dir = registry
        .data_dir()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| ApiError::Conflict(ServerBackupError::InMemory.to_string()))?;
    let summary =
        run_blocking(|| backup::backup_server(&registry, &data_dir, &dir, since.as_deref()))
            .map_err(|e| match e {
                ServerBackupError::Exists(_) => ApiError::Conflict(e.to_string()),
                ServerBackupError::NoSuchBackup(_) => ApiError::NotFound(e.to_string()),
                other => ApiError::Db(crate::error::DrevoError::Io(std::io::Error::other(
                    other.to_string(),
                ))),
            })?;
    let mut out = serde_json::to_value(&summary)
        .map_err(|e| ApiError::Db(crate::error::DrevoError::Io(std::io::Error::other(e))))?;
    out["name"] = serde_json::Value::String(name);
    out["path"] = serde_json::Value::String(dir.display().to_string());
    Ok(Json(out))
}

/// `GET /admin/backups` — the backups in the backup directory, newest
/// first: `{"backups": [{name, created_at_ms, databases, …}]}`.
async fn admin_backups(
    State(state): State<NativeApiState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Some(root) = state.backup_root() else {
        return Ok(Json(serde_json::json!({ "backups": [] })));
    };
    let mut backups = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Ok(summary) = crate::backup::ServerBackup::read(&entry.path()) {
                let mut item = serde_json::to_value(&summary).unwrap_or_default();
                item["name"] = serde_json::Value::String(name);
                backups.push(item);
            }
        }
    }
    backups.sort_by_key(|b| std::cmp::Reverse(b["created_at_ms"].as_i64().unwrap_or(0)));
    Ok(Json(serde_json::json!({
        "directory": root.display().to_string(),
        "backups": backups,
    })))
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

    // Write throughput on throwaway in-memory native stores — never the live graph.
    let scratch = crate::native_service::NativeService::in_memory();
    let t0 = Instant::now();
    for i in 0..incr_n {
        scratch.create_node(mk("bench-incr", i))?;
    }
    let incr_secs = t0.elapsed().as_secs_f64();

    let scratch2 = crate::native_service::NativeService::in_memory();
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
///
/// `text2cypher` is `{"model": "<model or null>"}` while
/// `drevo.cypher.fromText` has an upstream configured, else `null`; the web
/// UI's **Ask** button reads it. The upstream URL is not exposed.
async fn status(State(state): State<NativeApiState>) -> Json<serde_json::Value> {
    let text2cypher = crate::text2cypher::installed()
        .map(|generator| serde_json::json!({ "model": generator.model() }));
    Json(serde_json::json!({
        "name": "drevo",
        "version": crate::VERSION,
        "engine": "native-durable",
        "uptime_seconds": state.started_at.elapsed().as_secs(),
        // Stable replica identity for the multi-writer/P2P substrate (issue
        // #389). A string so a u64 survives JSON clients that use f64 numbers.
        "origin": state.service.origin_id().0.to_string(),
        "text2cypher": text2cypher,
    }))
}

/// `POST /cypher` — parse and execute on the durable service, with catalog
/// admin and per-database routing (issue #523).
///
/// A leading `SHOW DATABASES`, `CREATE DATABASE <name> [IF NOT EXISTS]`, or
/// `USE <name> [<query>]` (recognised by [`crate::cypher::admin`]) is handled
/// against the [`DatabaseRegistry`]; `USE <name> <query>` runs the trailing
/// query against the named database. Everything else runs on the default
/// database, exactly as before.
async fn cypher(
    State(state): State<NativeApiState>,
    signed_in: Option<axum::Extension<crate::security::SignedIn>>,
    origin: Option<axum::Extension<crate::security::Origin>>,
    body: Result<Json<CypherRequest>, JsonRejection>,
) -> Result<Json<CypherResponse>, ApiError> {
    let origin = origin.map_or_else(|| crate::security::Origin::new("http"), |o| o.0);
    let Json(CypherRequest {
        query,
        params,
        timeout_ms,
    }) = body?;
    let query =
        query.ok_or_else(|| ApiError::BadRequest("field 'query' is required".to_string()))?;
    let params = params.unwrap_or_default();
    let signed_in = signed_in.map(|axum::Extension(user)| user);

    let parsed = crate::cypher::security_admin::parse(&query);
    // A user who must change the password may run only that change.
    if signed_in
        .as_ref()
        .is_some_and(|u| u.password_change_required)
        && !matches!(
            parsed,
            Some(Ok(
                crate::cypher::security_admin::SecurityCommand::AlterCurrentUserPassword { .. }
            ))
        )
    {
        return Err(credentials_expired());
    }
    if let Some(parsed) = parsed {
        return run_security_command(&state, parsed, &params, signed_in.as_ref(), &origin)
            .map(Json);
    }
    // The signed-in user's privileges (#636); `None` checks nothing.
    let context = state
        .security
        .context_for(signed_in.as_ref(), state.registry.default_name());
    let caller = Caller {
        security: &state.security,
        origin: &origin,
        user: signed_in.as_ref().map(|u| u.name.as_str()),
        context: context.as_ref(),
        timeout: timeout_ms
            .filter(|&ms| ms > 0)
            .map(std::time::Duration::from_millis),
    };
    if let Some(command) = crate::cypher::admin::parse(&query) {
        return handle_admin_cypher(&state, command, params, &caller).map(Json);
    }

    Ok(Json(run_blocking(|| {
        run_cypher_on(
            &state.service,
            state.registry.default_name(),
            &query,
            params,
            &caller,
        )
    })?))
}

/// Run a synchronous, possibly long statement (Cypher execution) from async
/// server code without parking a tokio worker (#547).
///
/// The executor is synchronous and holds the service's index lock for the
/// whole statement. Called straight from an `async fn`, a slow statement pins
/// its worker thread, and a few of them in flight starve the runtime: on the
/// live server even `GET /health`, which takes no lock, stopped answering.
/// On a multi-thread runtime [`tokio::task::block_in_place`] first hands the
/// worker's other tasks to a fresh worker, so liveness, `/status` and new
/// connections keep being served however long the statement runs. It cannot
/// be used on a current-thread runtime, where it panics (that is the flavour
/// `#[tokio::test]` uses by default). There the work runs inline, exactly as
/// before.
pub(crate) fn run_blocking<R>(work: impl FnOnce() -> R) -> R {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(work)
        }
        _ => work(),
    }
}

/// Parse and execute `query` (with JSON `params`) against `service`, projecting
/// the result into the HTTP [`CypherResponse`]. The single execution path for
/// both the default database and a `USE`-selected one.
/// The address an HTTP connection comes from, as `axum`'s connect info, for
/// the audit log (#637). One type for plain and TLS listeners.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientAddr(pub std::net::SocketAddr);

impl
    axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, tokio::net::TcpListener>>
    for ClientAddr
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        Self(*stream.remote_addr())
    }
}

/// Who sends an HTTP statement: for privilege checks and the audit log.
struct Caller<'a> {
    security: &'a crate::security::Security,
    origin: &'a crate::security::Origin,
    user: Option<&'a str>,
    /// The user's privileges; `None` checks nothing.
    context: Option<&'a drevo_auth::privileges::SecurityContext>,
    /// The statement's own time limit (`timeout_ms`), if the client set one.
    timeout: Option<std::time::Duration>,
}

fn run_cypher_on(
    service: &NativeService,
    database: &str,
    query: &str,
    params: serde_json::Map<String, serde_json::Value>,
    caller: &Caller<'_>,
) -> Result<CypherResponse, ApiError> {
    let context = caller.context;
    let params = params
        .into_iter()
        .map(|(k, v)| (k, json_to_cypher_value(v)))
        .collect();
    let ast = parser::parse(query)
        .map_err(|e| ApiError::BadRequest(format!("Cypher parse error: {e}")))?;
    let restriction =
        context.map(|context| crate::cypher::executor::Restriction { context, database });
    let who = crate::transactions::Caller {
        surface: caller.origin.surface,
        client: caller.origin.client.clone(),
        user: caller.user.map(str::to_string),
        connection: None,
    };
    let deadline = caller.timeout.map(|limit| {
        crate::cypher::executor::deadline::Deadline::client(limit, service.statement_timeout())
    });
    let result = crate::transactions::scope(who, query, || {
        crate::cypher::executor::deadline::with_deadline(deadline, || {
            service.execute_as(&ast, params, restriction)
        })
    })
    .map_err(|e| {
        crate::problems::note_exec_error("http", database, query, &e);
        match e {
            // Writes paused by a prepared two-phase-commit transaction (#556)
            // are a retryable 503, not a client error.
            crate::cypher::executor::ExecError::Storage(
                err @ crate::error::DrevoError::PreparedTransactionPending(_),
            ) => ApiError::Db(err),
            // A declared constraint refused the write (#604): 409 with its code.
            crate::cypher::executor::ExecError::Storage(
                err @ crate::error::DrevoError::ConstraintViolation { .. },
            ) => ApiError::Db(err),
            // A write to a read-only database (#642): 403 with Neo4j's code.
            crate::cypher::executor::ExecError::Storage(
                err @ crate::error::DrevoError::ReadOnly,
            ) => ApiError::Db(err),
            // `drevo.tx.*` on an unknown / heuristically rolled-back gid
            // (#590): the same status and code as the resolve routes.
            crate::cypher::executor::ExecError::PreparedTransactionUnresolved {
                reason, ..
            } => unresolved_error(reason, format!("Cypher execution error: {e}")),
            // A missing privilege (#636): 403 with Neo4j's code.
            crate::cypher::executor::ExecError::Forbidden { message } => {
                caller
                    .security
                    .record_forbidden(caller.origin, caller.user, &message, Some(query));
                forbidden(message)
            }
            e => ApiError::BadRequest(format!("Cypher execution error: {e}")),
        }
    })?;
    Ok(exec_result_to_response(result))
}

/// `GET /transactions/prepared` — the prepared, unresolved two-phase-commit
/// transactions of the default database (#556), by ascending gid.
async fn list_prepared_http(State(state): State<NativeApiState>) -> Json<serde_json::Value> {
    let prepared: Vec<serde_json::Value> = state
        .service
        .list_prepared()
        .into_iter()
        .map(|p| {
            serde_json::json!({
                "gid": p.gid,
                "prepared_at": crate::cypher::executor::iso8601_utc(p.prepared_at_ms),
                "op_count": p.op_count,
            })
        })
        .collect();
    Json(serde_json::json!({ "prepared": prepared }))
}

/// Map a failed two-phase-commit resolution onto an HTTP error.
fn resolve_error(e: crate::native::ResolveError) -> ApiError {
    use crate::cypher::executor::UnresolvedPrepared;
    match e {
        crate::native::ResolveError::UnknownGid(_) => {
            unresolved_error(UnresolvedPrepared::UnknownGid, e.to_string())
        }
        crate::native::ResolveError::HeuristicRollback(_) => {
            unresolved_error(UnresolvedPrepared::HeuristicRollback, e.to_string())
        }
        crate::native::ResolveError::Io(msg) => {
            ApiError::Db(crate::error::DrevoError::Io(std::io::Error::other(msg)))
        }
        crate::native::ResolveError::ReadOnly => ApiError::Db(crate::error::DrevoError::ReadOnly),
    }
}

/// 404 for an unknown gid, 409 for a heuristic rollback, each with the stable
/// code Bolt reports (#590).
fn unresolved_error(
    reason: crate::cypher::executor::UnresolvedPrepared,
    message: String,
) -> ApiError {
    use crate::cypher::executor::UnresolvedPrepared;
    let status = match reason {
        UnresolvedPrepared::UnknownGid => axum::http::StatusCode::NOT_FOUND,
        UnresolvedPrepared::HeuristicRollback => axum::http::StatusCode::CONFLICT,
    };
    ApiError::Coded {
        status,
        code: reason.code(),
        message,
    }
}

/// `POST /transactions/prepared/{gid}/commit` — apply a prepared transaction
/// (#556). 404 for an unknown gid.
async fn commit_prepared_http(
    State(state): State<NativeApiState>,
    axum::extract::Path(gid): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let service = Arc::clone(&state.service);
    run_blocking(|| service.commit_prepared(&gid)).map_err(resolve_error)?;
    Ok(Json(serde_json::json!({ "gid": gid, "committed": true })))
}

/// `POST /transactions/prepared/{gid}/rollback` — discard a prepared
/// transaction (#556). 404 for an unknown gid.
async fn rollback_prepared_http(
    State(state): State<NativeApiState>,
    axum::extract::Path(gid): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .service
        .rollback_prepared(&gid)
        .map_err(resolve_error)?;
    Ok(Json(serde_json::json!({ "gid": gid, "rolled_back": true })))
}

/// `POST /transactions/prepared/{gid}/heuristic-rollback` — an operator rolls
/// back an abandoned prepared transaction heuristically (#556): a later
/// coordinator commit gets 409 instead of a silent 404.
async fn heuristic_rollback_prepared_http(
    State(state): State<NativeApiState>,
    axum::extract::Path(gid): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .service
        .heuristic_rollback_prepared(&gid)
        .map_err(resolve_error)?;
    Ok(Json(
        serde_json::json!({ "gid": gid, "rolled_back": true, "heuristic": true }),
    ))
}

/// Query string of `GET /problems`.
#[derive(Debug, serde::Deserialize)]
struct ProblemsQuery {
    /// Return problems with `seq >= since` (default 0: everything retained).
    #[serde(default)]
    since: u64,
}

/// Query string of `GET /report`.
#[derive(Debug, serde::Deserialize)]
struct ReportQuery {
    /// The database to describe (default: the default database).
    db: Option<String>,
}

/// The `owner/repo` of this crate's GitHub repository, for the Web UI's
/// prefilled issue link, from `Cargo.toml`'s `repository`.
fn issue_repo() -> &'static str {
    env!("CARGO_PKG_REPOSITORY")
        .trim_end_matches('/')
        .trim_start_matches("https://github.com/")
}

/// `GET /report?db=<name>` — the server half of the Web UI's "Report a
/// problem" (#552): build and engine, effective limits, the secret-free
/// embeddings status, the database's size and labels, and the recent problem
/// feed. Everything here may end up in a public GitHub issue, so nothing
/// secret is included (the embeddings status carries only `api_key_set`).
async fn problem_report(
    State(state): State<NativeApiState>,
    axum::extract::Query(q): axum::extract::Query<ReportQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (database, service) = match q.db {
        Some(name) => {
            let service = state
                .registry
                .get(&name)
                .ok_or_else(|| ApiError::NotFound(format!("database '{name}' not found")))?;
            (name, service)
        }
        None => (
            state.registry.default_name().to_string(),
            Arc::clone(&state.service),
        ),
    };
    let (nodes, edges, labels) = run_blocking(|| graph_summary(&service))?;
    let (problems, _) = crate::problems::ProblemLog::global().since(0);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    Ok(Json(serde_json::json!({
        "generated_at": crate::cypher::executor::iso8601_utc(now_ms),
        "server": {
            "name": "drevo",
            "version": crate::VERSION,
            "git_sha": crate::GIT_SHA,
            "build_date": crate::BUILD_DATE,
            "engine": "native-durable",
            "uptime_seconds": state.started_at.elapsed().as_secs(),
            "origin": state.service.origin_id().0.to_string(),
        },
        "config": {
            "statement_timeout_ms": service
                .statement_timeout()
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            "embeddings": state.embeddings_config.as_ref().map(|store| store.status()),
        },
        "graph": {
            "database": database,
            "nodes": nodes,
            "edges": edges,
            "labels": labels,
        },
        "problems": problems,
        "issue_repo": issue_repo(),
    })))
}

/// `(nodes, edges, labels)` of `service`, through Cypher so the scans are
/// bounded by the statement timeout like any other read.
fn graph_summary(service: &NativeService) -> Result<(i64, i64, Vec<String>), ApiError> {
    let run = |text: &str| -> Result<Vec<Vec<crate::cypher::executor::Value>>, ApiError> {
        let failed =
            |e: String| ApiError::Db(crate::error::DrevoError::Io(std::io::Error::other(e)));
        let ast = parser::parse(text).map_err(|e| failed(e.to_string()))?;
        Ok(service
            .execute(&ast, std::collections::HashMap::new())
            .map_err(|e| failed(e.to_string()))?
            .rows)
    };
    let count = |text: &str| -> Result<i64, ApiError> {
        Ok(match run(text)?.first().and_then(|row| row.first()) {
            Some(crate::cypher::executor::Value::Integer(n)) => *n,
            _ => 0,
        })
    };
    let nodes = count("MATCH (n) RETURN count(n)")?;
    let edges = count("MATCH ()-[r]->() RETURN count(r)")?;
    let labels = run("CALL db.labels() YIELD label RETURN label ORDER BY label")?
        .into_iter()
        .filter_map(|row| match row.into_iter().next() {
            Some(crate::cypher::executor::Value::String(s)) => Some(s),
            _ => None,
        })
        .collect();
    Ok((nodes, edges, labels))
}

/// `GET /problems?since=<seq>` — the server's recent WARN/ERROR events (#552),
/// oldest first, plus the `next` cursor for incremental polling by the Web UI.
async fn problems(
    axum::extract::Query(q): axum::extract::Query<ProblemsQuery>,
) -> Json<serde_json::Value> {
    let (problems, next) = crate::problems::ProblemLog::global().since(q.since);
    Json(serde_json::json!({ "problems": problems, "next": next }))
}

/// Handle a catalog admin command (issue #523): `SHOW DATABASES` lists the
/// registry, `CREATE DATABASE` registers a new in-memory database, and `USE`
/// routes a trailing query to the named database. A bare `USE` (no trailing
/// query) only validates that the database exists — an HTTP request carries no
/// session to hold the selection across requests.
fn handle_admin_cypher(
    state: &NativeApiState,
    command: crate::cypher::admin::AdminCommand,
    params: serde_json::Map<String, serde_json::Value>,
    caller: &Caller<'_>,
) -> Result<CypherResponse, ApiError> {
    use crate::cypher::admin::AdminCommand;
    let context = caller.context;
    match command {
        AdminCommand::ShowDatabases => Ok(show_databases_response(state, context)),
        AdminCommand::CreateDatabase { .. }
            if context.is_some_and(|c| {
                !c.has_dbms(drevo_auth::privileges::Action::DatabaseManagement)
            }) =>
        {
            let reason = "CREATE DATABASE needs DATABASE MANAGEMENT";
            caller
                .security
                .record_forbidden(caller.origin, caller.user, reason, None);
            Err(forbidden(reason.to_string()))
        }
        AdminCommand::CreateDatabase {
            name,
            if_not_exists,
        } => match state.registry.create(&name) {
            Ok(_) => Ok(empty_cypher_response()),
            // `IF NOT EXISTS` makes a pre-existing name a no-op, not a 409.
            Err(RegistryError::AlreadyExists(_)) if if_not_exists => Ok(empty_cypher_response()),
            Err(e) => Err(e.into()),
        },
        AdminCommand::AlterDatabaseAccess { .. }
            if context.is_some_and(|c| {
                !c.has_dbms(drevo_auth::privileges::Action::DatabaseManagement)
            }) =>
        {
            let reason = "ALTER DATABASE needs DATABASE MANAGEMENT";
            caller
                .security
                .record_forbidden(caller.origin, caller.user, reason, None);
            Err(forbidden(reason.to_string()))
        }
        AdminCommand::AlterDatabaseAccess { name, read_only } => {
            state.registry.set_access(&name, read_only)?;
            Ok(empty_cypher_response())
        }
        AdminCommand::Use { name, query } => {
            let service = state
                .registry
                .get(&name)
                .ok_or_else(|| ApiError::NotFound(format!("database '{name}' not found")))?;
            match query {
                Some(inner) => {
                    run_blocking(|| run_cypher_on(&service, &name, &inner, params, caller))
                }
                None => Ok(empty_cypher_response()),
            }
        }
    }
}

/// Run a user-management command (`CREATE USER`, `SHOW USERS`, …). There is
/// no signed-in user on HTTP yet, so the current-user commands see none.
fn run_security_command(
    state: &NativeApiState,
    parsed: Result<crate::cypher::security_admin::SecurityCommand, String>,
    params: &serde_json::Map<String, serde_json::Value>,
    current_user: Option<&crate::security::SignedIn>,
    origin: &crate::security::Origin,
) -> Result<CypherResponse, ApiError> {
    use crate::security::{ParamValue, SecurityError};
    let lookup = |name: &str| {
        params.get(name).map(|v| match v {
            serde_json::Value::String(s) => ParamValue::String(s.clone()),
            _ => ParamValue::Other,
        })
    };
    let outcome = parsed.map_err(SecurityError::syntax).and_then(|command| {
        state
            .security
            .execute_from(origin, &command, current_user, &lookup)
    });
    match outcome {
        Ok(result) => Ok(exec_result_to_response(result)),
        Err(e) => Err(security_error(e)),
    }
}

/// A [`SecurityError`](crate::security::SecurityError) as an HTTP error with
/// its Neo4j code.
fn security_error(e: crate::security::SecurityError) -> ApiError {
    ApiError::Coded {
        status: StatusCode::from_u16(e.kind.http_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        code: e.kind.code(),
        message: e.message,
    }
}

/// 403 for a missing privilege, with Neo4j's code (#636).
fn forbidden(message: String) -> ApiError {
    ApiError::Coded {
        status: StatusCode::FORBIDDEN,
        code: "Neo.ClientError.Security.Forbidden",
        message,
    }
}

fn credentials_expired() -> ApiError {
    security_error(crate::security::SecurityError {
        kind: crate::security::SecurityErrorKind::CredentialsExpired,
        message: crate::security::CHANGE_PASSWORD_FIRST.to_string(),
    })
}

/// The name of the Web UI's session cookie.
const SESSION_COOKIE: &str = "drevo_session";

/// The session token from a request's `drevo_session` cookie.
fn session_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == SESSION_COOKIE && !value.is_empty()).then(|| value.to_string())
        })
}

/// Paths a client may reach without logging in: health and server info, the
/// Web UI's own files, and the sign-in itself.
fn is_open_path(method: &axum::http::Method, path: &str) -> bool {
    use axum::http::Method;
    let read = method == Method::GET || method == Method::HEAD;
    (read && matches!(path, "/" | "/health" | "/ready" | "/status"))
        || (read && (path == "/ui" || path.starts_with("/ui/")))
        || (method == Method::POST && path == "/auth/login")
}

/// Login check (issue #634): when the server requires a login, every request
/// outside [`is_open_path`] must carry `Authorization: Basic` / `Bearer` or
/// the session cookie. The signed-in user goes into the request extensions.
/// A user who must change the password reaches only `/cypher` (for
/// `ALTER CURRENT USER`) and `/auth/*`.
async fn require_login(
    State(state): State<NativeApiState>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // Where the request comes from, for the audit log (#637); the handlers
    // read it from the request's extensions.
    let origin = crate::security::Origin::new("http").from_client(
        req.extensions()
            .get::<axum::extract::ConnectInfo<ClientAddr>>()
            .map(|c| c.0 .0),
    );
    req.extensions_mut().insert(origin.clone());
    if !state.security.requires_login() || is_open_path(req.method(), req.uri().path()) {
        return next.run(req).await;
    }
    let authorization = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let cookie = session_cookie(req.headers());
    let user = match state
        .security
        .login_http(&origin, authorization.as_deref(), cookie.as_deref())
    {
        Ok(user) => user,
        Err(e) => {
            let unauthenticated = e.kind == crate::security::SecurityErrorKind::Unauthenticated;
            let mut resp = security_error(e).into_response();
            if unauthenticated {
                // A Bearer challenge, so browsers do not pop up their own
                // Basic login box over the Web UI's sign-in screen.
                resp.headers_mut().insert(
                    axum::http::header::WWW_AUTHENTICATE,
                    axum::http::HeaderValue::from_static(r#"Bearer realm="drevo""#),
                );
            }
            return resp;
        }
    };
    let path = req.uri().path();
    if user.password_change_required && path != "/cypher" && !path.starts_with("/auth/") {
        return credentials_expired().into_response();
    }
    if let Some(refused) = refuse_route(&state, req.method(), path, &user) {
        state
            .security
            .record_forbidden(&origin, Some(&user.name), &refused, None);
        return forbidden(refused).into_response();
    }
    req.extensions_mut().insert(user);
    next.run(req).await
}

/// What a route outside `/cypher` needs (#636). These routes serve the graph
/// as stored, without the per-user filtering of `/cypher`, so they need
/// privileges under which filtering would change nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouteNeed {
    /// Nothing beyond a login.
    Login,
    /// Reading every node, relationship and property.
    ReadGraph,
    /// Reading and writing everything.
    WriteGraph,
    /// `DATABASE MANAGEMENT`.
    DatabaseManagement,
    /// `ALL DBMS PRIVILEGES`: server administration.
    Admin,
}

fn route_need(method: &axum::http::Method, path: &str) -> RouteNeed {
    let read = method == axum::http::Method::GET;
    if path.starts_with("/export/")
        || path == "/search/fts"
        || path == "/facets"
        || path == "/paths/shortest"
    {
        RouteNeed::ReadGraph
    } else if path == "/nodes" || path == "/edges" || path.starts_with("/nodes/") {
        if read {
            RouteNeed::ReadGraph
        } else {
            RouteNeed::WriteGraph
        }
    } else if path == "/import/json" {
        RouteNeed::WriteGraph
    } else if (path == "/databases" && !read) || path.starts_with("/databases/") {
        RouteNeed::DatabaseManagement
    } else if path.starts_with("/storage/")
        || path.starts_with("/admin/")
        || path.starts_with("/config/")
        || path.starts_with("/transactions/")
        || path == "/problems"
        || path == "/report"
    {
        RouteNeed::Admin
    } else {
        RouteNeed::Login
    }
}

/// Why `user` may not use this route, or `None` when they may.
fn refuse_route(
    state: &NativeApiState,
    method: &axum::http::Method,
    path: &str,
    user: &crate::security::SignedIn,
) -> Option<String> {
    use drevo_auth::privileges::Action;
    let need = route_need(method, path);
    if need == RouteNeed::Login {
        return None;
    }
    let database = state.registry.default_name();
    let context = state.security.context_for(Some(user), database)?;
    let allowed = match need {
        RouteNeed::Login => true,
        RouteNeed::ReadGraph => context.reads_everything(database),
        RouteNeed::WriteGraph => {
            context.reads_everything(database) && context.writes_everything(database)
        }
        RouteNeed::DatabaseManagement => context.has_dbms(Action::DatabaseManagement),
        RouteNeed::Admin => context.has_dbms(Action::AllDbms),
    };
    (!allowed).then(|| match need {
        RouteNeed::ReadGraph => format!(
            "{path} serves the whole graph and needs read access to all of it; \
             query through /cypher, which shows what you may see"
        ),
        RouteNeed::WriteGraph => {
            format!("{path} writes the graph and needs read and write access to all of it")
        }
        RouteNeed::DatabaseManagement => format!("{path} needs DATABASE MANAGEMENT"),
        _ => format!("{path} is server administration and needs ALL DBMS PRIVILEGES"),
    })
}

/// `POST /auth/login` body.
#[derive(serde::Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

/// `POST /auth/login {username, password}` — log in and start a session: the
/// reply carries the session token (for `Authorization: Bearer`) and sets it
/// as the Web UI's `drevo_session` cookie (HttpOnly, SameSite=Strict).
async fn auth_login(
    State(state): State<NativeApiState>,
    origin: Option<axum::Extension<crate::security::Origin>>,
    body: Result<Json<LoginRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(LoginRequest { username, password }) = body?;
    let security = state.security.clone();
    let origin = origin.map_or_else(|| crate::security::Origin::new("http"), |o| o.0);
    let (user, token) = run_blocking(move || {
        let user = security
            .login(&origin, Some("basic"), Some(&username), Some(&password))
            .map_err(security_error)?;
        let token = security.issue_session(&user.name).ok_or_else(|| {
            security_error(crate::security::SecurityError {
                kind: crate::security::SecurityErrorKind::Unauthenticated,
                message: crate::security::LOGIN_REFUSED.to_string(),
            })
        })?;
        Ok::<_, ApiError>((user, token))
    })?;
    let mut resp = Json(serde_json::json!({
        "token": token,
        "user": user.name,
        "passwordChangeRequired": user.password_change_required,
    }))
    .into_response();
    let cookie = format!(
        "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/{}",
        secure_flag(&state)
    );
    if let Ok(value) = axum::http::HeaderValue::from_str(&cookie) {
        resp.headers_mut()
            .insert(axum::http::header::SET_COOKIE, value);
    }
    Ok(resp)
}

/// `POST /auth/logout` — end the caller's session (cookie or Bearer token) and
/// clear the cookie.
async fn auth_logout(
    State(state): State<NativeApiState>,
    origin: Option<axum::Extension<crate::security::Origin>>,
    signed_in: Option<axum::Extension<crate::security::SignedIn>>,
    headers: axum::http::HeaderMap,
) -> Response {
    if let Some(axum::Extension(user)) = &signed_in {
        let origin = origin.map_or_else(|| crate::security::Origin::new("http"), |o| o.0);
        state.security.record_logout(&origin, &user.name);
    }
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .map(|t| t.trim().to_string());
    for token in [bearer, session_cookie(&headers)].into_iter().flatten() {
        state.security.revoke_session(&token);
    }
    let mut resp = Json(serde_json::json!({})).into_response();
    let cleared = format!(
        "{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0{}",
        secure_flag(&state)
    );
    if let Ok(value) = axum::http::HeaderValue::from_str(&cleared) {
        resp.headers_mut()
            .insert(axum::http::header::SET_COOKIE, value);
    }
    resp
}

/// `; Secure` when the server serves HTTPS, else nothing.
fn secure_flag(state: &NativeApiState) -> &'static str {
    if state.secure_cookies {
        "; Secure"
    } else {
        ""
    }
}

/// `GET /auth/me` — the signed-in user: `{user, passwordChangeRequired}`;
/// `user` is `null` when the server needs no login.
async fn auth_me(
    signed_in: Option<axum::Extension<crate::security::SignedIn>>,
) -> Json<serde_json::Value> {
    match signed_in {
        Some(axum::Extension(user)) => Json(serde_json::json!({
            "user": user.name,
            "passwordChangeRequired": user.password_change_required,
        })),
        None => Json(serde_json::json!({ "user": null, "passwordChangeRequired": false })),
    }
}

/// A `SHOW DATABASES` result: one `name` column, one row per database.
fn show_databases_response(
    state: &NativeApiState,
    context: Option<&drevo_auth::privileges::SecurityContext>,
) -> CypherResponse {
    // A database the signed-in user may not access is not listed (#636).
    let rows = state
        .registry
        .list()
        .into_iter()
        .filter(|name| context.is_none_or(|c| c.can_access(name)))
        .map(|name| {
            let access = access_of(&state.registry, &name);
            vec![
                serde_json::Value::String(name),
                serde_json::Value::String(access.to_string()),
            ]
        })
        .collect();
    CypherResponse {
        columns: vec!["name".to_string(), "access".to_string()],
        rows,
        stats: crate::api::CypherStats::default(),
        graph: crate::api::CypherGraph::default(),
    }
}

/// The `/databases` answer: every name, the default and each one's access.
fn database_list(registry: &DatabaseRegistry) -> DatabaseListResponse {
    let databases = registry.list();
    let access = databases
        .iter()
        .map(|name| (name.clone(), access_of(registry, name)))
        .collect();
    DatabaseListResponse {
        databases,
        default: registry.default_name(),
        access,
        read_only_server: registry.server_read_only(),
    }
}

/// A database's access as `SHOW DATABASES` reports it: `read-only` or
/// `read-write`.
fn access_of(registry: &DatabaseRegistry, name: &str) -> &'static str {
    if registry.is_read_only(name) {
        "read-only"
    } else {
        "read-write"
    }
}

/// An empty successful Cypher result (no columns, no rows) — the acknowledgement
/// for a `CREATE DATABASE` or a bare `USE`.
fn empty_cypher_response() -> CypherResponse {
    CypherResponse {
        columns: Vec::new(),
        rows: Vec::new(),
        stats: crate::api::CypherStats::default(),
        graph: crate::api::CypherGraph::default(),
    }
}
