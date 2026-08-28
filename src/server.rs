//! Server-binary configuration and runtime helpers.
//!
//! Introduced by Phase 8.5 audit task `00112` to lift the previously
//! inlined env-var parsing out of `src/bin/server.rs` so each rule
//! (port bounds, host validity, data-dir non-emptiness) lives behind a
//! unit test. The binary itself is now a thin shim around
//! [`crate::server::Config::from_env`] + [`crate::server::run`].
//!
//! ## Rules this module implements
//!
//! - `drevo-rust` §"Error Handling" — _"Never `unwrap()` / `expect()`
//!   in library code"._ Every fallible code path returns a typed
//!   [`crate::server::ConfigError`]; the binary's `main` is the single
//!   boundary that logs the error and exits with a non-zero status.
//! - `drevo-rust` §"Async / Tokio" — the public API is sync where
//!   nothing awaits; only [`crate::server::run`] is `async`.
//! - `drevo-database` §"HTTP API" — the container convention
//!   (`0.0.0.0:8080`, `/data/drevo.redb`) is encoded as the
//!   [`crate::server::Config`] defaults so test code and the binary agree.
//!
//! ## Environment variables
//!
//! | Variable          | Default     | Description                       |
//! |-------------------|-------------|-----------------------------------|
//! | `DREVO_HOST`      | `0.0.0.0`   | Bind address (IPv4, IPv6, or DNS) |
//! | `DREVO_PORT`      | `8080`      | TCP port (1..=65535)              |
//! | `DREVO_DATA_DIR`  | `/data`     | Directory holding `drevo.redb`    |
//! | `DREVO_ENGINE`    | `kv`        | Cypher execution engine: `kv` (today's storage engine) or `native` (read-only queries served from the in-memory native mirror, writes still on KV — RFC #307 Phase 6) |
//!
//! With the `embeddings-proxy` feature (Phase 19 task `00217`), three more
//! variables opt the server into hosting `POST /v1/embeddings` by proxying a
//! configured upstream. They are read only when the feature is compiled in;
//! the upstream is taken solely from configuration, never from a request (the
//! SSRF boundary — OWASP A10):
//!
//! | Variable                    | Default | Description                         |
//! |-----------------------------|---------|-------------------------------------|
//! | `DREVO_EMBEDDINGS_UPSTREAM` | (unset) | Upstream embeddings URL (http/https); unset ⇒ `/v1/embeddings` answers 503 |
//! | `DREVO_EMBEDDINGS_API_KEY`  | (unset) | Bearer token forwarded to the upstream |
//! | `DREVO_EMBEDDINGS_MODEL`    | (unset) | Default model when a request omits `model` |
//!
//! ## Signal handling
//!
//! Graceful shutdown is driven by [`crate::server::shutdown_signal`]. On Unix it
//! races `SIGINT` (Ctrl+C) and `SIGTERM`; on non-Unix targets only
//! `Ctrl+C` is observed — Windows console `Ctrl+Break` and Windows
//! service-stop notifications are **not** wired today and the process
//! relies on `Ctrl+C` or `axum::serve`'s implicit drop. Tracked as a
//! Phase 8.5 follow-up under task `00113`'s cross-cutting items.

#![cfg(feature = "http")]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use crate::api::{build_router, ApiState};
use crate::bolt::listener::{accept_and_run_session, accept_and_run_session_with_mirror};
use crate::catalog::Catalog;

/// Default bind address — every interface (container convention).
const DEFAULT_HOST: &str = "0.0.0.0";
/// Default HTTP port — 8080 is the de-facto unprivileged HTTP port
/// for containers.
const DEFAULT_PORT: u16 = 8080;
/// Default data directory — matches the volume mount in `Dockerfile`.
const DEFAULT_DATA_DIR: &str = "/data";
/// Filename of the redb file inside [`DEFAULT_DATA_DIR`].
const DB_FILENAME: &str = "drevo.redb";

/// First non-privileged TCP port on most POSIX systems. Ports below
/// this value require `CAP_NET_BIND_SERVICE` (or root). Operators can
/// still set them — [`Config::is_privileged_port`] flags it so the
/// binary can emit an explicit warning.
const PRIVILEGED_PORT_CEILING: u16 = 1024;

/// Server-binary configuration parsed from environment variables.
///
/// Construct with [`Config::from_env`]; consume via [`run`].
#[derive(Debug, Clone)]
pub struct Config {
    /// Bind host. Accepts IPv4/IPv6 literals (`0.0.0.0`, `::1`) and
    /// DNS names (validated lazily inside [`Config::socket_addr`]).
    pub host: String,
    /// TCP port to listen on.
    pub port: u16,
    /// Directory that holds the redb database file. The full path is
    /// produced by [`Config::db_path`].
    pub data_dir: PathBuf,
    /// Which engine serves Cypher queries (engine flip, RFC #307 Phase 6).
    pub engine: EngineMode,
}

/// Cypher execution engine selection, parsed from `DREVO_ENGINE`.
///
/// `Native` routes read-only queries through the per-database
/// [`crate::native_mirror::NativeMirror`]; writes (and reads while the
/// mirror is stale) execute on the KV engine either way, so the choice
/// never affects durability or answers — only read latency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EngineMode {
    /// Every query executes on the KV storage engine (today's default).
    #[default]
    Kv,
    /// Read-only queries are served from the native read mirror.
    Native,
}

/// Errors produced while parsing or validating a [`Config`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// `DREVO_PORT` could not be parsed as a `u16` in `1..=65535`, or
    /// was explicitly zero (which `bind(2)` interprets as "kernel
    /// chooses a port" and is invariably an operator mistake here).
    #[error("invalid DREVO_PORT value `{value}`: {reason}")]
    InvalidPort {
        /// The raw env-var value.
        value: String,
        /// Human-readable parse failure.
        reason: String,
    },
    /// `DREVO_HOST` could not be resolved to a `SocketAddr` together
    /// with the configured port.
    #[error("invalid DREVO_HOST value `{value}`: {reason}")]
    InvalidHost {
        /// The raw env-var value.
        value: String,
        /// Human-readable parse failure.
        reason: String,
    },
    /// `DREVO_DATA_DIR` was set to an empty string. (Non-empty values
    /// — absolute or relative — are accepted; existence is verified
    /// later when [`Catalog::open`](crate::catalog::Catalog::open) tries to
    /// open the data directory.)
    #[error("invalid DREVO_DATA_DIR: {reason}")]
    InvalidDataDir {
        /// Human-readable parse failure.
        reason: String,
    },
    /// `DREVO_ENGINE` was set to something other than `kv` or `native`.
    #[error("invalid DREVO_ENGINE value `{value}`: expected `kv` or `native`")]
    InvalidEngine {
        /// The raw env-var value.
        value: String,
    },
}

impl Config {
    /// Parse a [`Config`] from a getter that mimics [`std::env::var`].
    ///
    /// Splitting the getter from `std::env` lets the tests exercise
    /// each validation rule deterministically without mutating the
    /// process-global environment.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when any of the three known variables
    /// is present but malformed. Missing variables fall back to the
    /// documented defaults (no error).
    pub fn from_env<F>(getter: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let host = getter("DREVO_HOST").unwrap_or_else(|| DEFAULT_HOST.to_string());
        if host.is_empty() {
            return Err(ConfigError::InvalidHost {
                value: host,
                reason: "host must not be empty".to_string(),
            });
        }

        let port = match getter("DREVO_PORT") {
            None => DEFAULT_PORT,
            Some(raw) => parse_port(&raw)?,
        };

        let data_dir_raw = getter("DREVO_DATA_DIR").unwrap_or_else(|| DEFAULT_DATA_DIR.to_string());
        if data_dir_raw.is_empty() {
            return Err(ConfigError::InvalidDataDir {
                reason: "DREVO_DATA_DIR must not be empty".to_string(),
            });
        }

        let engine = match getter("DREVO_ENGINE") {
            None => EngineMode::default(),
            Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
                "kv" => EngineMode::Kv,
                "native" => EngineMode::Native,
                _ => return Err(ConfigError::InvalidEngine { value: raw }),
            },
        };

        Ok(Self {
            host,
            port,
            data_dir: PathBuf::from(data_dir_raw),
            engine,
        })
    }

    /// Resolve the bind address.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidHost`] when `host:port` cannot be
    /// parsed as a [`SocketAddr`].
    pub fn socket_addr(&self) -> Result<SocketAddr, ConfigError> {
        // IPv6 literals contain colons and must be bracketed before
        // appending `:port` so the SocketAddr parser can disambiguate
        // the port separator from the address colons. The colon test
        // also accepts bracketed IPv6 input that the operator might
        // have provided directly (`[::1]`).
        let host_for_addr = if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let raw = format!("{}:{}", host_for_addr, self.port);
        raw.parse::<SocketAddr>()
            .map_err(|err| ConfigError::InvalidHost {
                value: self.host.clone(),
                reason: err.to_string(),
            })
    }

    /// Full path to the redb database file (`<data_dir>/drevo.redb`).
    #[must_use]
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join(DB_FILENAME)
    }

    /// True when the configured port is below `1024` and therefore
    /// requires elevated privileges on most POSIX systems. The binary
    /// uses this to emit a one-shot warning at startup; it never
    /// rejects the value.
    #[must_use]
    pub const fn is_privileged_port(&self) -> bool {
        self.port < PRIVILEGED_PORT_CEILING
    }
}

fn parse_port(raw: &str) -> Result<u16, ConfigError> {
    let port: u16 =
        raw.parse()
            .map_err(|err: std::num::ParseIntError| ConfigError::InvalidPort {
                value: raw.to_string(),
                reason: err.to_string(),
            })?;
    if port == 0 {
        return Err(ConfigError::InvalidPort {
            value: raw.to_string(),
            reason: "port must be in 1..=65535 (0 means 'kernel-chosen' which is not supported by the server)"
                .to_string(),
        });
    }
    Ok(port)
}

// ---------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------

/// Top-level runtime errors surfaced by [`run`]. Distinct from
/// [`ConfigError`] so the binary can choose different exit codes per
/// failure mode if it grows that need.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// Failed to bind the TCP listener.
    #[error("failed to bind TCP listener on {addr}: {source}")]
    Bind {
        /// Address that could not be bound.
        addr: SocketAddr,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Axum returned an error while serving requests.
    #[error("server error: {0}")]
    Serve(#[source] std::io::Error),
    /// Configuration was invalid.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Failed to open the multi-database catalog rooted at the data
    /// directory (scan failure or the default database could not be
    /// opened).
    #[error("failed to open database catalog: {0}")]
    CatalogOpen(#[from] crate::catalog::CatalogError),
    /// The embeddings proxy was requested via `DREVO_EMBEDDINGS_UPSTREAM` but
    /// its configuration is invalid (bad URL, unbuildable client). Fail fast
    /// so a misconfigured RAG backend is loud, not silently degraded.
    #[cfg(feature = "embeddings-proxy")]
    #[error("invalid embeddings configuration: {0}")]
    Embeddings(String),
}

/// Attach an embeddings backend to `state` from the environment, when the
/// `embeddings-proxy` feature is compiled in and `DREVO_EMBEDDINGS_UPSTREAM`
/// is set. A no-op otherwise, so the default binary keeps `/v1/embeddings`
/// answering `503` ("not configured").
#[cfg(feature = "embeddings-proxy")]
fn configure_embeddings(state: ApiState) -> Result<ApiState, RunError> {
    use crate::embeddings::{EmbeddingBackend, EmbeddingsConfig, ProxyBackend};
    match EmbeddingsConfig::from_env(|key| std::env::var(key).ok())
        .map_err(|e| RunError::Embeddings(e.to_string()))?
    {
        Some(cfg) => {
            tracing::info!(upstream = %cfg.upstream, "embeddings proxy enabled");
            let backend =
                ProxyBackend::new(cfg).map_err(|e| RunError::Embeddings(e.to_string()))?;
            Ok(state.with_embeddings_backend(EmbeddingBackend::Proxy(backend)))
        }
        None => Ok(state),
    }
}

/// No-op when the proxy backend is not compiled in — `/v1/embeddings` then
/// always answers `503`.
#[cfg(not(feature = "embeddings-proxy"))]
fn configure_embeddings(state: ApiState) -> Result<ApiState, RunError> {
    Ok(state)
}

/// Install a server-side query embedder on the catalog's default database so
/// `drevo.semantic.query` can embed query text (#251 slice 3), when the
/// `embeddings-proxy` feature is built and `DREVO_EMBEDDINGS_UPSTREAM` is set.
///
/// The embedder is set on the **default** handle only — the one HTTP `/cypher`
/// and the Bolt listener share (Bolt has no database selection wired yet), so
/// this covers both server-side Cypher paths. Databases opened lazily by name
/// through the catalog do not receive it and report "not configured"; wiring
/// the multi-database case is a follow-up. A no-op (leaving
/// `drevo.semantic.query` to report "not configured") when the feature is off
/// or the upstream is unset.
#[cfg(feature = "embeddings-proxy")]
fn configure_query_embedder(catalog: &crate::catalog::Catalog) -> Result<(), RunError> {
    use crate::embeddings::{EmbeddingsConfig, SyncEmbedder};
    match EmbeddingsConfig::from_env(|key| std::env::var(key).ok())
        .map_err(|e| RunError::Embeddings(e.to_string()))?
    {
        Some(cfg) => {
            let embedder =
                SyncEmbedder::from_config(cfg).map_err(|e| RunError::Embeddings(e.to_string()))?;
            if catalog
                .default_db()
                .set_embedder(std::sync::Arc::new(embedder))
            {
                tracing::info!("drevo.semantic.query embedder installed on default database");
            }
            Ok(())
        }
        None => Ok(()),
    }
}

/// No-op when the proxy backend is not compiled in.
#[cfg(not(feature = "embeddings-proxy"))]
fn configure_query_embedder(_catalog: &crate::catalog::Catalog) -> Result<(), RunError> {
    Ok(())
}

/// Open the database, bind the TCP listener, and serve until a
/// shutdown signal is observed.
///
/// All log lines go through `tracing` — initialise the subscriber in
/// `main` before calling. The function is async because it awaits the
/// server future.
///
/// # Errors
///
/// Returns [`RunError`] on database-open, bind, or serve failure. The
/// caller (`main`) is expected to log the error and exit with a
/// non-zero status.
pub async fn run(cfg: Config) -> Result<(), RunError> {
    // Announce the running build up front so every log stream records which
    // drevo binary is serving — parity with the version reported by `/`,
    // `/status`, and the Bolt handshake. This is the first thing `run` does, so
    // the `tests/server_binary_tests.rs` wiring test can observe it immediately.
    tracing::info!(version = crate::VERSION, "starting drevo");
    let addr = cfg.socket_addr()?;

    if cfg.is_privileged_port() {
        tracing::warn!(
            port = cfg.port,
            "DREVO_PORT is below 1024 — most systems require CAP_NET_BIND_SERVICE \
             (or root) to bind privileged ports"
        );
    }

    // Open the multi-database catalog rooted at the data directory. Every
    // `<name>.redb` file becomes a database; `default` maps to the legacy
    // `drevo.redb`, so a pre-catalog data directory opens unchanged. The
    // default handle is what the Bolt listener shares (Bolt has no
    // database-selection wired yet).
    tracing::info!(dir = %cfg.data_dir.display(), "opening database catalog");
    let catalog = Arc::new(Catalog::open(cfg.data_dir.clone())?);
    tracing::info!(databases = ?catalog.list(), "catalog ready");
    // Install the server-side query embedder for `drevo.semantic.query` on the
    // shared default handle (#251 slice 3); no-op unless `embeddings-proxy` is
    // built and `DREVO_EMBEDDINGS_UPSTREAM` is set.
    configure_query_embedder(&catalog)?;
    // Engine flip (RFC #307 Phase 6): in native mode, hand out per-database
    // read mirrors and warm the default database's one so the first reads
    // are already served natively. A failed initial build only degrades to
    // KV-served reads (never wrong answers), so it warns instead of failing
    // startup.
    let mirrors = match cfg.engine {
        EngineMode::Kv => None,
        EngineMode::Native => {
            let registry = Arc::new(crate::native_mirror::MirrorRegistry::new());
            let default_db = catalog.default_db();
            let default_mirror = registry.for_db(&default_db);
            match default_mirror.rebuild_blocking(&default_db) {
                Ok(()) => tracing::info!("engine=native — read mirror warm on default database"),
                Err(err) => tracing::warn!(
                    error = %err,
                    "engine=native — initial mirror build failed; reads fall back to KV until a rebuild succeeds"
                ),
            }
            Some(registry)
        }
    };
    let state = ApiState::with_catalog(Arc::clone(&catalog));
    let state = match &mirrors {
        Some(registry) => state.with_native_mirrors(Arc::clone(registry)),
        None => state,
    };
    // Opt-in embeddings proxy (Phase 19 task `00217`); no-op unless the
    // `embeddings-proxy` feature is built and `DREVO_EMBEDDINGS_UPSTREAM` set.
    let state = configure_embeddings(state)?;
    let db = Arc::clone(&state.db);
    let shutdown_state = state.clone();
    let router = build_router(state);

    // Optional Bolt protocol listener (Neo4j-compatible), opt-in via the
    // `DREVO_BOLT_PORT` env var. It shares the SAME `Drevo` handle as the HTTP
    // server: redb is single-process, so HTTP + Web UI + Bolt must live in one
    // process on one handle (you cannot run a second process against the same
    // file). Runs without authentication (Authenticator = None). Sessions end
    // when the process exits — no separate graceful drain.
    if let Some(bolt_port) = std::env::var("DREVO_BOLT_PORT")
        .ok()
        .and_then(|raw| raw.parse::<u16>().ok())
    {
        let bolt_addr = SocketAddr::new(addr.ip(), bolt_port);
        let bolt_listener = tokio::net::TcpListener::bind(bolt_addr)
            .await
            .map_err(|source| RunError::Bind {
                addr: bolt_addr,
                source,
            })?;
        tracing::info!(%bolt_addr, "bolt listening");
        let bolt_db = Arc::clone(&db);
        // In native mode Bolt shares the default database's read mirror —
        // Bolt has no database selection wired, so the default handle's
        // mirror covers every session.
        let bolt_mirror = mirrors.as_ref().map(|registry| registry.for_db(&db));
        tokio::spawn(async move {
            loop {
                match bolt_listener.accept().await {
                    Ok((socket, _peer)) => {
                        let conn_db = Arc::clone(&bolt_db);
                        let conn_mirror = bolt_mirror.clone();
                        tokio::spawn(async move {
                            let ended = match conn_mirror {
                                Some(mirror) => {
                                    accept_and_run_session_with_mirror(socket, &conn_db, &mirror)
                                        .await
                                }
                                None => accept_and_run_session(socket, &conn_db).await,
                            };
                            if let Err(err) = ended {
                                tracing::warn!(error = %err, "bolt session ended with error");
                            }
                        });
                    }
                    Err(err) => tracing::warn!(error = %err, "bolt accept failed"),
                }
            }
        });
    }

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|source| RunError::Bind { addr, source })?;

    tracing::info!(%addr, "listening");

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            // Flip /health and /ready to 503 *before* axum begins
            // draining in-flight requests so that load balancers stop
            // forwarding new traffic during the drain window.
            shutdown_state.signal_shutdown();
            tracing::info!("shutdown signal received, draining");
        })
        .await
        .map_err(RunError::Serve)?;

    tracing::info!("shut down cleanly");
    Ok(())
}

/// Future that resolves on the first observed shutdown signal.
///
/// Unix: races `SIGINT` (Ctrl+C) and `SIGTERM` (Kubernetes pod
/// termination). Non-Unix: only `Ctrl+C` is observed; `SIGTERM`
/// is unavailable on Windows. Windows console `Ctrl+Break` and
/// Windows service-control-manager stop notifications are tracked
/// as a follow-up under task `00113`.
///
/// Exposed publicly only for the unit-test in `tests/` that
/// asserts the future is `Send`.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            // Installing Ctrl+C handler twice on the same runtime
            // can fail; the operator can still kill the process.
            tracing::error!(error = %err, "failed to install Ctrl+C handler");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(err) => {
                tracing::error!(error = %err, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
