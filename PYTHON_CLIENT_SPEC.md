# GrapeVine Python Client — Specification

> **Repository**: separate repo (`grapevine-py` or `grapevine-client-python`)
> **Status**: specification draft — to be reviewed and updated after every GrapeVine task/subtask
> **Last reviewed**: 2026-02-20, pre-development phase

## 1. Overview

A Python client library for the GrapeVine graph+vector database. Communicates with GrapeVine server via HTTP REST API. Distributed as a PyPI package.

```python
from grapevine import GrapeVineClient

db = GrapeVineClient("http://localhost:8080")

# Nodes
db.insert_node(1, labels=["server"], props={"name": "web-01"}, embedding=[0.1, 0.2, 0.3])
db.insert_node(2, labels=["server"], props={"name": "db-01"}, embedding=[0.4, 0.5, 0.6])
node = db.get_node(1)

# Edges
db.insert_edge(1, "DEPENDS_ON", 2, props={"weight": 1.0})

# Graph queries
neighbors = db.neighbors(1, depth=2)
path = db.shortest_path(1, 2)

# Vector queries
similar = db.similar([0.11, 0.19, 0.31], limit=5)

# Combined queries
results = db.similar_neighbors(1, depth=2, vector=[0.11, 0.19, 0.31], limit=3)
```

## 2. Target Audience

- Python developers using GrapeVine as a backend service (Docker container)
- Data scientists working with graph + embedding workflows
- RAG pipelines needing graph-aware vector search

## 3. Technical Stack

| Component | Choice | Rationale |
|-----------|--------|-----------|
| HTTP client | `httpx` | Async support, modern API, typed |
| Serialization | JSON (stdlib) | Matches GrapeVine HTTP API format |
| Type hints | Full typing + `dataclasses` | IDE support, validation |
| Python version | >= 3.10 | Modern syntax, `match`, `|` union types |
| Build system | `hatch` / `hatchling` | PEP 517 compliant, modern |
| Testing | `pytest` + `pytest-asyncio` | Standard, async support |
| Linting | `ruff` | Fast, replaces flake8+isort+black |

## 4. Package Structure

```
grapevine-py/
├── pyproject.toml
├── src/
│   └── grapevine/
│       ├── __init__.py          # Public API re-exports
│       ├── client.py            # GrapeVineClient (sync)
│       ├── async_client.py      # AsyncGrapeVineClient
│       ├── models.py            # Node, Edge, SearchResult, Path dataclasses
│       ├── exceptions.py        # GrapeVineError, NodeNotFound, etc.
│       └── _http.py             # Low-level HTTP transport
├── tests/
│   ├── conftest.py              # Fixtures (Docker container, test data)
│   ├── test_nodes.py
│   ├── test_edges.py
│   ├── test_graph.py
│   ├── test_vector.py
│   ├── test_combined.py
│   └── test_async.py
└── examples/
    ├── quickstart.py
    ├── rag_pipeline.py
    └── knowledge_graph.py
```

## 5. Data Models

### 5.1 Node

```python
@dataclass
class Node:
    id: int
    labels: list[str]
    properties: dict[str, str | int | float | bool]
    embedding: list[float] | None = None
```

### 5.2 Edge

```python
@dataclass
class Edge:
    src: int
    dst: int
    edge_type: str
    properties: dict[str, str | int | float | bool]
```

### 5.3 SearchResult

```python
@dataclass
class SearchResult:
    node: Node
    score: float          # distance/similarity score
```

### 5.4 Path

```python
@dataclass
class Path:
    nodes: list[int]      # ordered node IDs
    edges: list[Edge]     # edges connecting them
    length: int
```

## 6. API Surface

### 6.1 Node Operations

| Method | HTTP | Endpoint | Returns |
|--------|------|----------|---------|
| `insert_node(id, labels, props, embedding?)` | `POST` | `/nodes` | `Node` |
| `get_node(id)` | `GET` | `/nodes/{id}` | `Node` |
| `update_node(id, labels?, props?, embedding?)` | `PATCH` | `/nodes/{id}` | `Node` |
| `delete_node(id)` | `DELETE` | `/nodes/{id}` | `None` |

### 6.2 Edge Operations

| Method | HTTP | Endpoint | Returns |
|--------|------|----------|---------|
| `insert_edge(src, edge_type, dst, props?)` | `POST` | `/edges` | `Edge` |
| `get_edges(node_id, direction?, edge_type?)` | `GET` | `/nodes/{id}/edges` | `list[Edge]` |
| `delete_edge(src, edge_type, dst)` | `DELETE` | `/edges/{src}/{type}/{dst}` | `None` |

### 6.3 Graph Traversal

| Method | HTTP | Endpoint | Returns |
|--------|------|----------|---------|
| `neighbors(node_id, depth, edge_type?)` | `GET` | `/nodes/{id}/neighbors` | `list[Node]` |
| `shortest_path(src, dst)` | `GET` | `/paths/shortest` | `Path` |
| `subgraph(node_id, depth)` | `GET` | `/nodes/{id}/subgraph` | `dict` (nodes + edges) |

### 6.4 Vector Search

| Method | HTTP | Endpoint | Returns |
|--------|------|----------|---------|
| `similar(vector, limit, metric?)` | `POST` | `/search/similar` | `list[SearchResult]` |
| `similar_neighbors(node_id, depth, vector, limit)` | `POST` | `/search/similar_neighbors` | `list[SearchResult]` |
| `subgraph_similar(node_id, depth, limit)` | `POST` | `/search/subgraph_similar` | `list[SearchResult]` |

### 6.5 Admin

| Method | HTTP | Endpoint | Returns |
|--------|------|----------|---------|
| `status()` | `GET` | `/status` | `dict` (counts, uptime) |
| `health()` | `GET` | `/health` | `bool` |

## 7. HTTP API Contract (GrapeVine Server Side)

> This section defines the HTTP API that GrapeVine server must implement. It serves as a contract between the server (Rust) and the client (Python).

### 7.1 Common Conventions

- Content-Type: `application/json`
- Errors: `{"error": "<code>", "message": "<human-readable>"}`
- HTTP status codes: 200 (OK), 201 (Created), 404 (Not Found), 400 (Bad Request), 500 (Internal Error)
- All IDs are integers (u64)
- Embeddings are arrays of f32

### 7.2 Request/Response Examples

#### Insert Node
```
POST /nodes
{
    "id": 1,
    "labels": ["server"],
    "properties": {"name": "web-01", "cpu": 4},
    "embedding": [0.1, 0.2, 0.3]
}
→ 201 { "id": 1, "labels": [...], "properties": {...}, "embedding": [...] }
```

#### Get Node
```
GET /nodes/1
→ 200 { "id": 1, "labels": [...], "properties": {...}, "embedding": [...] }
→ 404 { "error": "node_not_found", "message": "Node 1 not found" }
```

#### Insert Edge
```
POST /edges
{
    "src": 1,
    "edge_type": "DEPENDS_ON",
    "dst": 2,
    "properties": {"weight": 1.0}
}
→ 201 { "src": 1, "dst": 2, "edge_type": "DEPENDS_ON", "properties": {...} }
```

#### Vector Search
```
POST /search/similar
{
    "vector": [0.11, 0.19, 0.31],
    "limit": 5,
    "metric": "cosine"
}
→ 200 { "results": [{"node": {...}, "score": 0.95}, ...] }
```

#### Similar Neighbors (combined query)
```
POST /search/similar_neighbors
{
    "node_id": 1,
    "depth": 2,
    "vector": [0.11, 0.19, 0.31],
    "limit": 3
}
→ 200 { "results": [{"node": {...}, "score": 0.92}, ...] }
```

## 8. Error Handling

```python
class GrapeVineError(Exception):
    """Base exception"""
    status_code: int
    error_code: str
    message: str

class NodeNotFoundError(GrapeVineError): ...
class EdgeNotFoundError(GrapeVineError): ...
class InvalidEmbeddingError(GrapeVineError): ...
class ConnectionError(GrapeVineError): ...
class ServerError(GrapeVineError): ...
```

## 9. Async Support

Both sync and async interfaces:

```python
# Sync
from grapevine import GrapeVineClient
db = GrapeVineClient("http://localhost:8080")
node = db.get_node(1)

# Async
from grapevine import AsyncGrapeVineClient
db = AsyncGrapeVineClient("http://localhost:8080")
node = await db.get_node(1)
```

## 10. Docker Integration

The Python client is designed to work with GrapeVine running in Docker:

```python
# docker-compose.yml runs GrapeVine on port 8080
db = GrapeVineClient("http://localhost:8080")

# Or with custom Docker network
db = GrapeVineClient("http://grapevine:8080")
```

Test fixtures use `testcontainers-python` or `docker` SDK to spin up GrapeVine for integration tests:

```python
@pytest.fixture(scope="session")
def grapevine_server():
    container = docker.from_env().containers.run(
        "grapevine:latest",
        ports={"8080/tcp": None},
        detach=True,
    )
    port = container.ports["8080/tcp"][0]["HostPort"]
    yield f"http://localhost:{port}"
    container.stop()
    container.remove()
```

## 11. Versioning and Compatibility

- Client version follows server API version (major.minor)
- Client MUST validate server version on connect via `GET /status`
- Backward-compatible: client v1.1 works with server v1.0 and v1.1
- Breaking changes: major version bump on both sides

## 12. Spec Review Protocol

> **CRITICAL**: This specification MUST be reviewed and updated after every GrapeVine development session.

After completing any task or subtask in the GrapeVine server:

1. **Check API impact** — does the completed task affect the HTTP API contract?
2. **Update models** — if server-side types changed, update Section 5 (Data Models)
3. **Update endpoints** — if new functionality was added, update Section 6-7
4. **Update error codes** — if new error types were added, update Section 8
5. **Add changelog entry** — record what changed and why (Section 13)
6. **Flag inconsistencies** — if server deviates from this spec, flag it explicitly

## 13. Changelog

| Date | Task | Change | Rationale |
|------|------|--------|-----------|
| 2026-02-20 | — | Initial specification created | Pre-development planning |

---

> **Note**: This spec will evolve as the GrapeVine server is developed. Each Phase completion should trigger a thorough review of this document.
