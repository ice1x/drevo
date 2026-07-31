//! OpenAI-compatible text-embedding endpoint (Phase 19, issue #217).
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
//! # Backends
//!
//! The endpoint itself (types, validation, routing) is always compiled with
//! the `http` feature and adds **no** dependencies — the default binary stays
//! lean. Until a backend is configured it answers `503 Service Unavailable`
//! ("embeddings backend not configured"), exactly as the semantic-facet path
//! answers `400` when no embedder is present.
//!
//! A concrete backend is opt-in:
//!
//! - [`ProxyBackend`](crate::embeddings::ProxyBackend) (feature
//!   `embeddings-proxy`) forwards the request to a
//!   configured upstream (OpenAI, Ollama, vLLM, …) and passes the response
//!   back. This keeps drevo dependency-free by default while making one
//!   instance the whole RAG backend when enabled.
//!
//! # Security — SSRF (OWASP A10)
//!
//! The upstream is taken **only** from server configuration
//! ([`EmbeddingsConfig::from_env`](crate::embeddings::EmbeddingsConfig::from_env),
//! `DREVO_EMBEDDINGS_UPSTREAM`). It is never read from the request body:
//! [`EmbeddingsRequest`](crate::embeddings::EmbeddingsRequest) has no URL
//! field, so a
//! caller can never redirect drevo's outbound call at an internal address
//! (e.g. the cloud metadata endpoint). This is a type-level guarantee, locked
//! by tests.

use serde::{Deserialize, Serialize};

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
/// Only `model` and `input` are recognised. Any other field a client sends is
/// ignored by serde — in particular there is **no** way to specify an upstream
/// URL from the request (see the module-level SSRF note).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EmbeddingsRequest {
    /// Requested model name. Forwarded to the upstream; when empty, the
    /// server's configured default model (if any) is used.
    #[serde(default)]
    pub model: String,
    /// Text(s) to embed.
    pub input: EmbeddingInput,
}

fn list_object() -> String {
    "list".to_string()
}

fn embedding_object() -> String {
    "embedding".to_string()
}

/// One embedding in a response, matching OpenAI's `data[]` element.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingData {
    /// Always `"embedding"`.
    #[serde(default = "embedding_object")]
    pub object: String,
    /// Zero-based position of this embedding, matching the input order.
    pub index: usize,
    /// The embedding vector.
    pub embedding: Vec<f32>,
}

/// Token accounting, mirroring OpenAI's `usage`. drevo does not tokenise, so
/// when proxying it reflects whatever the upstream reports (default zeros).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens in the prompt/input.
    #[serde(default)]
    pub prompt_tokens: usize,
    /// Total tokens billed.
    #[serde(default)]
    pub total_tokens: usize,
}

/// An OpenAI-compatible embeddings response body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingsResponse {
    /// Always `"list"`.
    #[serde(default = "list_object")]
    pub object: String,
    /// The embeddings, one per input, in input order.
    pub data: Vec<EmbeddingData>,
    /// The model that produced the embeddings.
    #[serde(default)]
    pub model: String,
    /// Token accounting.
    #[serde(default)]
    pub usage: Usage,
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
    /// — a server-configuration fault (`500`).
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
/// testable without a network client. Always normalises `input` to an array,
/// which every OpenAI-compatible upstream accepts.
#[must_use]
pub fn build_upstream_body(
    req: &EmbeddingsRequest,
    config: &EmbeddingsConfig,
) -> serde_json::Value {
    serde_json::json!({
        "model": effective_model(req, config),
        "input": req.input.texts(),
    })
}

/// Parse an upstream response body into an [`EmbeddingsResponse`]. Pure (no
/// I/O). When the upstream omits `model`, `fallback_model` is stamped in so the
/// caller always sees which model was used.
///
/// # Errors
///
/// Returns [`EmbeddingsError::Upstream`] when the bytes are not a valid
/// OpenAI-shaped embeddings response.
pub fn parse_upstream_response(
    bytes: &[u8],
    fallback_model: &str,
) -> Result<EmbeddingsResponse, EmbeddingsError> {
    let mut resp: EmbeddingsResponse = serde_json::from_slice(bytes)
        .map_err(|e| EmbeddingsError::Upstream(format!("malformed upstream response: {e}")))?;
    if resp.model.is_empty() {
        resp.model = fallback_model.to_string();
    }
    Ok(resp)
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
    /// Produce embeddings for `req`.
    ///
    /// # Errors
    ///
    /// Propagates the backend's [`EmbeddingsError`] (upstream failure, invalid
    /// response, …).
    #[cfg_attr(not(feature = "embeddings-proxy"), allow(unused_variables))]
    pub async fn embed(
        &self,
        req: &EmbeddingsRequest,
    ) -> Result<EmbeddingsResponse, EmbeddingsError> {
        match *self {
            #[cfg(feature = "embeddings-proxy")]
            Self::Proxy(ref p) => p.embed(req).await,
        }
    }
}

/// Proxy backend: forwards each request to a configured upstream and passes the
/// response back. Enabled by the `embeddings-proxy` feature (pulls in an HTTP
/// client); off by default so the standard binary carries no extra dependency.
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

    /// Forward `req` to the configured upstream and return its response.
    ///
    /// # Errors
    ///
    /// Returns [`EmbeddingsError::Upstream`] when the upstream is unreachable,
    /// returns a non-2xx status, or sends a body that is not a valid
    /// OpenAI-shaped embeddings response.
    pub async fn embed(
        &self,
        req: &EmbeddingsRequest,
    ) -> Result<EmbeddingsResponse, EmbeddingsError> {
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
        parse_upstream_response(&bytes, &effective_model(req, &self.config))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn request_ignores_unknown_fields_no_url_smuggling() {
        // The SSRF boundary at the type level: extra fields (a would-be
        // upstream override) are accepted and dropped, not honoured.
        let req: EmbeddingsRequest = serde_json::from_str(
            r#"{"model":"m","input":"hi","url":"http://169.254.169.254","base_url":"x"}"#,
        )
        .unwrap();
        assert_eq!(req.model, "m");
        assert_eq!(req.input, EmbeddingInput::Single("hi".into()));
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
        };
        let body = build_upstream_body(&req, &cfg("https://u/v1/embeddings"));
        assert_eq!(body["model"], "m");
        assert_eq!(body["input"], serde_json::json!(["hi"]));
    }

    #[test]
    fn upstream_body_uses_config_model_when_request_omits() {
        let req = EmbeddingsRequest {
            model: String::new(),
            input: EmbeddingInput::Batch(vec!["a".into(), "b".into()]),
        };
        let mut c = cfg("https://u/v1/embeddings");
        c.model = Some("default-model".into());
        let body = build_upstream_body(&req, &c);
        assert_eq!(body["model"], "default-model");
        assert_eq!(body["input"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn parse_upstream_response_ok() {
        let upstream = r#"{
            "object":"list",
            "data":[{"object":"embedding","index":0,"embedding":[0.1,0.2]}],
            "model":"text-embedding-3-small",
            "usage":{"prompt_tokens":3,"total_tokens":3}
        }"#;
        let resp = parse_upstream_response(upstream.as_bytes(), "fallback").unwrap();
        assert_eq!(resp.object, "list");
        assert_eq!(resp.data.len(), 1);
        assert_eq!(resp.data[0].index, 0);
        assert_eq!(resp.data[0].embedding, vec![0.1, 0.2]);
        assert_eq!(resp.model, "text-embedding-3-small");
        assert_eq!(resp.usage.total_tokens, 3);
    }

    #[test]
    fn parse_upstream_response_stamps_fallback_model() {
        let upstream = r#"{"data":[{"index":0,"embedding":[1.0]}]}"#;
        let resp = parse_upstream_response(upstream.as_bytes(), "fallback-model").unwrap();
        // Defaults filled in: object strings and model fallback.
        assert_eq!(resp.object, "list");
        assert_eq!(resp.data[0].object, "embedding");
        assert_eq!(resp.model, "fallback-model");
    }

    #[test]
    fn parse_upstream_response_rejects_garbage() {
        let err = parse_upstream_response(b"not json", "m").unwrap_err();
        assert!(matches!(err, EmbeddingsError::Upstream(_)));
    }

    #[test]
    fn response_serialises_openai_shape() {
        let resp = EmbeddingsResponse {
            object: list_object(),
            data: vec![EmbeddingData {
                object: embedding_object(),
                index: 0,
                embedding: vec![0.5, 0.25],
            }],
            model: "m".into(),
            usage: Usage::default(),
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["object"], "list");
        assert_eq!(v["data"][0]["object"], "embedding");
        assert_eq!(v["data"][0]["index"], 0);
        assert_eq!(v["data"][0]["embedding"], serde_json::json!([0.5, 0.25]));
        assert_eq!(v["model"], "m");
        assert_eq!(v["usage"]["total_tokens"], 0);
    }
}
