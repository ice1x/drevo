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
> in Cypher), drevo synthesises a unique placeholder title for you. The placeholder is
> invisible to Cypher: `CREATE (n:X {x: 1})` yields a node whose `n.title` is `null` and whose
> `keys(n)` is `["x"]`, exactly as in Neo4j — and `SET b = properties(a)` never copies it.

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
- **Web UI** — a Cytoscape.js graph explorer at `/ui` on the HTTP server. The top bar takes a
  Cypher query or full-text search terms. Enter runs a one-line query, Shift+Enter adds a
  line, and Ctrl/Cmd+Enter runs a query of any length. Alt+↑ / Alt+↓ step through earlier
  queries, and the ▾ button lists the last 50; the history stays in your browser. **Ask** turns
  a question in plain language into Cypher with the server's
  [text-to-Cypher model](admin-guide.md#text-to-cypher-drevocypherfromtext-opt-in) and puts the
  query in the editor for you to check and run. While a request runs, the status bar shows how
  long it has taken, and **Stop** stops waiting for it. The left rail lists the most common node labels;
  **Filter labels** finds any other, and clicking a label shows a sample of its nodes. **Colour
  by** colours the graph by label or by a property such as `type`, with a legend. Bigger nodes
  have more connections; the most connected ones, and the neighbours of the node you click, stay
  labelled at any zoom. Right-click a node for more: expand or isolate its neighbourhood, put a
  query for it in the editor, copy its id or Cypher, or hide it from the canvas. The
  inspector on the right shows the clicked node's or relationship's fields, with dates in UTC; a
  relationship's **from** and **to** open those nodes. A query's result opens in the main area under three
  tabs: **Graph**, **Table** and **JSON**. A result with nodes opens as a graph; one with only
  values, such as `MATCH (t:Task) RETURN t.title, t.status`, opens as a table. In the table a
  node cell opens that node's neighbourhood, and **Copy CSV** copies the rows. Server problems (a
  query stopped by the statement timeout, storage errors, any server WARN/ERROR) and failed UI
  requests show up as notifications. The ⚑ button (or **Report…** on a notification) builds a
  bug report: server build and config, graph size, recent problems, your last failing query
  with a ready-to-run Rust repro test, recent UI errors and a screenshot of the graph. You
  review and edit the text, then **Open GitHub issue** opens a prefilled issue in a new tab
  under your own GitHub login, with the screenshot copied to the clipboard for pasting.
  **Download report** saves the full JSON report for attaching. Nothing leaves your machine
  until you open the issue. API keys are removed, but queries and log lines are included and
  the issue is public, so check the text first.
- **MCP** — a separate [drevo-mcp](https://github.com/ice1x/drevo-mcp) server exposes graph tools to AI agents (Claude Code, Cline), talking to `drevo-server` over HTTP / Bolt.

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

These mirror the integration suites in [`tests/`](https://github.com/ice1x/drevo/tree/main/tests). Each shows the kind of graph and a
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

### Two names, one entity

An agent that remembers what it reads soon knows «Пушкин» and «А. С. Пушкин»
as two people, with the facts about the poet split between them. drevo
resolves this in three steps: find the pairs, review them, merge.

Ask for the pairs that probably name the same thing. Entities of different
types are never paired, so the street «Пушкина» (a `LOCATION`) stays apart:

```text
CALL drevo.entity.candidates('Entity', {threshold: 0.9})
YIELD a, b, score, reasons
RETURN a.name, b.name, score, reasons
```

Each row says why it was offered — `['name:1.00', 'shared_token:пушкин']`.
After a person or an agent has agreed, fold the second node into the first:

```text
MATCH (keep:Entity {name: 'Пушкин'}), (dup:Entity {name: 'А. С. Пушкин'})
CALL drevo.entity.merge(keep, [dup], {strategy: 'keepFirst'})
YIELD kept, merged, relationshipsMoved
RETURN kept.aliases, merged, relationshipsMoved
```

The facts move to «Пушкин», the old spelling becomes an alias, and later
mentions of it find the kept entity:

```text
CALL drevo.memory.rememberEntity(null, 'А.С. Пушкин', 'PERSON', null, {resolve: 'alias'})
YIELD node
RETURN node.name
```

`resolve: 'fuzzy'` goes further and reuses a sufficiently similar name,
recording near-misses as `POSSIBLY_SAME` for review; the default stays exact,
because a wrong automatic merge loses data. See
[Entity resolution](cypher-reference.md#entity-resolution).

---

## 6. Graph-RAG inside drevo

An assistant that answers from your graph needs, for each question, the few facts that matter —
small enough to fit the prompt, ordered so the most relevant come first. drevo builds that context
itself, in one call:

1. **The question comes in.** "Why did Mila panic before the meeting?"
2. **drevo builds the context.** `drevo.rag.context` searches for the nodes the question is about
   (by words and, with an embedder, by meaning), walks out from them to the facts around them,
   ranks what it found by how closely it connects to those first hits, and writes the best of it as
   text within your token budget.

   ```text
   CALL drevo.rag.context('Why did Mila panic before the meeting?',
                          {label: 'Entry', format: 'markdown', tokenBudget: 1500})
   YIELD text, tokens, truncated
   ```

   ```text
   ## Entities
   - **Monday shift** (Entry): Panic before the team meeting, heart racing.
   - **Mila** (Person): Keeps a mood journal
   - **Everyone will see I am incompetent** (Thought)
   - **Mind reading** (Distortion): Assuming what others think

   ## Relations
   - (Mila) -[WROTE]-> (Monday shift)
   - (Monday shift) -[CONTAINS]-> (Everyone will see I am incompetent)
   - (Everyone will see I am incompetent) -[IS_A]-> (Mind reading)
   ```

3. **Your code writes the prompt.** Put `text` under a heading such as "What the journal says" and
   the question after it.
4. **The model answers** from that context. drevo never calls the chat model here; it only hands
   over the context.

The same call is `POST /rag/context` for clients without Cypher — see the
[Admin Guide](admin-guide.md#graph-rag-context-post-ragcontext). `drevo.rag.retrieve` returns the
same nodes as rows with their scores, for when you format the context yourself.

A few knobs change the most:

- **`format`** — `'markdown'` reads well, `'triples'` (`A | REL | B` lines) is the cheapest in
  tokens, `'json'` is for programs.
- **`tokenBudget`** — the context never exceeds it; nodes that do not fit are skipped whole and
  `truncated` says so.
- **`hops`** and **`rank`** — how far from the first hits to look (default 2) and how to order what
  is found (`'ppr'`, the default, favours what is closely connected to the best hits).
- **`asOf`** — facts written by `drevo.memory.assertFact` are shown as they stood at that time.

Every option is in the [Cypher Reference](cypher-reference.md#graph-rag-retrieval).

### Explain a context

When an answer comes out wrong, the first question is what the model was given. Name the
conversation in the call and drevo keeps a record of each context it builds:

```text
CALL drevo.rag.context('Why did Mila panic before the meeting?',
                       {session: 'chat-7', tokenBudget: 1500})
YIELD text, traceId
```

The record is a `:ContextTrace` node: the question, the text handed over, the settings used, and a
link to every node that went in and to every node that was found but left out — with why (it did
not fit the budget, or the search stopped at its node limit). It is also linked to the newest
message of the conversation, so it sits next to the turn that asked. To read it back:

```text
CALL drevo.rag.explain($traceId)
YIELD node, status, rank, tokens, reason
RETURN rank, status, coalesce(node.name, node.title) AS name, tokens, reason
```

A missing fact shows as a node that is not there at all; a budget cut as a `skipped` row with
reason `budget`; a bad ranking as the right node far down the list. After you add the missing fact,
`CALL drevo.rag.replay($traceId)` builds the same context again on today's graph and says whether
the text changed — without recording a new trace. `drevo.rag.traces('chat-7', 10)` lists a
conversation's latest records, and an administrator can delete old ones with
`drevo.rag.pruneTraces('2025-01-01')` or let the server do it (`DREVO_RAG_TRACE_RETENTION_DAYS`).
`trace: true` records a context without naming a conversation. Recording is a write, so it needs
the right to create nodes. Details are in the
[Cypher Reference](cypher-reference.md#context-traces).

### Feed it a book

The context is only as good as the graph. `drevo.extract.ingest` builds that graph from plain text
with the chat model the server is configured with: it cuts the text into passages, asks the model
for the people, places, organisations, objects and events in each and the facts it states, and
writes them as long-term memory — each fact citing the passage and the exact words it came from.

```text
CALL drevo.extract.ingest($chapter, {
  source: {id: 'book-1', title: 'The Long Winter', kind: 'document'},
  types: ['PERSON', 'LOCATION', 'ORGANIZATION', 'SHIP'],
  schemaHint: 'A novel about a whaling crew.'
})
YIELD chunks, entities, facts, skipped
```

`skipped` counts what the model claimed but the text does not say: every quote is looked up in its
passage before anything is written, so an invented one never gets in. Feed the chapters one call at
a time; running the same chapter again changes nothing. Try a chapter with `dryRun: true` first to
see the counts without writing.

Afterwards the usual tools work on the book: `drevo.rag.context('Who sailed with Ahab?')` finds the
characters and their facts, and `drevo.memory.evidence('Ahab', 5)` shows the passages behind them,
to quote in an answer. To extract every new document as it is written, install the trigger shown in
the [Admin Guide](admin-guide.md#extraction-drevoextract-opt-in).

---

## 7. Where to go next

- Exact query syntax → [Cypher Reference](cypher-reference.md)
- Calling drevo from code → [SDK Reference](sdk-reference.md)
- Running drevo in production → [Admin Guide](admin-guide.md)
- Coming from Neo4j → [Migration Guide](migration-guide.md)
