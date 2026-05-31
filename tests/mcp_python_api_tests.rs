//! Integration tests for the `python_api_*` MCP tools (Phase 16 task
//! `00121`).
//!
//! These spawn the real `target/<profile>/drevo-mcp` binary, complete
//! the MCP handshake, and exercise `python_api_list`,
//! `python_api_describe`, and `python_api_examples` end-to-end through
//! the line-delimited JSON-RPC stdio protocol — the same path a Cline /
//! Claude Code / Claude Desktop client takes. The tools serve a catalog
//! derived (at compile time) from the `drevo-py` type stubs + README, so
//! the assertions below pin the real published Python surface.

#![cfg(feature = "redb-backend")]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use serde_json::{json, Value};
use tempfile::TempDir;

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_drevo-mcp"))
}

/// Spawn the server against a fresh temp data dir and complete the
/// `initialize` + `notifications/initialized` handshake so subsequent
/// `tools/call`s are accepted.
fn spawn_initialized() -> (Child, BufReader<std::process::ChildStdout>, TempDir) {
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
    let mut reader = BufReader::new(stdout);

    let _ = round_trip(
        &mut child,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "python-api-e2e", "version": "0.0.0"}
            }
        }),
    );
    {
        let stdin = child.stdin.as_mut().expect("child stdin");
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        )
        .expect("write notification");
        stdin.flush().expect("flush");
    }
    (child, reader, tmp)
}

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

/// Call a tool and parse the JSON payload out of the `text` content
/// block (the server JSON-serialises tool results into a text block).
fn call_tool(
    child: &mut Child,
    reader: &mut BufReader<std::process::ChildStdout>,
    id: u64,
    name: &str,
    arguments: Value,
) -> Value {
    let resp = round_trip(
        child,
        reader,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }),
    );
    assert_eq!(
        resp["result"]["isError"], false,
        "tool returned an error: {resp}"
    );
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    serde_json::from_str(text).expect("tool payload is JSON")
}

fn shutdown(mut child: Child) -> std::process::ExitStatus {
    drop(child.stdin.take());
    child.wait().expect("child exit")
}

// ── tools/list advertises the three tools ──────────────────────────────

#[test]
fn tools_list_advertises_python_api_tools() {
    let (mut child, mut reader, _tmp) = spawn_initialized();
    let list = round_trip(
        &mut child,
        &mut reader,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();
    for expected in [
        "python_api_list",
        "python_api_describe",
        "python_api_examples",
    ] {
        assert!(names.contains(&expected), "tools/list missing {expected}");
    }
    shutdown(child);
}

// ── python_api_list ────────────────────────────────────────────────────

#[test]
fn list_enumerates_full_surface_and_filters_by_prefix() {
    let (mut child, mut reader, _tmp) = spawn_initialized();

    let all = call_tool(&mut child, &mut reader, 2, "python_api_list", json!({}));
    let count = all["count"].as_u64().expect("count");
    assert!(count >= 30, "expected the full Python surface, got {count}");

    let rag = call_tool(
        &mut child,
        &mut reader,
        3,
        "python_api_list",
        json!({"prefix": "drevo.rag"}),
    );
    let rag_names: Vec<&str> = rag["symbols"]
        .as_array()
        .expect("array")
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(!rag_names.is_empty());
    assert!(rag_names.iter().all(|n| n.starts_with("drevo.rag")));
    assert!(rag_names.contains(&"drevo.rag.Retriever"));

    shutdown(child);
}

// ── python_api_describe ────────────────────────────────────────────────

#[test]
fn describe_returns_signature_and_docstring_for_known_symbol() {
    let (mut child, mut reader, _tmp) = spawn_initialized();

    let described = call_tool(
        &mut child,
        &mut reader,
        2,
        "python_api_describe",
        json!({"name": "Drevo.create_node"}),
    );
    assert_eq!(described["found"], true);
    assert_eq!(described["symbol"]["name"], "drevo.Drevo.create_node");
    assert_eq!(described["symbol"]["kind"], "method");
    assert_eq!(
        described["symbol"]["signature"],
        "def create_node(self, new_node: NewNode) -> Node"
    );

    // A class carries its docstring.
    let drevo = call_tool(
        &mut child,
        &mut reader,
        3,
        "python_api_describe",
        json!({"name": "drevo.Drevo"}),
    );
    assert_eq!(drevo["symbol"]["kind"], "class");
    assert!(drevo["symbol"]["docstring"]
        .as_str()
        .unwrap()
        .contains("Embedded graph database handle"));

    shutdown(child);
}

#[test]
fn describe_miss_returns_found_false_with_suggestions() {
    let (mut child, mut reader, _tmp) = spawn_initialized();
    let miss = call_tool(
        &mut child,
        &mut reader,
        2,
        "python_api_describe",
        json!({"name": "create_nodezzz"}),
    );
    assert_eq!(miss["found"], false);
    assert!(miss["suggestions"].is_array());
    shutdown(child);
}

// ── python_api_examples ────────────────────────────────────────────────

#[test]
fn examples_fuzzy_search_returns_relevant_snippet() {
    let (mut child, mut reader, _tmp) = spawn_initialized();

    let result = call_tool(
        &mut child,
        &mut reader,
        2,
        "python_api_examples",
        json!({"intent": "create a node", "limit": 3}),
    );
    let examples = result["examples"].as_array().expect("examples array");
    assert!(!examples.is_empty(), "intent should match a README example");
    assert!(examples[0]["code"]
        .as_str()
        .unwrap()
        .contains("create_node"));

    // A RAG-flavoured intent surfaces the retriever example.
    let rag = call_tool(
        &mut child,
        &mut reader,
        3,
        "python_api_examples",
        json!({"intent": "retrieve a graph rag context"}),
    );
    let rag_examples = rag["examples"].as_array().expect("array");
    assert!(rag_examples
        .iter()
        .any(|e| e["code"].as_str().unwrap().contains("Retriever")));

    shutdown(child);
}

#[test]
fn examples_missing_intent_is_a_tool_error() {
    let (mut child, mut reader, _tmp) = spawn_initialized();
    // Missing the required `intent` → invalid-params, surfaced as a
    // JSON-RPC error (not a tool-content block).
    let resp = round_trip(
        &mut child,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "python_api_examples", "arguments": {}}
        }),
    );
    assert_eq!(resp["error"]["code"], -32602);
    shutdown(child);
}
