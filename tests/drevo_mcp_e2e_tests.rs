//! End-to-end tests for the Phase 15 task `00090` `drevo-mcp` binary.
//!
//! These tests spawn the real `target/debug/drevo-mcp` process,
//! send line-delimited JSON-RPC envelopes via stdin, read line-
//! delimited responses from stdout, and assert on the parsed JSON
//! shape. They cover the full MCP handshake (`initialize`,
//! `notifications/initialized`, `tools/list`, `tools/call`,
//! `ping`) against a temp-dir-backed Drevo handle.
//!
//! The MCP validation E2E suite proper (Phase 15 task `00091`)
//! will extend this file with the count/labels/rels/traversal/
//! properties coverage; for `00090` we just need to prove the
//! protocol pipe works.

#![cfg(feature = "redb-backend")]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use serde_json::{json, Value};
use tempfile::TempDir;

/// Locate the freshly-built `drevo-mcp` binary. Cargo puts it under
/// `target/<profile>/drevo-mcp` relative to the workspace root.
fn binary_path() -> PathBuf {
    // CARGO_BIN_EXE_<bin-name> is set by Cargo when running tests
    // declared under a `[[bin]]`. It works on all OSes and removes
    // the need to guess the profile dir.
    PathBuf::from(env!("CARGO_BIN_EXE_drevo-mcp"))
}

/// Spawn the MCP server against a temp data dir, returning the
/// `Child` handle, a buffered stdout reader, and the temp dir guard.
/// Dropping the `TempDir` cleans the on-disk redb file.
fn spawn() -> (Child, BufReader<std::process::ChildStdout>, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let data_dir = tmp.path().join("drevo-data");
    let mut child = Command::new(binary_path())
        .arg("--data-dir")
        .arg(&data_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn drevo-mcp");
    let stdout = child.stdout.take().expect("child stdout");
    (child, BufReader::new(stdout), tmp)
}

/// Send one JSON-RPC line and read one response line. Panics on
/// I/O failure (broken pipe = test failure).
fn round_trip(
    child: &mut Child,
    reader: &mut BufReader<std::process::ChildStdout>,
    request: &Value,
) -> Value {
    let stdin = child.stdin.as_mut().expect("child stdin");
    let line = serde_json::to_string(request).expect("serialise request");
    writeln!(stdin, "{line}").expect("write to child stdin");
    stdin.flush().expect("flush child stdin");
    let mut response = String::new();
    reader.read_line(&mut response).expect("read child stdout");
    serde_json::from_str(response.trim()).expect("response is JSON")
}

/// Send a notification (no response expected). The caller is
/// responsible for not subsequently calling `read_line` against
/// the notification's slot.
fn send_notification(child: &mut Child, notification: &Value) {
    let stdin = child.stdin.as_mut().expect("child stdin");
    let line = serde_json::to_string(notification).expect("serialise notification");
    writeln!(stdin, "{line}").expect("write notification");
    stdin.flush().expect("flush");
}

/// Cleanly shut the child down by dropping its stdin (sends EOF).
/// Waits for the process to exit and returns its status.
fn shutdown(mut child: Child) -> std::process::ExitStatus {
    drop(child.stdin.take()); // EOF on stdin → server returns Ok(())
    child.wait().expect("child exit")
}

// ── Tests ──────────────────────────────────────────────────────────────

#[test]
fn full_handshake_succeeds_against_real_binary() {
    let (mut child, mut reader, _tmp) = spawn();

    // 1. initialize
    let init = round_trip(
        &mut child,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "drevo-mcp-e2e", "version": "0.0.0"}
            }
        }),
    );
    assert_eq!(init["jsonrpc"], "2.0");
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(init["result"]["serverInfo"]["name"], "drevo-mcp");

    // 2. notifications/initialized — no response
    send_notification(
        &mut child,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );

    // 3. tools/list
    let list = round_trip(
        &mut child,
        &mut reader,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    let tools = list["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 7, "baseline tool count must be 7");
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();
    assert!(names.contains(&"drevo_health_check"));
    assert!(names.contains(&"drevo_search_fts"));

    // 4. tools/call drevo_health_check
    let call = round_trip(
        &mut child,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "drevo_health_check", "arguments": {}}
        }),
    );
    assert_eq!(call["result"]["isError"], false);
    let text = call["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    let payload: Value = serde_json::from_str(text).expect("payload is JSON");
    assert_eq!(payload["healthy"], true);

    // 5. ping
    let ping = round_trip(
        &mut child,
        &mut reader,
        &json!({"jsonrpc": "2.0", "id": 4, "method": "ping"}),
    );
    assert_eq!(ping["result"], json!({}));

    let status = shutdown(child);
    assert!(status.success(), "binary exited with {status}");
}

#[test]
fn count_nodes_returns_zero_against_fresh_data_dir() {
    let (mut child, mut reader, _tmp) = spawn();

    // Have to initialize first to be a well-behaved MCP client.
    let _init = round_trip(
        &mut child,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "e2e"}}
        }),
    );

    let count = round_trip(
        &mut child,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "drevo_count_nodes", "arguments": {}}
        }),
    );
    let text = count["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    let payload: Value = serde_json::from_str(text).expect("payload is JSON");
    assert_eq!(payload["count"], 0);

    shutdown(child);
}

#[test]
fn unknown_method_returns_minus_32601() {
    let (mut child, mut reader, _tmp) = spawn();
    let resp = round_trip(
        &mut child,
        &mut reader,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "prompts/list"}),
    );
    assert_eq!(resp["error"]["code"], -32601);
    shutdown(child);
}

#[test]
fn parse_error_emits_null_id_response() {
    let (mut child, mut reader, _tmp) = spawn();
    // Send raw invalid JSON.
    let stdin = child.stdin.as_mut().expect("child stdin");
    writeln!(stdin, "not valid json").expect("write");
    stdin.flush().expect("flush");
    let mut line = String::new();
    reader.read_line(&mut line).expect("read");
    let v: Value = serde_json::from_str(line.trim()).expect("response is JSON");
    assert_eq!(v["id"], Value::Null);
    assert_eq!(v["error"]["code"], -32700);
    shutdown(child);
}

#[test]
fn binary_help_flag_exits_successfully() {
    let output = Command::new(binary_path())
        .arg("--help")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("run --help");
    assert!(output.status.success(), "--help must exit 0");
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    assert!(stdout.contains("drevo-mcp"));
    assert!(stdout.contains("--data-dir"));
}

#[test]
fn binary_version_flag_prints_crate_version() {
    let output = Command::new(binary_path())
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("run --version");
    assert!(output.status.success(), "--version must exit 0");
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    assert!(stdout.starts_with("drevo-mcp "));
}

#[test]
fn unknown_flag_exits_with_error_code() {
    let output = Command::new(binary_path())
        .arg("--no-such-flag")
        .stderr(Stdio::piped())
        .output()
        .expect("run unknown flag");
    assert!(!output.status.success(), "unknown flag must exit non-zero");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown argument"),
        "stderr must explain the failure, got: {stderr:?}"
    );
}

#[test]
fn graceful_shutdown_on_stdin_eof() {
    let (child, _reader, _tmp) = spawn();
    let status = shutdown(child);
    assert!(status.success());
}
