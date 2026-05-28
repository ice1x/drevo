//! Phase 15 task `00091` — MCP validation E2E suite.
//!
//! Exercises the seven baseline tools shipped in `00090`
//! (`drevo_health_check`, `drevo_count_nodes`, `drevo_node_get`,
//! `drevo_node_get_by_uuid`, `drevo_search_fts`, `drevo_bfs`,
//! `drevo_list_nodes_by_kind`) against a **populated** Drevo
//! corpus reached through the real `drevo-mcp` binary over stdio.
//!
//! README task wording: "MCP validation E2E suite — count, labels,
//! rels, traversal, properties". This file is the implementation of
//! that scope.
//!
//! ## How the fixture works
//!
//! `populate_then_spawn()` creates a tempdir, opens a `Drevo`
//! handle directly, inserts a small but heterogeneous corpus
//! (4 nodes × 3 kinds + 3 edges + properties on every node),
//! **closes** the handle (redb takes an exclusive file lock — the
//! child process can't open the same DB until we release it), then
//! spawns `target/debug/drevo-mcp` against the same directory. The
//! caller drives the MCP wire from there.
//!
//! Returned [`Corpus`] holds the numeric ids + hyphenated UUIDs the
//! test cases assert against; without it tests would have to fish
//! ids out of `list_nodes_by_kind` responses every time.
//!
//! ## Scope vs `00090`
//!
//! `tests/drevo_mcp_e2e_tests.rs` (shipped with `00090`) covers the
//! **protocol** surface — handshake, error envelope, --help/--version
//! exit codes. This file covers the **data** surface — what each
//! tool returns when there's actually something to return. The two
//! files together are the full Phase 15 read-only validation.
//!
//! Spawn / round-trip helpers are intentionally duplicated from
//! `drevo_mcp_e2e_tests.rs` (rather than factored into a shared
//! `tests/common/` module) — keeping each integration-test file
//! self-contained matches the existing project pattern and keeps
//! the `00091` scope visible in a single grep.

#![cfg(feature = "redb-backend")]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use serde_json::{json, Value};
use tempfile::TempDir;

use drevo::db::Drevo;
use drevo::model::{NewEdge, NewNode};

/// Numeric ids + hyphenated UUIDs of every node in the fixture
/// corpus — populated by [`populate_then_spawn`] before the binary
/// is spawned and used by the test cases to assert against `node_get`,
/// `bfs`, etc.
struct Corpus {
    alice_id: u64,
    alice_uuid: String,
    bob_id: u64,
    drevo_project_id: u64,
    /// Currently unused — kept on the corpus so the next test that
    /// needs a hyphenated UUID for the drevo project can just read
    /// it off without re-deriving from `[u8; 16]`.
    #[allow(dead_code)]
    drevo_project_uuid: String,
    ship_task_id: u64,
}

/// Convert a 16-byte UUID (as stored on [`drevo::model::Node`])
/// into the hyphenated string form the MCP `drevo_node_get_by_uuid`
/// tool accepts.
fn uuid_to_hyphenated(bytes: &[u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

/// Locate the freshly-built `drevo-mcp` binary — `CARGO_BIN_EXE_*`
/// is set automatically by Cargo for binaries declared under `[[bin]]`.
fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_drevo-mcp"))
}

/// Build a `serde_json::Value` properties map from a slice of
/// `(key, value)` pairs. Saves repeating `serde_json::Map::new()`
/// rituals in every fixture row.
fn props(entries: &[(&str, Value)]) -> drevo::model::Properties {
    let mut p = HashMap::new();
    for (k, v) in entries {
        p.insert((*k).to_string(), v.clone());
    }
    drevo::model::Properties(p)
}

/// Pre-populate the fixture corpus, close the Drevo, then spawn
/// the MCP server against the same data dir. Returns the [`Corpus`]
/// plus the child process, a buffered stdout reader, and the temp
/// dir guard (drop it to clean the on-disk redb).
fn populate_then_spawn() -> (Corpus, Child, BufReader<std::process::ChildStdout>, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let data_dir = tmp.path().join("drevo-data");
    let corpus = populate(&data_dir);
    let (child, reader) = spawn_against(&data_dir);
    (corpus, child, reader, tmp)
}

fn populate(data_dir: &Path) -> Corpus {
    let drevo = Drevo::open(data_dir).expect("open Drevo for population");
    let alice = drevo
        .create_node(NewNode {
            kind: "person".to_string(),
            title: "Alice".to_string(),
            body: String::new(),
            body_html: String::new(),
            properties: props(&[("email", json!("alice@example.com"))]),
        })
        .expect("create alice");
    let bob = drevo
        .create_node(NewNode {
            kind: "person".to_string(),
            title: "Bob".to_string(),
            body: String::new(),
            body_html: String::new(),
            properties: props(&[("email", json!("bob@example.com"))]),
        })
        .expect("create bob");
    let drevo_project = drevo
        .create_node(NewNode {
            kind: "project".to_string(),
            title: "drevo".to_string(),
            body: String::new(),
            body_html: String::new(),
            properties: props(&[("description", json!("Embedded graph database"))]),
        })
        .expect("create project");
    let ship_task = drevo
        .create_node(NewNode {
            kind: "task".to_string(),
            title: "Ship 00091".to_string(),
            body: String::new(),
            body_html: String::new(),
            properties: props(&[("priority", json!("high")), ("due", json!("2026-05-29"))]),
        })
        .expect("create task");

    // Edges: Alice owns drevo; Bob owns drevo; Ship 00091 depends_on drevo.
    // All three exercise different `direction` / `edge_kind` paths in bfs.
    drevo
        .create_edge(NewEdge {
            kind: "owns".to_string(),
            from_id: alice.id,
            to_id: drevo_project.id,
            weight: 1.0,
            properties: drevo::model::Properties(HashMap::new()),
        })
        .expect("alice owns drevo");
    drevo
        .create_edge(NewEdge {
            kind: "owns".to_string(),
            from_id: bob.id,
            to_id: drevo_project.id,
            weight: 1.0,
            properties: drevo::model::Properties(HashMap::new()),
        })
        .expect("bob owns drevo");
    drevo
        .create_edge(NewEdge {
            kind: "depends_on".to_string(),
            from_id: ship_task.id,
            to_id: drevo_project.id,
            weight: 1.0,
            properties: drevo::model::Properties(HashMap::new()),
        })
        .expect("ship depends_on drevo");

    let alice_uuid = uuid_to_hyphenated(&alice.uuid);
    let drevo_project_uuid = uuid_to_hyphenated(&drevo_project.uuid);
    drevo.close().expect("close Drevo before spawn");

    Corpus {
        alice_id: alice.id,
        alice_uuid,
        bob_id: bob.id,
        drevo_project_id: drevo_project.id,
        drevo_project_uuid,
        ship_task_id: ship_task.id,
    }
}

fn spawn_against(data_dir: &Path) -> (Child, BufReader<std::process::ChildStdout>) {
    let mut child = Command::new(binary_path())
        .arg("--data-dir")
        .arg(data_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn drevo-mcp");
    let stdout = child.stdout.take().expect("child stdout");
    (child, BufReader::new(stdout))
}

/// Send a JSON-RPC envelope and read the next response line.
/// Panics on I/O failure (broken pipe = test failure).
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

/// Convenience wrapper: send `tools/call` and return the parsed
/// JSON payload (the tool's structured result, NOT the wrapping
/// MCP `text` content block). Panics if the response is an error
/// envelope or if `isError: true`.
fn tools_call(
    child: &mut Child,
    reader: &mut BufReader<std::process::ChildStdout>,
    name: &str,
    arguments: Value,
    request_id: i64,
) -> Value {
    let resp = round_trip(
        child,
        reader,
        &json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }),
    );
    assert!(
        resp["error"].is_null(),
        "tools/call {name} returned JSON-RPC error: {}",
        resp["error"]
    );
    assert_eq!(
        resp["result"]["isError"], false,
        "tools/call {name} reported isError:true: {}",
        resp["result"]
    );
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    serde_json::from_str(text).expect("payload is JSON")
}

/// Initialise the MCP server (one-shot — every test does this
/// before it can issue `tools/call`).
fn handshake(child: &mut Child, reader: &mut BufReader<std::process::ChildStdout>) {
    let _ = round_trip(
        child,
        reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "mcp-validation-e2e", "version": "0.0.0"}
            }
        }),
    );
}

/// Drop stdin (EOF → server returns) then wait for the child.
fn shutdown(mut child: Child) {
    drop(child.stdin.take());
    let status = child.wait().expect("child wait");
    assert!(status.success(), "drevo-mcp exited with {status}");
}

// ── 1. count surface ───────────────────────────────────────────────────

#[test]
fn count_nodes_returns_total_after_populate() {
    let (_corpus, mut child, mut reader, _tmp) = populate_then_spawn();
    handshake(&mut child, &mut reader);
    let payload = tools_call(&mut child, &mut reader, "drevo_count_nodes", json!({}), 1);
    assert_eq!(
        payload["count"], 4,
        "count must reflect the 4 nodes inserted by populate()"
    );
    shutdown(child);
}

// ── 2. labels (list_nodes_by_kind) surface ─────────────────────────────

#[test]
fn list_nodes_by_kind_returns_two_persons() {
    let (_corpus, mut child, mut reader, _tmp) = populate_then_spawn();
    handshake(&mut child, &mut reader);
    let payload = tools_call(
        &mut child,
        &mut reader,
        "drevo_list_nodes_by_kind",
        json!({"kind": "person"}),
        1,
    );
    let nodes = payload["nodes"].as_array().expect("nodes array");
    assert_eq!(nodes.len(), 2, "expected 2 person nodes: {nodes:?}");
    let titles: Vec<&str> = nodes
        .iter()
        .map(|n| n["title"].as_str().expect("title"))
        .collect();
    assert!(titles.contains(&"Alice"));
    assert!(titles.contains(&"Bob"));
    shutdown(child);
}

#[test]
fn list_nodes_by_kind_returns_one_project() {
    let (_corpus, mut child, mut reader, _tmp) = populate_then_spawn();
    handshake(&mut child, &mut reader);
    let payload = tools_call(
        &mut child,
        &mut reader,
        "drevo_list_nodes_by_kind",
        json!({"kind": "project"}),
        1,
    );
    let nodes = payload["nodes"].as_array().expect("nodes array");
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["title"], "drevo");
    shutdown(child);
}

#[test]
fn list_nodes_by_kind_returns_one_task() {
    let (_corpus, mut child, mut reader, _tmp) = populate_then_spawn();
    handshake(&mut child, &mut reader);
    let payload = tools_call(
        &mut child,
        &mut reader,
        "drevo_list_nodes_by_kind",
        json!({"kind": "task"}),
        1,
    );
    let nodes = payload["nodes"].as_array().expect("nodes array");
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["title"], "Ship 00091");
    shutdown(child);
}

#[test]
fn list_nodes_by_kind_unknown_returns_empty() {
    let (_corpus, mut child, mut reader, _tmp) = populate_then_spawn();
    handshake(&mut child, &mut reader);
    let payload = tools_call(
        &mut child,
        &mut reader,
        "drevo_list_nodes_by_kind",
        json!({"kind": "nonexistent"}),
        1,
    );
    let nodes = payload["nodes"].as_array().expect("nodes array");
    assert!(nodes.is_empty(), "unknown kind must return empty list");
    shutdown(child);
}

// ── 3. properties (node_get) surface ───────────────────────────────────

#[test]
fn node_get_returns_full_node_with_properties() {
    let (corpus, mut child, mut reader, _tmp) = populate_then_spawn();
    handshake(&mut child, &mut reader);
    let payload = tools_call(
        &mut child,
        &mut reader,
        "drevo_node_get",
        json!({"id": corpus.alice_id}),
        1,
    );
    let node = &payload["node"];
    assert!(!node.is_null(), "alice must be found");
    assert_eq!(node["title"], "Alice");
    assert_eq!(node["kind"], "person");
    assert_eq!(
        node["properties"]["email"], "alice@example.com",
        "properties map must round-trip the email"
    );
    shutdown(child);
}

#[test]
fn node_get_returns_null_for_missing_id() {
    let (_corpus, mut child, mut reader, _tmp) = populate_then_spawn();
    handshake(&mut child, &mut reader);
    let payload = tools_call(
        &mut child,
        &mut reader,
        "drevo_node_get",
        json!({"id": 999_999}),
        1,
    );
    assert!(payload["node"].is_null(), "missing id must return null");
    shutdown(child);
}

#[test]
fn node_get_task_returns_multiple_properties() {
    let (corpus, mut child, mut reader, _tmp) = populate_then_spawn();
    handshake(&mut child, &mut reader);
    let payload = tools_call(
        &mut child,
        &mut reader,
        "drevo_node_get",
        json!({"id": corpus.ship_task_id}),
        1,
    );
    let node = &payload["node"];
    assert_eq!(node["properties"]["priority"], "high");
    assert_eq!(node["properties"]["due"], "2026-05-29");
    shutdown(child);
}

// ── 4. node_get_by_uuid round-trip ─────────────────────────────────────

#[test]
fn node_get_by_uuid_returns_the_same_node() {
    let (corpus, mut child, mut reader, _tmp) = populate_then_spawn();
    handshake(&mut child, &mut reader);
    let payload = tools_call(
        &mut child,
        &mut reader,
        "drevo_node_get_by_uuid",
        json!({"uuid": corpus.alice_uuid}),
        1,
    );
    let node = &payload["node"];
    assert!(!node.is_null());
    assert_eq!(node["title"], "Alice");
    assert_eq!(node["id"], corpus.alice_id);
    shutdown(child);
}

#[test]
fn node_get_by_uuid_rejects_malformed_uuid() {
    let (_corpus, mut child, mut reader, _tmp) = populate_then_spawn();
    handshake(&mut child, &mut reader);
    // Malformed UUID → JSON-RPC -32602 (invalid_params from the
    // tool's parse step).
    let resp = round_trip(
        &mut child,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "drevo_node_get_by_uuid",
                "arguments": {"uuid": "not-a-uuid"}
            }
        }),
    );
    assert_eq!(
        resp["error"]["code"], -32602,
        "malformed UUID must surface as -32602 invalid_params"
    );
    shutdown(child);
}

// ── 5. rels + traversal surface (bfs) ──────────────────────────────────

#[test]
fn bfs_from_project_inward_reaches_both_owners() {
    let (corpus, mut child, mut reader, _tmp) = populate_then_spawn();
    handshake(&mut child, &mut reader);
    // direction: "in" → follow incoming `owns` edges. From drevo
    // project we should reach both Alice and Bob.
    let payload = tools_call(
        &mut child,
        &mut reader,
        "drevo_bfs",
        json!({
            "start_id": corpus.drevo_project_id,
            "max_depth": 2,
            "direction": "in",
            "edge_kind": "owns"
        }),
        1,
    );
    let nodes = payload["nodes"].as_array().expect("nodes array");
    assert_eq!(
        nodes.len(),
        2,
        "expected to reach Alice and Bob via `owns` edges: {nodes:?}"
    );
    let ids: Vec<u64> = nodes
        .iter()
        .map(|n| n["id"].as_u64().expect("id"))
        .collect();
    assert!(ids.contains(&corpus.alice_id));
    assert!(ids.contains(&corpus.bob_id));
    shutdown(child);
}

#[test]
fn bfs_edge_kind_filter_isolates_depends_on() {
    let (corpus, mut child, mut reader, _tmp) = populate_then_spawn();
    handshake(&mut child, &mut reader);
    // From the task, follow only `depends_on` edges outward — we
    // should reach the drevo project, NOT the people who own it.
    let payload = tools_call(
        &mut child,
        &mut reader,
        "drevo_bfs",
        json!({
            "start_id": corpus.ship_task_id,
            "max_depth": 3,
            "direction": "out",
            "edge_kind": "depends_on"
        }),
        1,
    );
    let nodes = payload["nodes"].as_array().expect("nodes array");
    assert_eq!(
        nodes.len(),
        1,
        "edge_kind=`depends_on` must restrict the frontier"
    );
    assert_eq!(nodes[0]["id"], corpus.drevo_project_id);
    shutdown(child);
}

#[test]
fn bfs_max_depth_zero_returns_empty() {
    let (corpus, mut child, mut reader, _tmp) = populate_then_spawn();
    handshake(&mut child, &mut reader);
    let payload = tools_call(
        &mut child,
        &mut reader,
        "drevo_bfs",
        json!({"start_id": corpus.drevo_project_id, "max_depth": 0}),
        1,
    );
    let nodes = payload["nodes"].as_array().expect("nodes array");
    assert!(nodes.is_empty(), "max_depth=0 must return empty");
    shutdown(child);
}

// ── 6. search_fts surface ──────────────────────────────────────────────

#[test]
fn search_fts_finds_node_by_title() {
    let (_corpus, mut child, mut reader, _tmp) = populate_then_spawn();
    handshake(&mut child, &mut reader);
    let payload = tools_call(
        &mut child,
        &mut reader,
        "drevo_search_fts",
        json!({"query": "Alice", "limit": 5}),
        1,
    );
    let results = payload["results"].as_array().expect("results array");
    assert!(
        !results.is_empty(),
        "FTS for `Alice` must return at least one hit"
    );
    let titles: Vec<&str> = results
        .iter()
        .map(|r| r["node"]["title"].as_str().expect("title"))
        .collect();
    assert!(
        titles.contains(&"Alice"),
        "Alice node must be in the result set: {titles:?}"
    );
    shutdown(child);
}

#[test]
fn search_fts_finds_project_by_title_substring() {
    let (corpus, mut child, mut reader, _tmp) = populate_then_spawn();
    handshake(&mut child, &mut reader);
    let payload = tools_call(
        &mut child,
        &mut reader,
        "drevo_search_fts",
        json!({"query": "drevo", "limit": 5}),
        1,
    );
    let results = payload["results"].as_array().expect("results array");
    let ids: Vec<u64> = results
        .iter()
        .map(|r| r["node"]["id"].as_u64().expect("id"))
        .collect();
    assert!(
        ids.contains(&corpus.drevo_project_id),
        "drevo project must be in FTS results for `drevo`"
    );
    shutdown(child);
}

#[test]
fn search_fts_no_match_returns_empty() {
    let (_corpus, mut child, mut reader, _tmp) = populate_then_spawn();
    handshake(&mut child, &mut reader);
    let payload = tools_call(
        &mut child,
        &mut reader,
        "drevo_search_fts",
        json!({"query": "zzz_no_match_here", "limit": 5}),
        1,
    );
    let results = payload["results"].as_array().expect("results array");
    assert!(results.is_empty());
    shutdown(child);
}

// ── 7. health_check + invalid_params sanity ────────────────────────────

#[test]
fn health_check_reports_healthy_on_populated_drevo() {
    let (_corpus, mut child, mut reader, _tmp) = populate_then_spawn();
    handshake(&mut child, &mut reader);
    let payload = tools_call(&mut child, &mut reader, "drevo_health_check", json!({}), 1);
    assert_eq!(payload["healthy"], true);
    shutdown(child);
}

#[test]
fn node_get_missing_id_argument_returns_invalid_params() {
    let (_corpus, mut child, mut reader, _tmp) = populate_then_spawn();
    handshake(&mut child, &mut reader);
    let resp = round_trip(
        &mut child,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "drevo_node_get", "arguments": {}}
        }),
    );
    assert_eq!(
        resp["error"]["code"], -32602,
        "missing required argument must be -32602 invalid_params"
    );
    shutdown(child);
}
