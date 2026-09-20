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
//! | `DREVO_DATA_DIR`  | `/data`     | Directory holding `native.wal`    |
//! | `DREVO_ENGINE`    | `native-durable` | Serving engine. `native-durable` (default) — the WAL-backed native engine IS the store of record (`<data_dir>/native.wal`), no KV. `kv` still parses but its serving mode was removed (epic #444): a `kv` process warns and is served by the native engine. |
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

/// Default bind address — every interface (container convention).
const DEFAULT_HOST: &str = "0.0.0.0";
/// Default HTTP port — 8080 is the de-facto unprivileged HTTP port
/// for containers.
const DEFAULT_PORT: u16 = 8080;
/// Default data directory — matches the volume mount in `Dockerfile`.
const DEFAULT_DATA_DIR: &str = "/data";

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
    /// Directory that holds the durable native store (`<data_dir>/native.wal`)
    /// and the persisted embeddings config.
    pub data_dir: PathBuf,
    /// Which engine serves Cypher queries (engine flip, RFC #307 Phase 6).
    pub engine: EngineMode,
}

/// Cypher execution engine selection, parsed from `DREVO_ENGINE`: the legacy
/// KV store, or the durable native engine (the store of record).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EngineMode {
    /// The durable native engine IS the store of record — no KV store at
    /// all — and the **only** serving mode (epic #444). Serves the full
    /// native HTTP surface ([`crate::native_api::build_native_router`]) and,
    /// when `DREVO_BOLT_PORT` is set, Bolt.
    #[default]
    NativeDurable,
    /// Legacy KV storage engine. Its *serving* path has been removed — the
    /// value still parses (the KV code still compiles for the test corpus),
    /// but [`run`] no longer serves it: a `DREVO_ENGINE=kv` process warns and
    /// serves with the durable native engine instead.
    Kv,
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
    /// later when the durable native store opens the data directory.)
    #[error("invalid DREVO_DATA_DIR: {reason}")]
    InvalidDataDir {
        /// Human-readable parse failure.
        reason: String,
    },
    /// `DREVO_ENGINE` was set to an unknown engine name.
    #[error("invalid DREVO_ENGINE value `{value}`: expected `kv`, `native`, or `native-durable`")]
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
                "native-durable" => EngineMode::NativeDurable,
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
    /// The durable native store could not be opened (WAL recovery,
    /// compaction, or index build failed) in `DREVO_ENGINE=native-durable`
    /// mode.
    #[error("failed to open the durable native store: {0}")]
    NativeOpen(#[source] crate::error::DrevoError),
    /// The embeddings proxy was requested via `DREVO_EMBEDDINGS_UPSTREAM` but
    /// its configuration is invalid (bad URL, unbuildable client). Fail fast
    /// so a misconfigured RAG backend is loud, not silently degraded.
    #[cfg(feature = "embeddings-proxy")]
    #[error("invalid embeddings configuration: {0}")]
    Embeddings(String),
    /// The text-to-Cypher proxy was requested via `DREVO_TEXT2CYPHER_UPSTREAM`
    /// but its configuration is invalid (issue #429).
    #[cfg(feature = "embeddings-proxy")]
    #[error("invalid text-to-Cypher configuration: {0}")]
    Text2Cypher(String),
}

/// Build the shared, persisted embeddings config store from `cfg`: the
/// `<data_dir>/embeddings_config.json` file (a value the operator set through
/// the Web UI on a previous run) wins, falling back to the classic
/// `DREVO_EMBEDDINGS_*` env vars. This single store backs the `/v1/embeddings`
/// proxy, the semantic-query embedder, and the `/config/embeddings` endpoint,
/// so a Web-UI change takes effect live and survives a restart.
#[cfg(feature = "embeddings-proxy")]
fn build_embeddings_store(
    cfg: &Config,
) -> Result<std::sync::Arc<crate::embeddings::EmbeddingsConfigStore>, RunError> {
    use crate::embeddings::{EmbeddingsConfig, EmbeddingsConfigStore};
    let env_cfg = EmbeddingsConfig::from_env(|key| std::env::var(key).ok())
        .map_err(|e| RunError::Embeddings(e.to_string()))?;
    Ok(EmbeddingsConfigStore::load(
        cfg.data_dir.join("embeddings_config.json"),
        env_cfg,
    ))
}

/// Without the proxy feature the store still backs the config endpoint; a
/// malformed env is swallowed rather than failing the boot (nothing proxies).
#[cfg(not(feature = "embeddings-proxy"))]
fn build_embeddings_store(
    cfg: &Config,
) -> Result<std::sync::Arc<crate::embeddings::EmbeddingsConfigStore>, RunError> {
    use crate::embeddings::{EmbeddingsConfig, EmbeddingsConfigStore};
    let env_cfg = EmbeddingsConfig::from_env(|key| std::env::var(key).ok()).unwrap_or(None);
    Ok(EmbeddingsConfigStore::load(
        cfg.data_dir.join("embeddings_config.json"),
        env_cfg,
    ))
}

/// Install the process-global text-to-Cypher generator (`drevo.cypher.fromText`,
/// issue #429) when the `embeddings-proxy` feature is built and
/// `DREVO_TEXT2CYPHER_UPSTREAM` is set. Engine-agnostic (a process global), so
/// it runs once for both the KV and durable-native serving paths. A no-op when
/// unconfigured — the procedure then reports "not configured".
#[cfg(feature = "embeddings-proxy")]
fn configure_text2cypher() -> Result<(), RunError> {
    use crate::text2cypher::{SyncCypherGenerator, Text2CypherConfig};
    let Some(config) = Text2CypherConfig::from_env(|key| std::env::var(key).ok())
        .map_err(|e| RunError::Text2Cypher(e.to_string()))?
    else {
        return Ok(());
    };
    let generator = SyncCypherGenerator::from_config(config)
        .map_err(|e| RunError::Text2Cypher(e.to_string()))?;
    crate::text2cypher::install(std::sync::Arc::new(generator));
    tracing::info!("drevo.cypher.fromText generator installed");
    Ok(())
}

/// No-op when the proxy backend is not compiled in.
#[cfg(not(feature = "embeddings-proxy"))]
fn configure_text2cypher() -> Result<(), RunError> {
    Ok(())
}

/// Install the server-side query embedder on the durable native service
/// (`DREVO_ENGINE=native-durable`), when the `embeddings-proxy` feature is
/// built and `DREVO_EMBEDDINGS_UPSTREAM` is set — so `drevo.semantic.embed`
/// and `drevo.semantic.query` work on the zero-redb server too. A no-op
/// otherwise (they report "not configured" / the capability error).
#[cfg(feature = "embeddings-proxy")]
fn configure_native_query_embedder(
    service: &crate::native_service::NativeService,
    store: std::sync::Arc<crate::embeddings::EmbeddingsConfigStore>,
) -> Result<(), RunError> {
    use crate::embeddings::SyncEmbedder;
    let embedder =
        SyncEmbedder::from_store(store).map_err(|e| RunError::Embeddings(e.to_string()))?;
    if service.set_embedder(std::sync::Arc::new(embedder)) {
        tracing::info!("semantic query embedder installed on the durable native store");
    }
    Ok(())
}

/// Attach the store-backed embeddings proxy to the durable-native HTTP state —
/// the native counterpart of `configure_embeddings`. Installed unconditionally
/// (when the feature is built) so a later Web-UI configuration enables
/// `/v1/embeddings` with no restart.
#[cfg(feature = "embeddings-proxy")]
fn configure_native_embeddings(
    state: crate::native_api::NativeApiState,
    store: std::sync::Arc<crate::embeddings::EmbeddingsConfigStore>,
) -> Result<crate::native_api::NativeApiState, RunError> {
    use crate::embeddings::{EmbeddingBackend, ProxyBackend};
    if let Some(snap) = store.snapshot() {
        tracing::info!(upstream = %snap.upstream, "embeddings proxy enabled");
    }
    let backend = ProxyBackend::new(store).map_err(|e| RunError::Embeddings(e.to_string()))?;
    Ok(state.with_embeddings_backend(EmbeddingBackend::Proxy(backend)))
}

/// No-op when the proxy backend is not compiled in — `/v1/embeddings` then
/// always answers `503`.
#[cfg(not(feature = "embeddings-proxy"))]
fn configure_native_embeddings(
    state: crate::native_api::NativeApiState,
    _store: std::sync::Arc<crate::embeddings::EmbeddingsConfigStore>,
) -> Result<crate::native_api::NativeApiState, RunError> {
    Ok(state)
}

/// No-op when the proxy backend is not compiled in.
#[cfg(not(feature = "embeddings-proxy"))]
fn configure_native_query_embedder(
    _service: &crate::native_service::NativeService,
    _store: std::sync::Arc<crate::embeddings::EmbeddingsConfigStore>,
) -> Result<(), RunError> {
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

    // Install the text-to-Cypher generator (issue #429) once, before the engine
    // branch: it is a process-global proxy, shared by both serving paths.
    configure_text2cypher()?;

    if cfg.is_privileged_port() {
        tracing::warn!(
            port = cfg.port,
            "DREVO_PORT is below 1024 — most systems require CAP_NET_BIND_SERVICE \
             (or root) to bind privileged ports"
        );
    }

    // The durable native engine is the only store of record and the only
    // serving path (epic #444 — the KV serving mode has been removed). A
    // `DREVO_ENGINE=kv` process still parses its config (the KV code compiles
    // for the test corpus) but is served by the native engine, with a warning
    // so the operator notices the stale setting.
    if cfg.engine == EngineMode::Kv {
        tracing::warn!(
            "DREVO_ENGINE=kv: the KV serving mode has been removed; serving with the durable \
             native engine instead. Drop DREVO_ENGINE or set it to `native-durable`."
        );
    }
    run_native_durable(cfg, addr).await
}

/// Serve `DREVO_ENGINE=native-durable`: the WAL-backed native engine as the
/// store of record — no KV store, no catalog. Opens (or creates)
/// `<data_dir>/native.wal`, compacts and indexes it, and serves the minimal
/// native HTTP surface plus (when `DREVO_BOLT_PORT` is set) a Bolt listener
/// whose sessions execute on the durable service — autocommit statements
/// only; `BEGIN` is refused until the executor can drive native
/// transactions.
async fn run_native_durable(cfg: Config, addr: SocketAddr) -> Result<(), RunError> {
    let wal = cfg.data_dir.join("native.wal");
    tracing::info!(wal = %wal.display(), "engine=native-durable — opening the durable native store");
    let service = std::sync::Arc::new(
        crate::native_service::NativeService::open(&wal).map_err(RunError::NativeOpen)?,
    );
    tracing::info!("durable native store ready");
    // Shared, persisted embeddings config store (Web-UI-settable), same as the
    // KV path: file wins over `DREVO_EMBEDDINGS_*`.
    let embeddings_store = build_embeddings_store(&cfg)?;
    // Opt-in server-side query embedder — the durable-engine counterpart of
    // `configure_query_embedder`; no-op unless the `embeddings-proxy` feature
    // is built.
    configure_native_query_embedder(&service, embeddings_store.clone())?;

    // Optional Bolt listener — same opt-in as the KV path, served by the
    // durable-native session (autocommit only; BEGIN is refused).
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
        tracing::info!(%bolt_addr, "bolt listening (native-durable)");
        let bolt_service = std::sync::Arc::clone(&service);
        tokio::spawn(async move {
            loop {
                match bolt_listener.accept().await {
                    Ok((socket, _peer)) => {
                        let conn_service = std::sync::Arc::clone(&bolt_service);
                        tokio::spawn(async move {
                            if let Err(err) = crate::bolt::listener::accept_and_run_session_durable(
                                socket,
                                &conn_service,
                            )
                            .await
                            {
                                tracing::warn!(error = %err, "bolt session ended with error");
                            }
                        });
                    }
                    Err(err) => tracing::warn!(error = %err, "bolt accept failed"),
                }
            }
        });
    }

    let state = crate::native_api::NativeApiState::new(service);
    // Opt-in embeddings proxy, exactly like the KV path — the restart
    // tooling probes POST /v1/embeddings after boot. Store-backed so the key is
    // Web-UI-settable.
    let state = state.with_embeddings_config_store(embeddings_store.clone());
    let state = configure_native_embeddings(state, embeddings_store)?;
    let shutdown_state = state.clone();
    let router = crate::native_api::build_native_router(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|source| RunError::Bind { addr, source })?;
    tracing::info!(%addr, "listening (native-durable)");

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
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
