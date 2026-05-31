//! [`Tool`] trait + [`ToolRegistry`] + the seven baseline tools
//! wrapping [`crate::db::Drevo`].
//!
//! Each tool wraps one or two `Drevo` calls behind a tiny JSON
//! adapter:
//!
//! 1. The static [`Tool::descriptor`] returns the MCP `Tool`
//!    metadata (name, description, `inputSchema`) — used by
//!    `tools/list`.
//! 2. The dynamic [`Tool::call`] takes an `arguments: Value` (per
//!    the input schema) plus a `&Drevo` handle, executes the call,
//!    and returns either a JSON-serialisable result body OR a
//!    [`JsonRpcError`] in the `-32000` server-error band.
//!
//! The dispatcher in [`super::server`] does NOT special-case any
//! tool — it just looks the name up in the [`ToolRegistry`] and
//! invokes `call`. New tools can be added in follow-up sessions
//! without touching the dispatch layer.
//!
//! ## Wire shape per tool
//!
//! Every successful tool call returns a JSON object with the
//! tool-specific fields. The server wraps that object as a single
//! `text` [`super::protocol::ToolCallContent`] block — i.e. the
//! resulting `tools/call` response looks like:
//!
//! ```json
//! {
//!   "content": [{"type": "text", "text": "{\"count\": 42}"}],
//!   "isError": false
//! }
//! ```
//!
//! The `text` payload is itself JSON-serialised because most MCP
//! clients today (Cline, Claude Code) don't yet support structured
//! `tools/call` returns and round-trip the text through their LLM
//! prompt. Once clients catch up the server can switch to a
//! structured-content block without breaking older callers.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::db::Drevo;
use crate::model::Direction;

use super::protocol::{JsonRpcError, Tool as ToolMeta};
use super::python_api::ApiCatalog;

/// Static MCP tool definition + dynamic dispatcher.
///
/// Trait objects (`Box<dyn Tool>`) are used here because the registry
/// holds a heterogeneous set of tools that share no input/output
/// types. The trait is small: one method to return metadata, one to
/// execute against `&Drevo`. We deliberately do NOT make `call`
/// async — the underlying `Drevo` API is synchronous, and using
/// `Future` here would force the dispatcher to pull in an async
/// runtime for no benefit.
pub trait Tool: Send + Sync {
    /// Returns the static MCP `Tool` descriptor for `tools/list`
    /// responses. Called once per `tools/list` request — keep it
    /// cheap (no allocations beyond the strings themselves).
    fn descriptor(&self) -> ToolMeta;

    /// Executes the tool. `arguments` is parsed per the
    /// `descriptor().input_schema` JSON Schema — the implementation
    /// is responsible for re-validating types before consuming
    /// them (the dispatch layer does NOT validate the schema).
    ///
    /// Returns a JSON-serialisable result body on success, or a
    /// `-32000` [`JsonRpcError`] on tool-level failure (caller-
    /// recoverable; surfaced through `isError: true`).
    fn call(&self, drevo: &Drevo, arguments: &Value) -> Result<Value, JsonRpcError>;
}

/// Registry mapping tool names → implementations.
///
/// Uses a `BTreeMap` (not `HashMap`) so `tools/list` returns the
/// tools in a stable alphabetical order — easier for clients (and
/// test snapshots) to compare against.
pub struct ToolRegistry {
    tools: BTreeMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    /// Build an empty registry. Tools are added via [`Self::register`].
    pub fn empty() -> Self {
        Self {
            tools: BTreeMap::new(),
        }
    }

    /// Build the default registry with the seven baseline tools
    /// from this module. This is what `Server::with_default_tools`
    /// uses — wrapped here so tests can swap it out for an empty
    /// or a single-tool registry without touching server code.
    pub fn default_tools() -> Self {
        let mut r = Self::empty();
        r.register(HealthCheck);
        r.register(CountNodes);
        r.register(NodeGet);
        r.register(NodeGetByUuid);
        r.register(SearchFts);
        r.register(Bfs);
        r.register(ListNodesByKind);
        r.register(PythonApiList);
        r.register(PythonApiDescribe);
        r.register(PythonApiExamples);
        r
    }

    /// Add a tool to the registry. Panics if the tool's `name`
    /// collides with an existing one — collisions indicate a typo
    /// or an unintentional duplicate, not a recoverable runtime
    /// state.
    pub fn register<T: Tool + 'static>(&mut self, tool: T) {
        let meta = tool.descriptor();
        if self.tools.contains_key(&meta.name) {
            panic!("MCP tool registered twice: {}", meta.name);
        }
        self.tools.insert(meta.name, Box::new(tool));
    }

    /// Returns the descriptors of every registered tool in
    /// alphabetical order. Backing for `tools/list`.
    pub fn list(&self) -> Vec<ToolMeta> {
        self.tools.values().map(|t| t.descriptor()).collect()
    }

    /// Look up a tool by name. Returns `None` if not registered —
    /// the dispatcher turns this into a `-32602` invalid-params
    /// error with the unknown name as `data`.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    /// Total number of registered tools — useful for sanity tests.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the registry has zero tools.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

// ── Baseline tools ─────────────────────────────────────────────────────

/// `drevo_health_check` — probe the underlying Drevo handle.
/// Wraps [`Drevo::health_check`]; takes no arguments.
pub struct HealthCheck;

impl Tool for HealthCheck {
    fn descriptor(&self) -> ToolMeta {
        ToolMeta {
            name: "drevo_health_check".to_string(),
            description: "Probe the underlying Drevo storage handle. \
                Returns {\"healthy\": true} on success."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, drevo: &Drevo, _arguments: &Value) -> Result<Value, JsonRpcError> {
        drevo
            .health_check()
            .map_err(|e| JsonRpcError::tool_error(format!("health_check failed: {e}"), None))?;
        Ok(json!({ "healthy": true }))
    }
}

/// `drevo_count_nodes` — return the number of nodes currently in
/// the graph. Implemented via `list_recent(usize::MAX)` and counting
/// the result; suitable for graphs up to ~1M nodes. Future task can
/// add a dedicated `Drevo::count_nodes()` if perf demands it.
pub struct CountNodes;

impl Tool for CountNodes {
    fn descriptor(&self) -> ToolMeta {
        ToolMeta {
            name: "drevo_count_nodes".to_string(),
            description: "Return the total number of nodes in the graph. \
                Result shape: {\"count\": u64}."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, drevo: &Drevo, _arguments: &Value) -> Result<Value, JsonRpcError> {
        let nodes = drevo
            .list_recent(usize::MAX)
            .map_err(|e| JsonRpcError::tool_error(format!("count_nodes failed: {e}"), None))?;
        Ok(json!({ "count": nodes.len() as u64 }))
    }
}

/// `drevo_node_get` — fetch a single node by numeric id.
pub struct NodeGet;

#[derive(Debug, Deserialize)]
struct NodeGetArgs {
    id: u64,
}

impl Tool for NodeGet {
    fn descriptor(&self) -> ToolMeta {
        ToolMeta {
            name: "drevo_node_get".to_string(),
            description: "Fetch a single node by its numeric id. \
                Result: {\"node\": Node|null}."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "minimum": 0, "description": "Node id"}
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, drevo: &Drevo, arguments: &Value) -> Result<Value, JsonRpcError> {
        let args: NodeGetArgs = serde_json::from_value(arguments.clone())
            .map_err(|e| JsonRpcError::invalid_params(format!("node_get: {e}")))?;
        let node = drevo
            .get_node(args.id)
            .map_err(|e| JsonRpcError::tool_error(format!("node_get failed: {e}"), None))?;
        Ok(json!({ "node": node }))
    }
}

/// `drevo_node_get_by_uuid` — fetch a single node by its 16-byte
/// UUID, accepted as a hyphenated UUID string (the same form
/// drevo-py and HTTP-API consumers use).
pub struct NodeGetByUuid;

#[derive(Debug, Deserialize)]
struct NodeGetByUuidArgs {
    uuid: String,
}

impl Tool for NodeGetByUuid {
    fn descriptor(&self) -> ToolMeta {
        ToolMeta {
            name: "drevo_node_get_by_uuid".to_string(),
            description: "Fetch a single node by its hyphenated UUID \
                string. Result: {\"node\": Node|null}."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "uuid": {
                        "type": "string",
                        "description": "Node UUID, hyphenated form (e.g. \"550e8400-e29b-41d4-a716-446655440000\")"
                    }
                },
                "required": ["uuid"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, drevo: &Drevo, arguments: &Value) -> Result<Value, JsonRpcError> {
        let args: NodeGetByUuidArgs = serde_json::from_value(arguments.clone())
            .map_err(|e| JsonRpcError::invalid_params(format!("node_get_by_uuid: {e}")))?;
        let bytes = parse_uuid(&args.uuid)
            .map_err(|e| JsonRpcError::invalid_params(format!("uuid parse: {e}")))?;
        let node = drevo
            .get_node_by_uuid(&bytes)
            .map_err(|e| JsonRpcError::tool_error(format!("node_get_by_uuid failed: {e}"), None))?;
        Ok(json!({ "node": node }))
    }
}

/// `drevo_search_fts` — full-text search against the FTS index.
/// Returns scored nodes ranked by trigram-match strength.
pub struct SearchFts;

#[derive(Debug, Deserialize)]
struct SearchFtsArgs {
    query: String,
    #[serde(default = "default_search_limit")]
    limit: usize,
}

fn default_search_limit() -> usize {
    20
}

impl Tool for SearchFts {
    fn descriptor(&self) -> ToolMeta {
        ToolMeta {
            name: "drevo_search_fts".to_string(),
            description: "Full-text search over node titles. \
                Returns scored nodes ranked by trigram-match strength. \
                Result: {\"results\": [ScoredNode]}."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query"},
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 1000,
                        "default": 20,
                        "description": "Maximum number of results to return"
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, drevo: &Drevo, arguments: &Value) -> Result<Value, JsonRpcError> {
        let args: SearchFtsArgs = serde_json::from_value(arguments.clone())
            .map_err(|e| JsonRpcError::invalid_params(format!("search_fts: {e}")))?;
        let results = drevo
            .search_fts(&args.query, args.limit)
            .map_err(|e| JsonRpcError::tool_error(format!("search_fts failed: {e}"), None))?;
        Ok(json!({ "results": results }))
    }
}

/// `drevo_bfs` — breadth-first traversal from a start node, bounded
/// by `max_depth`. Returns the visited nodes in BFS order (start
/// node NOT included, per the [`Drevo::bfs`] contract).
pub struct Bfs;

#[derive(Debug, Deserialize)]
struct BfsArgs {
    start_id: u64,
    #[serde(default = "default_bfs_depth")]
    max_depth: u8,
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    edge_kind: Option<String>,
}

fn default_bfs_depth() -> u8 {
    3
}

impl Tool for Bfs {
    fn descriptor(&self) -> ToolMeta {
        ToolMeta {
            name: "drevo_bfs".to_string(),
            description: "Breadth-first traversal from a start node. \
                Returns visited nodes in BFS order, start excluded. \
                Result: {\"nodes\": [Node]}."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "start_id": {"type": "integer", "minimum": 0},
                    "max_depth": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 255,
                        "default": 3
                    },
                    "direction": {
                        "type": "string",
                        "enum": ["out", "in", "both"],
                        "default": "out"
                    },
                    "edge_kind": {
                        "type": ["string", "null"],
                        "description": "If set, only follow edges of this kind"
                    }
                },
                "required": ["start_id"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, drevo: &Drevo, arguments: &Value) -> Result<Value, JsonRpcError> {
        let args: BfsArgs = serde_json::from_value(arguments.clone())
            .map_err(|e| JsonRpcError::invalid_params(format!("bfs: {e}")))?;
        let direction = match args.direction.as_deref() {
            Some("out") | None => Direction::Outgoing,
            Some("in") => Direction::Incoming,
            Some("both") => Direction::Both,
            Some(other) => {
                return Err(JsonRpcError::invalid_params(format!(
                    "bfs: direction must be one of [out, in, both], got {other:?}"
                )))
            }
        };
        let nodes = drevo
            .bfs(
                args.start_id,
                args.max_depth,
                direction,
                args.edge_kind.as_deref(),
            )
            .map_err(|e| JsonRpcError::tool_error(format!("bfs failed: {e}"), None))?;
        Ok(json!({ "nodes": nodes }))
    }
}

/// `drevo_list_nodes_by_kind` — page through nodes of a given kind.
/// Wraps [`Drevo::list_nodes_by_kind`].
pub struct ListNodesByKind;

#[derive(Debug, Deserialize)]
struct ListNodesByKindArgs {
    kind: String,
    #[serde(default = "default_list_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}

fn default_list_limit() -> usize {
    50
}

impl Tool for ListNodesByKind {
    fn descriptor(&self) -> ToolMeta {
        ToolMeta {
            name: "drevo_list_nodes_by_kind".to_string(),
            description: "Page through nodes of a given kind (label). \
                Result: {\"nodes\": [Node]}."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "description": "Node kind / label"},
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 1000,
                        "default": 50
                    },
                    "offset": {"type": "integer", "minimum": 0, "default": 0}
                },
                "required": ["kind"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, drevo: &Drevo, arguments: &Value) -> Result<Value, JsonRpcError> {
        let args: ListNodesByKindArgs = serde_json::from_value(arguments.clone())
            .map_err(|e| JsonRpcError::invalid_params(format!("list_nodes_by_kind: {e}")))?;
        let nodes = drevo
            .list_nodes_by_kind(&args.kind, args.limit, args.offset)
            .map_err(|e| {
                JsonRpcError::tool_error(format!("list_nodes_by_kind failed: {e}"), None)
            })?;
        Ok(json!({ "nodes": nodes }))
    }
}

// ── Python-API introspection tools (task 00121) ────────────────────────
//
// These three tools answer "where is the Python API documented?" — in
// the queryable [`ApiCatalog`], not a hand-maintained markdown file.
// They are the only tools in this module that do NOT touch the `&Drevo`
// handle: the catalog is derived at compile time from the `drevo-py`
// type stubs + README, so it is identical for every database and needs
// no live data. The handle argument is accepted (the [`Tool`] trait
// requires it) and ignored.

/// `python_api_list` — enumerate Python API symbols, optionally
/// filtered by a dotted-name prefix.
pub struct PythonApiList;

#[derive(Debug, Deserialize)]
struct PythonApiListArgs {
    #[serde(default)]
    prefix: String,
}

impl Tool for PythonApiList {
    fn descriptor(&self) -> ToolMeta {
        ToolMeta {
            name: "python_api_list".to_string(),
            description: "Enumerate symbols of the drevo-py Python API \
                (classes, methods, functions, attributes, constants). \
                Optionally filter by a dotted-name prefix such as \
                \"drevo.Drevo\" or \"Retriever\"; empty lists everything. \
                Result: {\"count\": u64, \"symbols\": [{name, kind, signature}]}."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prefix": {
                        "type": "string",
                        "default": "",
                        "description": "Dotted-name prefix filter (case-insensitive); empty for all"
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, _drevo: &Drevo, arguments: &Value) -> Result<Value, JsonRpcError> {
        let args: PythonApiListArgs = serde_json::from_value(arguments.clone())
            .map_err(|e| JsonRpcError::invalid_params(format!("python_api_list: {e}")))?;
        let catalog = ApiCatalog::builtin();
        let symbols: Vec<Value> = catalog
            .list(&args.prefix)
            .into_iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "kind": s.kind.as_str(),
                    "signature": s.signature,
                })
            })
            .collect();
        Ok(json!({ "count": symbols.len() as u64, "symbols": symbols }))
    }
}

/// `python_api_describe` — return the signature + docstring for one
/// Python API symbol, resolved by qualified, suffix, or simple name.
pub struct PythonApiDescribe;

#[derive(Debug, Deserialize)]
struct PythonApiDescribeArgs {
    name: String,
}

impl Tool for PythonApiDescribe {
    fn descriptor(&self) -> ToolMeta {
        ToolMeta {
            name: "python_api_describe".to_string(),
            description:
                "Describe one symbol of the drevo-py Python API: \
                its kind, signature, and docstring. Accepts a qualified \
                name (\"drevo.Drevo.create_node\"), a class-qualified name \
                (\"Drevo.create_node\"), or a bare name (\"create_node\"). \
                On a miss returns {\"found\": false, \"suggestions\": [..]}; \
                on a hit {\"found\": true, \"symbol\": {name, kind, signature, docstring, parent}}."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Symbol name (qualified, class-qualified, or bare)"
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, _drevo: &Drevo, arguments: &Value) -> Result<Value, JsonRpcError> {
        let args: PythonApiDescribeArgs = serde_json::from_value(arguments.clone())
            .map_err(|e| JsonRpcError::invalid_params(format!("python_api_describe: {e}")))?;
        let catalog = ApiCatalog::builtin();
        match catalog.describe(&args.name) {
            Some(symbol) => Ok(json!({
                "found": true,
                "symbol": {
                    "name": symbol.name,
                    "kind": symbol.kind.as_str(),
                    "signature": symbol.signature,
                    "docstring": symbol.docstring,
                    "parent": symbol.parent,
                }
            })),
            None => Ok(json!({
                "found": false,
                "suggestions": catalog.suggest(&args.name, 5),
            })),
        }
    }
}

/// `python_api_examples` — fuzzy-search the curated usage examples for
/// snippets matching a free-text intent.
pub struct PythonApiExamples;

#[derive(Debug, Deserialize)]
struct PythonApiExamplesArgs {
    intent: String,
    #[serde(default = "default_examples_limit")]
    limit: usize,
}

fn default_examples_limit() -> usize {
    5
}

impl Tool for PythonApiExamples {
    fn descriptor(&self) -> ToolMeta {
        ToolMeta {
            name: "python_api_examples".to_string(),
            description: "Fuzzy-search drevo-py usage examples by intent \
                (e.g. \"create a node\", \"retrieve a RAG context\", \
                \"vector search\"). Returns the best-matching code \
                snippets, ranked by token overlap. \
                Result: {\"count\": u64, \"examples\": [{title, code, source}]}."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "intent": {
                        "type": "string",
                        "description": "Free-text description of what you want to do"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 50,
                        "default": 5,
                        "description": "Maximum number of examples to return"
                    }
                },
                "required": ["intent"],
                "additionalProperties": false
            }),
        }
    }

    fn call(&self, _drevo: &Drevo, arguments: &Value) -> Result<Value, JsonRpcError> {
        let args: PythonApiExamplesArgs = serde_json::from_value(arguments.clone())
            .map_err(|e| JsonRpcError::invalid_params(format!("python_api_examples: {e}")))?;
        let catalog = ApiCatalog::builtin();
        let examples: Vec<Value> = catalog
            .search_examples(&args.intent, args.limit)
            .into_iter()
            .map(|ex| {
                json!({
                    "title": ex.title,
                    "code": ex.code,
                    "source": ex.source,
                })
            })
            .collect();
        Ok(json!({ "count": examples.len() as u64, "examples": examples }))
    }
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Parse a hyphenated UUID string into the 16-byte form drevo
/// stores. We don't pull the `uuid` crate as a dependency for this
/// one helper — the parse is six hex segments separated by hyphens,
/// trivial in pure Rust.
fn parse_uuid(s: &str) -> Result<[u8; 16], String> {
    // Expected form: 8-4-4-4-12 = 36 chars.
    if s.len() != 36 {
        return Err(format!("UUID must be 36 characters, got {}", s.len()));
    }
    let segments: Vec<&str> = s.split('-').collect();
    if segments.len() != 5 {
        return Err(format!(
            "UUID must have 5 hyphen-separated segments, got {}",
            segments.len()
        ));
    }
    let expected_lens = [8usize, 4, 4, 4, 12];
    for (seg, want) in segments.iter().zip(expected_lens.iter()) {
        if seg.len() != *want {
            return Err(format!(
                "UUID segment length mismatch: expected {want}, got {} ({seg:?})",
                seg.len()
            ));
        }
    }
    let hex: String = segments.concat();
    let mut bytes = [0u8; 16];
    for (i, byte) in bytes.iter_mut().enumerate() {
        let hi = hex_char(hex.as_bytes()[i * 2])?;
        let lo = hex_char(hex.as_bytes()[i * 2 + 1])?;
        *byte = (hi << 4) | lo;
    }
    Ok(bytes)
}

fn hex_char(c: u8) -> Result<u8, String> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(format!("non-hex character in UUID: {:?}", c as char)),
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_has_ten_tools() {
        let r = ToolRegistry::default_tools();
        assert_eq!(
            r.len(),
            10,
            "seven baseline + three python_api introspection tools"
        );
        assert!(!r.is_empty());
    }

    #[test]
    fn default_registry_lists_tools_in_alphabetical_order() {
        let r = ToolRegistry::default_tools();
        let names: Vec<String> = r.list().into_iter().map(|t| t.name).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "tools/list must return tools alphabetically");
    }

    #[test]
    fn default_registry_contains_every_baseline_tool_name() {
        let r = ToolRegistry::default_tools();
        for expected in [
            "drevo_bfs",
            "drevo_count_nodes",
            "drevo_health_check",
            "drevo_list_nodes_by_kind",
            "drevo_node_get",
            "drevo_node_get_by_uuid",
            "drevo_search_fts",
            "python_api_describe",
            "python_api_examples",
            "python_api_list",
        ] {
            assert!(
                r.get(expected).is_some(),
                "baseline tool `{expected}` not registered"
            );
        }
    }

    #[test]
    fn empty_registry_is_empty() {
        let r = ToolRegistry::empty();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        assert!(r.get("drevo_health_check").is_none());
    }

    #[test]
    #[should_panic(expected = "MCP tool registered twice")]
    fn registry_panics_on_duplicate_name() {
        let mut r = ToolRegistry::empty();
        r.register(HealthCheck);
        r.register(HealthCheck);
    }

    #[test]
    fn parse_uuid_round_trip() {
        let s = "550e8400-e29b-41d4-a716-446655440000";
        let bytes = parse_uuid(s).expect("parse");
        assert_eq!(bytes[0], 0x55);
        assert_eq!(bytes[1], 0x0e);
        assert_eq!(bytes[15], 0x00);
    }

    #[test]
    fn parse_uuid_rejects_wrong_length() {
        assert!(parse_uuid("too-short").is_err());
        assert!(parse_uuid("550e8400-e29b-41d4-a716-44665544000").is_err());
    }

    #[test]
    fn parse_uuid_rejects_non_hex() {
        let s = "550e8400-e29b-41d4-a716-44665544XX00";
        assert!(parse_uuid(s).is_err());
    }

    // Tool-level integration smoke tests against an in-memory Drevo.
    // These exercise the full descriptor + call path without going
    // through the JSON-RPC layer; the JSON-RPC layer gets its own
    // tests in `super::server`.

    #[cfg(feature = "redb-backend")]
    #[test]
    fn health_check_against_in_memory_drevo() {
        let drevo = Drevo::open_in_memory().expect("open in-memory");
        let tool = HealthCheck;
        let result = tool.call(&drevo, &json!({})).expect("call");
        assert_eq!(result["healthy"], true);
    }

    #[cfg(feature = "redb-backend")]
    #[test]
    fn count_nodes_returns_zero_on_empty_drevo() {
        let drevo = Drevo::open_in_memory().expect("open in-memory");
        let tool = CountNodes;
        let result = tool.call(&drevo, &json!({})).expect("call");
        assert_eq!(result["count"], 0);
    }

    #[cfg(feature = "redb-backend")]
    #[test]
    fn count_nodes_counts_inserted_nodes() {
        use crate::model::NewNode;
        let drevo = Drevo::open_in_memory().expect("open in-memory");
        for i in 0..5 {
            drevo
                .create_node(NewNode {
                    kind: "test".to_string(),
                    title: format!("node-{i}"),
                    body: String::new(),
                    body_html: String::new(),
                    properties: Default::default(),
                })
                .expect("create");
        }
        let tool = CountNodes;
        let result = tool.call(&drevo, &json!({})).expect("call");
        assert_eq!(result["count"], 5);
    }

    #[cfg(feature = "redb-backend")]
    #[test]
    fn node_get_returns_null_when_missing() {
        let drevo = Drevo::open_in_memory().expect("open in-memory");
        let tool = NodeGet;
        let result = tool.call(&drevo, &json!({"id": 999_999})).expect("call");
        assert!(result["node"].is_null());
    }

    #[cfg(feature = "redb-backend")]
    #[test]
    fn node_get_invalid_params_returns_invalid_params_error() {
        let drevo = Drevo::open_in_memory().expect("open in-memory");
        let tool = NodeGet;
        let err = tool
            .call(&drevo, &json!({"wrong_key": 1}))
            .expect_err("must fail");
        assert_eq!(err.code, -32602, "missing `id` must be invalid-params");
    }

    #[cfg(feature = "redb-backend")]
    #[test]
    fn search_fts_returns_empty_results_for_no_match() {
        let drevo = Drevo::open_in_memory().expect("open in-memory");
        let tool = SearchFts;
        let result = tool
            .call(&drevo, &json!({"query": "no match here", "limit": 5}))
            .expect("call");
        let arr = result["results"].as_array().expect("results must be array");
        assert!(arr.is_empty());
    }

    #[cfg(feature = "redb-backend")]
    #[test]
    fn bfs_invalid_direction_is_invalid_params() {
        let drevo = Drevo::open_in_memory().expect("open in-memory");
        let tool = Bfs;
        let err = tool
            .call(&drevo, &json!({"start_id": 1, "direction": "sideways"}))
            .expect_err("must reject bad direction");
        assert_eq!(err.code, -32602);
    }

    // ── Python-API introspection tools ─────────────────────────────────
    //
    // These do not consult the handle, but the `Tool::call` contract
    // takes one, so we build a throwaway in-memory Drevo to drive them.

    #[cfg(feature = "redb-backend")]
    #[test]
    fn python_api_list_enumerates_and_filters() {
        let drevo = Drevo::open_in_memory().expect("open in-memory");
        let tool = PythonApiList;

        // Empty prefix → the whole surface.
        let all = tool.call(&drevo, &json!({})).expect("call");
        let count = all["count"].as_u64().expect("count");
        assert!(count >= 30, "expected the full surface, got {count}");
        let symbols = all["symbols"].as_array().expect("symbols array");
        assert_eq!(symbols.len() as u64, count);
        // Each entry carries name + kind + signature.
        let first = &symbols[0];
        assert!(first["name"].is_string());
        assert!(first["kind"].is_string());
        assert!(first["signature"].is_string());

        // Prefix filter narrows to a namespace.
        let rag = tool
            .call(&drevo, &json!({"prefix": "drevo.rag"}))
            .expect("call");
        let rag_syms = rag["symbols"].as_array().expect("array");
        assert!(!rag_syms.is_empty());
        assert!(rag_syms
            .iter()
            .all(|s| s["name"].as_str().unwrap().starts_with("drevo.rag")));
    }

    #[cfg(feature = "redb-backend")]
    #[test]
    fn python_api_describe_hit_and_miss() {
        let drevo = Drevo::open_in_memory().expect("open in-memory");
        let tool = PythonApiDescribe;

        let hit = tool
            .call(&drevo, &json!({"name": "create_node"}))
            .expect("call");
        assert_eq!(hit["found"], true);
        assert_eq!(hit["symbol"]["name"], "drevo.Drevo.create_node");
        assert_eq!(hit["symbol"]["kind"], "method");
        assert!(hit["symbol"]["signature"]
            .as_str()
            .unwrap()
            .starts_with("def create_node("));

        let miss = tool
            .call(&drevo, &json!({"name": "create_nodez"}))
            .expect("call");
        assert_eq!(miss["found"], false);
        assert!(miss["suggestions"].is_array());
    }

    #[cfg(feature = "redb-backend")]
    #[test]
    fn python_api_describe_missing_name_is_invalid_params() {
        let drevo = Drevo::open_in_memory().expect("open in-memory");
        let tool = PythonApiDescribe;
        let err = tool
            .call(&drevo, &json!({"wrong": "x"}))
            .expect_err("must fail");
        assert_eq!(err.code, -32602);
    }

    #[cfg(feature = "redb-backend")]
    #[test]
    fn python_api_examples_searches_by_intent() {
        let drevo = Drevo::open_in_memory().expect("open in-memory");
        let tool = PythonApiExamples;
        let result = tool
            .call(&drevo, &json!({"intent": "create a node", "limit": 3}))
            .expect("call");
        let examples = result["examples"].as_array().expect("examples array");
        assert!(!examples.is_empty(), "intent should match a README example");
        let top = &examples[0];
        assert!(top["code"].as_str().unwrap().contains("create_node"));
        assert!(top["source"].as_str().unwrap().ends_with("README.md"));
    }
}
