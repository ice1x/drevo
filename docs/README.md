# drevo Documentation

**drevo** is an embeddable, cross-platform **graph–vector database** with a Neo4j-compatible
Cypher query layer, a Bolt wire protocol, full-text and vector search, and first-class Rust,
Python, HTTP, and MCP interfaces. It runs as a single binary, in-process as a library, or
compiled to WebAssembly — no external services required.

These guides are the user-facing companion to the design-oriented
[project README](../README.md). They are organised by audience and task.

## Guides

| Guide | Read it when you want to… |
|-------|---------------------------|
| [User Guide](user-guide.md) | Understand the data model and run your first queries — concepts, the five target scenarios, and end-to-end workflows. |
| [Cypher Reference](cypher-reference.md) | Look up the exact Cypher subset drevo supports. Every example is executed as a test. |
| [SDK Reference](sdk-reference.md) | Call drevo from code — the Rust API, the Python SDK (incl. graph-RAG helpers), the HTTP API, the Bolt protocol, and the MCP tools. |
| [Admin Guide](admin-guide.md) | Deploy and operate drevo — Docker, Kubernetes, configuration, observability, backups, auth, replication, and streaming ingestion. |
| [Migration Guide](migration-guide.md) | Move an existing Neo4j database into drevo, and understand the Neo4j-compatibility surface. |

## The 30-second tour

drevo stores a property graph: **nodes** (each with a `kind`, a unique `title`, a Markdown
`body`, and arbitrary JSON `properties`) connected by directed, weighted **edges** (each with
a `kind` and JSON `properties`). On top of that model it layers:

- **Cypher** — `MATCH` / `CREATE` / `MERGE` / `SET` / `DELETE`, aggregation, variable-length
  paths, plus the drevo extensions `keywords()` and `similar()`. See the
  [Cypher Reference](cypher-reference.md).
- **Traversal** — BFS, DFS, weighted shortest path (Dijkstra), bounded subgraph extraction.
- **Full-text search** — trigram index with Okapi BM25 ranking, faceting, keyword extraction.
- **Vector search** — per-node embeddings with cosine / Euclidean / dot-product distance and
  an HNSW index for approximate nearest-neighbour queries.
- **Graph analytics** — PageRank and Louvain community detection over the whole graph.

## Target scenarios

drevo is designed around five concrete graph-notebook scenarios, used throughout these docs
and the test suite:

1. **CBT journal** — thoughts, moods, cognitive distortions, and the links between them.
2. **Story / book editor** — chapters, scenes, characters, and narrative flow.
3. **IT task manager** — tasks, people, dependencies, and assignments.
4. **ERP system** — orders, products, customers, departments.
5. **Bug tracker** — issues, severities, components, and developers.

Start with the [User Guide](user-guide.md).
