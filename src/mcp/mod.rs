//! Model Context Protocol (MCP) server for an embedded [`crate::db::Drevo`]
//! handle.
//!
//! MCP is a JSON-RPC 2.0 protocol over stdio used by AI clients (Cline,
//! Claude Code, Claude Desktop) to discover and call tools exposed by a
//! server process. This module ships the protocol layer, the tool
//! registry, and the stdio dispatch loop used by the `drevo-mcp`
//! binary (Phase 15 task `00090`).
//!
//! ## Why a custom implementation
//!
//! The protocol surface we actually need is small: `initialize`,
//! `tools/list`, `tools/call`, `ping`, `notifications/initialized`,
//! plus the JSON-RPC error envelope. Pulling in a third-party MCP SDK
//! would add ~5 transitive dependencies for code we can write in under
//! ~600 LOC against `serde_json` (already a dep). Pure Rust, no extra
//! supply-chain surface, no version-pin churn.
//!
//! ## Module layout
//!
//! - [`crate::mcp::protocol`] — JSON-RPC 2.0 envelope + MCP message
//!   types (`InitializeResult`, `Tool`, `ToolCallResult`, …) with
//!   serde round-trip tests.
//! - [`crate::mcp::tools`] — the [`crate::mcp::tools::Tool`] trait,
//!   the [`crate::mcp::tools::ToolRegistry`], and the seven baseline
//!   tools that wrap [`crate::db::Drevo`] (`drevo_health_check`,
//!   `drevo_count_nodes`, `drevo_node_get`, `drevo_node_get_by_uuid`,
//!   `drevo_search_fts`, `drevo_bfs`, `drevo_list_nodes_by_kind`).
//! - [`crate::mcp::server`] — the sync stdio dispatcher. Reads one
//!   JSON object per line, dispatches via the registry, writes one
//!   JSON object per line. EOF on stdin = graceful shutdown.
//!
//! ## What's intentionally out of scope for task `00090`
//!
//! - `resources/*`, `prompts/*`, `sampling/*`, `logging/*` — declared
//!   unsupported in the `initialize` response. Future tasks can add
//!   these if the use case appears.
//! - Pagination on `tools/list` — the server returns the full list
//!   in one response. The MCP spec permits this when no `cursor` is
//!   sent in the request.
//! - Progress notifications, cancellation — also future work.
//! - Per-tool authorisation — embedded mode trusts the local client.
//!   When the MCP server eventually serves remote clients, an auth
//!   layer lands in a separate task.
//!
//! ## E2E suite — task `00091`
//!
//! The MCP validation E2E suite (Phase 15 task `00091`) lives in
//! `tests/drevo_mcp_e2e_tests.rs` and exercises every baseline tool
//! through the actual stdio protocol. This module ships the bare
//! minimum surface that `00091` needs (count / labels / rels /
//! traversal / properties).

pub mod protocol;
pub mod server;
pub mod tools;

pub use protocol::{
    JsonRpcError, JsonRpcId, JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, McpServerInfo,
    Tool as ToolMeta, ToolCallContent, ToolCallResult,
};
pub use server::Server;
pub use tools::{Tool, ToolRegistry};
