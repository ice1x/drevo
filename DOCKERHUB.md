# drevo

**An embedded graph + vector database in a single file — Neo4j-compatible, with built-in full-text search, HNSW vector search, and an OpenAI-compatible embeddings proxy. No external services.**

`drevo` is a self-contained graph database written in Rust. It stores everything in one [`redb`](https://github.com/cberner/redb) file, speaks the **Neo4j Bolt** wire protocol and **Cypher**, and ships a Web UI, an HTTP API, BM25 full-text search, and vector search — all in one small container with zero external dependencies.

---

## Features

- **Graph + vector in one engine** — nodes, typed relationships, properties, and `Value::Vector` with an HNSW index for joint graph + semantic queries (RAG-ready).
- **Neo4j-compatible** — a hand-written Cypher executor (`CREATE`/`MATCH`/`MERGE`/`SET`/`DELETE`/`WHERE`/`WITH`/`UNWIND`/`FOREACH`/aggregations/variable-length paths) served over the **Bolt** wire protocol, so `cypher-shell` and the official Neo4j drivers connect out of the box.
- **Full-text search** — Okapi BM25 over a trigram index. Indexes node `title`/`body`, **all string node properties**, and **relationship properties** — `CALL fts.search(query, k)` and `CALL fts.searchRelationships(query, k)`.
- **OpenAI-compatible embeddings proxy** — `POST /v1/embeddings` transparently forwards to your configured upstream (OpenAI / Voyage / any compatible endpoint). Opt-in and SSRF-safe: it answers `503` until configured.
- **Web UI** — an interactive graph explorer at `/ui`.
- **Durable & portable** — a version-stamped on-disk format; a single `.redb` file travels between a local session, a cloud worker, and a mobile app.

---

## Quick start

```bash
docker run -d --name drevo \
  -p 8080:8080 \
  -p 7687:7687 \
  -v "$PWD/data:/data" \
  ice1x/drevo:latest
```

- **HTTP API + Web UI** → http://localhost:8080  (open http://localhost:8080/ui)
- **Bolt** (Neo4j drivers / `cypher-shell`) → `bolt://localhost:7687`
- **Health check** → `GET /health` returns `{"status":"ok"}`

Connect with any Neo4j driver:

```python
from neo4j import GraphDatabase
drv = GraphDatabase.driver("bolt://localhost:7687")
with drv.session() as s:
    s.run("CREATE (:Note {title:'hello', body:'first node'})")
    for r in s.run("CALL fts.search('hello', 5) YIELD node, score RETURN node.title, score"):
        print(r.values())
```

---

## Data persistence

The database is a single file at **`/data/drevo.redb`**. Bind-mount `/data` (as above) to keep it on the host — it survives image upgrades, and the version-stamped format stays backward-compatible.

The container runs as a non-root user. If the bind-mounted folder is owned by your host user, run the container as that user so it can take redb's write lock:

```bash
docker run -d --name drevo \
  -p 8080:8080 -p 7687:7687 \
  -u "$(id -u):$(id -g)" \
  -v "$PWD/data:/data" \
  ice1x/drevo:latest
```

---

## Configuration (environment variables)

| Variable | Default | Description |
|---|---|---|
| `DREVO_HOST` | `0.0.0.0` | HTTP bind address |
| `DREVO_PORT` | `8080` | HTTP API + Web UI port |
| `DREVO_DATA_DIR` | `/data` | Directory holding `drevo.redb` |
| `DREVO_BOLT_PORT` | `7687` | Bolt (Neo4j-compatible) port |

**Embeddings proxy** (optional — enables `POST /v1/embeddings`; without these it returns `503`):

| Variable | Example | Description |
|---|---|---|
| `DREVO_EMBEDDINGS_UPSTREAM` | `https://api.openai.com/v1/embeddings` | Upstream embeddings endpoint |
| `DREVO_EMBEDDINGS_API_KEY` | `sk-…` | Upstream API key (kept server-side; never exposed) |
| `DREVO_EMBEDDINGS_MODEL` | `text-embedding-3-small` | Default model when the request omits one |

```bash
docker run -d --name drevo \
  -p 8080:8080 -p 7687:7687 \
  -v "$PWD/data:/data" \
  -e DREVO_EMBEDDINGS_UPSTREAM=https://api.openai.com/v1/embeddings \
  -e DREVO_EMBEDDINGS_MODEL=text-embedding-3-small \
  -e DREVO_EMBEDDINGS_API_KEY=sk-your-key \
  ice1x/drevo:latest
```

---

## Tags

- `latest` — the most recent release.
- `X.Y.Z` (e.g. `0.0.3`) — immutable, pinned versions.

## Source & docs

GitHub: <https://github.com/ice1x/drevo>
