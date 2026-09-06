//! OpenAI-compatible text-embedding endpoint (Phase 19, issues #217 + passthrough).
//!
//! drevo is a graph **+ vector store**: it persists `Value::Vector` nodes,
//! builds an HNSW index, and searches them. It deliberately does not *host* an
//! embedder — turning text into vectors is the caller's job. That makes a
//! drevo deployment non–self-contained for RAG: you always need a second
//! service just to embed text.
//!
//! This module closes that gap by exposing an OpenAI-shaped endpoint so a
//! single drevo instance can serve graph, vector search, **and** embeddings:
//!
//! ```text
//! POST /v1/embeddings
//! { "model": "<name>", "input": ["text a", "text b"] }
//! -> { "object": "list", "data": [ { "object": "embedding", "index": 0,
//!      "embedding": [ ... ] }, ... ], "model": "<name>", "usage": { ... } }
//! ```
//!
//! # Provider-agnostic passthrough
//!
//! The proxy is a **transparent passthrough**, not a re-typed adapter, so it
//! works with every OpenAI-shaped provider even though their optional fields
//! differ:
//!
//! - **Request** — `model` + `input` are recognised (and validated), but every
//!   other field the client sends (`dimensions`, `encoding_format`, `user` for
//!   OpenAI; `input_type`, `output_dimension` for Voyage / Anthropic-recommended;
//!   anything future) is **forwarded verbatim** to the upstream.
//! - **Response** — the upstream body is returned **verbatim** as JSON, so
//!   base64-encoded embeddings (`encoding_format: "base64"`) and any extra
//!   fields survive unchanged.
//!
//! The only normalisation is: `input` is forwarded as an array (every
//! OpenAI-compatible upstream accepts that form), and a missing/empty `model`
//! is filled from the server's configured default.
//!
//! # Backends
//!
//! The endpoint itself (validation, routing) is always compiled with the
//! `http` feature and adds **no** dependencies — the default binary stays lean.
//! Until a backend is configured it answers `503 Service Unavailable`
//! ("embeddings backend not configured"), exactly as the semantic-facet path
//! answers `400` when no embedder is present.
//!
//! A concrete backend is opt-in:
//!
//! - [`ProxyBackend`](crate::embeddings::ProxyBackend) (feature
//!   `embeddings-proxy`) forwards the request to a configured upstream (OpenAI,
//!   Voyage, Ollama, vLLM, …) and passes the response back. This keeps drevo
//!   dependency-free by default while making one instance the whole RAG backend
//!   when enabled.
//!
//! # Security — SSRF (OWASP A10)
//!
//! The outbound destination is taken **only** from server configuration
//! ([`EmbeddingsConfig::from_env`](crate::embeddings::EmbeddingsConfig::from_env),
//! `DREVO_EMBEDDINGS_UPSTREAM`) — **never** from the request. A request field
//! is forwarded *to that fixed upstream* but can never change **where** drevo
//! connects, so a caller cannot redirect the outbound call at an internal
//! address (e.g. the cloud metadata endpoint). Even a body carrying a `url` /
//! `base_url` key is harmless: it rides along to the operator's configured
//! upstream, which ignores it; the connection target is unaffected. This is a
//! configuration-boundary guarantee, locked by tests.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The `input` field of an embeddings request. OpenAI accepts either a single
/// string or an array of strings; both deserialize here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingInput {
    /// A single piece of text.
    Single(String),
    /// A batch of texts embedded in one call.
    Batch(Vec<String>),
}

impl EmbeddingInput {
    /// Normalise to a list of texts, regardless of the wire form.
    #[must_use]
    pub fn texts(&self) -> Vec<String> {
        match self {
            Self::Single(s) => vec![s.clone()],
            Self::Batch(v) => v.clone(),
        }
    }

    /// True when there is nothing to embed: an empty batch, or text that is
    /// entirely empty. Used to reject no-op requests with `400` before any
    /// backend is consulted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Single(s) => s.is_empty(),
            Self::Batch(v) => v.is_empty() || v.iter().all(String::is_empty),
        }
    }
}

/// An OpenAI-compatible embeddings request body.
///
/// `model` and `input` are recognised and validated; **every other field is
/// captured in [`extra`](Self::extra) and forwarded verbatim** to the upstream
/// (`dimensions`, `encoding_format`, `user`, `input_type`, …). There is no URL
/// field, and `extra` cannot change the outbound destination (see the
/// module-level SSRF note) — it only rides along to the configured upstream.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EmbeddingsRequest {
    /// Requested model name. Forwarded to the upstream; when empty, the
    /// server's configured default model (if any) is used.
    #[serde(default)]
    pub model: String,
    /// Text(s) to embed.
    pub input: EmbeddingInput,
    /// Every other top-level field the client sent, forwarded verbatim to the
    /// upstream so provider-specific parameters pass through unchanged.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Errors surfaced by the embeddings path.
#[derive(Debug, thiserror::Error)]
pub enum EmbeddingsError {
    /// No backend is configured, so the endpoint cannot serve requests.
    #[error("embeddings backend not configured")]
    NotConfigured,
    /// The request was well-formed JSON but semantically invalid (e.g. empty
    /// input) — a `400`.
    #[error("invalid embeddings request: {0}")]
    InvalidInput(String),
    /// The configured upstream URL is malformed or uses an unsupported scheme
    /// — a server-configuration fault.
    #[error("invalid embeddings upstream: {0}")]
    InvalidUpstream(String),
    /// The upstream call failed or returned an unexpected response — a bad
    /// gateway (`502`).
    #[error("embeddings upstream error: {0}")]
    Upstream(String),
}

/// Server-side configuration for the embeddings upstream.
///
/// Constructed from the environment only ([`Self::from_env`]); there is no way
/// to derive it from a request. This is the SSRF boundary: the upstream is an
/// operator choice, never an attacker's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingsConfig {
    /// Full URL of the upstream OpenAI-compatible embeddings endpoint, e.g.
    /// `https://api.openai.com/v1/embeddings` or `http://localhost:11434/v1/embeddings`.
    pub upstream: String,
    /// Optional bearer token sent as `Authorization: Bearer <key>`.
    pub api_key: Option<String>,
    /// Optional default model, used when a request omits `model`.
    pub model: Option<String>,
}

impl EmbeddingsConfig {
    /// Read the embeddings configuration from a getter mimicking
    /// [`std::env::var`].
    ///
    /// Returns `Ok(None)` when `DREVO_EMBEDDINGS_UPSTREAM` is unset — the
    /// endpoint then reports "not configured". Recognised variables:
    ///
    /// | Variable                    | Meaning                              |
    /// |-----------------------------|--------------------------------------|
    /// | `DREVO_EMBEDDINGS_UPSTREAM` | Upstream embeddings URL (http/https) |
    /// | `DREVO_EMBEDDINGS_API_KEY`  | Bearer token (optional)              |
    /// | `DREVO_EMBEDDINGS_MODEL`    | Default model when request omits it  |
    ///
    /// # Errors
    ///
    /// Returns [`EmbeddingsError::InvalidUpstream`] when the URL is empty or
    /// does not use the `http`/`https` scheme.
    pub fn from_env<F>(getter: F) -> Result<Option<Self>, EmbeddingsError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let upstream = match getter("DREVO_EMBEDDINGS_UPSTREAM") {
            None => return Ok(None),
            Some(u) => u,
        };
        let upstream = validate_upstream(&upstream)?;
        let api_key = getter("DREVO_EMBEDDINGS_API_KEY")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let model = getter("DREVO_EMBEDDINGS_MODEL")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Ok(Some(Self {
            upstream,
            api_key,
            model,
        }))
    }
}

/// Validate and normalise an upstream URL: trimmed, non-empty, `http`/`https`
/// only (the SSRF boundary — an operator choice, never a request's).
fn validate_upstream(raw: &str) -> Result<String, EmbeddingsError> {
    let upstream = raw.trim().to_string();
    if upstream.is_empty() {
        return Err(EmbeddingsError::InvalidUpstream(
            "embeddings upstream must not be empty".to_string(),
        ));
    }
    if !(upstream.starts_with("http://") || upstream.starts_with("https://")) {
        return Err(EmbeddingsError::InvalidUpstream(format!(
            "unsupported scheme in `{upstream}` (expected http:// or https://)"
        )));
    }
    Ok(upstream)
}

/// A partial update to the embeddings configuration, as accepted by the
/// `POST /config/embeddings` endpoint and the Web UI settings form.
///
/// Merge semantics (a form always sends `upstream`, and leaves the key field
/// blank to keep the existing secret):
/// * `upstream` — required and validated (`http`/`https`).
/// * `api_key` — `None` or empty **keeps** the current key (so the model can be
///   changed without re-entering the secret); a non-empty value replaces it.
/// * `model` — `None` or empty clears the default model; a value sets it.
#[derive(Debug, Clone, Deserialize)]
pub struct EmbeddingsConfigUpdate {
    /// New upstream URL (required, validated).
    pub upstream: String,
    /// New bearer token, or `None`/empty to keep the existing one.
    #[serde(default)]
    pub api_key: Option<String>,
    /// New default model, or `None`/empty for no default.
    #[serde(default)]
    pub model: Option<String>,
}

/// A **secret-free** view of the embeddings configuration, returned by
/// `GET /config/embeddings`. The API key itself is never included — only
/// whether one is set — so the endpoint (and the Web UI) can display the
/// configuration without ever echoing the token back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EmbeddingsStatus {
    /// Whether a usable upstream is configured.
    pub configured: bool,
    /// The configured upstream URL, if any.
    pub upstream: Option<String>,
    /// The configured default model, if any.
    pub model: Option<String>,
    /// Whether a bearer token is set (never the token itself).
    pub api_key_set: bool,
}

/// A shared, hot-reloadable, optionally-persisted store for the runtime
/// embeddings configuration (RFC #307 follow-up: the API key/upstream/model
/// become Web-UI settings instead of start-only env vars).
///
/// Both the `/v1/embeddings` proxy and the semantic-query embedder read a
/// [`Self::snapshot`] on every call, so a write through [`Self::apply`] takes
/// effect immediately with no restart and no backend rebuild. When a `path` is
/// set the config is persisted there as JSON
/// with `0600` permissions (the API key lives at the same trust level as the
/// env file it replaces) and reloaded on the next boot.
#[derive(Debug)]
pub struct EmbeddingsConfigStore {
    /// Where to persist the config, if anywhere (`None` = in-memory only).
    path: Option<std::path::PathBuf>,
    /// The live config (`None` = not configured; the endpoint answers 503).
    config: std::sync::RwLock<Option<EmbeddingsConfig>>,
}

impl EmbeddingsConfigStore {
    /// An in-memory store (no persistence) seeded with `config`. Used by tests
    /// and by builds that do not resolve a data directory.
    pub fn in_memory(config: Option<EmbeddingsConfig>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            path: None,
            config: std::sync::RwLock::new(config),
        })
    }

    /// A persisted store rooted at `path`. If the file exists and parses, its
    /// config wins (a value the operator set through the UI on a previous run);
    /// otherwise `env_fallback` seeds it (the classic `DREVO_EMBEDDINGS_*`
    /// path). A malformed file falls back to `env_fallback` rather than failing
    /// the boot.
    pub fn load(
        path: std::path::PathBuf,
        env_fallback: Option<EmbeddingsConfig>,
    ) -> std::sync::Arc<Self> {
        let config = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<EmbeddingsConfig>(&text)
                .ok()
                .or(env_fallback),
            Err(_) => env_fallback,
        };
        std::sync::Arc::new(Self {
            path: Some(path),
            config: std::sync::RwLock::new(config),
        })
    }

    /// A clone of the current config, or `None` when unconfigured. Taken on
    /// every proxy/embedder call so writes are picked up live.
    pub fn snapshot(&self) -> Option<EmbeddingsConfig> {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// A secret-free status view for the config endpoint / Web UI.
    pub fn status(&self) -> EmbeddingsStatus {
        match &*self.config.read().unwrap_or_else(|e| e.into_inner()) {
            Some(c) => EmbeddingsStatus {
                configured: true,
                upstream: Some(c.upstream.clone()),
                model: c.model.clone(),
                api_key_set: c.api_key.is_some(),
            },
            None => EmbeddingsStatus {
                configured: false,
                upstream: None,
                model: None,
                api_key_set: false,
            },
        }
    }

    /// Apply a partial update (validating the upstream), persist it if a path
    /// is set, and swap it in live. Returns the new secret-free status.
    ///
    /// A blank `api_key` keeps the current secret; persistence is attempted
    /// **before** the in-memory swap, so a disk failure leaves the running
    /// config unchanged and is reported to the caller.
    pub fn apply(
        &self,
        update: EmbeddingsConfigUpdate,
    ) -> Result<EmbeddingsStatus, EmbeddingsError> {
        let upstream = validate_upstream(&update.upstream)?;
        let current = self.snapshot();
        let api_key = match update.api_key.map(|s| s.trim().to_string()) {
            Some(k) if !k.is_empty() => Some(k),
            // Blank or absent: keep whatever key is already configured.
            _ => current.and_then(|c| c.api_key),
        };
        let model = update
            .model
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let next = EmbeddingsConfig {
            upstream,
            api_key,
            model,
        };

        if let Some(path) = &self.path {
            persist_config(path, &next)?;
        }
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = Some(next);
        Ok(self.status())
    }
}

/// Write `config` to `path` as pretty JSON with `0600` permissions, atomically
/// (temp file in the same directory, then rename). The API key is stored here
/// at the same trust level as the env file it replaces.
fn persist_config(
    path: &std::path::Path,
    config: &EmbeddingsConfig,
) -> Result<(), EmbeddingsError> {
    /// Write `bytes` to `tmp` (owner-only on unix) then rename onto `path`.
    fn write_atomic(
        tmp: &std::path::Path,
        path: &std::path::Path,
        bytes: &[u8],
    ) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(tmp, bytes)?;
        }
        std::fs::rename(tmp, path)
    }

    let json = serde_json::to_string_pretty(config)
        .map_err(|e| EmbeddingsError::InvalidUpstream(format!("cannot serialise config: {e}")))?;
    let tmp = path.with_extension("json.tmp");
    write_atomic(&tmp, path, json.as_bytes()).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        EmbeddingsError::InvalidUpstream(format!("cannot persist embeddings config: {e}"))
    })
}

/// The effective model for a request: the request's own model, or the
/// configured default when the request omits it.
fn effective_model(req: &EmbeddingsRequest, config: &EmbeddingsConfig) -> String {
    if req.model.is_empty() {
        config.model.clone().unwrap_or_default()
    } else {
        req.model.clone()
    }
}

/// Build the JSON body forwarded to the upstream. Pure (no I/O) so it is unit
/// testable without a network client.
///
/// Starts from the request's passthrough [`extra`](EmbeddingsRequest::extra)
/// fields (so provider-specific parameters survive), then sets `model` (from
/// the request or the configured default) and `input` (normalised to an array,
/// which every OpenAI-compatible upstream accepts). `model` / `input` always
/// win over any collision — though `extra` can never contain them, since they
/// are consumed by the named fields during deserialization.
#[must_use]
pub fn build_upstream_body(req: &EmbeddingsRequest, config: &EmbeddingsConfig) -> Value {
    let mut body = req.extra.clone();
    body.insert(
        "model".to_string(),
        Value::String(effective_model(req, config)),
    );
    body.insert(
        "input".to_string(),
        Value::Array(req.input.texts().into_iter().map(Value::String).collect()),
    );
    Value::Object(body)
}

/// A configured embeddings backend.
///
/// This is an enum rather than a trait object so the crate stays free of an
/// async-trait dependency. When no backend feature is enabled it is an
/// uninhabited type, and [`crate::api::ApiState`] simply holds `None` — the
/// endpoint answers `503`.
pub enum EmbeddingBackend {
    /// Forward requests to a configured OpenAI-compatible upstream.
    #[cfg(feature = "embeddings-proxy")]
    Proxy(ProxyBackend),
}

impl EmbeddingBackend {
    /// Produce embeddings for `req`, returning the upstream's JSON response
    /// verbatim (passthrough).
    ///
    /// # Errors
    ///
    /// Propagates the backend's [`EmbeddingsError`] (upstream failure, invalid
    /// response, …).
    #[cfg_attr(not(feature = "embeddings-proxy"), allow(unused_variables))]
    pub async fn embed(&self, req: &EmbeddingsRequest) -> Result<Value, EmbeddingsError> {
        match *self {
            #[cfg(feature = "embeddings-proxy")]
            Self::Proxy(ref p) => p.embed(req).await,
        }
    }
}

/// Proxy backend: forwards each request to a configured upstream and passes the
/// response back verbatim. Enabled by the `embeddings-proxy` feature (pulls in
/// an HTTP client); off by default so the standard binary carries no extra
/// dependency.
#[cfg(feature = "embeddings-proxy")]
pub struct ProxyBackend {
    client: reqwest::Client,
    store: std::sync::Arc<EmbeddingsConfigStore>,
}

#[cfg(feature = "embeddings-proxy")]
impl ProxyBackend {
    /// Build a proxy backend that reads the shared runtime config store on
    /// every request, so a config change through
    /// [`EmbeddingsConfigStore::apply`] takes effect with no rebuild.
    ///
    /// # Errors
    ///
    /// Returns [`EmbeddingsError::InvalidUpstream`] when the HTTP client cannot
    /// be constructed.
    pub fn new(store: std::sync::Arc<EmbeddingsConfigStore>) -> Result<Self, EmbeddingsError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| EmbeddingsError::InvalidUpstream(e.to_string()))?;
        Ok(Self { client, store })
    }

    /// Convenience: a proxy over a fixed config (wrapped in an in-memory store).
    /// Handy for tests and callers with a static configuration.
    ///
    /// # Errors
    ///
    /// Returns [`EmbeddingsError::InvalidUpstream`] when the HTTP client cannot
    /// be constructed.
    pub fn from_config(config: EmbeddingsConfig) -> Result<Self, EmbeddingsError> {
        Self::new(EmbeddingsConfigStore::in_memory(Some(config)))
    }

    /// Forward `req` to the currently-configured upstream and return its
    /// response body verbatim as JSON.
    ///
    /// # Errors
    ///
    /// Returns [`EmbeddingsError::NotConfigured`] when no upstream is set, or
    /// [`EmbeddingsError::Upstream`] when the upstream is unreachable, returns a
    /// non-2xx status, or sends a body that is not valid JSON.
    pub async fn embed(&self, req: &EmbeddingsRequest) -> Result<Value, EmbeddingsError> {
        let config = self
            .store
            .snapshot()
            .ok_or(EmbeddingsError::NotConfigured)?;
        let body = build_upstream_body(req, &config);
        let mut builder = self.client.post(&config.upstream).json(&body);
        if let Some(key) = &config.api_key {
            builder = builder.bearer_auth(key);
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| EmbeddingsError::Upstream(e.to_string()))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| EmbeddingsError::Upstream(e.to_string()))?;
        if !status.is_success() {
            return Err(EmbeddingsError::Upstream(format!(
                "upstream returned {status}: {}",
                String::from_utf8_lossy(&bytes).trim()
            )));
        }
        // Passthrough: return the upstream body verbatim (base64 embeddings and
        // any provider-specific fields survive because nothing is re-typed).
        serde_json::from_slice(&bytes)
            .map_err(|e| EmbeddingsError::Upstream(format!("malformed upstream response: {e}")))
    }
}

/// Extract a single embedding vector from an OpenAI-shaped embeddings
/// response (#251 slice 3).
///
/// Reads `data[0].embedding` and requires it to be a **numeric array**,
/// returning it as `Vec<f32>`. This is deliberately strict:
///
/// - A **base64 string** embedding (`encoding_format: "base64"`, which the
///   passthrough proxy forwards verbatim) is **rejected** with a clear error
///   rather than silently mis-read — `drevo.semantic.query` needs the raw
///   floats to run a cosine scan, so the caller must not request base64.
/// - A missing/empty `data` array, a missing `embedding` field, or a
///   non-numeric element are all reported as upstream faults.
///
/// # Errors
///
/// Returns [`EmbeddingsError::Upstream`] when the response does not carry a
/// numeric `data[0].embedding` array.
pub fn embedding_vec_from_response(resp: &Value) -> Result<Vec<f32>, EmbeddingsError> {
    let data = resp
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| EmbeddingsError::Upstream("response missing `data` array".to_string()))?;
    let first = data
        .first()
        .ok_or_else(|| EmbeddingsError::Upstream("response `data` array is empty".to_string()))?;
    let embedding = first.get("embedding").ok_or_else(|| {
        EmbeddingsError::Upstream("response missing `data[0].embedding`".to_string())
    })?;
    match embedding {
        Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for (index, el) in arr.iter().enumerate() {
                let n = el.as_f64().ok_or_else(|| {
                    EmbeddingsError::Upstream(format!(
                        "embedding element at index {index} is not a number"
                    ))
                })?;
                out.push(n as f32);
            }
            Ok(out)
        }
        Value::String(_) => Err(EmbeddingsError::Upstream(
            "upstream returned a base64-encoded embedding; drevo.semantic.query requires a \
             numeric array (do not request encoding_format=base64)"
                .to_string(),
        )),
        _ => Err(EmbeddingsError::Upstream(
            "`data[0].embedding` is neither a numeric array nor a string".to_string(),
        )),
    }
}

/// A synchronous, server-side text embedder for `drevo.semantic.query`
/// (#251 slice 3).
///
/// The Cypher / Bolt executor is **synchronous** and runs *on a tokio worker
/// thread*, so it cannot `block_on` an async embedder (that panics with a
/// "runtime within a runtime" error). Implementors bridge that gap — see
/// [`SyncEmbedder`], which owns a dedicated OS thread with its own
/// current-thread runtime — and expose a plain blocking `embed_query`.
///
/// Installed on a [`Drevo`](crate::db::Drevo) handle via
/// `set_embedder`; the `drevo.semantic.query` procedure calls `embed_text`,
/// which delegates here. Feature-gated on `http` (the module itself is).
pub trait TextEmbedder: Send + Sync {
    /// Embed one query string into a vector, blocking until the upstream
    /// responds.
    ///
    /// # Errors
    ///
    /// Returns an [`EmbeddingsError`] when the upstream call fails or its
    /// response does not carry a usable numeric embedding.
    fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbeddingsError>;

    /// The configured model id this embedder serves, if known (#267 capability
    /// introspection). `None` by default; a concrete backend overrides it.
    fn model(&self) -> Option<String> {
        None
    }

    /// The configured upstream endpoint URL, if known (#267). Never a secret —
    /// the bearer token is not part of it. `None` by default.
    fn upstream(&self) -> Option<String> {
        None
    }
}

/// A [`TextEmbedder`] that drives the async [`ProxyBackend`] from synchronous
/// code without nesting tokio runtimes (#251 slice 3).
///
/// It owns a **dedicated OS thread** running a private current-thread tokio
/// runtime and receives jobs over an mpsc channel. `embed_query` posts the
/// text and blocks on a reply channel, so the *caller's* thread (a worker of
/// the server's multi-threaded runtime) never runs `block_on` — sidestepping
/// the runtime-in-runtime panic that a direct `Handle::block_on` or
/// `reqwest::blocking` would hit. Enabled by `embeddings-proxy` (which pulls
/// in the HTTP client and, transitively, tokio).
#[cfg(feature = "embeddings-proxy")]
pub struct SyncEmbedder {
    sender: std::sync::mpsc::Sender<EmbedJob>,
    // The worker thread lives as long as the sender: dropping `SyncEmbedder`
    // drops `sender`, the worker's `recv` returns `Err`, and the thread exits.
    // Not joined on drop (best-effort teardown); the handle is retained so the
    // thread is owned rather than detached.
    _worker: std::thread::JoinHandle<()>,
    // #267 capability introspection (never the API key): read live from the
    // shared store so `drevo.semantic.info` reflects the current config after a
    // Web-UI change, not just what was set at boot.
    store: std::sync::Arc<EmbeddingsConfigStore>,
}

/// One embedding request handed to the [`SyncEmbedder`] worker thread, with a
/// one-shot reply channel for the result.
#[cfg(feature = "embeddings-proxy")]
struct EmbedJob {
    text: String,
    reply: std::sync::mpsc::Sender<Result<Vec<f32>, EmbeddingsError>>,
}

#[cfg(feature = "embeddings-proxy")]
impl SyncEmbedder {
    /// Spawn the worker thread and return a handle that reads the shared
    /// runtime config store. Because the worker forwards through a
    /// store-backed [`ProxyBackend`], a config change through
    /// [`EmbeddingsConfigStore::apply`] is picked up on the next query with no
    /// restart.
    ///
    /// # Errors
    ///
    /// Returns [`EmbeddingsError::InvalidUpstream`] when the HTTP client or the
    /// worker thread / runtime cannot be constructed.
    pub fn from_store(
        store: std::sync::Arc<EmbeddingsConfigStore>,
    ) -> Result<Self, EmbeddingsError> {
        let backend = ProxyBackend::new(store.clone())?;
        let (sender, receiver) = std::sync::mpsc::channel::<EmbedJob>();
        let worker = std::thread::Builder::new()
            .name("drevo-embedder".to_string())
            .spawn(move || {
                // The runtime is single-threaded and confined to this OS
                // thread, so `block_on` here never nests inside the server's
                // worker-pool runtime.
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    // Cannot build a runtime: exit the thread. Every pending and
                    // future send then fails with a disconnect error, which
                    // `embed_query` maps to a clean upstream error.
                    Err(_) => return,
                };
                while let Ok(job) = receiver.recv() {
                    let req = EmbeddingsRequest {
                        model: String::new(),
                        input: EmbeddingInput::Single(job.text),
                        extra: Map::new(),
                    };
                    let result = runtime
                        .block_on(backend.embed(&req))
                        .and_then(|value| embedding_vec_from_response(&value));
                    // The receiver may have given up (dropped its end); ignore.
                    let _ = job.reply.send(result);
                }
            })
            .map_err(|e| EmbeddingsError::InvalidUpstream(e.to_string()))?;
        Ok(Self {
            sender,
            _worker: worker,
            store,
        })
    }

    /// Convenience: an embedder over a fixed config (wrapped in an in-memory
    /// store). Handy for tests and callers with a static configuration.
    ///
    /// # Errors
    ///
    /// Returns [`EmbeddingsError::InvalidUpstream`] when the worker thread or
    /// HTTP client cannot be constructed.
    pub fn from_config(config: EmbeddingsConfig) -> Result<Self, EmbeddingsError> {
        Self::from_store(EmbeddingsConfigStore::in_memory(Some(config)))
    }
}

#[cfg(feature = "embeddings-proxy")]
impl TextEmbedder for SyncEmbedder {
    fn model(&self) -> Option<String> {
        self.store.snapshot().and_then(|c| c.model)
    }

    fn upstream(&self) -> Option<String> {
        self.store.snapshot().map(|c| c.upstream)
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbeddingsError> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.sender
            .send(EmbedJob {
                text: text.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| EmbeddingsError::Upstream("embedder worker thread stopped".to_string()))?;
        reply_rx.recv().map_err(|_| {
            EmbeddingsError::Upstream("embedder worker dropped the request".to_string())
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(upstream: &str) -> EmbeddingsConfig {
        EmbeddingsConfig {
            upstream: upstream.to_string(),
            api_key: None,
            model: None,
        }
    }

    #[test]
    fn input_single_and_batch_normalise() {
        assert_eq!(
            EmbeddingInput::Single("a".into()).texts(),
            vec!["a".to_string()]
        );
        assert_eq!(
            EmbeddingInput::Batch(vec!["a".into(), "b".into()]).texts(),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn input_emptiness() {
        assert!(EmbeddingInput::Batch(vec![]).is_empty());
        assert!(EmbeddingInput::Batch(vec![String::new()]).is_empty());
        assert!(EmbeddingInput::Single(String::new()).is_empty());
        assert!(!EmbeddingInput::Single("x".into()).is_empty());
        assert!(!EmbeddingInput::Batch(vec!["x".into()]).is_empty());
    }

    #[test]
    fn request_parses_string_and_array_input() {
        let single: EmbeddingsRequest =
            serde_json::from_str(r#"{"model":"m","input":"hello"}"#).unwrap();
        assert_eq!(single.input, EmbeddingInput::Single("hello".into()));
        let batch: EmbeddingsRequest =
            serde_json::from_str(r#"{"model":"m","input":["a","b"]}"#).unwrap();
        assert_eq!(
            batch.input,
            EmbeddingInput::Batch(vec!["a".into(), "b".into()])
        );
    }

    #[test]
    fn request_captures_extra_fields_for_passthrough() {
        // Provider-specific params (and even a stray `url`) land in `extra`,
        // ready to be forwarded verbatim — never dropped, never a destination.
        let req: EmbeddingsRequest = serde_json::from_str(
            r#"{"model":"m","input":"hi","dimensions":256,"input_type":"query","url":"http://169.254.169.254"}"#,
        )
        .unwrap();
        assert_eq!(req.model, "m");
        assert_eq!(req.input, EmbeddingInput::Single("hi".into()));
        assert_eq!(req.extra.get("dimensions"), Some(&json!(256)));
        assert_eq!(req.extra.get("input_type"), Some(&json!("query")));
        assert_eq!(req.extra.get("url"), Some(&json!("http://169.254.169.254")));
        // `model` / `input` are consumed by the named fields, not duplicated.
        assert!(!req.extra.contains_key("model"));
        assert!(!req.extra.contains_key("input"));
    }

    #[test]
    fn request_model_defaults_to_empty() {
        let req: EmbeddingsRequest = serde_json::from_str(r#"{"input":"hi"}"#).unwrap();
        assert_eq!(req.model, "");
    }

    #[test]
    fn config_from_env_absent_is_none() {
        let out = EmbeddingsConfig::from_env(|_| None).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn config_from_env_reads_all_fields() {
        let env = |k: &str| match k {
            "DREVO_EMBEDDINGS_UPSTREAM" => Some("https://api.openai.com/v1/embeddings".to_string()),
            "DREVO_EMBEDDINGS_API_KEY" => Some("sk-secret".to_string()),
            "DREVO_EMBEDDINGS_MODEL" => Some("text-embedding-3-small".to_string()),
            _ => None,
        };
        let cfg = EmbeddingsConfig::from_env(env).unwrap().unwrap();
        assert_eq!(cfg.upstream, "https://api.openai.com/v1/embeddings");
        assert_eq!(cfg.api_key.as_deref(), Some("sk-secret"));
        assert_eq!(cfg.model.as_deref(), Some("text-embedding-3-small"));
    }

    #[test]
    fn config_from_env_rejects_non_http_scheme() {
        let env =
            |k: &str| (k == "DREVO_EMBEDDINGS_UPSTREAM").then(|| "file:///etc/passwd".to_string());
        let err = EmbeddingsConfig::from_env(env).unwrap_err();
        assert!(matches!(err, EmbeddingsError::InvalidUpstream(_)));
    }

    #[test]
    fn config_from_env_rejects_empty_upstream() {
        let env = |k: &str| (k == "DREVO_EMBEDDINGS_UPSTREAM").then(|| "   ".to_string());
        let err = EmbeddingsConfig::from_env(env).unwrap_err();
        assert!(matches!(err, EmbeddingsError::InvalidUpstream(_)));
    }

    #[test]
    fn config_blank_api_key_and_model_are_none() {
        let env = |k: &str| match k {
            "DREVO_EMBEDDINGS_UPSTREAM" => Some("http://localhost:11434/v1/embeddings".to_string()),
            "DREVO_EMBEDDINGS_API_KEY" => Some("  ".to_string()),
            "DREVO_EMBEDDINGS_MODEL" => Some(String::new()),
            _ => None,
        };
        let cfg = EmbeddingsConfig::from_env(env).unwrap().unwrap();
        assert!(cfg.api_key.is_none());
        assert!(cfg.model.is_none());
    }

    fn update(
        upstream: &str,
        api_key: Option<&str>,
        model: Option<&str>,
    ) -> EmbeddingsConfigUpdate {
        EmbeddingsConfigUpdate {
            upstream: upstream.to_string(),
            api_key: api_key.map(str::to_string),
            model: model.map(str::to_string),
        }
    }

    #[test]
    fn store_snapshot_none_is_not_configured() {
        let store = EmbeddingsConfigStore::in_memory(None);
        assert!(store.snapshot().is_none());
        let s = store.status();
        assert!(!s.configured);
        assert!(!s.api_key_set);
        assert!(s.upstream.is_none());
    }

    #[test]
    fn store_apply_sets_config_and_status_hides_key() {
        let store = EmbeddingsConfigStore::in_memory(None);
        let status = store
            .apply(update(
                "https://api.openai.com/v1/embeddings",
                Some("sk-secret"),
                Some("m"),
            ))
            .unwrap();
        // Status reports configured + key-set, but NEVER the key itself.
        assert!(status.configured);
        assert!(status.api_key_set);
        assert_eq!(
            status.upstream.as_deref(),
            Some("https://api.openai.com/v1/embeddings")
        );
        assert_eq!(status.model.as_deref(), Some("m"));
        // The status type has no field that could carry the secret.
        let json = serde_json::to_string(&status).unwrap();
        assert!(
            !json.contains("sk-secret"),
            "status leaked the api key: {json}"
        );
        // The live snapshot (used by the proxy) does carry the key.
        assert_eq!(
            store.snapshot().unwrap().api_key.as_deref(),
            Some("sk-secret")
        );
    }

    #[test]
    fn store_apply_blank_key_keeps_existing_secret() {
        let store = EmbeddingsConfigStore::in_memory(None);
        store
            .apply(update("https://u/v1/embeddings", Some("sk-keep"), None))
            .unwrap();
        // A later update with a blank key changes only the model, keeping the key.
        store
            .apply(update("https://u/v1/embeddings", Some("  "), Some("m2")))
            .unwrap();
        let snap = store.snapshot().unwrap();
        assert_eq!(snap.api_key.as_deref(), Some("sk-keep"));
        assert_eq!(snap.model.as_deref(), Some("m2"));
        // An absent key field also keeps it.
        store
            .apply(update("https://u/v1/embeddings", None, None))
            .unwrap();
        assert_eq!(
            store.snapshot().unwrap().api_key.as_deref(),
            Some("sk-keep")
        );
    }

    #[test]
    fn store_apply_rejects_bad_upstream() {
        let store = EmbeddingsConfigStore::in_memory(None);
        let err = store
            .apply(update("file:///etc/passwd", Some("k"), None))
            .unwrap_err();
        assert!(matches!(err, EmbeddingsError::InvalidUpstream(_)));
        // A rejected update leaves the store unconfigured.
        assert!(store.snapshot().is_none());
    }

    #[test]
    fn store_load_persists_and_reloads_across_boots() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("embeddings_config.json");

        let store = EmbeddingsConfigStore::load(path.clone(), None);
        assert!(store.snapshot().is_none());
        store
            .apply(update(
                "https://api.openai.com/v1/embeddings",
                Some("sk-persist"),
                Some("m"),
            ))
            .unwrap();

        // A fresh store rooted at the same path reloads the persisted config
        // (the file wins over the env fallback).
        let reloaded = EmbeddingsConfigStore::load(path.clone(), None);
        let snap = reloaded.snapshot().unwrap();
        assert_eq!(snap.upstream, "https://api.openai.com/v1/embeddings");
        assert_eq!(snap.api_key.as_deref(), Some("sk-persist"));
        assert_eq!(snap.model.as_deref(), Some("m"));

        // The persisted file is owner-only (0600) on unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "config file must be 0600, got {mode:o}");
        }
    }

    #[test]
    fn upstream_body_forwards_model_and_normalised_input() {
        let req = EmbeddingsRequest {
            model: "m".into(),
            input: EmbeddingInput::Single("hi".into()),
            extra: Map::new(),
        };
        let body = build_upstream_body(&req, &cfg("https://u/v1/embeddings"));
        assert_eq!(body["model"], "m");
        assert_eq!(body["input"], json!(["hi"]));
    }

    #[test]
    fn upstream_body_uses_config_model_when_request_omits() {
        let req = EmbeddingsRequest {
            model: String::new(),
            input: EmbeddingInput::Batch(vec!["a".into(), "b".into()]),
            extra: Map::new(),
        };
        let mut c = cfg("https://u/v1/embeddings");
        c.model = Some("default-model".into());
        let body = build_upstream_body(&req, &c);
        assert_eq!(body["model"], "default-model");
        assert_eq!(body["input"], json!(["a", "b"]));
    }

    #[test]
    fn embedding_vec_parses_numeric_array() {
        let resp = json!({"data": [{"embedding": [0.1, 0.2, 3, -0.5]}]});
        let v = embedding_vec_from_response(&resp).unwrap();
        assert_eq!(v, vec![0.1_f32, 0.2, 3.0, -0.5]);
    }

    #[test]
    fn embedding_vec_rejects_base64_string() {
        // encoding_format=base64 makes the passthrough return a string; we must
        // reject it, not mis-read it, since the cosine scan needs raw floats.
        let resp = json!({"data": [{"embedding": "gAAAAB=="}]});
        let err = embedding_vec_from_response(&resp).unwrap_err();
        assert!(matches!(err, EmbeddingsError::Upstream(_)));
        assert!(err.to_string().contains("base64"));
    }

    #[test]
    fn embedding_vec_rejects_missing_and_empty_data() {
        assert!(matches!(
            embedding_vec_from_response(&json!({"object": "list"})).unwrap_err(),
            EmbeddingsError::Upstream(_)
        ));
        assert!(matches!(
            embedding_vec_from_response(&json!({"data": []})).unwrap_err(),
            EmbeddingsError::Upstream(_)
        ));
        assert!(matches!(
            embedding_vec_from_response(&json!({"data": [{"index": 0}]})).unwrap_err(),
            EmbeddingsError::Upstream(_)
        ));
    }

    #[test]
    fn embedding_vec_rejects_non_numeric_element() {
        let resp = json!({"data": [{"embedding": [0.1, "oops", 0.3]}]});
        assert!(matches!(
            embedding_vec_from_response(&resp).unwrap_err(),
            EmbeddingsError::Upstream(_)
        ));
    }

    #[test]
    fn upstream_body_forwards_extra_fields_verbatim() {
        // The passthrough contract: provider-specific params reach the upstream.
        let req: EmbeddingsRequest = serde_json::from_str(
            r#"{"model":"voyage-3","input":"hi","input_type":"document","dimensions":512}"#,
        )
        .unwrap();
        let body = build_upstream_body(&req, &cfg("https://u/v1/embeddings"));
        assert_eq!(body["model"], "voyage-3");
        assert_eq!(body["input"], json!(["hi"]));
        assert_eq!(body["input_type"], json!("document"));
        assert_eq!(body["dimensions"], json!(512));
    }
}
