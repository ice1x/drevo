//! Server-binary configuration and runtime helpers.
//!
//! Introduced by Phase 8.5 audit task `00112` to lift the previously
//! inlined env-var parsing out of [`crate::bin::server`] so each rule
//! (port bounds, host validity, data-dir non-emptiness) lives behind a
//! unit test. The binary itself is now a thin shim around
//! [`Config::from_env`] + [`run`].
//!
//! ## Rules this module implements
//!
//! - `drevo-rust` §"Error Handling" — _"Never `unwrap()` / `expect()`
//!   in library code"._ Every fallible code path returns a typed
//!   [`ConfigError`]; the binary's `main` is the single boundary that
//!   logs the error and exits with a non-zero status.
//! - `drevo-rust` §"Async / Tokio" — the public API is sync where
//!   nothing awaits; only [`run`] is `async`.
//! - `drevo-database` §"HTTP API" — the container convention
//!   (`0.0.0.0:8080`, `/data/drevo.redb`) is encoded as the
//!   [`Config`] defaults so test code and the binary agree.
//!
//! ## Environment variables
//!
//! | Variable          | Default     | Description                       |
//! |-------------------|-------------|-----------------------------------|
//! | `DREVO_HOST`      | `0.0.0.0`   | Bind address (IPv4, IPv6, or DNS) |
//! | `DREVO_PORT`      | `8080`      | TCP port (1..=65535)              |
//! | `DREVO_DATA_DIR`  | `/data`     | Directory holding `drevo.redb`    |
//!
//! ## Signal handling
//!
//! Graceful shutdown is driven by [`shutdown_signal`]. On Unix it
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
use crate::db::Drevo;

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
    /// later when [`Drevo::open`] tries to create the file.)
    #[error("invalid DREVO_DATA_DIR: {reason}")]
    InvalidDataDir {
        /// Human-readable parse failure.
        reason: String,
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

        Ok(Self {
            host,
            port,
            data_dir: PathBuf::from(data_dir_raw),
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
    /// Failed to open the redb database file.
    #[error("failed to open database at {path}: {source}")]
    DatabaseOpen {
        /// Database path that could not be opened.
        path: PathBuf,
        /// Underlying [`crate::error::DrevoError`].
        #[source]
        source: crate::error::DrevoError,
    },
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
    let addr = cfg.socket_addr()?;
    let db_path = cfg.db_path();

    if cfg.is_privileged_port() {
        tracing::warn!(
            port = cfg.port,
            "DREVO_PORT is below 1024 — most systems require CAP_NET_BIND_SERVICE \
             (or root) to bind privileged ports"
        );
    }

    tracing::info!(path = %db_path.display(), "opening database");
    let db = Drevo::open(&db_path).map_err(|source| RunError::DatabaseOpen {
        path: db_path.clone(),
        source,
    })?;
    let state = ApiState::new(Arc::new(db));
    let shutdown_state = state.clone();
    let router = build_router(state);

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
/// Exposed publicly only for the unit-test in [`tests/`] that
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
