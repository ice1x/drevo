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
//! | `DREVO_BACKUP_DIR` | `<data_dir>/backups` | Where `POST /admin/backup` writes physical backups |
//! | `DREVO_READ_ONLY` | `false`     | Serve every database read-only: reads, search and export work, writes and schema changes fail with `Neo.ClientError.General.ForbiddenOnReadOnlyDatabase`, and nothing in the data directory is written |
//! | `DREVO_AUTH`      | (unset)     | First user when there are none (`auth` feature), as Neo4j's `NEO4J_AUTH`: `user/password`, `none`, or unset for `neo4j` with a generated password that is logged once and must be changed. Users live in `<data_dir>/security.json`. |
//! | `DREVO_BOLT_PORT` | (unset)     | Serve Bolt on this port, same host as HTTP |
//! | `DREVO_TX_ISOLATION` | `snapshot` | When an explicit transaction may commit after others committed: `snapshot` (unless they touched the same records) or `serializable` (only if nothing committed) |
//! | `DREVO_TX_CONFLICT_HISTORY` | `10000` | Recent commits remembered to decide whether a transaction overlapped with them |
//! | `DREVO_MAX_CONNECTIONS` | `512` | Bolt connections served at once; one more is refused at `HELLO` with `Neo.TransientError.Request.NoThreadsAvailable`; `0` = no limit |
//! | `DREVO_HTTP_MAX_IN_FLIGHT` | `256` | HTTP requests served at once; one more gets `503` + `Retry-After`; health and metrics are exempt; `0` = no limit |
//! | `DREVO_IDLE_SESSION_TIMEOUT_MS` | `0` (off) | Close a Bolt connection that holds an open transaction and sends nothing for this long, rolling the transaction back |
//! | `DREVO_TLS_CERT`  | (unset)     | PEM certificate chain; with `DREVO_TLS_KEY`, HTTP is served as HTTPS, Bolt as `bolt+s://`, and gRPC and Arrow Flight over TLS (`bolt-tls` feature) |
//! | `DREVO_TLS_KEY`   | (unset)     | PEM private key (PKCS#8, PKCS#1 or SEC1) for `DREVO_TLS_CERT` |
//! | `DREVO_ENCRYPTION_KEY_FILE` | (unset) | File holding a 256-bit key as 64 hex digits; every database's log and sidecars are encrypted with it (`encryption` feature) |
//! | `DREVO_ENCRYPTION_KEY` | (unset) | The 64 hex digits themselves, instead of `DREVO_ENCRYPTION_KEY_FILE` |
//! | `DREVO_AUDIT_LOG` | `off`       | Security audit log (`auth` feature): `off`, `stdout`, or a file path (JSON lines, rotated at 10 MiB, 5 old files kept) |
//! | `DREVO_AUDIT_QUERY_TEXT` | `false` | Put a refused statement's text in its audit event |
//! | `DREVO_OIDC_ISSUER` | (unset) | Accept bearer tokens from this OpenID Connect provider (`oidc` feature): its exact `iss`, `https://` (or `http://` on this machine). Logins are then required |
//! | `DREVO_OIDC_AUDIENCE` | (required with an issuer) | A value the token's `aud` must hold, usually the client id |
//! | `DREVO_OIDC_USERNAME_CLAIM` | `sub` | The claim naming the user (`preferred_username`, `email`, …) |
//! | `DREVO_OIDC_ROLES_CLAIM` | `groups` | The claim listing the user's groups |
//! | `DREVO_OIDC_ROLE_MAP` | (none) | Groups to roles, `group=role` pairs: `admins=admin,devs=editor`. Unlisted groups give no role |
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
    /// Where `POST /admin/backup` writes backups (`DREVO_BACKUP_DIR`);
    /// `<data_dir>/backups` when unset.
    pub backup_dir: Option<PathBuf>,
    /// Which engine serves Cypher queries (engine flip, RFC #307 Phase 6).
    pub engine: EngineMode,
    /// Per-statement wall-clock limit from `DREVO_QUERY_TIMEOUT_MS` (#547);
    /// `None` (unset or `0`) means no limit.
    pub query_timeout: Option<std::time::Duration>,
    /// How long a Bolt connection holding an open transaction may send
    /// nothing before it is closed and the transaction rolled back, from
    /// `DREVO_IDLE_SESSION_TIMEOUT_MS`; `None` (unset or `0`) means no limit.
    pub idle_session_timeout: Option<std::time::Duration>,
    /// Bolt connections served at once, from `DREVO_MAX_CONNECTIONS` (default
    /// [`crate::limits::DEFAULT_MAX_CONNECTIONS`]; `0` = no limit). One more is
    /// refused at `HELLO` with `Neo.TransientError.Request.NoThreadsAvailable`.
    pub max_connections: usize,
    /// HTTP requests served at once, from `DREVO_HTTP_MAX_IN_FLIGHT` (default
    /// [`crate::limits::DEFAULT_HTTP_MAX_IN_FLIGHT`]; `0` = no limit). One more
    /// gets `503` with `Retry-After`; `/health`, `/ready` and `/metrics` are
    /// always served.
    pub http_max_in_flight: usize,
    /// How many recent commits each database remembers to classify a rejected
    /// explicit-transaction commit, from `DREVO_TX_CONFLICT_HISTORY` (issue
    /// #749; default [`drevo_core::conflict::DEFAULT_HISTORY`]).
    pub tx_conflict_history: usize,
    /// The isolation of explicit transactions that do not ask for one, from
    /// `DREVO_TX_ISOLATION` (`snapshot`, the default, or `serializable`).
    pub tx_isolation: crate::native::Isolation,
    /// Report a prepared two-phase-commit transaction (#556) at ERROR once it
    /// is older than this, from `DREVO_PREPARED_TX_WARN_SECS` (default 60;
    /// `0` disables the alert).
    pub prepared_tx_warn: Option<std::time::Duration>,
    /// Opt-in heuristic rollback of prepared transactions older than this,
    /// from `DREVO_PREPARED_TX_TIMEOUT_SECS` (#556; unset or `0` = never —
    /// the default, as resolving behind the coordinator's back can break
    /// atomicity across stores).
    pub prepared_tx_timeout: Option<std::time::Duration>,
    /// Body formats the HTTP API offers next to JSON, from
    /// `DREVO_HTTP_FORMATS` (issue #581; default `json`).
    pub http_formats: crate::wire_format::WireFormats,
    /// Write-ahead-log encoding from `DREVO_WAL_FORMAT` (issue #582; default
    /// `json`). Applied to every store at open, by compaction.
    pub wal_format: crate::wal_format::WalFormat,
    /// Port of the gRPC API from `DREVO_GRPC_PORT` (issue #583); `None`
    /// (unset) serves no gRPC. Needs a build with the `grpc` feature.
    pub grpc_port: Option<u16>,
    /// Port of the Arrow Flight endpoint from `DREVO_FLIGHT_PORT` (issue
    /// #584); `None` serves none. Needs the `arrow-flight` feature.
    pub flight_port: Option<u16>,
    /// Port of the Bolt listener from `DREVO_BOLT_PORT`; `None` (unset or not
    /// a port) serves no Bolt.
    pub bolt_port: Option<u16>,
    /// The certificate and key from `DREVO_TLS_CERT` / `DREVO_TLS_KEY`; when
    /// set, HTTP is served as HTTPS and Bolt as `bolt+s://`. Needs the
    /// `bolt-tls` feature.
    pub tls: Option<TlsFiles>,
    /// Where the security audit log goes, from `DREVO_AUDIT_LOG` (#637).
    pub audit_log: AuditLog,
    /// Whether a refused statement's text goes into its audit event, from
    /// `DREVO_AUDIT_QUERY_TEXT`.
    pub audit_query_text: bool,
    /// Serve every database read-only, from `DREVO_READ_ONLY`.
    pub read_only: bool,
    /// The key every store is encrypted with, from
    /// `DREVO_ENCRYPTION_KEY_FILE` or `DREVO_ENCRYPTION_KEY`;
    /// `None` keeps the data in the clear. Its `Debug` output hides it.
    #[cfg(feature = "encryption")]
    pub encryption_key: Option<drevo_core::encryption::Key>,
    /// The identity provider whose bearer tokens sign users in, from
    /// `DREVO_OIDC_*`; `None` without `DREVO_OIDC_ISSUER`.
    pub oidc: Option<OidcSettings>,
    /// How to create the first user, from `DREVO_AUTH` (Neo4j's `NEO4J_AUTH`
    /// format): `user/password`, `none`, or unset for `neo4j` with a generated
    /// password. Its `Debug` output hides the password.
    #[cfg(feature = "auth")]
    pub auth: drevo_auth::users::Bootstrap,
}

/// Where security audit events go (#637).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AuditLog {
    /// Nowhere (failed logins and refusals still reach the server log and
    /// the Web UI's problem feed).
    #[default]
    Off,
    /// JSON lines on standard output.
    Stdout,
    /// JSON lines appended to this file, rotated by size.
    File(PathBuf),
}

/// An OpenID Connect provider, from `DREVO_OIDC_*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcSettings {
    /// The tokens' exact `iss`; the discovery document is under it.
    pub issuer: String,
    /// A value the tokens' `aud` must hold.
    pub audience: String,
    /// The claim naming the user.
    pub username_claim: String,
    /// The claim listing the user's groups.
    pub roles_claim: Option<String>,
    /// Group → roles.
    pub role_map: std::collections::BTreeMap<String, Vec<String>>,
}

/// The PEM files a TLS-serving server reads at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsFiles {
    /// The certificate chain, leaf first.
    pub cert: PathBuf,
    /// The private key.
    pub key: PathBuf,
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
    /// The encryption key could not be read or is malformed, both key
    /// variables are set, or the build has no encryption.
    #[error("invalid encryption key: {reason}")]
    InvalidEncryptionKey {
        /// What is wrong.
        reason: String,
    },
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
    /// `DREVO_PREPARED_TX_TIMEOUT_SECS` was not a non-negative integer.
    #[error(
        "invalid DREVO_PREPARED_TX_TIMEOUT_SECS value `{value}`: expected seconds (0 = never)"
    )]
    InvalidPreparedTxTimeout {
        /// The raw env-var value.
        value: String,
    },
    /// `DREVO_PREPARED_TX_WARN_SECS` was not a non-negative integer.
    #[error("invalid DREVO_PREPARED_TX_WARN_SECS value `{value}`: expected seconds (0 = off)")]
    InvalidPreparedTxWarn {
        /// The raw env-var value.
        value: String,
    },
    /// `DREVO_QUERY_TIMEOUT_MS` was not a non-negative integer.
    #[error(
        "invalid DREVO_QUERY_TIMEOUT_MS value `{value}`: expected milliseconds (0 = no limit)"
    )]
    InvalidQueryTimeout {
        /// The raw env-var value.
        value: String,
    },
    /// `DREVO_IDLE_SESSION_TIMEOUT_MS` was not a non-negative integer.
    #[error(
        "invalid DREVO_IDLE_SESSION_TIMEOUT_MS value `{value}`: expected milliseconds (0 = no limit)"
    )]
    InvalidIdleSessionTimeout {
        /// The raw env-var value.
        value: String,
    },
    /// `DREVO_MAX_CONNECTIONS` was not a non-negative integer.
    #[error("invalid DREVO_MAX_CONNECTIONS value `{value}`: expected a number of connections (0 = no limit)")]
    InvalidMaxConnections {
        /// The raw env-var value.
        value: String,
    },
    /// `DREVO_HTTP_MAX_IN_FLIGHT` was not a non-negative integer.
    #[error("invalid DREVO_HTTP_MAX_IN_FLIGHT value `{value}`: expected a number of requests (0 = no limit)")]
    InvalidHttpMaxInFlight {
        /// The raw env-var value.
        value: String,
    },
    /// `DREVO_TX_CONFLICT_HISTORY` was not a positive integer.
    #[error(
        "invalid DREVO_TX_CONFLICT_HISTORY value `{value}`: expected the number of commits to remember (at least 1)"
    )]
    InvalidTxConflictHistory {
        /// The raw env-var value.
        value: String,
    },
    /// `DREVO_TX_ISOLATION` was neither `snapshot` nor `serializable`.
    #[error("invalid DREVO_TX_ISOLATION value `{value}`: expected `snapshot` or `serializable`")]
    InvalidTxIsolation {
        /// The raw env-var value.
        value: String,
    },
    /// `DREVO_HTTP_FORMATS` named an unknown format or one not compiled in.
    #[error("invalid DREVO_HTTP_FORMATS: {0}")]
    InvalidHttpFormats(#[from] crate::wire_format::FormatsError),
    /// `DREVO_GRPC_PORT` was not a port, or the build has no gRPC API.
    #[error("invalid DREVO_GRPC_PORT `{value}`: {reason}")]
    InvalidGrpcPort {
        /// The raw env-var value.
        value: String,
        /// Why it was rejected.
        reason: String,
    },
    /// `DREVO_FLIGHT_PORT` was not a port, or the build has no Flight endpoint.
    #[error("invalid DREVO_FLIGHT_PORT `{value}`: {reason}")]
    InvalidFlightPort {
        /// The raw env-var value.
        value: String,
        /// Why it was rejected.
        reason: String,
    },
    /// `DREVO_WAL_FORMAT` named an unknown format or one not compiled in.
    #[error(
        "invalid DREVO_WAL_FORMAT: {0}; rebuild with the matching `format-*` feature if needed"
    )]
    InvalidWalFormat(#[from] crate::wal_format::WalFormatError),
    /// Only one of `DREVO_TLS_CERT` / `DREVO_TLS_KEY` was set, or the build
    /// has no TLS.
    #[error("invalid audit settings: {reason}")]
    InvalidAudit {
        /// Why they were rejected.
        reason: String,
    },
    /// `DREVO_READ_ONLY` is not a boolean.
    #[error("invalid DREVO_READ_ONLY value `{value}`: expected true or false")]
    InvalidReadOnly {
        /// The value as given.
        value: String,
    },
    /// A `DREVO_OIDC_*` setting is missing or malformed, or the build has
    /// no `oidc` feature.
    #[error("invalid identity provider settings: {reason}")]
    InvalidOidc {
        /// Why they were rejected.
        reason: String,
    },
    /// Only one of `DREVO_TLS_CERT` / `DREVO_TLS_KEY` was set, or the build
    /// has no TLS.
    #[error("invalid TLS settings: {reason}")]
    InvalidTls {
        /// Why they were rejected.
        reason: String,
    },
    /// `DREVO_AUTH` was malformed, or the build has no user management. The
    /// message never contains the password.
    #[error("invalid DREVO_AUTH: {reason}")]
    InvalidAuth {
        /// Why it was rejected.
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

        let engine = match getter("DREVO_ENGINE") {
            None => EngineMode::default(),
            Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
                "kv" => EngineMode::Kv,
                "native-durable" => EngineMode::NativeDurable,
                _ => return Err(ConfigError::InvalidEngine { value: raw }),
            },
        };

        let query_timeout = match getter("DREVO_QUERY_TIMEOUT_MS") {
            None => None,
            Some(raw) => match raw.trim().parse::<u64>() {
                Ok(0) => None,
                Ok(ms) => Some(std::time::Duration::from_millis(ms)),
                Err(_) => return Err(ConfigError::InvalidQueryTimeout { value: raw }),
            },
        };

        let idle_session_timeout = match getter("DREVO_IDLE_SESSION_TIMEOUT_MS") {
            None => None,
            Some(raw) => match raw.trim().parse::<u64>() {
                Ok(0) => None,
                Ok(ms) => Some(std::time::Duration::from_millis(ms)),
                Err(_) => return Err(ConfigError::InvalidIdleSessionTimeout { value: raw }),
            },
        };

        let max_connections = match getter("DREVO_MAX_CONNECTIONS") {
            None => crate::limits::DEFAULT_MAX_CONNECTIONS,
            Some(raw) => raw
                .trim()
                .parse::<usize>()
                .map_err(|_| ConfigError::InvalidMaxConnections { value: raw })?,
        };
        let http_max_in_flight = match getter("DREVO_HTTP_MAX_IN_FLIGHT") {
            None => crate::limits::DEFAULT_HTTP_MAX_IN_FLIGHT,
            Some(raw) => raw
                .trim()
                .parse::<usize>()
                .map_err(|_| ConfigError::InvalidHttpMaxInFlight { value: raw })?,
        };

        let tx_conflict_history = match getter("DREVO_TX_CONFLICT_HISTORY") {
            None => drevo_core::conflict::DEFAULT_HISTORY,
            Some(raw) => match raw.trim().parse::<usize>() {
                Ok(n) if n >= 1 => n,
                _ => return Err(ConfigError::InvalidTxConflictHistory { value: raw }),
            },
        };
        let tx_isolation = match getter("DREVO_TX_ISOLATION") {
            None => crate::native::Isolation::Snapshot,
            Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
                "snapshot" => crate::native::Isolation::Snapshot,
                "serializable" => crate::native::Isolation::Serializable,
                _ => return Err(ConfigError::InvalidTxIsolation { value: raw }),
            },
        };

        let prepared_tx_warn = match getter("DREVO_PREPARED_TX_WARN_SECS") {
            None => Some(std::time::Duration::from_secs(60)),
            Some(raw) => match raw.trim().parse::<u64>() {
                Ok(0) => None,
                Ok(secs) => Some(std::time::Duration::from_secs(secs)),
                Err(_) => return Err(ConfigError::InvalidPreparedTxWarn { value: raw }),
            },
        };

        let prepared_tx_timeout = match getter("DREVO_PREPARED_TX_TIMEOUT_SECS") {
            None => None,
            Some(raw) => match raw.trim().parse::<u64>() {
                Ok(0) => None,
                Ok(secs) => Some(std::time::Duration::from_secs(secs)),
                Err(_) => return Err(ConfigError::InvalidPreparedTxTimeout { value: raw }),
            },
        };

        let http_formats = match getter("DREVO_HTTP_FORMATS") {
            None => crate::wire_format::WireFormats::default(),
            Some(raw) => raw.parse()?,
        };

        let wal_format = match getter("DREVO_WAL_FORMAT") {
            None => crate::wal_format::WalFormat::default(),
            Some(raw) => raw.parse()?,
        };

        let grpc_port = match getter("DREVO_GRPC_PORT") {
            None => None,
            Some(raw) => {
                let port = parse_port(raw.trim()).map_err(|_| ConfigError::InvalidGrpcPort {
                    value: raw.clone(),
                    reason: "expected a port number 1-65535".to_string(),
                })?;
                if !cfg!(feature = "grpc") {
                    return Err(ConfigError::InvalidGrpcPort {
                        value: raw,
                        reason: "this build has no gRPC API; rebuild with the `grpc` feature"
                            .to_string(),
                    });
                }
                Some(port)
            }
        };

        let flight_port = match getter("DREVO_FLIGHT_PORT") {
            None => None,
            Some(raw) => {
                let port = parse_port(raw.trim()).map_err(|_| ConfigError::InvalidFlightPort {
                    value: raw.clone(),
                    reason: "expected a port number 1-65535".to_string(),
                })?;
                if !cfg!(feature = "arrow-flight") {
                    return Err(ConfigError::InvalidFlightPort {
                        value: raw,
                        reason: "this build has no Arrow Flight endpoint; rebuild with the \
                                 `arrow-flight` feature"
                            .to_string(),
                    });
                }
                Some(port)
            }
        };

        let bolt_port = getter("DREVO_BOLT_PORT").and_then(|raw| raw.trim().parse::<u16>().ok());

        let tls = match (getter("DREVO_TLS_CERT"), getter("DREVO_TLS_KEY")) {
            (None, None) => None,
            (Some(cert), Some(key)) if !cert.is_empty() && !key.is_empty() => {
                if !cfg!(feature = "bolt-tls") {
                    return Err(ConfigError::InvalidTls {
                        reason: "this build has no TLS; rebuild with the `bolt-tls` feature"
                            .to_string(),
                    });
                }
                Some(TlsFiles {
                    cert: PathBuf::from(cert),
                    key: PathBuf::from(key),
                })
            }
            _ => {
                return Err(ConfigError::InvalidTls {
                    reason: "set both DREVO_TLS_CERT and DREVO_TLS_KEY, or neither".to_string(),
                })
            }
        };

        let audit_log = match getter("DREVO_AUDIT_LOG").as_deref().map(str::trim) {
            None | Some("") => AuditLog::Off,
            Some(v) if v.eq_ignore_ascii_case("off") => AuditLog::Off,
            Some(v) if !cfg!(feature = "auth") => {
                return Err(ConfigError::InvalidAudit {
                    reason: format!(
                        "DREVO_AUDIT_LOG={v}: this build has no user management; rebuild with the `auth` feature"
                    ),
                })
            }
            Some(v) if v.eq_ignore_ascii_case("stdout") => AuditLog::Stdout,
            Some(path) => AuditLog::File(PathBuf::from(path)),
        };
        let audit_query_text = match getter("DREVO_AUDIT_QUERY_TEXT").as_deref().map(str::trim) {
            None | Some("") => false,
            Some(v) => match v.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => true,
                "0" | "false" | "no" | "off" => false,
                _ => {
                    return Err(ConfigError::InvalidAudit {
                        reason: format!("DREVO_AUDIT_QUERY_TEXT={v}: expected true or false"),
                    })
                }
            },
        };

        let read_only = match getter("DREVO_READ_ONLY").as_deref().map(str::trim) {
            None | Some("") => false,
            Some(v) => match v.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => true,
                "0" | "false" | "no" | "off" => false,
                _ => {
                    return Err(ConfigError::InvalidReadOnly {
                        value: v.to_string(),
                    })
                }
            },
        };

        let oidc = oidc_settings(&getter)?;
        #[cfg(feature = "encryption")]
        let encryption_key = encryption_key(&getter)?;
        #[cfg(not(feature = "encryption"))]
        if getter("DREVO_ENCRYPTION_KEY_FILE").is_some() || getter("DREVO_ENCRYPTION_KEY").is_some()
        {
            return Err(ConfigError::InvalidEncryptionKey {
                reason: "this build has no encryption; rebuild with the `encryption` feature"
                    .to_string(),
            });
        }

        let auth_raw = getter("DREVO_AUTH");
        #[cfg(feature = "auth")]
        let auth = drevo_auth::users::Bootstrap::parse(auth_raw.as_deref()).map_err(|e| {
            ConfigError::InvalidAuth {
                reason: format!("{e}; expected user/password or none"),
            }
        })?;
        #[cfg(not(feature = "auth"))]
        if auth_raw.is_some_and(|raw| !raw.eq_ignore_ascii_case("none")) {
            return Err(ConfigError::InvalidAuth {
                reason: "this build has no user management; rebuild with the `auth` feature"
                    .to_string(),
            });
        }

        Ok(Self {
            #[cfg(feature = "auth")]
            auth,
            host,
            port,
            data_dir: PathBuf::from(data_dir_raw),
            backup_dir: getter("DREVO_BACKUP_DIR")
                .filter(|d| !d.trim().is_empty())
                .map(PathBuf::from),
            engine,
            query_timeout,
            idle_session_timeout,
            max_connections,
            http_max_in_flight,
            tx_conflict_history,
            tx_isolation,
            prepared_tx_warn,
            prepared_tx_timeout,
            http_formats,
            wal_format,
            grpc_port,
            flight_port,
            bolt_port,
            tls,
            audit_log,
            audit_query_text,
            read_only,
            #[cfg(feature = "encryption")]
            encryption_key,
            oidc,
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
        // Port 0 asks the system for a free port, which is never privileged.
        self.port != 0 && self.port < PRIVILEGED_PORT_CEILING
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
    /// The text-to-Cypher provider (`DREVO_TEXT2CYPHER_*` or the stored
    /// setting) is invalid, or its generator could not be built.
    #[error("invalid text-to-Cypher configuration: {0}")]
    Text2Cypher(String),
    /// The users could not be loaded from `<data_dir>/security.json`, or the
    /// first user could not be created. A damaged file stops the server
    /// rather than starting it with no users.
    #[cfg(feature = "auth")]
    #[error("cannot open the server's users: {0}")]
    Security(String),
    /// The TLS certificate or key could not be read or used.
    #[error("cannot set up TLS: {0}")]
    Tls(String),
}

/// Open the server's users from `<data_dir>/security.json` and create the
/// first one from `DREVO_AUTH` when there are none, logging what happened.
#[cfg(feature = "auth")]
fn configure_security(cfg: &Config) -> Result<crate::security::Security, RunError> {
    use drevo_auth::users::BootstrapOutcome;
    let (security, outcome) = crate::security::open_server_security(Some(&cfg.data_dir), &cfg.auth)
        .map_err(|e| RunError::Security(e.to_string()))?;
    match outcome {
        BootstrapOutcome::Disabled => tracing::info!("DREVO_AUTH=none: no first user created"),
        BootstrapOutcome::AlreadyInitialised => {}
        BootstrapOutcome::Created => tracing::info!("created the first user from DREVO_AUTH"),
        BootstrapOutcome::Generated { user, password } => tracing::warn!(
            user = %user,
            password = %password,
            "created the first user with a generated password; it must be changed at the first \
             login. Set DREVO_AUTH=user/password to choose it instead"
        ),
    }
    // The security audit log (#637).
    let security = match &cfg.audit_log {
        AuditLog::Off => security,
        AuditLog::Stdout => security.with_audit(
            std::sync::Arc::new(drevo_auth::audit::Stdout),
            cfg.audit_query_text,
        ),
        AuditLog::File(path) => {
            let sink =
                drevo_auth::audit::JsonLinesFile::open(path, AUDIT_FILE_BYTES, AUDIT_FILES_KEPT)
                    .map_err(|e| {
                        RunError::Security(format!(
                            "cannot open the audit log {}: {e}",
                            path.display()
                        ))
                    })?;
            tracing::info!(path = %path.display(), "security audit log");
            security.with_audit(std::sync::Arc::new(sink), cfg.audit_query_text)
        }
    };
    #[cfg(feature = "oidc")]
    let security = with_identity_provider(security, cfg.oidc.as_ref())?;
    Ok(security)
}

/// The encryption key from `DREVO_ENCRYPTION_KEY_FILE` (a file of 64
/// hexadecimal digits, kept outside the data directory) or
/// `DREVO_ENCRYPTION_KEY` (the digits themselves); `None` when neither is
/// set.
///
/// # Errors
/// [`ConfigError::InvalidEncryptionKey`] when both are set, the file cannot
/// be read, or the key is malformed.
#[cfg(feature = "encryption")]
pub fn encryption_key<F>(getter: &F) -> Result<Option<drevo_core::encryption::Key>, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let invalid = |reason: String| ConfigError::InvalidEncryptionKey { reason };
    let file = getter("DREVO_ENCRYPTION_KEY_FILE").filter(|v| !v.trim().is_empty());
    let inline = getter("DREVO_ENCRYPTION_KEY").filter(|v| !v.trim().is_empty());
    let text = match (file, inline) {
        (Some(_), Some(_)) => {
            return Err(invalid(
                "set DREVO_ENCRYPTION_KEY_FILE or DREVO_ENCRYPTION_KEY, not both".to_string(),
            ))
        }
        (Some(path), None) => std::fs::read_to_string(path.trim()).map_err(|e| {
            invalid(format!(
                "cannot read DREVO_ENCRYPTION_KEY_FILE `{}`: {e}",
                path.trim()
            ))
        })?,
        (None, Some(text)) => text,
        (None, None) => return Ok(None),
    };
    drevo_core::encryption::Key::from_hex(&text)
        .map(Some)
        .map_err(|e| invalid(e.to_string()))
}

/// The identity provider from `DREVO_OIDC_*`, or `None` without an issuer.
fn oidc_settings<F>(getter: &F) -> Result<Option<OidcSettings>, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let invalid = |reason: String| ConfigError::InvalidOidc { reason };
    let value = |name: &str| getter(name).map(|v| v.trim().to_string());
    let Some(issuer) = value("DREVO_OIDC_ISSUER").filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if !cfg!(feature = "oidc") {
        return Err(invalid(
            "DREVO_OIDC_ISSUER is set, but this build has no identity provider logins; rebuild \
             with the `oidc` feature"
                .to_string(),
        ));
    }
    #[cfg(feature = "oidc")]
    crate::oidc::HttpJwks::new(&issuer).map_err(|e| invalid(format!("DREVO_OIDC_ISSUER: {e}")))?;
    let audience = value("DREVO_OIDC_AUDIENCE")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| invalid("DREVO_OIDC_AUDIENCE is required with DREVO_OIDC_ISSUER".into()))?;
    let username_claim = match value("DREVO_OIDC_USERNAME_CLAIM") {
        None => "sub".to_string(),
        Some(v) if v.is_empty() => {
            return Err(invalid("DREVO_OIDC_USERNAME_CLAIM is empty".to_string()))
        }
        Some(v) => v,
    };
    let roles_claim = Some(
        value("DREVO_OIDC_ROLES_CLAIM")
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "groups".to_string()),
    );
    #[cfg(feature = "oidc")]
    let role_map =
        drevo_auth::oidc::parse_role_map(&value("DREVO_OIDC_ROLE_MAP").unwrap_or_default())
            .map_err(|e| invalid(format!("DREVO_OIDC_ROLE_MAP: {e}")))?;
    #[cfg(not(feature = "oidc"))]
    let role_map = std::collections::BTreeMap::new();
    Ok(Some(OidcSettings {
        issuer,
        audience,
        username_claim,
        roles_claim,
        role_map,
    }))
}

/// Accept the identity provider's bearer tokens (`DREVO_OIDC_*`).
#[cfg(feature = "oidc")]
fn with_identity_provider(
    security: crate::security::Security,
    oidc: Option<&OidcSettings>,
) -> Result<crate::security::Security, RunError> {
    use drevo_auth::oidc::{ClaimMapping, TokenValidator, ValidatorConfig};
    let Some(oidc) = oidc else {
        return Ok(security);
    };
    let source = crate::oidc::HttpJwks::new(&oidc.issuer).map_err(RunError::Security)?;
    let validator = TokenValidator::new(
        ValidatorConfig::new(&oidc.issuer, &oidc.audience),
        ClaimMapping {
            username_claim: oidc.username_claim.clone(),
            roles_claim: oidc.roles_claim.clone(),
            role_map: oidc.role_map.clone(),
        },
        std::sync::Arc::new(source),
    );
    tracing::info!(issuer = %oidc.issuer, audience = %oidc.audience, "identity provider logins");
    Ok(security.with_oidc(std::sync::Arc::new(validator)))
}

/// An audit log file rotates past this size…
#[cfg(feature = "auth")]
const AUDIT_FILE_BYTES: u64 = 10 * 1024 * 1024;
/// …and this many old files are kept.
#[cfg(feature = "auth")]
const AUDIT_FILES_KEPT: usize = 5;

/// Without the `auth` feature there is no user management.
#[cfg(not(feature = "auth"))]
fn configure_security(_cfg: &Config) -> Result<crate::security::Security, RunError> {
    Ok(crate::security::Security::default())
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

/// The text-to-Cypher provider store (`drevo.cypher.fromText`): the
/// `<data_dir>/text2cypher_config.json` file an operator set through the Web
/// UI wins, falling back to `DREVO_TEXT2CYPHER_*`. The generator for the
/// effective provider is installed as the process global here, once, for
/// both serving paths; a later `POST /config/text2cypher` replaces it.
fn configure_text2cypher(
    cfg: &Config,
) -> Result<std::sync::Arc<crate::text2cypher::Text2CypherConfigStore>, RunError> {
    use crate::text2cypher::{Text2CypherConfig, Text2CypherConfigStore, TEXT2CYPHER_CONFIG_FILE};
    let env_cfg = Text2CypherConfig::from_env(|key| std::env::var(key).ok())
        .map_err(|e| RunError::Text2Cypher(e.to_string()))?;
    let store = Text2CypherConfigStore::load(cfg.data_dir.join(TEXT2CYPHER_CONFIG_FILE), env_cfg);
    if let Some(config) = store.snapshot() {
        crate::text2cypher::install_generator(&config)
            .map_err(|e| RunError::Text2Cypher(e.to_string()))?;
        tracing::info!(model = %config.model, "drevo.cypher.fromText generator installed");
    }
    Ok(store)
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
    serve(cfg, None).await
}

/// The addresses a started server listens on. With port `0` in the
/// [`Config`] the system picks a free port, and these are the ones it picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listening {
    /// The HTTP API (HTTPS with a certificate).
    pub http: SocketAddr,
    /// The Bolt port, when one is served.
    pub bolt: Option<SocketAddr>,
    /// The gRPC API, when it is served.
    pub grpc: Option<SocketAddr>,
    /// The Arrow Flight endpoint, when it is served.
    pub flight: Option<SocketAddr>,
}

/// [`run`], sending the addresses the server listens on to `announce` once
/// every listener is bound — for a caller that starts the server on port
/// `0` and then connects to it:
///
/// ```no_run
/// # async fn demo(dir: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
/// let data = dir.to_string_lossy().to_string();
/// let mut cfg = drevo::server::Config::from_env(|k| match k {
///     "DREVO_HOST" => Some("127.0.0.1".to_string()),
///     "DREVO_DATA_DIR" => Some(data.clone()),
///     _ => None,
/// })?;
/// cfg.port = 0;
/// let (tx, rx) = tokio::sync::oneshot::channel();
/// tokio::spawn(drevo::server::run_announcing(cfg, tx));
/// let listening = rx.await?;
/// println!("serving on {}", listening.http);
/// # Ok(()) }
/// ```
///
/// # Errors
///
/// As [`run`]. A server that fails before binding everything drops
/// `announce` without sending.
pub async fn run_announcing(
    cfg: Config,
    announce: tokio::sync::oneshot::Sender<Listening>,
) -> Result<(), RunError> {
    serve(cfg, Some(announce)).await
}

async fn serve(
    cfg: Config,
    announce: Option<tokio::sync::oneshot::Sender<Listening>>,
) -> Result<(), RunError> {
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
    // Every store opened from here on (default database, registry, …) uses
    // the configured log format; compaction on open converts older logs.
    crate::native_service::NativeService::set_default_wal_format(cfg.wal_format);
    tracing::info!(wal_format = cfg.wal_format.name(), "write-ahead log format");
    // Every store opened from here on is encrypted with the configured key.
    #[cfg(feature = "encryption")]
    {
        tracing::info!(
            encrypted = cfg.encryption_key.is_some(),
            "encryption at rest"
        );
        crate::native_service::NativeService::set_encryption_key(cfg.encryption_key.clone());
    }
    run_native_durable(cfg, addr, announce).await
}

/// Serve `DREVO_ENGINE=native-durable`: the WAL-backed native engine as the
/// store of record — no KV store, no catalog. Opens (or creates)
/// `<data_dir>/native.wal`, compacts and indexes it, and serves the minimal
/// native HTTP surface plus (when `DREVO_BOLT_PORT` is set) a Bolt listener
/// whose sessions execute on the durable service — autocommit statements
/// only; `BEGIN` is refused until the executor can drive native
/// transactions.
async fn run_native_durable(
    cfg: Config,
    addr: SocketAddr,
    announce: Option<tokio::sync::oneshot::Sender<Listening>>,
) -> Result<(), RunError> {
    let mut listening = Listening {
        http: addr,
        bolt: None,
        grpc: None,
        flight: None,
    };
    // Read the certificate first, so a bad one stops the server before it
    // opens anything.
    let tls = load_tls(&cfg)?;
    let wal = cfg.data_dir.join("native.wal");
    tracing::info!(wal = %wal.display(), read_only = cfg.read_only, "engine=native-durable — opening the durable native store");
    let opened = if cfg.read_only {
        crate::native_service::NativeService::open_read_only(&wal)
    } else {
        crate::native_service::NativeService::open(&wal)
    };
    let service = std::sync::Arc::new(opened.map_err(RunError::NativeOpen)?);
    tracing::info!("durable native store ready");
    // Shared, persisted embeddings config store (Web-UI-settable), same as the
    // KV path: file wins over `DREVO_EMBEDDINGS_*`.
    let embeddings_store = build_embeddings_store(&cfg)?;
    // The text-to-Cypher provider (`drevo.cypher.fromText`): the stored
    // setting wins over `DREVO_TEXT2CYPHER_*`; its generator is installed as
    // the process global here, and the Web UI can replace it later.
    let text2cypher_store = configure_text2cypher(&cfg)?;
    // Opt-in server-side query embedder — the durable-engine counterpart of
    // `configure_query_embedder`; no-op unless the `embeddings-proxy` feature
    // is built.
    configure_native_query_embedder(&service, embeddings_store.clone())?;

    // Shared multi-database catalog (issue #523): built once and handed to both
    // the HTTP state and the Bolt listener so `CREATE DATABASE` / `USE` and a
    // Bolt `session(database="…")` see the same named databases regardless of
    // which protocol created them. Durable: non-default databases persist in
    // `<data_dir>/databases/<name>/`, and every one from a previous run is
    // re-opened here on startup.
    let opened = if cfg.read_only {
        crate::database_registry::DatabaseRegistry::with_durable_dir_read_only(
            std::sync::Arc::clone(&service),
            cfg.data_dir.clone(),
        )
    } else {
        crate::database_registry::DatabaseRegistry::with_durable_dir(
            std::sync::Arc::clone(&service),
            cfg.data_dir.clone(),
        )
    };
    let registry = std::sync::Arc::new(opened.map_err(|e| {
        RunError::NativeOpen(crate::error::DrevoError::Io(std::io::Error::other(
            e.to_string(),
        )))
    })?);
    // Opt-in statement timeout (#547): every database, including ones created
    // later through the catalog, stops a statement past this limit.
    registry.set_statement_timeout(cfg.query_timeout);
    // Bolt connections that hold a transaction open and go silent.
    registry.set_idle_session_timeout(cfg.idle_session_timeout);
    // How many Bolt connections and HTTP requests are served at once.
    registry.limits().bolt.set_max(cfg.max_connections);
    registry.limits().http.set_max(cfg.http_max_in_flight);
    // How many recent commits a database remembers to say why an explicit
    // transaction's commit was rejected (issue #749).
    registry.set_conflict_history(cfg.tx_conflict_history);
    // When an explicit transaction may commit after others committed.
    registry.set_isolation(cfg.tx_isolation);
    // The server's users, shared by HTTP, Bolt, gRPC and Flight (issue #633).
    let security = configure_security(&cfg)?;
    // The Web UI's signed-in sessions are listed by `SHOW CONNECTIONS`.
    #[cfg(feature = "auth")]
    registry
        .default_service()
        .transactions()
        .set_http_sessions(std::sync::Arc::new(security.clone()));
    // Two-phase commit (#556): a prepared transaction blocks every write, so
    // one left waiting too long is reported at ERROR (and thus in the Web UI's
    // problem feed). Checked every 10 s across every database.
    if cfg.prepared_tx_warn.is_some() || cfg.prepared_tx_timeout.is_some() {
        let warn_after = cfg.prepared_tx_warn;
        let timeout = cfg.prepared_tx_timeout;
        let watched = std::sync::Arc::clone(&registry);
        tokio::spawn(async move {
            let mut reported: std::collections::HashMap<String, std::collections::HashSet<String>> =
                std::collections::HashMap::new();
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                tick.tick().await;
                let now = crate::model::now_ms();
                for name in watched.list() {
                    if let Some(service) = watched.get(&name) {
                        if let Some(timeout) = timeout {
                            crate::problems::heuristic_rollback_expired(
                                &name, &service, now, timeout,
                            );
                        }
                        if let Some(warn_after) = warn_after {
                            let prepared = service.list_prepared();
                            let seen = reported.entry(name.clone()).or_default();
                            crate::problems::report_stale_prepared(
                                &name, &prepared, now, warn_after, seen,
                            );
                        }
                    }
                }
            }
        });
    }
    if let Some(limit) = cfg.query_timeout {
        tracing::info!(
            timeout_ms = limit.as_millis() as u64,
            "statement timeout enabled"
        );
    }

    // Optional Bolt listener — same opt-in as the KV path, served by the
    // durable-native session; statements route to the database named by their
    // `db` selector via the shared catalog (issue #523).
    if let Some(bolt_port) = cfg.bolt_port {
        let bolt_addr = SocketAddr::new(addr.ip(), bolt_port);
        let bolt_listener = tokio::net::TcpListener::bind(bolt_addr)
            .await
            .map_err(|source| RunError::Bind {
                addr: bolt_addr,
                source,
            })?;
        let bolt_addr = bolt_listener.local_addr().unwrap_or(bolt_addr);
        listening.bolt = Some(bolt_addr);
        let bolt_tls = tls.as_ref().map(TlsSetup::acceptor);
        tracing::info!(%bolt_addr, tls = bolt_tls.is_some(), "bolt listening (native-durable)");
        let bolt_registry = std::sync::Arc::clone(&registry);
        let bolt_security = security.clone();
        tokio::spawn(async move {
            loop {
                match bolt_listener.accept().await {
                    Ok((socket, peer)) => {
                        let conn_registry = std::sync::Arc::clone(&bolt_registry);
                        let conn_security = bolt_security.clone();
                        let conn_tls = bolt_tls.clone();
                        let gate = &conn_registry.limits().bolt;
                        let Some(pass) = gate.try_enter() else {
                            // Over DREVO_MAX_CONNECTIONS: finish the handshake
                            // so the driver can read why, refuse at HELLO,
                            // close. Bounded, so slow clients cannot pile up.
                            let max = gate.max();
                            if gate.should_warn() {
                                tracing::warn!(
                                    %peer,
                                    max_connections = max,
                                    refused = gate.rejected(),
                                    "bolt connection refused: DREVO_MAX_CONNECTIONS reached"
                                );
                            }
                            tokio::spawn(async move {
                                let refuse = refuse_bolt_connection(socket, conn_tls, max);
                                let _ =
                                    tokio::time::timeout(std::time::Duration::from_secs(5), refuse)
                                        .await;
                            });
                            continue;
                        };
                        tokio::spawn(async move {
                            let _pass = pass;
                            if let Err(err) = serve_bolt_connection(
                                socket,
                                conn_tls,
                                &conn_registry,
                                conn_security,
                            )
                            .await
                            {
                                if err.is_client_disconnect() {
                                    tracing::debug!(error = %err, "bolt client disconnected");
                                } else {
                                    tracing::warn!(error = %err, "bolt session ended with error");
                                }
                            }
                        });
                    }
                    Err(err) => tracing::warn!(error = %err, "bolt accept failed"),
                }
            }
        });
    }

    // Optional gRPC API (issue #583), on the same host.
    #[cfg(feature = "grpc")]
    if let Some(grpc_port) = cfg.grpc_port {
        let grpc_addr = SocketAddr::new(addr.ip(), grpc_port);
        let grpc_listener = tokio::net::TcpListener::bind(grpc_addr)
            .await
            .map_err(|source| RunError::Bind {
                addr: grpc_addr,
                source,
            })?;
        let grpc_addr = grpc_listener.local_addr().unwrap_or(grpc_addr);
        listening.grpc = Some(grpc_addr);
        // Over TLS with the HTTPS certificate when there is one.
        let grpc_tls = tls.as_ref().map(TlsSetup::rpc);
        tracing::info!(%grpc_addr, tls = grpc_tls.is_some(), "grpc listening");
        let grpc_registry = std::sync::Arc::clone(&registry);
        let grpc_security = security.clone();
        tokio::spawn(async move {
            let served = match grpc_tls {
                #[cfg(feature = "bolt-tls")]
                Some(tls) => {
                    crate::grpc::serve_tls_with_security(
                        grpc_registry,
                        grpc_security,
                        grpc_listener,
                        &tls,
                        shutdown_signal(),
                    )
                    .await
                }
                #[cfg(not(feature = "bolt-tls"))]
                Some(never) => match never {},
                None => {
                    crate::grpc::serve_with_security(
                        grpc_registry,
                        grpc_security,
                        grpc_listener,
                        shutdown_signal(),
                    )
                    .await
                }
            };
            if let Err(err) = served {
                tracing::error!(error = %err, "grpc server stopped");
            }
        });
    }

    // Optional Arrow Flight endpoint (issue #584), on the same host.
    #[cfg(feature = "arrow-flight")]
    if let Some(flight_port) = cfg.flight_port {
        let flight_addr = SocketAddr::new(addr.ip(), flight_port);
        let flight_listener =
            tokio::net::TcpListener::bind(flight_addr)
                .await
                .map_err(|source| RunError::Bind {
                    addr: flight_addr,
                    source,
                })?;
        let flight_addr = flight_listener.local_addr().unwrap_or(flight_addr);
        listening.flight = Some(flight_addr);
        // Over TLS with the HTTPS certificate when there is one.
        let flight_tls = tls.as_ref().map(TlsSetup::rpc);
        tracing::info!(%flight_addr, tls = flight_tls.is_some(), "arrow flight listening");
        let flight_registry = std::sync::Arc::clone(&registry);
        let flight_security = security.clone();
        tokio::spawn(async move {
            let served = match flight_tls {
                #[cfg(feature = "bolt-tls")]
                Some(tls) => {
                    crate::flight::serve_tls_with_security(
                        flight_registry,
                        flight_security,
                        flight_listener,
                        &tls,
                        shutdown_signal(),
                    )
                    .await
                }
                #[cfg(not(feature = "bolt-tls"))]
                Some(never) => match never {},
                None => {
                    crate::flight::serve_with_security(
                        flight_registry,
                        flight_security,
                        flight_listener,
                        shutdown_signal(),
                    )
                    .await
                }
            };
            if let Err(err) = served {
                tracing::error!(error = %err, "arrow flight server stopped");
            }
        });
    }

    let state = crate::native_api::NativeApiState::with_registry(service, registry)
        .with_wire_formats(cfg.http_formats.clone())
        .with_security(security)
        .with_secure_cookies(tls.is_some());
    let state = match &cfg.backup_dir {
        Some(dir) => state.with_backup_dir(dir.clone()),
        None => state,
    };
    // Opt-in embeddings proxy, exactly like the KV path — the restart
    // tooling probes POST /v1/embeddings after boot. Store-backed so the key is
    // Web-UI-settable.
    let state = state.with_embeddings_config_store(embeddings_store.clone());
    let state = state.with_text2cypher_config_store(text2cypher_store.clone());
    let state = configure_native_embeddings(state, embeddings_store)?;
    let shutdown_state = state.clone();
    let router = crate::native_api::build_native_router(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|source| RunError::Bind { addr, source })?;
    let addr = listener.local_addr().unwrap_or(addr);
    listening.http = addr;
    tracing::info!(%addr, https = tls.is_some(), "listening (native-durable)");
    if let Some(announce) = announce {
        // The caller may have stopped waiting; the server serves regardless.
        let _ = announce.send(listening);
    }
    tracing::info!(
        formats = ?cfg.http_formats.enabled().iter().map(|f| f.name()).collect::<Vec<_>>(),
        "http body formats"
    );

    let drain = async move {
        shutdown_signal().await;
        shutdown_state.signal_shutdown();
        tracing::info!("shutdown signal received, draining");
    };
    match tls {
        #[cfg(feature = "bolt-tls")]
        Some(tls) => axum::serve(
            crate::https::TlsListener::new(listener, &tls.0),
            router.into_make_service_with_connect_info::<crate::native_api::ClientAddr>(),
        )
        .with_graceful_shutdown(drain)
        .await
        .map_err(RunError::Serve)?,
        _ => axum::serve(
            listener,
            router.into_make_service_with_connect_info::<crate::native_api::ClientAddr>(),
        )
        .with_graceful_shutdown(drain)
        .await
        .map_err(RunError::Serve)?,
    }

    tracing::info!("shut down cleanly");
    Ok(())
}

/// The server's certificate, read from `DREVO_TLS_CERT` / `DREVO_TLS_KEY`.
#[cfg(feature = "bolt-tls")]
struct TlsSetup(crate::bolt::tls::TlsConfig);

/// Without the `bolt-tls` feature there is never a certificate.
#[cfg(not(feature = "bolt-tls"))]
enum TlsSetup {}

impl TlsSetup {
    #[cfg(feature = "bolt-tls")]
    fn acceptor(&self) -> BoltTls {
        self.0.acceptor()
    }

    #[cfg(not(feature = "bolt-tls"))]
    fn acceptor(&self) -> BoltTls {
        match *self {}
    }

    /// The certificate for the gRPC API and Arrow Flight.
    #[cfg(all(feature = "bolt-tls", any(feature = "grpc", feature = "arrow-flight")))]
    fn rpc(&self) -> RpcTls {
        self.0.clone()
    }

    #[cfg(all(
        not(feature = "bolt-tls"),
        any(feature = "grpc", feature = "arrow-flight")
    ))]
    fn rpc(&self) -> RpcTls {
        match *self {}
    }
}

#[cfg(all(feature = "bolt-tls", any(feature = "grpc", feature = "arrow-flight")))]
type RpcTls = crate::bolt::tls::TlsConfig;
/// Without the `bolt-tls` feature gRPC and Flight are never served over TLS.
#[cfg(all(
    not(feature = "bolt-tls"),
    any(feature = "grpc", feature = "arrow-flight")
))]
enum RpcTls {}

#[cfg(feature = "bolt-tls")]
type BoltTls = tokio_rustls::TlsAcceptor;
/// Without the `bolt-tls` feature Bolt is never served over TLS.
#[cfg(not(feature = "bolt-tls"))]
#[derive(Clone)]
enum BoltTls {}

/// Read the certificate and key named by the config, if any.
#[cfg(feature = "bolt-tls")]
fn load_tls(cfg: &Config) -> Result<Option<TlsSetup>, RunError> {
    let Some(files) = &cfg.tls else {
        return Ok(None);
    };
    // rustls needs a process-wide crypto provider; another part of the
    // process may have installed it already.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let tls =
        crate::bolt::tls::TlsConfig::from_pem_files(&files.cert, &files.key).map_err(|e| {
            RunError::Tls(format!(
                "certificate {} / key {}: {e}",
                files.cert.display(),
                files.key.display()
            ))
        })?;
    Ok(Some(TlsSetup(tls)))
}

#[cfg(not(feature = "bolt-tls"))]
fn load_tls(_cfg: &Config) -> Result<Option<TlsSetup>, RunError> {
    Ok(None)
}

/// One Bolt connection: over TLS (`bolt+s://`) when the server has a
/// certificate, else plain.
async fn serve_bolt_connection(
    socket: tokio::net::TcpStream,
    tls: Option<BoltTls>,
    registry: &std::sync::Arc<crate::database_registry::DatabaseRegistry>,
    security: crate::security::Security,
) -> crate::bolt::error::BoltResult<()> {
    match tls {
        #[cfg(feature = "bolt-tls")]
        Some(acceptor) => {
            crate::bolt::tls::accept_and_run_session_tls_with_security(
                socket, &acceptor, registry, security,
            )
            .await
        }
        #[cfg(not(feature = "bolt-tls"))]
        Some(never) => match never {},
        None => {
            crate::bolt::listener::accept_and_run_session_durable_with_security(
                socket, registry, security,
            )
            .await
        }
    }
}

/// Refuse a Bolt connection over the limit: complete the handshake, answer
/// the first message with `Neo.TransientError.Request.NoThreadsAvailable`
/// naming the limit, and close.
async fn refuse_bolt_connection(
    socket: tokio::net::TcpStream,
    tls: Option<BoltTls>,
    max: usize,
) -> crate::bolt::error::BoltResult<()> {
    let message = crate::limits::bolt_refused_message(max);
    match tls {
        #[cfg(feature = "bolt-tls")]
        Some(acceptor) => {
            crate::bolt::tls::accept_and_refuse_tls(socket, &acceptor, &message).await
        }
        #[cfg(not(feature = "bolt-tls"))]
        Some(never) => match never {},
        None => crate::bolt::listener::accept_and_refuse(socket, &message).await,
    }
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
