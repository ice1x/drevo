# drevo-mcp

An **external [FastMCP](https://github.com/jlowin/fastmcp) server** that exposes a
running [`drevo-server`](../../README.md) as Model Context Protocol tools for
Claude Desktop, Claude Code, Cline, and other MCP clients.

Unlike the in-tree Rust `drevo-mcp` binary (task `00090`), this server **talks to
drevo over HTTP** and **never opens the redb file**. So it does not fight the
server for redb's single-process file lock — the container owns the file, this
process is just an HTTP client. That makes it trivial to debug: every tool maps to
an endpoint you can hit with `curl`.

```
MCP client  ──stdio(MCP)──▶  drevo-mcp (this)  ──HTTP──▶  drevo-server (container)  ──▶  drevo.redb
```

## Install

```bash
pip install -e tools/drevo-mcp          # from the repo root
```

## Run

Point it at a running `drevo-server` (see the repo README "Quick Start —
Container + External MCP" for bringing the container up):

```bash
export DREVO_HTTP_URL=http://localhost:8080   # default; override if elsewhere
python -m drevo_mcp                            # speaks MCP over stdio
```

Smoke-test the wire without an MCP client:

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  | python -m drevo_mcp
```

## Connect an MCP client

Add to the client's MCP config (e.g. `claude_desktop_config.json`):

```json
"drevo": {
  "command": "python",
  "args": ["-m", "drevo_mcp"],
  "env": { "DREVO_HTTP_URL": "http://localhost:8080" }
}
```

## Tools (read-only)

| Tool | drevo endpoint |
|------|----------------|
| `health` | `GET /health` |
| `node_get(node_id)` | `GET /nodes/{id}` |
| `list_nodes_by_kind(kind, limit, offset)` | `GET /nodes?kind=` |
| `search_fts(query, limit)` | `POST /search/fts` |
| `neighbors(node_id, direction, kind, depth)` | `GET /nodes/{id}/neighbors` |
| `subgraph(node_id, depth)` | `GET /nodes/{id}/subgraph` |
| `shortest_path(from_id, to_id)` | `GET /paths/shortest` |
| `count_nodes` | `GET /export/json` (counts the dump — no count endpoint yet) |

## Develop / test

```bash
cd tools/drevo-mcp
pip install -e ".[dev]"
pytest
mypy --strict drevo_mcp/
ruff check . && black --check .
```
