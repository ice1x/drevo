//! Text-to-Cypher LLM proxy (issue #429) — the `drevo.cypher.fromText(prompt)`
//! procedure's backend, modeled on the `/v1/embeddings` proxy
//! ([`crate::embeddings`]).
//!
//! # What this is
//!
//! A **thin proxy** that turns a natural-language question into a Cypher query
//! by forwarding it — together with the live graph schema — to an
//! operator-configured LLM upstream (OpenAI/Ollama/vLLM/… `chat/completions`).
//! It **returns** the generated Cypher string; it never executes it, so an
//! LLM-authored mutation can only run if the client explicitly chooses to.
//!
//! # Shape (mirrors [`crate::embeddings`])
//!
//! - [`Text2CypherConfig`](crate::text2cypher::Text2CypherConfig) is built from
//!   the environment only
//!   ([`from_env`](crate::text2cypher::Text2CypherConfig::from_env)) — the
//!   upstream is an operator choice, never a request's (the SSRF boundary).
//!   `http`/`https` only.
//! - [`CypherGenerator`](crate::text2cypher::CypherGenerator) is the synchronous
//!   trait the Cypher executor depends on; it is engine- and HTTP-agnostic, so
//!   the procedure compiles in every build and a test can install a fake
//!   generator with no network.
//! - `ChatProxyBackend` / `SyncCypherGenerator` (gated on the existing
//!   `embeddings-proxy` feature) are the `reqwest`-backed implementation. The
//!   sync generator bridges the async client from the executor's synchronous,
//!   tokio-worker-thread context via a dedicated OS thread + current-thread
//!   runtime — exactly like [`crate::embeddings::SyncEmbedder`], sidestepping
//!   the runtime-in-runtime panic.
//! - A process-global install
//!   ([`install`](crate::text2cypher::install) /
//!   [`installed`](crate::text2cypher::installed)) is set once at server
//!   startup: text-to-Cypher is stateless and process-wide, so no
//!   per-[`Drevo`](crate::db::Drevo)-handle threading is needed.

use std::sync::Arc;

/// Why a text-to-Cypher request failed.
#[derive(Debug, thiserror::Error)]
pub enum Text2CypherError {
    /// No upstream is configured, so the procedure cannot serve requests.
    #[error(
        "text-to-Cypher is not configured on this server \
         (set DREVO_TEXT2CYPHER_UPSTREAM/MODEL/API_KEY)"
    )]
    NotConfigured,
    /// The configured upstream URL is malformed or uses an unsupported scheme —
    /// a server-configuration fault.
    #[error("invalid text-to-Cypher upstream: {0}")]
    InvalidUpstream(String),
    /// The upstream call failed or returned an unexpected response.
    #[error("text-to-Cypher upstream error: {0}")]
    Upstream(String),
}

/// Server-side configuration for the text-to-Cypher upstream.
///
/// Constructed from the environment only ([`Self::from_env`]); there is no way
/// to derive it from a request. This is the SSRF boundary: the upstream is an
/// operator choice, never an attacker's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text2CypherConfig {
    /// Full URL of the upstream OpenAI-compatible `chat/completions` endpoint,
    /// e.g. `https://api.openai.com/v1/chat/completions` or
    /// `http://localhost:11434/v1/chat/completions`.
    pub upstream: String,
    /// Optional bearer token sent as `Authorization: Bearer <key>`.
    pub api_key: Option<String>,
    /// The chat model to request (e.g. `gpt-4o-mini`, `llama3.1`).
    pub model: String,
}

/// The default model used when `DREVO_TEXT2CYPHER_MODEL` is unset.
const DEFAULT_MODEL: &str = "gpt-4o-mini";

impl Text2CypherConfig {
    /// Read the text-to-Cypher configuration from a getter mimicking
    /// [`std::env::var`].
    ///
    /// Returns `Ok(None)` when `DREVO_TEXT2CYPHER_UPSTREAM` is unset — the
    /// procedure then reports "not configured". Recognised variables:
    ///
    /// | Variable                      | Meaning                                   |
    /// |-------------------------------|-------------------------------------------|
    /// | `DREVO_TEXT2CYPHER_UPSTREAM`  | Upstream chat/completions URL (http/https)|
    /// | `DREVO_TEXT2CYPHER_API_KEY`   | Bearer token (optional)                   |
    /// | `DREVO_TEXT2CYPHER_MODEL`     | Chat model (default `gpt-4o-mini`)        |
    ///
    /// # Errors
    ///
    /// Returns [`Text2CypherError::InvalidUpstream`] when the URL is empty or
    /// does not use the `http`/`https` scheme.
    pub fn from_env<F>(getter: F) -> Result<Option<Self>, Text2CypherError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let upstream = match getter("DREVO_TEXT2CYPHER_UPSTREAM") {
            None => return Ok(None),
            Some(u) => u,
        };
        let upstream = validate_upstream(&upstream)?;
        let api_key = getter("DREVO_TEXT2CYPHER_API_KEY")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let model = getter("DREVO_TEXT2CYPHER_MODEL")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        Ok(Some(Self {
            upstream,
            api_key,
            model,
        }))
    }
}

/// Validate and normalise an upstream URL: trimmed, non-empty, `http`/`https`
/// only (the SSRF boundary — an operator choice, never a request's).
fn validate_upstream(raw: &str) -> Result<String, Text2CypherError> {
    let upstream = raw.trim().to_string();
    if upstream.is_empty() {
        return Err(Text2CypherError::InvalidUpstream(
            "text-to-Cypher upstream must not be empty".to_string(),
        ));
    }
    if !(upstream.starts_with("http://") || upstream.starts_with("https://")) {
        return Err(Text2CypherError::InvalidUpstream(format!(
            "unsupported scheme in `{upstream}` (expected http:// or https://)"
        )));
    }
    Ok(upstream)
}

/// Build the system prompt sent to the LLM: fixed instructions plus the live
/// graph schema (labels, relationship types, property keys). Keeping the schema
/// in the prompt steers the model toward queries that actually match the graph.
#[must_use]
pub fn build_system_prompt(
    labels: &[String],
    rel_types: &[String],
    prop_keys: &[String],
) -> String {
    let join = |xs: &[String]| {
        if xs.is_empty() {
            "(none)".to_string()
        } else {
            xs.join(", ")
        }
    };
    format!(
        "You are a Cypher query generator for drevo, a Neo4j-compatible graph database.\n\
         Translate the user's natural-language question into a SINGLE Cypher query.\n\
         Return ONLY the Cypher query text — no explanation, no markdown code fences.\n\
         Use only the labels, relationship types, and property keys listed below. If the \
         question cannot be answered with this schema, return a single line starting with `//`.\n\
         \n\
         Schema:\n\
         - Node labels: {}\n\
         - Relationship types: {}\n\
         - Property keys: {}\n",
        join(labels),
        join(rel_types),
        join(prop_keys),
    )
}

/// Extract the Cypher query from an LLM message: strip a surrounding
/// markdown code fence (```` ```cypher ```` … ```` ``` ````) when present and
/// trim surrounding whitespace, so a model that wraps its answer still yields a
/// runnable query.
#[must_use]
pub fn extract_cypher(content: &str) -> String {
    let trimmed = content.trim();
    let Some(inner) = trimmed.strip_prefix("```") else {
        return trimmed.to_string();
    };
    // Drop the optional language tag on the opening fence's line, then the
    // closing fence.
    let after_lang = inner.split_once('\n').map_or("", |(_, rest)| rest);
    let body = after_lang
        .rsplit_once("```")
        .map_or(after_lang, |(before, _)| before);
    body.trim().to_string()
}

/// A synchronous text-to-Cypher generator — the procedure depends only on this
/// trait, so it is engine- and transport-agnostic and a test can supply a fake.
pub trait CypherGenerator: Send + Sync {
    /// Turn `question` into a Cypher query, given the pre-built `system` prompt
    /// (instructions + schema). Blocks until the upstream responds.
    ///
    /// # Errors
    ///
    /// Returns a [`Text2CypherError`] when the upstream call fails or its
    /// response does not carry usable content.
    fn generate(&self, system: &str, question: &str) -> Result<String, Text2CypherError>;

    /// The configured model id, if known (never a secret). `None` by default.
    fn model(&self) -> Option<String> {
        None
    }

    /// The configured upstream URL, if known (never a secret). `None` by default.
    fn upstream(&self) -> Option<String> {
        None
    }
}

/// The process-global generator, installed once at startup ([`install`]) and
/// read by the `drevo.cypher.fromText` procedure ([`installed`]). `None` until
/// configured, so the procedure reports "not configured" rather than failing
/// opaquely.
static GLOBAL: std::sync::RwLock<Option<Arc<dyn CypherGenerator>>> = std::sync::RwLock::new(None);

/// Install the process-global text-to-Cypher generator (idempotent: replaces
/// any prior one). Called at server startup when an upstream is configured.
pub fn install(generator: Arc<dyn CypherGenerator>) {
    let mut guard = GLOBAL.write().unwrap_or_else(|e| e.into_inner());
    *guard = Some(generator);
}

/// The currently-installed generator, or `None` when text-to-Cypher is not
/// configured.
#[must_use]
pub fn installed() -> Option<Arc<dyn CypherGenerator>> {
    GLOBAL.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Remove the installed generator (test-only reset of the process global).
#[cfg(test)]
pub fn clear() {
    let mut guard = GLOBAL.write().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

/// The `reqwest`-backed async proxy to the configured `chat/completions`
/// upstream.
#[cfg(feature = "embeddings-proxy")]
pub struct ChatProxyBackend {
    client: reqwest::Client,
    config: Text2CypherConfig,
}

#[cfg(feature = "embeddings-proxy")]
impl ChatProxyBackend {
    /// Build a proxy over a fixed config.
    ///
    /// # Errors
    ///
    /// Returns [`Text2CypherError::InvalidUpstream`] when the HTTP client cannot
    /// be constructed.
    pub fn new(config: Text2CypherConfig) -> Result<Self, Text2CypherError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| Text2CypherError::InvalidUpstream(e.to_string()))?;
        Ok(Self { client, config })
    }

    /// Forward the prompt to the upstream and return the extracted Cypher.
    ///
    /// # Errors
    ///
    /// Returns [`Text2CypherError::Upstream`] when the upstream is unreachable,
    /// returns a non-2xx status, or sends a body without a usable
    /// `choices[0].message.content`.
    pub async fn generate(&self, system: &str, question: &str) -> Result<String, Text2CypherError> {
        let body = serde_json::json!({
            "model": self.config.model,
            "temperature": 0,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": question },
            ],
        });
        let mut builder = self.client.post(&self.config.upstream).json(&body);
        if let Some(key) = &self.config.api_key {
            builder = builder.bearer_auth(key);
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| Text2CypherError::Upstream(e.to_string()))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| Text2CypherError::Upstream(e.to_string()))?;
        if !status.is_success() {
            return Err(Text2CypherError::Upstream(format!(
                "upstream returned {status}: {}",
                String::from_utf8_lossy(&bytes).trim()
            )));
        }
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| Text2CypherError::Upstream(format!("malformed upstream response: {e}")))?;
        let content = value
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                Text2CypherError::Upstream(
                    "response missing `choices[0].message.content`".to_string(),
                )
            })?;
        Ok(extract_cypher(content))
    }
}

/// A [`CypherGenerator`] that drives the async [`ChatProxyBackend`] from
/// synchronous code without nesting tokio runtimes — a dedicated OS thread with
/// its own current-thread runtime, mirroring
/// [`crate::embeddings::SyncEmbedder`].
#[cfg(feature = "embeddings-proxy")]
pub struct SyncCypherGenerator {
    sender: std::sync::mpsc::Sender<GenerateJob>,
    _worker: std::thread::JoinHandle<()>,
    model: Option<String>,
    upstream: Option<String>,
}

/// One generation request handed to the [`SyncCypherGenerator`] worker, with a
/// one-shot reply channel.
#[cfg(feature = "embeddings-proxy")]
struct GenerateJob {
    system: String,
    question: String,
    reply: std::sync::mpsc::Sender<Result<String, Text2CypherError>>,
}

#[cfg(feature = "embeddings-proxy")]
impl SyncCypherGenerator {
    /// Spawn the worker thread over a fixed config.
    ///
    /// # Errors
    ///
    /// Returns [`Text2CypherError::InvalidUpstream`] when the HTTP client or the
    /// worker thread / runtime cannot be constructed.
    pub fn from_config(config: Text2CypherConfig) -> Result<Self, Text2CypherError> {
        let model = Some(config.model.clone());
        let upstream = Some(config.upstream.clone());
        let backend = ChatProxyBackend::new(config)?;
        let (sender, receiver) = std::sync::mpsc::channel::<GenerateJob>();
        let worker = std::thread::Builder::new()
            .name("drevo-text2cypher".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(_) => return,
                };
                while let Ok(job) = receiver.recv() {
                    let result = runtime.block_on(backend.generate(&job.system, &job.question));
                    let _ = job.reply.send(result);
                }
            })
            .map_err(|e| Text2CypherError::InvalidUpstream(e.to_string()))?;
        Ok(Self {
            sender,
            _worker: worker,
            model,
            upstream,
        })
    }
}

#[cfg(feature = "embeddings-proxy")]
impl CypherGenerator for SyncCypherGenerator {
    fn model(&self) -> Option<String> {
        self.model.clone()
    }

    fn upstream(&self) -> Option<String> {
        self.upstream.clone()
    }

    fn generate(&self, system: &str, question: &str) -> Result<String, Text2CypherError> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.sender
            .send(GenerateJob {
                system: system.to_string(),
                question: question.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| {
                Text2CypherError::Upstream("text-to-Cypher worker thread stopped".to_string())
            })?;
        reply_rx.recv().map_err(|_| {
            Text2CypherError::Upstream("text-to-Cypher worker dropped the request".to_string())
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k: &str| {
            owned
                .iter()
                .find(|(name, _)| name == k)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn from_env_none_when_upstream_unset() {
        let cfg = Text2CypherConfig::from_env(get(&[])).unwrap();
        assert!(cfg.is_none());
    }

    #[test]
    fn from_env_reads_upstream_key_and_model() {
        let cfg = Text2CypherConfig::from_env(get(&[
            (
                "DREVO_TEXT2CYPHER_UPSTREAM",
                "https://api.example/v1/chat/completions",
            ),
            ("DREVO_TEXT2CYPHER_API_KEY", "sk-123"),
            ("DREVO_TEXT2CYPHER_MODEL", "llama3.1"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(cfg.upstream, "https://api.example/v1/chat/completions");
        assert_eq!(cfg.api_key.as_deref(), Some("sk-123"));
        assert_eq!(cfg.model, "llama3.1");
    }

    #[test]
    fn from_env_defaults_model_when_unset() {
        let cfg = Text2CypherConfig::from_env(get(&[(
            "DREVO_TEXT2CYPHER_UPSTREAM",
            "http://localhost:11434/v1/chat/completions",
        )]))
        .unwrap()
        .unwrap();
        assert_eq!(cfg.model, DEFAULT_MODEL);
        assert!(cfg.api_key.is_none());
    }

    #[test]
    fn from_env_rejects_non_http_scheme() {
        let err = Text2CypherConfig::from_env(get(&[("DREVO_TEXT2CYPHER_UPSTREAM", "ftp://nope")]))
            .unwrap_err();
        assert!(matches!(err, Text2CypherError::InvalidUpstream(_)));
    }

    #[test]
    fn system_prompt_lists_schema() {
        let p = build_system_prompt(
            &["Person".to_string(), "City".to_string()],
            &["LIVES_IN".to_string()],
            &["name".to_string()],
        );
        assert!(p.contains("Person, City"));
        assert!(p.contains("LIVES_IN"));
        assert!(p.contains("name"));
    }

    #[test]
    fn system_prompt_marks_empty_schema() {
        let p = build_system_prompt(&[], &[], &[]);
        assert!(p.contains("Node labels: (none)"));
    }

    #[test]
    fn extract_cypher_strips_fences() {
        assert_eq!(
            extract_cypher("```cypher\nMATCH (n) RETURN n\n```"),
            "MATCH (n) RETURN n"
        );
        assert_eq!(
            extract_cypher("```\nMATCH (n) RETURN n\n```"),
            "MATCH (n) RETURN n"
        );
        assert_eq!(
            extract_cypher("  MATCH (n) RETURN n  "),
            "MATCH (n) RETURN n"
        );
    }

    struct FakeGenerator;
    impl CypherGenerator for FakeGenerator {
        fn generate(&self, _system: &str, question: &str) -> Result<String, Text2CypherError> {
            Ok(format!("// {question}"))
        }
    }

    #[test]
    fn global_install_and_read_back() {
        clear();
        assert!(installed().is_none());
        install(Arc::new(FakeGenerator));
        let g = installed().expect("installed");
        assert_eq!(g.generate("sys", "hello").unwrap(), "// hello");
        clear();
        assert!(installed().is_none());
    }
}
