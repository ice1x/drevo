# User Guide

This guide gets you from zero to a working graph. It covers drevo's data model, the ways you
can talk to it, and end-to-end workflows for each of the five target scenarios. For the exact
query language see the [Cypher Reference](cypher-reference.md); for API signatures see the
[SDK Reference](sdk-reference.md).

---

## 1. The data model

drevo stores a **directed property graph**.

### Nodes

A node is an entity. Every node has:

| Field | Meaning |
|-------|---------|
| `id` | Auto-increment `u64`, unique within the database. |
| `uuid` | A UUIDv7, globally unique and time-sortable. |
| `kind` | A classification string — the Cypher *label* (`Task`, `Person`, `Thought`…). |
| `title` | A human-readable name. **Globally unique** — drevo rejects a duplicate title with a conflict error. |
| `body` | Free-form Markdown. |
| `body_html` | Cached rendered HTML of `body`. |
| `created_at` / `updated_at` | Unix-millisecond timestamps. |
| `properties` | An arbitrary JSON object (`HashMap<String, serde_json::Value>`). |

> **The unique-title rule matters.** It is what lets you address a node by name and what keeps
> imports idempotent. When you don't have a natural unique name (e.g. anonymous nodes created
> in Cypher), drevo synthesises a unique placeholder title for you.

### Edges

An edge is a directed, weighted relationship:

| Field | Meaning |
|-------|---------|
| `id`, `uuid` | Identity, as for nodes. |
| `from_id` / `to_id` | The endpoints. |
| `kind` | The relationship type (Cypher `:TYPE`). |
| `weight` | An `f32` link strength (default `1.0`); used by shortest-path and PageRank. |
| `properties` | Arbitrary JSON. |

### Cypher labels and `kind`

A Cypher label maps onto a node's `kind`. drevo also supports **secondary labels**: `SET n:Extra`
stores additional labels in a reserved `_labels` property, and `MATCH (n:A:B)` matches a node
carrying *any* of the listed labels.

---

## 2. Ways to talk to drevo

drevo is the same engine no matter how you reach it:

- **In-process library** — link the `drevo` crate (Rust) or `import drevo` (Python) and call
  the [API](sdk-reference.md) directly. Zero network, embeddable, WASM-capable.
- **Cypher** — over the [Bolt protocol](sdk-reference.md#bolt-protocol) (port `7687`, works with
  official Neo4j drivers) or in-process via `parse` + `execute`.
- **HTTP API** — a REST surface on port `8080` for CRUD, traversal, search, and import/export.
- **Web UI** — a Cytoscape.js graph explorer at `/ui` on the HTTP server.
- **MCP** — the `drevo-mcp` stdio server exposes graph tools to AI agents (Claude Code, Cline).

Pick the smallest one that fits: a notebook app embeds the library; a migration script speaks
Cypher over Bolt; an AI agent uses MCP.

---

## 3. Your first graph (Python)

```python
import drevo

with drevo.Drevo.open_in_memory() as db:
    alice = db.create_node(drevo.NewNode(kind="Person", title="Alice"))
    task = db.create_node(drevo.NewNode(
        kind="Task",
        title="Ship docs",
        properties={"status": "pending", "priority": 3},
    ))
    db.create_edge(drevo.NewEdge(from_id=alice.id, to_id=task.id, kind="ASSIGNED_TO"))

    # Who is assigned what?
    for node in db.bfs(alice.id, max_depth=1, direction=drevo.Direction.OUT):
        print(node.kind, node.title)
```

The same in Rust:

```rust
use drevo::db::Drevo;
use drevo::model::{NewNode, NewEdge};

let db = Drevo::open_in_memory()?;
let alice = db.create_node(NewNode { kind: "Person".into(), title: "Alice".into(), ..Default::default() })?;
let task  = db.create_node(NewNode { kind: "Task".into(),   title: "Ship docs".into(), ..Default::default() })?;
db.create_edge(NewEdge { from_id: alice.id, to_id: task.id, kind: "ASSIGNED_TO".into(), ..Default::default() })?;
```

---

## 4. Querying

drevo gives you three complementary ways to ask questions:

1. **Pattern queries (Cypher)** — declarative, relationship-shaped questions.
   See the [Cypher Reference](cypher-reference.md).
2. **Traversal (API)** — `bfs`, `dfs`, `shortest_path`, `subgraph` for imperative walks.
3. **Search** — `search_fts` (BM25 full-text) and `vector_search` (semantic / embedding).

A typical retrieval-augmented-generation flow combines all three: full-text or vector search
finds seed nodes, then a bounded subgraph traversal gathers their neighbourhood as context.
The Python [`drevo.rag`](sdk-reference.md#graph-rag-helpers) module packages exactly this.

---

## 5. Worked scenarios

These mirror the integration suites in [`tests/`](../tests). Each shows the kind of graph and a
representative Cypher query (all queries below also appear, verified, in the
[Cypher Reference](cypher-reference.md)).

### CBT journal

Capture a thought, the cognitive distortions it exhibits, and the mood it produced, then review
patterns over time.

```text
(:Entry)-[:HAS_DISTORTION]->(:Distortion)
(:Entry)-[:HAD_MOOD]->(:Mood)
```

Find the most common distortions:

```text
MATCH (:Entry)-[:HAS_DISTORTION]->(d:Distortion)
RETURN d.kind AS distortion, count(*) AS occurrences
ORDER BY occurrences DESC
```

### Story / book editor

Model narrative flow between chapters and find what a chapter eventually leads to:

```text
MATCH (start:Chapter)-[:FLOWS_TO*1..3]->(later:Chapter)
RETURN start.title, later.title
```

### IT task manager

Track tasks, assignees, and blocking dependencies; ask "who is overloaded?":

```text
MATCH (p:Person)-[:ASSIGNED_TO]->(t:Task {status: 'pending'})
WITH p, count(t) AS open_tasks
WHERE open_tasks > 5
RETURN p.title, open_tasks
```

PageRank over the dependency graph answers "what unblocks the most work?" — see
[`pagerank`](sdk-reference.md#graph-analytics).

### ERP system

Orders, products, customers. Roll up revenue:

```text
MATCH (o:Order)
RETURN count(*) AS orders, sum(o.total) AS revenue, avg(o.total) AS avg_order
```

### Bug tracker

Issues with severities and components. Triage open criticals:

```text
MATCH (b:Bug)
WHERE b.severity IN ['high', 'critical'] AND b.assignee IS NOT NULL
RETURN b.title, b.severity
```

Louvain community detection clusters related issues — see
[`louvain_communities`](sdk-reference.md#graph-analytics).

---

## 6. Where to go next

- Exact query syntax → [Cypher Reference](cypher-reference.md)
- Calling drevo from code → [SDK Reference](sdk-reference.md)
- Running drevo in production → [Admin Guide](admin-guide.md)
- Coming from Neo4j → [Migration Guide](migration-guide.md)
