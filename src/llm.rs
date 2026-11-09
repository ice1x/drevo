//! A client for OpenAI-compatible `chat/completions` endpoints, shared by
//! the procedures that ask a language model: `drevo.cypher.fromText` and
//! `drevo.extract.*`.
//!
//! [`ChatCompleter`](crate::llm::ChatCompleter) is the synchronous interface the Cypher executor calls;
//! it knows nothing about HTTP, so a test can answer with a fake. With the
//! `embeddings-proxy` build feature, `ChatClient` is the `reqwest` client for
//! one endpoint and `SyncChatCompleter` drives it from synchronous code on a
//! thread of its own.
//!
//! ```
//! use drevo::llm::{ChatCompleter, ChatMessage, ChatRequest, LlmError};
//!
//! struct Echo;
//! impl ChatCompleter for Echo {
//!     fn complete(&self, request: &ChatRequest) -> Result<String, LlmError> {
//!         Ok(request.messages.last().map(|m| m.content.clone()).unwrap_or_default())
//!     }
//! }
//!
//! let request = ChatRequest::new(vec![
//!     ChatMessage::system("Answer briefly."),
//!     ChatMessage::user("ping"),
//! ]);
//! assert_eq!(Echo.complete(&request).unwrap(), "ping");
//! ```

/// Why a chat completion failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LlmError {
    /// The endpoint or client cannot be set up (bad URL, unbuildable client).
    #[error("{0}")]
    InvalidUpstream(String),
    /// The call failed or the answer had no usable content.
    #[error("{0}")]
    Upstream(String),
}

/// One message of a chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    /// `system`, `user` or `assistant`.
    pub role: String,
    /// What the message says.
    pub content: String,
}

impl ChatMessage {
    /// A `system` message.
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }

    /// A `user` message.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }

    /// An `assistant` message (a previous answer, when asking again).
    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
        }
    }
}

/// A chat to complete, optionally with a model other than the endpoint's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRequest {
    /// The model to ask; `None` uses the endpoint's configured one.
    pub model: Option<String>,
    /// The messages, oldest first.
    pub messages: Vec<ChatMessage>,
}

impl ChatRequest {
    /// A request for the endpoint's own model.
    #[must_use]
    pub fn new(messages: Vec<ChatMessage>) -> Self {
        Self {
            model: None,
            messages,
        }
    }
}

/// Something that completes a chat: the assistant's answer to `request`.
pub trait ChatCompleter: Send + Sync {
    /// The assistant's reply text. Blocks until the endpoint answers.
    ///
    /// # Errors
    ///
    /// [`LlmError::Upstream`] when the call fails or the reply has no text.
    fn complete(&self, request: &ChatRequest) -> Result<String, LlmError>;

    /// The model used when a request names none, if known.
    fn model(&self) -> Option<String> {
        None
    }

    /// The endpoint URL, if known (never a secret).
    fn upstream(&self) -> Option<String> {
        None
    }
}

/// Where and how to reach a `chat/completions` endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatEndpoint {
    /// The full `chat/completions` URL.
    pub upstream: String,
    /// The bearer token, if the endpoint needs one.
    pub api_key: Option<String>,
    /// The model asked when a request names none.
    pub model: String,
    /// How long one call may take; `None` waits as long as it takes.
    pub timeout: Option<std::time::Duration>,
}

/// The JSON body of a `chat/completions` call for `request`, asking `model`
/// when the request names none, at temperature 0.
#[must_use]
pub fn request_body(default_model: &str, request: &ChatRequest) -> serde_json::Value {
    let messages: Vec<serde_json::Value> = request
        .messages
        .iter()
        .map(|m| serde_json::json!({ "role": m.role, "content": m.content }))
        .collect();
    serde_json::json!({
        "model": request.model.as_deref().unwrap_or(default_model),
        "temperature": 0,
        "messages": messages,
    })
}

/// The reply text of a `chat/completions` answer:
/// `choices[0].message.content`.
///
/// # Errors
///
/// [`LlmError::Upstream`] when `body` is not JSON or has no such text.
pub fn reply_content(body: &[u8]) -> Result<String, LlmError> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| LlmError::Upstream(format!("malformed upstream response: {e}")))?;
    value
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            LlmError::Upstream("response missing `choices[0].message.content`".to_string())
        })
}

/// The `reqwest` client for one `chat/completions` endpoint.
#[cfg(feature = "embeddings-proxy")]
pub struct ChatClient {
    client: reqwest::Client,
    endpoint: ChatEndpoint,
}

#[cfg(feature = "embeddings-proxy")]
impl ChatClient {
    /// A client for `endpoint`.
    ///
    /// # Errors
    ///
    /// [`LlmError::InvalidUpstream`] when the HTTP client cannot be built.
    pub fn new(endpoint: ChatEndpoint) -> Result<Self, LlmError> {
        let mut builder = reqwest::Client::builder();
        if let Some(timeout) = endpoint.timeout {
            builder = builder.timeout(timeout);
        }
        let client = builder
            .build()
            .map_err(|e| LlmError::InvalidUpstream(e.to_string()))?;
        Ok(Self { client, endpoint })
    }

    /// The endpoint this client calls.
    #[must_use]
    pub fn endpoint(&self) -> &ChatEndpoint {
        &self.endpoint
    }

    /// Send `request` and return the assistant's reply text.
    ///
    /// # Errors
    ///
    /// [`LlmError::Upstream`] when the endpoint is unreachable, answers with
    /// a non-2xx status, or sends a body without
    /// `choices[0].message.content`.
    pub async fn complete(&self, request: &ChatRequest) -> Result<String, LlmError> {
        let body = request_body(&self.endpoint.model, request);
        let mut builder = self.client.post(&self.endpoint.upstream).json(&body);
        if let Some(key) = &self.endpoint.api_key {
            builder = builder.bearer_auth(key);
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| LlmError::Upstream(e.to_string()))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| LlmError::Upstream(e.to_string()))?;
        if !status.is_success() {
            return Err(LlmError::Upstream(format!(
                "upstream returned {status}: {}",
                String::from_utf8_lossy(&bytes).trim()
            )));
        }
        reply_content(&bytes)
    }
}

/// A [`ChatCompleter`] that drives a [`ChatClient`] from synchronous code
/// on a thread of its own with its own runtime, so it can be called from
/// inside another tokio runtime.
#[cfg(feature = "embeddings-proxy")]
pub struct SyncChatCompleter {
    sender: std::sync::mpsc::Sender<CompleteJob>,
    _worker: std::thread::JoinHandle<()>,
    label: &'static str,
    model: String,
    upstream: String,
}

/// One request handed to the [`SyncChatCompleter`] worker.
#[cfg(feature = "embeddings-proxy")]
struct CompleteJob {
    request: ChatRequest,
    reply: std::sync::mpsc::Sender<Result<String, LlmError>>,
}

#[cfg(feature = "embeddings-proxy")]
impl SyncChatCompleter {
    /// Start the worker thread `thread_name` for `endpoint`; `label` names
    /// the feature in the worker's own errors ("text-to-Cypher worker
    /// thread stopped").
    ///
    /// # Errors
    ///
    /// [`LlmError::InvalidUpstream`] when the client or the thread cannot
    /// be built.
    pub fn spawn(
        endpoint: ChatEndpoint,
        label: &'static str,
        thread_name: &str,
    ) -> Result<Self, LlmError> {
        let model = endpoint.model.clone();
        let upstream = endpoint.upstream.clone();
        let client = ChatClient::new(endpoint)?;
        let (sender, receiver) = std::sync::mpsc::channel::<CompleteJob>();
        let worker = std::thread::Builder::new()
            .name(thread_name.to_string())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                while let Ok(job) = receiver.recv() {
                    let result = runtime.block_on(client.complete(&job.request));
                    let _ = job.reply.send(result);
                }
            })
            .map_err(|e| LlmError::InvalidUpstream(e.to_string()))?;
        Ok(Self {
            sender,
            _worker: worker,
            label,
            model,
            upstream,
        })
    }
}

#[cfg(feature = "embeddings-proxy")]
impl ChatCompleter for SyncChatCompleter {
    fn complete(&self, request: &ChatRequest) -> Result<String, LlmError> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.sender
            .send(CompleteJob {
                request: request.clone(),
                reply: reply_tx,
            })
            .map_err(|_| LlmError::Upstream(format!("{} worker thread stopped", self.label)))?;
        reply_rx
            .recv()
            .map_err(|_| LlmError::Upstream(format!("{} worker dropped the request", self.label)))?
    }

    fn model(&self) -> Option<String> {
        Some(self.model.clone())
    }

    fn upstream(&self) -> Option<String> {
        Some(self.upstream.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_carries_the_model_and_the_messages_in_order() {
        let request = ChatRequest::new(vec![ChatMessage::system("s"), ChatMessage::user("u")]);
        let body = request_body("gpt-4o-mini", &request);
        assert_eq!(
            body,
            serde_json::json!({
                "model": "gpt-4o-mini",
                "temperature": 0,
                "messages": [
                    {"role": "system", "content": "s"},
                    {"role": "user", "content": "u"},
                ],
            })
        );
    }

    #[test]
    fn a_request_can_name_its_own_model() {
        let mut request = ChatRequest::new(vec![ChatMessage::user("u")]);
        request.model = Some("llama3.1".into());
        assert_eq!(request_body("gpt-4o-mini", &request)["model"], "llama3.1");
    }

    #[test]
    fn the_reply_is_the_first_choice() {
        let body = br#"{"choices": [{"message": {"role": "assistant", "content": "hi"}}]}"#;
        assert_eq!(reply_content(body).unwrap(), "hi");
    }

    #[test]
    fn a_reply_without_content_is_an_upstream_error() {
        let err = reply_content(br#"{"choices": []}"#).unwrap_err();
        assert_eq!(
            err,
            LlmError::Upstream("response missing `choices[0].message.content`".into())
        );
        let err = reply_content(b"<html>").unwrap_err();
        assert!(
            err.to_string().starts_with("malformed upstream response: "),
            "{err}"
        );
    }

    #[test]
    fn messages_have_their_roles() {
        assert_eq!(ChatMessage::system("a").role, "system");
        assert_eq!(ChatMessage::user("a").role, "user");
        assert_eq!(ChatMessage::assistant("a").role, "assistant");
    }
}
