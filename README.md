# 🎄 drevo — Embedded Graph Database for Knowledge Management

[![CI](https://github.com/ice1x/drevo/actions/workflows/ci.yml/badge.svg)](https://github.com/ice1x/drevo/actions/workflows/ci.yml)
[![Container image](https://img.shields.io/docker/v/ice1x/drevo?sort=semver&logo=docker&logoColor=white&label=docker%20hub)](https://hub.docker.com/r/ice1x/drevo)
[![Release](https://img.shields.io/github/v/tag/ice1x/drevo?sort=semver&label=release)](https://github.com/ice1x/drevo/tags)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.85-orange?logo=rust&logoColor=white)](Cargo.toml)
[![ACID](https://img.shields.io/badge/ACID-A·C·I·D%20verified-2e7d32)](#acid-transactions)
[![Agent memory](https://img.shields.io/badge/agent_memory-HTTP_%7C_Bolt_%7C_MCP-8A2BE2)](#agent-memory-graph)

A lightweight, embeddable graph database written in Rust, with **full ACID transactions** (MVCC snapshot isolation + a write-ahead log). Designed as the storage engine for cross-platform knowledge-base applications (similar to Obsidian), drevo runs natively on desktop (via FFI/Tauri), mobile (iOS/Android via C bindings), and in the browser (via WebAssembly). It also ships as a standalone HTTP server for containerised deployments.

**Drop-in memory backend for AI agents** (LangGraph, Claude Code, CrewAI, Cursor, multi-agent swarms) over HTTP / Bolt / MCP — persistent recall that survives sessions, agents, and machines. See the [Agent Memory Graph](#agent-memory-graph) use case.

> **📖 Documentation lives in [`docs/`](docs/)** and on the [docs site](https://ice1x.github.io/drevo/). See the [Documentation](#documentation) map below.

**Run it in a container** — HTTP API + embedded Web UI on `:8080`, Neo4j-compatible Bolt on `:7687`:

```bash
# Published image (Docker Hub badge above):
docker run -d --name drevo -p 8080:8080 -p 7687:7687 -v drevo-data:/data ice1x/drevo:latest

# …or build it from this repo (no registry access needed):
docker build -t drevo . && \
  docker run -d --name drevo -p 8080:8080 -p 7687:7687 -v drevo-data:/data drevo
```

Then open <http://localhost:8080/ui>. Full deploy options — Compose, host bind-mounts, custom Bolt port, Kubernetes — are in the [Admin Guide](docs/admin-guide.md#3-docker).

---

## Use Cases

drevo is the storage engine for a cross-platform graph notebook. Target scenarios:

### CBT Journal (Cognitive Behavioral Therapy)

Nodes: `thought`, `emotion`, `situation`, `cognitive_distortion`, `rational_response`. Edges: `triggered_by`, `leads_to`, `challenges`, `reframed_as`. The graph enables tracing chains of thoughts and finding recurring distortion patterns via traversal.

### Scenario / Book / Story Editor

Tree-structured narratives: nodes are `chapter`, `scene`, `character`, `location`, `plot_point`. Edges: `contains`, `follows`, `involves`, `takes_place_in`. Subgraph extraction gives a complete context for a scene. MCP integration allows AI agents to read/write the graph for co-authoring.

### IT Task Manager

Nodes: `task`, `epic`, `sprint`, `developer`, `component`. Edges: `assigned_to`, `blocks`, `part_of`, `depends_on`. BFS from a blocked task reveals the full dependency chain. Kind index enables board views (all tasks in a sprint).

### ERP System

Nodes: `order`, `product`, `customer`, `warehouse`, `invoice`. Edges: `ordered_by`, `contains`, `stored_in`, `billed_to`. Transactions ensure consistency when updating order status and inventory simultaneously.

### Bug Tracker / Control System

Nodes: `bug`, `feature`, `release`, `test_case`, `assignee`. Edges: `reported_in`, `fixed_by`, `verified_by`, `blocks_release`. FTS over bug descriptions, traversal for impact analysis.

### Agent Memory Graph

A persistent, queryable **memory backend for agent orchestrators** (LangGraph, Temporal, Claude Code, Cursor, CrewAI, multi-agent swarms) that survives across sessions, agents, and machines. Nodes: `agent`, `session`, `observation`, `decision`, `task`, `artifact`, `fact`, `preference`, `tool_call`. Edges: `observed_by`, `decided_by`, `performed_in_session`, `derived_from`, `contradicts`, `supersedes`, `references_artifact`, `belongs_to_task`, `honours_preference`, `produced_by_tool`.

The orchestrator hot path maps directly onto drevo's existing surface — no new engine features needed:

- **record_observation** → `create_node` + `create_edge`
- **recall** (FTS + kind filter, or `CALL fts.search(query, k)` over Bolt) → BM25 ranking
- **context_subgraph** (a bounded window for an LLM prompt) → `subgraph(id, depth)`
- **supersede** (mark memory stale) → a `supersedes` edge
- **compact** (prune low-confidence observations) → scan by kind + `delete_node`

The full flow is pinned by [`tests/scenario_agent_memory.rs`](tests/scenario_agent_memory.rs).

**Why drevo over the usual options** — its differentiator is hitting all five at once:

| | Embedded | Cross-platform | Graph-native | Built-in FTS | No external deps |
|---|:---:|:---:|:---:|:---:|:---:|
| Flat markdown (`MEMORY.md`) | ✅ | ✅ | ❌ | ❌ | ✅ |
| Vector DB (Chroma/Qdrant) | ~ | ~ | ❌ | ~ | ❌ |
| SQLite ad-hoc schema | ✅ | ✅ | ❌ | ~ | ✅ |
| **drevo** | ✅ | ✅ | ✅ | ✅ | ✅ |

Because drevo runs embedded on Linux/macOS/Windows, iOS/Android, WASM, and as a Docker/HTTP service, a **single memory graph file can travel** between a local Claude Code session, a cloud orchestrator run, and a mobile app — synced by file copy or over Bolt/HTTP.

> An agent-memory MCP server that exposes this surface to MCP-capable orchestrators lives in the separate [`ice1x/drevo-mcp`](https://github.com/ice1x/drevo-mcp) repo, not here.

### Common patterns across all scenarios

- **Node kinds** define domain entities — the `kind` field + `kind_index` provide filtered views
- **Edge kinds** define relationships — `scan_prefix` retrieves all edges of a given type
- **Properties** (HashMap) store domain-specific metadata without schema migration
- **FTS** enables search across all content (titles, bodies, properties)
- **Subgraph** extraction provides bounded context for AI agents (MCP)
- **Transactions** ensure consistency for multi-step operations
- **Cross-platform**: all scenarios must work identically on desktop, mobile (iOS/Android), and WASM

---

## Documentation

User- and operator-facing guides live in [`docs/`](docs/) and render on the [docs site](https://ice1x.github.io/drevo/). Design detail and the build history were moved out of this README to keep it lean:

| Doc | What it covers |
|---|---|
| [User Guide](docs/user-guide.md) | Getting started, core concepts, everyday queries |
| [Cypher Reference](docs/cypher-reference.md) | The supported Cypher surface (clauses, functions, procedures) |
| [SDK Reference](docs/sdk-reference.md) | The Python Graph-RAG SDK (`drevo-py`) |
| [Admin Guide](docs/admin-guide.md) | Deployment — Docker, Compose, Kubernetes — and operations |
| [Migration Guide](docs/migration-guide.md) | Importing an existing Neo4j graph into drevo |
| [Architecture & Design](docs/architecture.md) | Vision, requirements, data model, storage engine, Rust API surface, serialization, error handling, performance targets, crate layout, dependencies |
| [Benchmarks](docs/benchmarks.md) · [Native-core baseline](docs/native-core-baseline.md) · [Native load & concurrency](docs/native-load.md) | Measured performance, engine baselines, load/concurrency behaviour |
| [Adjacency Key Schema](docs/adjacency-key-schema.md) · [RFC: Native Graph Core](docs/rfc-native-core.md) | Internal storage schema and the native-engine design RFC |
| [Contributing](docs/contributing.md) | Coding conventions and the agent working model |
| [**Phase History**](PHASE-HISTORY.md) | Frozen log of completed phases (PoC → Phase 21, plus the Phase 8.5 audit) — the detailed build narrative that used to live here |

---

## Project Status

drevo is in active use. The default deployment engine is **`native-durable`** — an in-memory graph with a JSON-Lines write-ahead log as the store of record (zero redb); the original **redb** key-value backend stays selectable via `DREVO_ENGINE=kv`. Both engines serve the same HTTP / Bolt / Cypher / Web-UI surface.

- **Releases:** see [tags](https://github.com/ice1x/drevo/tags) and the [Docker Hub image](https://hub.docker.com/r/ice1x/drevo) (`ice1x/drevo`).
- **What's built:** the full history — engine, Cypher, Bolt, vectors, MVCC, Python SDK, semantic index, native core — is recorded in [`PHASE-HISTORY.md`](PHASE-HISTORY.md).
- **What's next / open work:** tracked in [GitHub issues](https://github.com/ice1x/drevo/issues), not in this file.

### ACID transactions

The default engine is fully **ACID**, and each property is proven by a dedicated Rust conformance suite that drives the engine directly (no HTTP layer):

| Property | Guarantee | Conformance suite |
|---|---|---|
| **A** — Atomicity | commit/rollback are all-or-nothing; a torn commit batch replays whole or not at all | [`drevo-core/tests/acid_atomicity.rs`](drevo-core/tests/acid_atomicity.rs) |
| **C** — Consistency | UNIQUE / property-EXISTS / NODE-KEY constraints + structural invariants enforced at commit; a violating tx aborts atomically | [`drevo-core/tests/acid_consistency.rs`](drevo-core/tests/acid_consistency.rs) |
| **I** — Isolation | MVCC snapshot isolation — no dirty read, repeatable read, write–write conflict detection | [`drevo-core/tests/acid_isolation.rs`](drevo-core/tests/acid_isolation.rs) |
| **D** — Durability | acknowledged writes are fsync'd before return and survive a crash; replay is idempotent; ids stay monotonic | [`drevo-core/tests/acid_durability.rs`](drevo-core/tests/acid_durability.rs) |

The model is described in [`docs/architecture.md`](docs/architecture.md) and [`docs/rfc-native-core.md`](docs/rfc-native-core.md).

---

## Quick Start — Container + External MCP (FastMCP)

Run drevo as a **server in a container** (the single owner of the redb file, serving
the HTTP API **and** the Web UI) and attach the **MCP server** — a *separate process*
that talks to the container over HTTP / Bolt — so an AI client can read the graph in
conversation. Because the MCP server never opens the redb file, it never fights the
container for redb's single-process lock; the Web UI and the MCP server query the same
data at once. The MCP server is maintained in its own repository:
**[github.com/ice1x/drevo-mcp](https://github.com/ice1x/drevo-mcp)**.

### 1. Bring up the container

```bash
# Put your redb file in a host folder, or point DREVO_DATA_DIR at an existing one.
mkdir -p ./data && cp /path/to/drevo.redb ./data/      # or: export DREVO_DATA_DIR=~/drevo_data

# Run as your own user so the container can take redb's write lock on the host folder.
DREVO_UID=$(id -u) DREVO_GID=$(id -g) \
  DREVO_DATA_DIR=${DREVO_DATA_DIR:-./data} docker compose up -d --build

curl -s localhost:8080/health      # {"status":"ok"}
open http://localhost:8080/ui      # the embedded graph Web UI (served by default)
```

Equivalent plain `docker run` (also enables the Neo4j-compatible Bolt listener — see
the Bolt drop-in MCP below — and survives reboots):

```bash
docker build -t drevo:latest .     # or: docker pull ice1x/drevo
docker run -d --name drevo --restart unless-stopped \
  --user "$(id -u):$(id -g)" \
  -p 8080:8080 -p 7688:7687 \      # host 7688 → container Bolt 7687 (7687 often taken by Neo4j)
  -v "$HOME/drevo_data:/data" \
  -e DREVO_BOLT_PORT=7687 \
  drevo:latest
```

The container bind-mounts the host folder to `/data` and serves `<folder>/drevo.redb`.
The Web UI is **fully self-contained** — Cytoscape.js is vendored and served from
`/ui/vendor/`, so the graph renders with no CDN access (works offline / behind privacy
browsers). Type a query → **Search** → click a result to draw its 2-hop subgraph.

### 2. Attach the MCP server

The MCP server lives in its own repository —
**[github.com/ice1x/drevo-mcp](https://github.com/ice1x/drevo-mcp)** — and offers
both an HTTP client (read-only tools mapped to `src/api.rs` endpoints) and a
Neo4j-compatible Bolt drop-in (the same knowledge-graph tool surface as a Neo4j
MCP, pointed at the container's Bolt port). Follow that repo's README to install
it and register the connector in your client's MCP config (Claude Desktop, the
Claude Code CLI `~/.claude/settings.json`, or Cline), pointing it at this
container:

- HTTP transport → `DREVO_HTTP_URL=http://localhost:8080`
- Bolt transport → `DREVO_BOLT_URL=bolt://localhost:7688` (host `:7688` in the `docker run` above)

Either transport holds **no** redb lock — the container's `drevo-server` is the
single file owner — so the Web UI and the MCP server run against the same data at
once.

---

## MCP Server

The [Model Context Protocol](https://modelcontextprotocol.io) server for drevo
lives in a **separate repository**:
**[github.com/ice1x/drevo-mcp](https://github.com/ice1x/drevo-mcp)** — *Drevo
Graph MCP*.

It runs as its own process and talks to a running `drevo-server` over HTTP / the
Neo4j-compatible Bolt port, so it **never opens the redb file** and never
contends for redb's single-process lock (the Web UI and the MCP server query the
same data at once). Bring drevo up with the container quick-start above, then
follow the MCP repo's README to connect Claude Desktop / Claude Code / Cline.

> History: an *embedded* `drevo-mcp` stdio binary (former tasks `00090` /
> `00121`) once shipped in this repo (`src/mcp/`, `src/bin/mcp.rs`). It opened
> the redb file in-process and therefore could not run alongside `drevo-server`
> on the same file — so it was removed in favour of the out-of-process MCP above.
> Likewise the in-tree helper packages `tools/drevo-mcp` (HTTP) and
> `tools/drevo-mcp-bolt` (Bolt) were folded into that external repository.

---

## License

MIT
