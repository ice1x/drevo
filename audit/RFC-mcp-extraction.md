# RFC — Extracting the MCP servers into a separate repository

Date: 2026-06-29
Status: Draft / feasibility — no code moved yet.
Companion to: the test-coverage audit (`tests/special_chars_content_tests.rs`,
vector-coverage findings below).

## Question

1. Does the MCP layer use the vector / embedding functionality?
2. Can the MCP layer be lifted out of the monorepo into its own repository?

## 1. Does MCP use vectors?

**No — none of the three MCP surfaces expose a dedicated vector/embedding
tool.** The vector subsystem lives entirely in the core library
(`drevo::db::Drevo::{set_embedding, set_embeddings_batch, get_embedding,
delete_embedding, embedding_count, vector_search, build_vector_index}`), the
Python binding (`drevo-py/src/handle.rs`, six bridge methods) and the Cypher
`similar(vector, query, threshold)` predicate.

| MCP surface | Transport | Tools | Vectors |
|---|---|---|---|
| Rust embedded (`src/mcp/`, bin `drevo-mcp`) | in-process `Drevo` | `drevo_health_check`, `drevo_count_nodes`, `drevo_node_get`, `drevo_node_get_by_uuid`, `drevo_search_fts`, `drevo_bfs`, `drevo_list_nodes_by_kind`, `python_api_list/describe/examples` | None. "vector search" appears only as documentation text inside the `python_api` catalog and in the FTS tool description. |
| Python HTTP (`tools/drevo-mcp/`) | `DrevoHttpClient` → HTTP API | `health`, `node_get`, `list_nodes_by_kind`, `search_fts`, `neighbors`, `subgraph`, `shortest_path`, `count_nodes` | None. The HTTP API deliberately omits vectors (the server hosts no embedder). |
| Python Bolt (`tools/drevo-mcp-bolt/`) | Bolt → Cypher | `create_entity`, `add_observations`, `delete_entity`, `create_relationship`, `delete_relationship`, `get_entity`, `search_knowledge`, `get_project_graph`, `list_projects`, `add_migration`, `get_migrations`, `apply_migration`, `run_cypher` | No dedicated tool, but `run_cypher` can run a `similar(...)` query, so vector search is **indirectly reachable**. |

Implication for extraction: vectors are *not* a coupling concern for the MCP
move. If a first-class vector MCP tool is ever wanted, it would be new surface,
not a relocation.

## 2. Can MCP be extracted? — Yes, with per-component effort

### 2a. Python HTTP MCP (`tools/drevo-mcp/`) — trivial

Already a self-contained pip package (own `pyproject.toml`, `tests/`). It talks
to drevo only over HTTP via `DrevoHttpClient`. **Zero source dependency on the
Rust crate.** Extraction = `git filter-repo --subdirectory-filter
tools/drevo-mcp` into a new repo, then publish to PyPI. Its tests
(`test_client.py`, `test_compose.py`, `test_tools.py`) travel unchanged.

### 2b. Python Bolt MCP (`tools/drevo-mcp-bolt/`) — trivial

Same shape: standalone package, talks Bolt only. No crate-source coupling.
Extraction is identical to 2a. Tests (`test_config.py`, `test_integration.py`)
travel unchanged. `test_integration.py` needs a running drevo Bolt endpoint —
the CI for the new repo must either spin up `drevo-server`/Bolt from a published
binary/container or mark those tests as requiring an external endpoint.

### 2c. Rust embedded MCP (`src/mcp/`, `src/bin/mcp.rs`, bin `drevo-mcp`) — feasible, one caveat

It consumes only the **public** API: `drevo::db::Drevo` and
`drevo::model::Direction`. No private internals. It can become its own crate
that depends on `drevo` as a normal dependency.

**The one coupling:** `src/mcp/python_api.rs` embeds three files from
`drevo-py/` at compile time:

```
include_str!("../../drevo-py/python/drevo/__init__.pyi")
include_str!("../../drevo-py/python/drevo/rag/__init__.pyi")
include_str!("../../drevo-py/README.md")
```

These power the `python_api_list/describe/examples` discovery tools. In a
separate repo there is no `../../drevo-py`. Options:

- **(A) Drop `python_api_*`.** Cleanest. Those tools document the *Python* API
  and arguably belong with `drevo-py`, not a generic MCP server. The embedded
  Rust MCP would then be a pure graph-access server over the public API.
- **(B) Vendor the three files** into the new repo and refresh them via CI/script
  from a pinned `drevo-py` version. Keeps the feature; adds drift risk.
- **(C) Generate them at build time** from a published `drevo-py` artifact. Most
  work, least drift.

Tests that move with it: `tests/drevo_mcp_e2e_tests.rs`,
`tests/mcp_python_api_tests.rs` (only if `python_api` is kept),
`tests/mcp_validation_e2e_tests.rs`, plus the `#[cfg(test)]` modules in
`src/mcp/*`.

### Trade-offs of moving the Rust MCP out

- **Lose the atomic-change workflow.** Today a public-API change and its MCP
  follow-up land in one PR; a separate repo must wait for a published `drevo`
  crate version and lags core. (Cross-ref memory: *importer/adapter lives
  outside core* applies to wire-protocol adapters; the embedded MCP is closer to
  a first-class server binary like `drevo-server`, which stays in core.)
- **More CI pipelines** on a single self-hosted runner (memory: CI is
  single-runner-contended) — every extracted repo is another serialized build.

### Recommendation

- Extract the **two Python MCPs** first — they are pure wire-protocol clients,
  zero crate coupling, lowest risk, and match the adapter-outside-core convention.
- Keep the **Rust embedded MCP** in-tree as a binary unless there is a concrete
  packaging/release driver; if it does move, resolve `python_api` via option (A).
- Vectors are orthogonal to all of the above.

## Migration checklist (when approved, per component)

1. Confirm the target repo is one the user owns (per git-workflow rules).
2. `git filter-repo --subdirectory-filter <path>` preserving history.
3. Wire CI for the new repo (lint + type-check + tests; external drevo endpoint
   for integration tests).
4. Publish (PyPI for Python, crates.io for the Rust crate).
5. Replace the in-tree copy with a dependency / submodule note, update docs and
   `docker-compose`/container references that launch the MCP.
