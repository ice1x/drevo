//! Stdio dispatcher — reads one JSON-RPC envelope per line of
//! stdin, dispatches it through the [`ToolRegistry`], writes one
//! JSON-RPC response per line to stdout. EOF on stdin = graceful
//! shutdown.
//!
//! The dispatcher is generic over `BufRead` + `Write` so tests can
//! drive it with `&[u8]` / `Vec<u8>` rather than spawning a real
//! process; only the binary entry-point at `src/bin/drevo-mcp.rs`
//! plugs in actual stdio.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::db::Drevo;

use super::protocol::{
    InitializeParams, InitializeResult, JsonRpcError, JsonRpcId, JsonRpcMessage, JsonRpcRequest,
    JsonRpcResponse, McpServerInfo, ServerCapabilities, ToolCallContent, ToolCallParams,
    ToolCallResult, ToolsCapability, MCP_PROTOCOL_VERSION,
};
use super::tools::ToolRegistry;

/// MCP stdio server.
///
/// Construction takes ownership of the [`Drevo`] handle so the
/// server has exclusive access for its lifetime — embedded mode is
/// single-client by design. The [`Server::run`] method blocks the
/// calling thread until stdin EOF; the binary entry-point runs it
/// on the main thread.
pub struct Server {
    drevo: Drevo,
    registry: ToolRegistry,
    /// Server name advertised in the `initialize` response — useful
    /// to override in tests / a future named-server feature.
    name: String,
    /// Server version mirrored from `CARGO_PKG_VERSION`.
    version: String,
}

impl Server {
    /// Build a server with the [`ToolRegistry::default_tools`]
    /// baseline — what `drevo-mcp` ships out of the box.
    pub fn new(drevo: Drevo) -> Self {
        Self {
            drevo,
            registry: ToolRegistry::default_tools(),
            name: "drevo-mcp".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Build a server with a custom tool registry — used by tests
    /// that swap in single-tool / empty / mocked registries.
    pub fn with_registry(drevo: Drevo, registry: ToolRegistry) -> Self {
        Self {
            drevo,
            registry,
            name: "drevo-mcp".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Override the server name advertised in `initialize`. Useful
    /// when the same binary serves multiple distinct logical
    /// "servers" in the client's MCP debugger.
    pub fn with_name<S: Into<String>>(mut self, name: S) -> Self {
        self.name = name.into();
        self
    }

    /// Override the server version (defaults to `CARGO_PKG_VERSION`).
    pub fn with_version<S: Into<String>>(mut self, version: S) -> Self {
        self.version = version.into();
        self
    }

    /// Run the stdio loop against the given reader/writer. Returns
    /// `Ok(())` on stdin EOF (graceful shutdown). I/O errors on
    /// stdout / stdin propagate as `Err`.
    ///
    /// The loop is line-delimited — one JSON envelope per line —
    /// matching the canonical MCP-over-stdio framing used by Cline,
    /// Claude Code, and Claude Desktop. Each response is followed
    /// by a `\n` and a `writer.flush()` so the client doesn't sit
    /// on a buffered write.
    pub fn run<R: BufRead, W: Write>(
        mut self,
        mut reader: R,
        mut writer: W,
    ) -> std::io::Result<()> {
        let mut line = String::new();
        loop {
            line.clear();
            let bytes = reader.read_line(&mut line)?;
            if bytes == 0 {
                // EOF — graceful shutdown.
                return Ok(());
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            self.dispatch_line(trimmed, &mut writer)?;
        }
    }

    /// Dispatch a single envelope. Public so tests can drive the
    /// dispatcher directly without going through `run`.
    pub fn dispatch_line<W: Write>(&mut self, line: &str, writer: &mut W) -> std::io::Result<()> {
        match JsonRpcMessage::parse(line) {
            Ok(JsonRpcMessage::Request(req)) => {
                let resp = self.handle_request(req);
                let line = serde_json::to_string(&resp)
                    .unwrap_or_else(|e| format!(r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32603,"message":"response serialise failed: {e}"}}}}"#));
                writer.write_all(line.as_bytes())?;
                writer.write_all(b"\n")?;
                writer.flush()?;
            }
            Ok(JsonRpcMessage::Notification(notif)) => {
                self.handle_notification(notif);
                // No response — by JSON-RPC §4.1.
            }
            Err(parse_err) => {
                // Parse / invalid-envelope errors get a `null`-id
                // response per JSON-RPC §5.1 fall-back semantics.
                // The `unwrap_or_else` fallback shape mirrors the
                // request branch above so the client always sees a
                // well-formed envelope even in the (theoretically
                // impossible) case where our own response struct
                // fails to serialise.
                let resp = JsonRpcResponse::error(JsonRpcId::Null, parse_err);
                let line = serde_json::to_string(&resp).unwrap_or_else(|e| {
                    format!(
                        r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32603,"message":"parse-error response serialise failed: {e}"}}}}"#
                    )
                });
                writer.write_all(line.as_bytes())?;
                writer.write_all(b"\n")?;
                writer.flush()?;
            }
        }
        Ok(())
    }

    fn handle_request(&self, req: JsonRpcRequest) -> JsonRpcResponse {
        let id = req.id.clone().unwrap_or(JsonRpcId::Null); // already enforced as Some by `parse`
        let result_or_err = match req.method.as_str() {
            "initialize" => self.handle_initialize(req.params.as_ref()),
            "tools/list" => self.handle_tools_list(),
            "tools/call" => self.handle_tools_call(req.params.as_ref()),
            "ping" => Ok(json!({})),
            other => Err(JsonRpcError::method_not_found(other)),
        };
        match result_or_err {
            Ok(result) => JsonRpcResponse::success(id, result),
            Err(err) => JsonRpcResponse::error(id, err),
        }
    }

    fn handle_notification(&self, _notif: JsonRpcRequest) {
        // The only notification we expect is
        // `notifications/initialized`. We don't currently react to
        // any notification (no telemetry, no state machine) — accept
        // and discard. If we ever care about the lifecycle state
        // ("client said initialized → start sending push tool list
        //  changes"), this is where it goes.
    }

    fn handle_initialize(&self, params: Option<&Value>) -> Result<Value, JsonRpcError> {
        // Parse params for protocolVersion logging — we don't fail
        // even if the client omits them.
        let _: InitializeParams = match params {
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|e| JsonRpcError::invalid_params(format!("initialize: {e}")))?,
            None => InitializeParams {
                protocol_version: MCP_PROTOCOL_VERSION.to_string(),
                capabilities: Value::Null,
                client_info: Value::Null,
            },
        };
        let result = InitializeResult {
            protocol_version: MCP_PROTOCOL_VERSION.to_string(),
            capabilities: ServerCapabilities {
                tools: ToolsCapability {
                    list_changed: false,
                },
            },
            server_info: McpServerInfo {
                name: self.name.clone(),
                version: self.version.clone(),
            },
            instructions: Some(
                "Use the drevo_* tools to read graph state. \
                 Tools are read-only in this build."
                    .to_string(),
            ),
        };
        // `InitializeResult` only contains owned strings + the
        // `Value::Null` capability/clientInfo defaults — so this
        // serialise cannot fail in any non-pathological build. The
        // `map_err` is here to satisfy the audit invariant
        // (`tests/crosscut_audit_tests.rs::no_unwrap_or_expect_in_library_source`),
        // not because there's a real failure mode to handle.
        serde_json::to_value(result)
            .map_err(|e| JsonRpcError::internal_error(format!("InitializeResult serialise: {e}")))
    }

    fn handle_tools_list(&self) -> Result<Value, JsonRpcError> {
        let tools = self.registry.list();
        Ok(json!({ "tools": tools }))
    }

    fn handle_tools_call(&self, params: Option<&Value>) -> Result<Value, JsonRpcError> {
        let params =
            params.ok_or_else(|| JsonRpcError::invalid_params("tools/call requires params"))?;
        let call_params: ToolCallParams = serde_json::from_value(params.clone())
            .map_err(|e| JsonRpcError::invalid_params(format!("tools/call: {e}")))?;
        let tool = self.registry.get(&call_params.name).ok_or_else(|| {
            JsonRpcError::invalid_params(format!("tool not registered: {}", call_params.name))
        })?;
        // Per MCP spec, tool-execution errors are RETURNED as a
        // tools/call result with `isError: true`, not as JSON-RPC
        // protocol errors. Only invalid-params / method-not-found
        // / parse-error flow up through JSON-RPC `error`.
        // The serde_json calls below can fail only on pathological
        // input (a tool returning a Value containing non-finite
        // floats, for example). The `map_err`s satisfy the audit
        // invariant (`no_unwrap_or_expect_in_library_source`) and
        // surface any pathological build as a `-32603 internal`
        // error rather than a panic.
        match tool.call(&self.drevo, &call_params.arguments) {
            Ok(payload) => {
                let text = serde_json::to_string(&payload).map_err(|e| {
                    JsonRpcError::internal_error(format!("tool payload serialise: {e}"))
                })?;
                let result = ToolCallResult {
                    content: vec![ToolCallContent::Text { text }],
                    is_error: false,
                };
                serde_json::to_value(result).map_err(|e| {
                    JsonRpcError::internal_error(format!("ToolCallResult serialise: {e}"))
                })
            }
            Err(err) if err.code == -32602 => {
                // invalid_params → bubble up as JSON-RPC error.
                Err(err)
            }
            Err(err) => {
                // Other tool errors → wrap as isError: true content.
                let text = serde_json::to_string(&json!({
                    "error": err.message,
                    "code": err.code,
                }))
                .map_err(|e| {
                    JsonRpcError::internal_error(format!("error payload serialise: {e}"))
                })?;
                let result = ToolCallResult {
                    content: vec![ToolCallContent::Text { text }],
                    is_error: true,
                };
                serde_json::to_value(result).map_err(|e| {
                    JsonRpcError::internal_error(format!("error ToolCallResult serialise: {e}"))
                })
            }
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
#[cfg(feature = "redb-backend")]
mod tests {
    use super::*;

    fn drive(server: &mut Server, lines: &[&str]) -> Vec<String> {
        let mut out = Vec::new();
        for line in lines {
            server
                .dispatch_line(line, &mut out)
                .expect("dispatch must not fail on I/O");
        }
        String::from_utf8(out)
            .expect("response bytes are utf-8")
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn new_test_server() -> Server {
        Server::new(Drevo::open_in_memory().expect("in-memory")).with_version("test-0.0.0")
    }

    #[test]
    fn initialize_returns_protocol_version_and_tools_capability() {
        let mut s = new_test_server();
        let out = drive(
            &mut s,
            &[
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#,
            ],
        );
        assert_eq!(out.len(), 1, "exactly one response");
        let v: Value = serde_json::from_str(&out[0]).expect("response is JSON");
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(v["result"]["serverInfo"]["name"], "drevo-mcp");
        assert_eq!(v["result"]["capabilities"]["tools"]["listChanged"], false);
    }

    #[test]
    fn tools_list_returns_all_registered_tools() {
        let mut s = new_test_server();
        let out = drive(
            &mut s,
            &[r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#],
        );
        let v: Value = serde_json::from_str(&out[0]).expect("response is JSON");
        let tools = v["result"]["tools"].as_array().expect("tools array");
        // Seven baseline drevo_* tools + three python_api_* tools.
        assert_eq!(tools.len(), 10);
        // Lock the alphabetical order — first tool name should be
        // `drevo_bfs`, last `python_api_list`.
        assert_eq!(tools[0]["name"], "drevo_bfs");
        assert_eq!(tools[tools.len() - 1]["name"], "python_api_list");
    }

    #[test]
    fn tools_call_health_check_returns_healthy() {
        let mut s = new_test_server();
        let out = drive(
            &mut s,
            &[
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"drevo_health_check","arguments":{}}}"#,
            ],
        );
        let v: Value = serde_json::from_str(&out[0]).expect("response is JSON");
        let text = v["result"]["content"][0]["text"]
            .as_str()
            .expect("text content");
        let payload: Value = serde_json::from_str(text).expect("payload is JSON");
        assert_eq!(payload["healthy"], true);
        assert_eq!(v["result"]["isError"], false);
    }

    #[test]
    fn tools_call_unknown_tool_is_invalid_params() {
        let mut s = new_test_server();
        let out = drive(
            &mut s,
            &[
                r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"drevo_does_not_exist","arguments":{}}}"#,
            ],
        );
        let v: Value = serde_json::from_str(&out[0]).expect("response is JSON");
        assert_eq!(v["error"]["code"], -32602);
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("drevo_does_not_exist"));
    }

    #[test]
    fn ping_returns_empty_object() {
        let mut s = new_test_server();
        let out = drive(&mut s, &[r#"{"jsonrpc":"2.0","id":5,"method":"ping"}"#]);
        let v: Value = serde_json::from_str(&out[0]).expect("response is JSON");
        assert_eq!(v["result"], serde_json::json!({}));
    }

    #[test]
    fn method_not_found_returns_minus_32601() {
        let mut s = new_test_server();
        let out = drive(
            &mut s,
            &[r#"{"jsonrpc":"2.0","id":6,"method":"prompts/list"}"#],
        );
        let v: Value = serde_json::from_str(&out[0]).expect("response is JSON");
        assert_eq!(v["error"]["code"], -32601);
    }

    #[test]
    fn parse_error_returns_null_id() {
        let mut s = new_test_server();
        let out = drive(&mut s, &[r#"not valid json"#]);
        let v: Value = serde_json::from_str(&out[0]).expect("response is JSON");
        assert_eq!(v["id"], serde_json::Value::Null);
        assert_eq!(v["error"]["code"], -32700);
    }

    #[test]
    fn notifications_initialized_emits_no_response() {
        let mut s = new_test_server();
        let out = drive(
            &mut s,
            &[r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#],
        );
        assert!(out.is_empty(), "notifications produce no response");
    }

    #[test]
    fn run_terminates_on_stdin_eof() {
        let drevo = Drevo::open_in_memory().expect("open");
        let s = Server::new(drevo).with_version("test-0.0.0");
        let input: &[u8] = b"";
        let mut output = Vec::new();
        s.run(input, &mut output).expect("run");
        assert!(output.is_empty());
    }

    #[test]
    fn run_processes_multiple_lines_in_order() {
        let drevo = Drevo::open_in_memory().expect("open");
        let s = Server::new(drevo).with_version("test-0.0.0");
        let input = b"\
{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}
{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}
";
        let mut output = Vec::new();
        s.run(&input[..], &mut output).expect("run");
        let lines: Vec<&str> = std::str::from_utf8(&output)
            .expect("utf-8")
            .lines()
            .collect();
        assert_eq!(lines.len(), 2);
        let v1: Value = serde_json::from_str(lines[0]).expect("first response");
        let v2: Value = serde_json::from_str(lines[1]).expect("second response");
        assert_eq!(v1["id"], 1);
        assert_eq!(v2["id"], 2);
    }

    #[test]
    fn run_ignores_empty_lines() {
        let drevo = Drevo::open_in_memory().expect("open");
        let s = Server::new(drevo).with_version("test-0.0.0");
        let input = b"\n\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n\n";
        let mut output = Vec::new();
        s.run(&input[..], &mut output).expect("run");
        let lines: Vec<&str> = std::str::from_utf8(&output)
            .expect("utf-8")
            .lines()
            .collect();
        assert_eq!(lines.len(), 1, "blank lines must be ignored");
    }
}
