//! JSON-RPC 2.0 + MCP message types for the stdio server.
//!
//! Two layers are modelled separately:
//!
//! 1. The **JSON-RPC 2.0 envelope** ([`JsonRpcRequest`],
//!    [`JsonRpcResponse`], [`JsonRpcError`], [`JsonRpcMessage`]) — the
//!    transport. Method name, params, ID. The id may be missing
//!    (notification), a number, or a string per JSON-RPC §4.
//! 2. The **MCP-specific bodies** ([`InitializeParams`],
//!    [`InitializeResult`], [`Tool`], [`ToolCallParams`],
//!    [`ToolCallResult`], …) — the wire shapes a conforming MCP
//!    server / client exchange inside the JSON-RPC envelope.
//!
//! Every type round-trips through `serde_json` and the inline tests
//! at the bottom lock the exact JSON shape against the MCP 2024-11-05
//! reference specification.
//!
//! ### Why one file
//!
//! These types form a single layer; splitting them would force
//! cyclic `use` relationships (MCP bodies embed JSON-RPC ids in their
//! errors). Keeping the file under ~600 LOC with embedded tests is
//! the same pattern `src/error.rs` and `src/storage/error.rs` follow.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// MCP protocol version this server advertises in the `initialize`
/// handshake. The 2024-11-05 spec is the widely-supported stable
/// snapshot at time of writing. Clients on older protocol versions
/// still receive a structured `initialize` response — the spec
/// instructs them to negotiate down — but the tool surface we expose
/// requires at least the 2024-11-05 `tools/*` methods.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

// ── JSON-RPC 2.0 envelope ──────────────────────────────────────────────

/// JSON-RPC 2.0 request id. Per the spec the id may be missing
/// (notification), a number, or a string — and an explicit `null`
/// is also legal (it implies a notification under JSON-RPC §4.1,
/// but in MCP practice we treat it as the wildcard error-response
/// id when the request id could not be parsed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    /// Numeric id, the most common form used by Claude clients.
    Number(i64),
    /// String id, sometimes used by clients that wrap numeric ids
    /// for tracing.
    String(String),
    /// Explicit `null` id — emitted in error responses when the
    /// request itself could not be parsed (JSON-RPC §5.1 fall-back).
    Null,
}

/// A JSON-RPC 2.0 request envelope. The `id` is `None` for
/// notifications (no response expected), `Some(...)` otherwise.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// MUST equal `"2.0"`.
    pub jsonrpc: String,
    /// Absent for notifications, present for normal requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<JsonRpcId>,
    /// Method name — e.g. `"initialize"`, `"tools/list"`,
    /// `"tools/call"`.
    pub method: String,
    /// Method-specific parameters. Absent for methods that take no
    /// params.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A JSON-RPC 2.0 response envelope. Exactly one of `result` or
/// `error` is `Some` per the spec; we enforce that at construction
/// via the [`JsonRpcResponse::success`] / [`JsonRpcResponse::error`]
/// constructors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// MUST equal `"2.0"`.
    pub jsonrpc: String,
    /// Matches the request id. For parse-error fall-backs the id is
    /// [`JsonRpcId::Null`].
    pub id: JsonRpcId,
    /// Set when the call succeeded. Mutually exclusive with `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Set when the call failed. Mutually exclusive with `result`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    /// Build a successful response with the given id and result body.
    pub fn success(id: JsonRpcId, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Build an error response with the given id and error envelope.
    pub fn error(id: JsonRpcId, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// JSON-RPC 2.0 error envelope. Reserved error codes per §5.1:
///
/// | Code      | Meaning                                |
/// |-----------|----------------------------------------|
/// | -32700    | Parse error                            |
/// | -32600    | Invalid request                        |
/// | -32601    | Method not found                       |
/// | -32602    | Invalid params                         |
/// | -32603    | Internal error                         |
/// | -32000…-32099 | Server-defined (we use -32000 for tool execution failures) |
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Numeric code per the table above.
    pub code: i32,
    /// Short human-readable message.
    pub message: String,
    /// Optional structured details — typically a JSON object with
    /// the failing tool name, the underlying error chain, etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    /// `-32700` parse error — the line was not valid JSON.
    pub fn parse_error<S: Into<String>>(message: S) -> Self {
        Self {
            code: -32700,
            message: message.into(),
            data: None,
        }
    }

    /// `-32600` invalid request — JSON was parseable but didn't match
    /// the JSON-RPC envelope shape.
    pub fn invalid_request<S: Into<String>>(message: S) -> Self {
        Self {
            code: -32600,
            message: message.into(),
            data: None,
        }
    }

    /// `-32601` method not found — the method name is not recognised
    /// by this server (e.g. `prompts/list` on a server that doesn't
    /// declare the `prompts` capability).
    pub fn method_not_found<S: Into<String>>(method: S) -> Self {
        Self {
            code: -32601,
            message: format!("method not found: {}", method.into()),
            data: None,
        }
    }

    /// `-32602` invalid params — params were of the wrong shape for
    /// the requested method.
    pub fn invalid_params<S: Into<String>>(message: S) -> Self {
        Self {
            code: -32602,
            message: message.into(),
            data: None,
        }
    }

    /// `-32603` internal error — the server reached a state it
    /// cannot recover from (e.g. lock poisoning).
    pub fn internal_error<S: Into<String>>(message: S) -> Self {
        Self {
            code: -32603,
            message: message.into(),
            data: None,
        }
    }

    /// `-32000` tool execution error — server-defined band per
    /// JSON-RPC §5.1. Used when a Drevo call itself fails (node not
    /// found, FTS query rejected, …) rather than the dispatch layer.
    pub fn tool_error<S: Into<String>>(message: S, data: Option<Value>) -> Self {
        Self {
            code: -32000,
            message: message.into(),
            data,
        }
    }
}

/// Either-or envelope for messages read off stdin. The dispatch loop
/// distinguishes notifications (no `id`, no response) from requests
/// (with `id`, response required) by branching on this enum.
#[derive(Debug, Clone)]
pub enum JsonRpcMessage {
    /// A normal request — response required.
    Request(JsonRpcRequest),
    /// A notification — no response sent.
    Notification(JsonRpcRequest),
}

impl JsonRpcMessage {
    /// Parse a single JSON-RPC envelope from a JSON string. Returns
    /// a [`JsonRpcError`] if the JSON is invalid OR the envelope
    /// shape doesn't conform to JSON-RPC 2.0.
    pub fn parse(line: &str) -> Result<Self, JsonRpcError> {
        let req: JsonRpcRequest = serde_json::from_str(line)
            .map_err(|e| JsonRpcError::parse_error(format!("JSON parse error: {e}")))?;
        if req.jsonrpc != "2.0" {
            return Err(JsonRpcError::invalid_request(format!(
                "jsonrpc field must be \"2.0\", got {:?}",
                req.jsonrpc
            )));
        }
        Ok(if req.id.is_some() {
            JsonRpcMessage::Request(req)
        } else {
            JsonRpcMessage::Notification(req)
        })
    }
}

// ── MCP-specific message bodies ────────────────────────────────────────

/// Client `initialize` params. We only inspect `protocolVersion` and
/// pass the rest through unchanged (`clientInfo`, capabilities are
/// recorded for logging but don't affect dispatch yet).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
    /// Protocol version the client speaks (e.g. `"2024-11-05"`).
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    /// Client-declared capabilities. We don't consume them but
    /// preserve the field so a round-tripped envelope stays exact.
    #[serde(default)]
    pub capabilities: Value,
    /// Optional client `{name, version}` identifier — surfaced in
    /// server logs.
    #[serde(rename = "clientInfo", default)]
    pub client_info: Value,
}

/// Server `initialize` response body. Returned as the `result` of
/// the JSON-RPC envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    /// Protocol version the server agrees to use — usually echoed
    /// from the client's request unless we need to negotiate down.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    /// Server capabilities. We always advertise `tools`; everything
    /// else is omitted so the client knows not to ask.
    pub capabilities: ServerCapabilities,
    /// Server `{name, version}` — shown in the client's MCP debugger.
    #[serde(rename = "serverInfo")]
    pub server_info: McpServerInfo,
    /// Optional human-readable instructions for the user / client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// Server capability flags returned in the `initialize` response.
/// Per the MCP spec, presence of a capability sub-object signals
/// support; absence signals lack of support.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerCapabilities {
    /// We declare the `tools` capability — clients may call
    /// `tools/list` and `tools/call`.
    pub tools: ToolsCapability,
}

/// Sub-capability flags for the `tools` family. `list_changed`
/// would let us push `notifications/tools/list_changed` when the
/// available tools change at runtime; we don't expose dynamic
/// registration today so it's `false`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsCapability {
    /// `true` if the server emits `notifications/tools/list_changed`
    /// — we don't (yet).
    #[serde(rename = "listChanged")]
    pub list_changed: bool,
}

/// Server identifier returned in the `initialize` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerInfo {
    /// Human-readable server name (`"drevo-mcp"`).
    pub name: String,
    /// Server version, mirrors the crate version.
    pub version: String,
}

/// MCP `Tool` descriptor returned by `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    /// Tool name — invoked by the client via `tools/call` with
    /// `name == "<this>"`. Kebab-case + namespaced (`drevo_*`) so
    /// they don't collide with tools from other MCP servers an AI
    /// client may have loaded simultaneously.
    pub name: String,
    /// Short human description — surfaced in the client UI.
    pub description: String,
    /// JSON Schema for the `arguments` map sent in `tools/call`.
    /// Embedded as `serde_json::Value` so we don't pull in a
    /// schema-validation crate; schema correctness is locked by
    /// inline tests below.
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// Params for `tools/call`. The `arguments` map matches the tool's
/// declared `inputSchema`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallParams {
    /// Which tool to call — matches a [`Tool::name`] previously
    /// returned by `tools/list`.
    pub name: String,
    /// Per-tool argument map. Absent / null when the tool takes no
    /// arguments.
    #[serde(default)]
    pub arguments: Value,
}

/// Body returned by `tools/call`. Per the MCP spec, the result is a
/// list of [`ToolCallContent`] blocks; clients render them in order.
/// We always return exactly one block — JSON-encoded for the calling
/// LLM to parse — but the spec allows for richer multi-block output
/// (mixed text + image + resource) in future extensions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResult {
    /// Ordered content blocks. Today always a single
    /// `ToolCallContent::Text` with serialised JSON inside.
    pub content: Vec<ToolCallContent>,
    /// `true` when the tool executed but reported a domain-level
    /// failure (e.g. node not found). Conventionally clients
    /// surface this distinctly from JSON-RPC protocol-level errors.
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

/// A single content block inside a [`ToolCallResult`]. Tagged via
/// the `type` discriminator per MCP spec §"Tools Content".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolCallContent {
    /// Plain-text block — what we always emit. The `text` payload is
    /// itself a JSON-encoded string (the tool's structured result)
    /// because most MCP clients today don't yet support structured
    /// `tools/call` returns natively.
    Text {
        /// The text payload.
        text: String,
    },
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonrpc_request_round_trips() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let parsed: JsonRpcRequest = serde_json::from_str(raw).expect("parse");
        assert_eq!(parsed.jsonrpc, "2.0");
        assert_eq!(parsed.id, Some(JsonRpcId::Number(1)));
        assert_eq!(parsed.method, "tools/list");
        assert!(parsed.params.is_none());
    }

    #[test]
    fn jsonrpc_notification_has_no_id() {
        let raw = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let msg = JsonRpcMessage::parse(raw).expect("parse");
        assert!(matches!(msg, JsonRpcMessage::Notification(_)));
    }

    #[test]
    fn jsonrpc_request_with_string_id() {
        let raw = r#"{"jsonrpc":"2.0","id":"abc-123","method":"ping"}"#;
        let msg = JsonRpcMessage::parse(raw).expect("parse");
        let req = match msg {
            JsonRpcMessage::Request(r) => r,
            _ => panic!("expected request"),
        };
        assert_eq!(req.id, Some(JsonRpcId::String("abc-123".to_string())));
    }

    #[test]
    fn jsonrpc_parse_rejects_wrong_version() {
        let raw = r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#;
        let err = JsonRpcMessage::parse(raw).expect_err("must reject");
        assert_eq!(err.code, -32600);
    }

    #[test]
    fn jsonrpc_parse_rejects_invalid_json() {
        let raw = r#"{not valid json"#;
        let err = JsonRpcMessage::parse(raw).expect_err("must reject");
        assert_eq!(err.code, -32700);
    }

    #[test]
    fn jsonrpc_response_success_omits_error_field() {
        let resp = JsonRpcResponse::success(JsonRpcId::Number(7), serde_json::json!({"ok": true}));
        let s = serde_json::to_string(&resp).expect("serialize");
        assert!(s.contains("\"result\""));
        assert!(!s.contains("\"error\""));
    }

    #[test]
    fn jsonrpc_response_error_omits_result_field() {
        let resp = JsonRpcResponse::error(
            JsonRpcId::Number(7),
            JsonRpcError::method_not_found("foo/bar"),
        );
        let s = serde_json::to_string(&resp).expect("serialize");
        assert!(s.contains("\"error\""));
        assert!(!s.contains("\"result\""));
        assert!(s.contains("method not found: foo/bar"));
    }

    #[test]
    fn initialize_result_serialises_per_mcp_spec() {
        let result = InitializeResult {
            protocol_version: MCP_PROTOCOL_VERSION.to_string(),
            capabilities: ServerCapabilities {
                tools: ToolsCapability {
                    list_changed: false,
                },
            },
            server_info: McpServerInfo {
                name: "drevo-mcp".to_string(),
                version: "0.1.0".to_string(),
            },
            instructions: Some("Use the drevo_* tools to read graph state.".to_string()),
        };
        let v = serde_json::to_value(&result).expect("serialise");
        // MCP spec field names — locked here so a refactor that
        // changes serde rename can't silently break clients.
        assert_eq!(v["protocolVersion"], "2024-11-05");
        assert_eq!(v["capabilities"]["tools"]["listChanged"], false);
        assert_eq!(v["serverInfo"]["name"], "drevo-mcp");
        assert_eq!(v["serverInfo"]["version"], "0.1.0");
        assert_eq!(
            v["instructions"],
            "Use the drevo_* tools to read graph state."
        );
    }

    #[test]
    fn tool_serialises_with_input_schema_field() {
        let tool = Tool {
            name: "drevo_health_check".to_string(),
            description: "Probe the underlying Drevo handle.".to_string(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        };
        let v = serde_json::to_value(&tool).expect("serialise");
        assert_eq!(v["name"], "drevo_health_check");
        assert_eq!(v["inputSchema"]["type"], "object");
        // MCP spec uses `inputSchema`, not `input_schema` — lock the
        // case so a serde-rename-attribute drift breaks here.
        assert!(v.get("input_schema").is_none());
    }

    #[test]
    fn tool_call_result_round_trips() {
        let result = ToolCallResult {
            content: vec![ToolCallContent::Text {
                text: "{\"count\":42}".to_string(),
            }],
            is_error: false,
        };
        let s = serde_json::to_string(&result).expect("serialise");
        // Lock the wire shape: content[].type == "text"; isError
        // (camelCase, not snake_case).
        assert!(s.contains("\"type\":\"text\""));
        assert!(s.contains("\"isError\":false"));
        // Round-trip.
        let parsed: ToolCallResult = serde_json::from_str(&s).expect("re-parse");
        assert!(!parsed.is_error);
        assert_eq!(parsed.content.len(), 1);
    }

    #[test]
    fn tool_call_params_arguments_default_to_null() {
        let raw = r#"{"name":"drevo_health_check"}"#;
        let parsed: ToolCallParams = serde_json::from_str(raw).expect("parse");
        assert_eq!(parsed.name, "drevo_health_check");
        assert!(parsed.arguments.is_null());
    }

    #[test]
    fn initialize_params_round_trips_with_capabilities() {
        let raw = r#"{
            "protocolVersion": "2024-11-05",
            "capabilities": {"roots": {"listChanged": false}},
            "clientInfo": {"name": "claude-code", "version": "1.0.0"}
        }"#;
        let parsed: InitializeParams = serde_json::from_str(raw).expect("parse");
        assert_eq!(parsed.protocol_version, "2024-11-05");
        assert_eq!(parsed.capabilities["roots"]["listChanged"], false);
        assert_eq!(parsed.client_info["name"], "claude-code");
    }
}
