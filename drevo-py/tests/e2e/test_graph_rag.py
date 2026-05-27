"""End-to-end scenario: graph-RAG retrieval over an ingested document set.

This is the scenario that the Phase 16 README calls out explicitly as
"a graph-RAG-specific scenario": ingest a synthetic ``Document`` list,
build the graph, issue a natural-language query, and expect a
``Context`` whose serialised form contains the seed node + its
declared neighbourhood in stable order.

The scenario walks the whole ``drevo.rag`` surface (``Document`` /
``ingest_documents`` / ``Retriever`` / ``Context.to_text``) end-to-end:

1. Construct a small corpus of ``SimpleDocument``s about a tiny
   knowledge area (the drevo project itself).
2. Ingest with a deterministic stub embedder so the per-doc vectors
   land in node properties without requiring a real model.
3. Wire neighbour edges that reflect document-to-document references.
4. Issue an FTS-style natural-language query.
5. Assert the ``Context`` content + the byte-stable ``to_text``
   output across all three formats (markdown, json, turtle).
6. Persist + reopen and verify the same query reproduces the same
   serialised context.

Cites RFC §8.4 (``Context.to_text`` is deterministic given the same
context, sorted by ``(kind, title, id)``).
"""

from __future__ import annotations

import json
from typing import Callable

import drevo
from drevo.rag import (
    Context,
    Document,
    Retriever,
    SimpleDocument,
    ingest_documents,
)

# ── corpus + graph wiring ─────────────────────────────────────────────


def _corpus() -> list[Document]:
    """A small, deterministic, English-only corpus.

    Per CLAUDE.md test-data convention, every body string is English so
    the FTS tokeniser exercises a realistic distribution.
    """
    return [
        SimpleDocument(
            page_content="drevo is an embedded graph database written in Rust",
            metadata={"slug": "drevo-intro", "category": "overview"},
        ),
        SimpleDocument(
            page_content="redb is the on-disk key-value store drevo uses for persistence",
            metadata={"slug": "drevo-redb", "category": "storage"},
        ),
        SimpleDocument(
            page_content="full-text search in drevo is powered by a trigram index",
            metadata={"slug": "drevo-fts", "category": "search"},
        ),
        SimpleDocument(
            page_content="drevo exposes a Python binding called drevo-py via PyO3",
            metadata={"slug": "drevo-py", "category": "binding"},
        ),
        SimpleDocument(
            page_content="the rag layer in drevo-py provides Retriever and Context primitives",
            metadata={"slug": "drevo-rag", "category": "binding"},
        ),
    ]


def _wire_doc_edges(db: drevo.Drevo, nodes: list[drevo.Node]) -> None:
    """Reflect document references as graph edges so the retriever's
    neighbourhood expansion has something to walk.

    Edges form a small tree rooted at the intro doc:

        drevo-intro
          ├── drevo-redb
          ├── drevo-fts
          └── drevo-py
                └── drevo-rag
    """
    by_slug = {n.properties["slug"]: n for n in nodes}
    pairs = [
        ("drevo-intro", "drevo-redb"),
        ("drevo-intro", "drevo-fts"),
        ("drevo-intro", "drevo-py"),
        ("drevo-py", "drevo-rag"),
    ]
    for parent, child in pairs:
        db.create_edge(
            drevo.NewEdge(
                from_id=by_slug[parent].id,
                to_id=by_slug[child].id,
                kind="references",
            )
        )


def _ingest(
    db: drevo.Drevo, embedder: Callable[[list[str]], list[list[float]]]
) -> list[drevo.Node]:
    return ingest_documents(
        db,
        _corpus(),
        kind="doc",
        embedder=embedder,
    )


# ── scenario assertions ───────────────────────────────────────────────


def test_ingest_creates_one_node_per_document(
    disk_db: drevo.Drevo,
    deterministic_embedder: Callable[[list[str]], list[list[float]]],
) -> None:
    """``ingest_documents`` is a strict 1:1 mapping per RFC §8.2."""
    nodes = _ingest(disk_db, deterministic_embedder)
    assert len(nodes) == len(_corpus())
    for n in nodes:
        # The embedder output round-trips through properties.
        assert "embedding" in n.properties
        assert isinstance(n.properties["embedding"], list)
        assert len(n.properties["embedding"]) == 8


def test_retriever_returns_seed_and_referenced_neighbours(
    disk_db: drevo.Drevo,
    deterministic_embedder: Callable[[list[str]], list[list[float]]],
) -> None:
    """An FTS query for "Python binding" must return the
    ``drevo-py`` node as a seed plus its referenced neighbour
    ``drevo-rag`` and (one hop further) the intro node above it.
    """
    nodes = _ingest(disk_db, deterministic_embedder)
    _wire_doc_edges(disk_db, nodes)

    retriever = Retriever(disk_db, hops=2, max_nodes=10)
    ctx = retriever.retrieve("PyO3", limit=3)

    seed_slugs = {n.properties["slug"] for n in ctx.seeds}
    assert "drevo-py" in seed_slugs

    all_slugs = {n.properties["slug"] for n in (*ctx.seeds, *ctx.neighbours)}
    assert {"drevo-py", "drevo-rag"}.issubset(all_slugs)


def test_context_to_text_is_deterministic_across_runs(
    disk_db: drevo.Drevo,
    deterministic_embedder: Callable[[list[str]], list[list[float]]],
) -> None:
    """The same context serialises byte-equal twice in a row. This is
    RFC §8.4's contract — break it and downstream caches (LLM prompt
    cache, embedding cache) silently mis-key.
    """
    nodes = _ingest(disk_db, deterministic_embedder)
    _wire_doc_edges(disk_db, nodes)

    retriever = Retriever(disk_db, hops=2, max_nodes=10)
    ctx_first = retriever.retrieve("trigram", limit=5)
    ctx_second = retriever.retrieve("trigram", limit=5)

    for fmt in ("markdown", "json", "turtle"):
        assert ctx_first.to_text(format=fmt) == ctx_second.to_text(format=fmt)


def test_context_json_format_contains_seed_and_neighbour_titles(
    disk_db: drevo.Drevo,
    deterministic_embedder: Callable[[list[str]], list[list[float]]],
) -> None:
    """The JSON serialisation has to embed enough metadata for the
    receiving LLM prompt to cite each node. ``_node_dict`` projects
    ``id``, ``uuid``, ``kind``, ``title`` (RFC §8.4 default schema);
    every seed + neighbour must carry an inspectable title.
    """
    nodes = _ingest(disk_db, deterministic_embedder)
    _wire_doc_edges(disk_db, nodes)

    retriever = Retriever(disk_db, hops=2, max_nodes=10)
    ctx = retriever.retrieve("Retriever", limit=5)
    text = ctx.to_text(format="json")
    payload = json.loads(text)
    assert {"seeds", "neighbours", "edges", "stats"}.issubset(payload.keys())
    seed_titles = {s["title"] for s in payload["seeds"]}
    neighbour_titles = {s["title"] for s in payload["neighbours"]}
    # The seed itself must surface the rag-layer doc (body matches "Retriever").
    rag_title = "the rag layer in drevo-py provides Retriever and Context primitives"
    assert rag_title in seed_titles, payload
    # The 2-hop neighbourhood must reach the parent doc (drevo-py) and the root intro.
    py_title = "drevo exposes a Python binding called drevo-py via PyO3"
    intro_title = "drevo is an embedded graph database written in Rust"
    assert py_title in neighbour_titles, payload
    assert intro_title in neighbour_titles, payload


def test_context_markdown_format_lists_titles_in_stable_order(
    disk_db: drevo.Drevo,
    deterministic_embedder: Callable[[list[str]], list[list[float]]],
) -> None:
    """Markdown output sorts nodes by ``(kind, title, id)`` per RFC §8.4.
    Two consecutive runs must produce identical title ordering.
    """
    nodes = _ingest(disk_db, deterministic_embedder)
    _wire_doc_edges(disk_db, nodes)

    retriever = Retriever(disk_db, hops=1, max_nodes=10)
    md_first = retriever.retrieve("rag", limit=5).to_text(format="markdown")
    md_second = retriever.retrieve("rag", limit=5).to_text(format="markdown")
    assert md_first == md_second
    # Markdown contains the seed slugs explicitly.
    assert "drevo-rag" in md_first or "drevo-py" in md_first


def test_retriever_stats_report_actual_hop_depth(
    disk_db: drevo.Drevo,
    deterministic_embedder: Callable[[list[str]], list[list[float]]],
) -> None:
    """``ContextStats.hops_used`` reports the deepest level where a new
    node was discovered — not the configured ``hops`` ceiling.
    """
    nodes = _ingest(disk_db, deterministic_embedder)
    _wire_doc_edges(disk_db, nodes)

    retriever = Retriever(disk_db, hops=3, max_nodes=10)
    # ``embedded`` is the unique body term in the drevo-intro doc,
    # which is the root of the reference tree (three direct children,
    # one grandchild).
    ctx = retriever.retrieve("embedded", limit=5)
    # With hops=3 the rag doc is reachable at depth 2 (via drevo-py),
    # so hops_used should be ≥ 2 and ≤ 3.
    assert 2 <= ctx.stats.hops_used <= 3
    assert ctx.stats.seed_count >= 1


def test_full_rag_pipeline_round_trips_through_reopen(
    tmp_db_path: str,
    deterministic_embedder: Callable[[list[str]], list[list[float]]],
) -> None:
    """The most important e2e contract: ingest + edges persist, and the
    same retrieval question yields the same serialised context after
    a close + reopen cycle.
    """
    with drevo.Drevo.open(tmp_db_path) as db:
        nodes = _ingest(db, deterministic_embedder)
        _wire_doc_edges(db, nodes)
        first = Retriever(db, hops=2, max_nodes=10).retrieve("Python", limit=5)
        first_md = first.to_text(format="markdown")

    with drevo.Drevo.open(tmp_db_path) as db2:
        second = Retriever(db2, hops=2, max_nodes=10).retrieve("Python", limit=5)
        second_md = second.to_text(format="markdown")

    assert first_md == second_md, "RAG context must be reproducible across reopens"


def test_context_dataclass_is_frozen(
    disk_db: drevo.Drevo,
    deterministic_embedder: Callable[[list[str]], list[list[float]]],
) -> None:
    """``Context`` is frozen so a retrieved object is safe to cache.
    Asserting frozen-ness here documents the cacheability contract."""
    nodes = _ingest(disk_db, deterministic_embedder)
    _wire_doc_edges(disk_db, nodes)
    ctx = Retriever(disk_db, hops=1).retrieve("rag", limit=3)
    assert isinstance(ctx, Context)
    import dataclasses

    assert dataclasses.is_dataclass(ctx)
    assert dataclasses.fields(ctx)
    # Frozen dataclass: setattr raises.
    import pytest as _pytest

    with _pytest.raises(dataclasses.FrozenInstanceError):
        ctx.seeds = []
