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

use serde::Deserialize;
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
        let upstream = upstream.trim().to_string();
        if upstream.is_empty() {
            return Err(EmbeddingsError::InvalidUpstream(
                "DREVO_EMBEDDINGS_UPSTREAM must not be empty".to_string(),
            ));
        }
        if !(upstream.starts_with("http://") || upstream.starts_with("https://")) {
            return Err(EmbeddingsError::InvalidUpstream(format!(
                "unsupported scheme in `{upstream}` (expected http:// or https://)"
            )));
        }
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
    config: EmbeddingsConfig,
}

#[cfg(feature = "embeddings-proxy")]
impl ProxyBackend {
    /// Build a proxy backend for the given configuration.
    ///
    /// # Errors
    ///
    /// Returns [`EmbeddingsError::InvalidUpstream`] when the HTTP client cannot
    /// be constructed.
    pub fn new(config: EmbeddingsConfig) -> Result<Self, EmbeddingsError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| EmbeddingsError::InvalidUpstream(e.to_string()))?;
        Ok(Self { client, config })
    }

    /// Forward `req` to the configured upstream and return its response body
    /// verbatim as JSON.
    ///
    /// # Errors
    ///
    /// Returns [`EmbeddingsError::Upstream`] when the upstream is unreachable,
    /// returns a non-2xx status, or sends a body that is not valid JSON.
    pub async fn embed(&self, req: &EmbeddingsRequest) -> Result<Value, EmbeddingsError> {
        let body = build_upstream_body(req, &self.config);
        let mut builder = self.client.post(&self.config.upstream).json(&body);
        if let Some(key) = &self.config.api_key {
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
