"""Runtime tests for the `drevo.rag` graph-RAG idioms layer (Phase 16 task 00117).

These tests exercise the *behaviour* of the pure-Python `drevo.rag` module
sitting on top of the PyO3 bindings. Whereas the text-level scaffolding
tests in `tests/python_rag_idioms_tests.rs` verify the on-disk file
structure (modules exist, the right names are exported, the type stubs
declare the right surface), this suite drives the actual code paths:

* the `Document` Protocol is structurally satisfied by both
  `SimpleDocument` and any third-party class with `page_content` + `metadata`;
* `ingest_documents` walks the doc list, applies the schema, calls
  `drevo.create_node`, and round-trips the embedder output;
* `expand_neighborhood` does bounded BFS, respects `kind_filter` and the
  `max_nodes` cap, and reports `hops_used` honestly;
* `Retriever.retrieve` resolves seeds from FTS / id / UUID and returns
  a deterministically-ordered `Context`;
* `Context.to_text` emits markdown / json / turtle in stable byte-equal
  output for the same input;
* `MMRReranker.rerank` honours the RFC §10 Q-4 semantics
  (`lambda_=1.0 → pure relevance`, `lambda_=0.0 → pure diversity`)
  and is closed-form deterministic given a stub embedder.

The 00118 unit-test task will *expand* this file — what lives here is
the minimum runtime gate that locks the 00117 contract: every public
symbol from `drevo.rag.__all__` is exercised in at least one happy-path
and at least one edge-case test.
"""

from __future__ import annotations

import json
import uuid
from dataclasses import dataclass
from typing import Any

import pytest

import drevo
from drevo import rag
from drevo.rag import (
    Context,
    ContextStats,
    Document,
    IngestSchema,
    MMRReranker,
    Retriever,
    SimpleDocument,
    expand_neighborhood,
    ingest_documents,
)

# ── Document Protocol ─────────────────────────────────────────────────


def test_document_protocol_is_runtime_checkable() -> None:
    """`isinstance(obj, Document)` must work — duck-typing at the boundary
    relies on the @runtime_checkable decorator from typing.Protocol."""
    d = SimpleDocument(page_content="hello", metadata={"source": "test"})
    assert isinstance(d, Document)


def test_arbitrary_class_satisfies_document_protocol() -> None:
    """Any class with `page_content: str` + `metadata: dict` must satisfy
    the Document protocol — that is the whole point of using Protocol
    instead of inheriting from langchain.Document (RFC §8.1)."""

    @dataclass
    class FakeLangChainDoc:
        page_content: str
        metadata: dict[str, Any]

    d = FakeLangChainDoc(page_content="hi", metadata={"k": "v"})
    assert isinstance(d, Document)


def test_simple_document_default_metadata_is_empty_dict() -> None:
    """SimpleDocument with no metadata arg must default to an empty dict
    — `ingest_documents` calls `dict(doc.metadata)` and would crash on None."""
    d = SimpleDocument(page_content="x")
    assert d.metadata == {}


# ── ingest_documents ──────────────────────────────────────────────────


def test_ingest_documents_creates_one_node_per_doc(drevo_db: drevo.Drevo) -> None:
    docs = [
        SimpleDocument(page_content="alpha body text", metadata={"source": "a"}),
        SimpleDocument(page_content="beta body text", metadata={"source": "b"}),
        SimpleDocument(page_content="gamma body text", metadata={"source": "c"}),
    ]
    nodes = ingest_documents(drevo_db, docs)
    assert len(nodes) == 3
    for node, doc in zip(nodes, docs):
        assert isinstance(node, drevo.Node)
        assert node.kind == "doc"
        # title is truncated content (≤200 chars) per RFC §8.2.
        assert node.title.startswith(doc.page_content[:40])


def test_ingest_documents_stores_full_text_under_text_property(
    drevo_db: drevo.Drevo,
) -> None:
    """Per RFC §8.2: `properties=doc.metadata | {"text": doc.page_content}`.
    Title is truncated; full text must be recoverable from properties."""
    content = "x" * 500  # longer than the 200-char title cap
    nodes = ingest_documents(drevo_db, [SimpleDocument(page_content=content)])
    assert len(nodes) == 1
    assert nodes[0].properties.get("text") == content


def test_ingest_documents_passes_metadata_through_to_properties(
    drevo_db: drevo.Drevo,
) -> None:
    docs = [SimpleDocument(page_content="hi", metadata={"author": "me", "year": 2026})]
    nodes = ingest_documents(drevo_db, docs)
    assert nodes[0].properties.get("author") == "me"
    assert nodes[0].properties.get("year") == 2026


def test_ingest_documents_schema_maps_kind_and_title_from_metadata(
    drevo_db: drevo.Drevo,
) -> None:
    schema = IngestSchema(kind_from="type", title_from="name")
    docs = [
        SimpleDocument(
            page_content="body",
            metadata={"type": "article", "name": "Hello world"},
        )
    ]
    nodes = ingest_documents(drevo_db, docs, schema=schema)
    assert nodes[0].kind == "article"
    assert nodes[0].title == "Hello world"


def test_ingest_documents_schema_property_map_renames_fields(
    drevo_db: drevo.Drevo,
) -> None:
    schema = IngestSchema(property_map={"author": "creator"})
    docs = [SimpleDocument(page_content="b", metadata={"author": "alice"})]
    nodes = ingest_documents(drevo_db, docs, schema=schema)
    # property_map renames the metadata key on the way into the node
    # properties (RFC §8.2 — "metadata key → property key").
    assert nodes[0].properties.get("creator") == "alice"


def test_ingest_documents_with_embedder_stores_embedding_property(
    drevo_db: drevo.Drevo,
) -> None:
    def fake_embedder(texts: list[str]) -> list[list[float]]:
        return [[float(len(t)), 1.0, 2.0] for t in texts]

    docs = [SimpleDocument(page_content="hi")]
    nodes = ingest_documents(drevo_db, docs, embedder=fake_embedder)
    assert nodes[0].properties.get("embedding") == [2.0, 1.0, 2.0]


def test_ingest_documents_empty_input_returns_empty_list(
    drevo_db: drevo.Drevo,
) -> None:
    assert ingest_documents(drevo_db, []) == []


def test_ingest_documents_overrides_default_kind(drevo_db: drevo.Drevo) -> None:
    nodes = ingest_documents(
        drevo_db,
        [SimpleDocument(page_content="hi")],
        kind="paper",
    )
    assert nodes[0].kind == "paper"


# ── expand_neighborhood ───────────────────────────────────────────────


def _build_chain(db: drevo.Drevo, length: int, kind: str = "note") -> list[drevo.Node]:
    """Create a linear chain of `length` nodes with edges 0→1→2→…→length-1."""
    nodes = [db.create_node(drevo.NewNode(kind=kind, title=f"chain-{i}")) for i in range(length)]
    for a, b in zip(nodes, nodes[1:]):
        db.create_edge(drevo.NewEdge(from_id=a.id, to_id=b.id, kind="next"))
    return nodes


def test_expand_neighborhood_zero_hops_returns_just_root(drevo_db: drevo.Drevo) -> None:
    nodes = _build_chain(drevo_db, 3)
    nh = expand_neighborhood(drevo_db, nodes[0].uuid, hops=0)
    ids = {n.id for n in nh.nodes}
    assert ids == {nodes[0].id}


def test_expand_neighborhood_one_hop_returns_immediate_neighbours(
    drevo_db: drevo.Drevo,
) -> None:
    nodes = _build_chain(drevo_db, 4)
    nh = expand_neighborhood(drevo_db, nodes[0].uuid, hops=1)
    ids = {n.id for n in nh.nodes}
    assert ids == {nodes[0].id, nodes[1].id}


def test_expand_neighborhood_two_hops_returns_two_hop_set(
    drevo_db: drevo.Drevo,
) -> None:
    nodes = _build_chain(drevo_db, 5)
    nh = expand_neighborhood(drevo_db, nodes[0].uuid, hops=2)
    ids = {n.id for n in nh.nodes}
    assert ids == {nodes[0].id, nodes[1].id, nodes[2].id}


def test_expand_neighborhood_respects_max_nodes(drevo_db: drevo.Drevo) -> None:
    nodes = _build_chain(drevo_db, 10)
    nh = expand_neighborhood(drevo_db, nodes[0].uuid, hops=9, max_nodes=3)
    # max_nodes caps total returned (root + 2 hops to stay under 3).
    assert len(nh.nodes) <= 3


def test_expand_neighborhood_kind_filter_excludes_other_kinds(
    drevo_db: drevo.Drevo,
) -> None:
    a = drevo_db.create_node(drevo.NewNode(kind="article", title="a"))
    b = drevo_db.create_node(drevo.NewNode(kind="comment", title="b"))
    c = drevo_db.create_node(drevo.NewNode(kind="article", title="c"))
    drevo_db.create_edge(drevo.NewEdge(from_id=a.id, to_id=b.id, kind="has"))
    drevo_db.create_edge(drevo.NewEdge(from_id=a.id, to_id=c.id, kind="has"))

    nh = expand_neighborhood(drevo_db, a.uuid, hops=1, kind_filter=["article"])
    kinds = {n.kind for n in nh.nodes}
    assert kinds == {"article"}
    ids = {n.id for n in nh.nodes}
    assert ids == {a.id, c.id}


def test_expand_neighborhood_reports_hops_used(drevo_db: drevo.Drevo) -> None:
    nodes = _build_chain(drevo_db, 2)
    nh = expand_neighborhood(drevo_db, nodes[0].uuid, hops=5)
    # Frontier exhausts after 1 hop — chain length 2 can't go deeper.
    assert nh.hops_used == 1


def test_expand_neighborhood_accepts_uuid_object(drevo_db: drevo.Drevo) -> None:
    nodes = _build_chain(drevo_db, 2)
    # node.uuid is a real uuid.UUID after the 00116 shim wrap.
    assert isinstance(nodes[0].uuid, uuid.UUID)
    nh = expand_neighborhood(drevo_db, nodes[0].uuid, hops=1)
    assert any(n.id == nodes[1].id for n in nh.nodes)


# ── Retriever ──────────────────────────────────────────────────────────


def test_retriever_retrieve_from_fts_query_string(drevo_db: drevo.Drevo) -> None:
    a = drevo_db.create_node(drevo.NewNode(kind="doc", title="alpha bravo charlie unique"))
    drevo_db.create_node(drevo.NewNode(kind="doc", title="unrelated content"))
    r = Retriever(drevo_db, hops=0)
    ctx = r.retrieve("unique", limit=5)
    assert any(n.id == a.id for n in ctx.seeds)


def test_retriever_retrieve_from_node_id(drevo_db: drevo.Drevo) -> None:
    nodes = _build_chain(drevo_db, 3)
    r = Retriever(drevo_db, hops=1)
    ctx = r.retrieve(nodes[0].id)
    assert any(n.id == nodes[0].id for n in ctx.seeds)
    # 1-hop expansion picks up the next chain node.
    all_ids = {n.id for n in ctx.seeds} | {n.id for n in ctx.neighbours}
    assert nodes[1].id in all_ids


def test_retriever_retrieve_from_uuid(drevo_db: drevo.Drevo) -> None:
    nodes = _build_chain(drevo_db, 2)
    r = Retriever(drevo_db, hops=0)
    ctx = r.retrieve(nodes[0].uuid)
    assert [n.id for n in ctx.seeds] == [nodes[0].id]


def test_retriever_returns_context_with_stats(drevo_db: drevo.Drevo) -> None:
    nodes = _build_chain(drevo_db, 3)
    r = Retriever(drevo_db, hops=1)
    ctx = r.retrieve(nodes[0].id)
    assert isinstance(ctx, Context)
    assert isinstance(ctx.stats, ContextStats)
    assert ctx.stats.seed_count == len(ctx.seeds)


def test_retriever_retrieve_with_embedding_is_not_yet_implemented(
    drevo_db: drevo.Drevo,
) -> None:
    r = Retriever(drevo_db)
    with pytest.raises(NotImplementedError):
        r.retrieve_with_embedding([0.1, 0.2, 0.3])


def test_retriever_unknown_seed_type_raises_typeerror(drevo_db: drevo.Drevo) -> None:
    r = Retriever(drevo_db)
    with pytest.raises(TypeError):
        r.retrieve(object())  # type: ignore[arg-type]


# ── Context.to_text ────────────────────────────────────────────────────


def _make_context(db: drevo.Drevo) -> Context:
    nodes = _build_chain(db, 3, kind="note")
    r = Retriever(db, hops=2)
    return r.retrieve(nodes[0].id)


def test_context_to_text_markdown_is_deterministic(drevo_db: drevo.Drevo) -> None:
    ctx = _make_context(drevo_db)
    out_a = ctx.to_text(format="markdown")
    out_b = ctx.to_text(format="markdown")
    assert out_a == out_b
    # Sanity: each chain title appears in the markdown rendering.
    for i in range(3):
        assert f"chain-{i}" in out_a


def test_context_to_text_default_format_is_markdown(drevo_db: drevo.Drevo) -> None:
    ctx = _make_context(drevo_db)
    assert ctx.to_text() == ctx.to_text(format="markdown")


def test_context_to_text_json_is_valid_json(drevo_db: drevo.Drevo) -> None:
    ctx = _make_context(drevo_db)
    parsed = json.loads(ctx.to_text(format="json"))
    assert "seeds" in parsed
    assert "neighbours" in parsed
    assert "edges" in parsed


def test_context_to_text_turtle_emits_turtle_prefix(drevo_db: drevo.Drevo) -> None:
    ctx = _make_context(drevo_db)
    out = ctx.to_text(format="turtle")
    assert "@prefix" in out


def test_context_to_text_unknown_format_raises_valueerror(
    drevo_db: drevo.Drevo,
) -> None:
    ctx = _make_context(drevo_db)
    with pytest.raises(ValueError):
        ctx.to_text(format="csv")


# ── MMRReranker ────────────────────────────────────────────────────────


def _stub_embedder(token_dim: dict[str, int]):
    """Build a deterministic embedder that maps each token to a fixed
    one-hot dimension. Lets the test author tune similarity precisely."""

    def embed(texts: list[str]) -> list[list[float]]:
        dim = max(token_dim.values()) + 1
        out: list[list[float]] = []
        for t in texts:
            vec = [0.0] * dim
            for tok in t.split():
                if tok in token_dim:
                    vec[token_dim[tok]] = 1.0
            out.append(vec)
        return out

    return embed


@dataclass
class _NodeStub:
    """Duck-typed Node stand-in for MMR rerank tests.

    `drevo.ScoredNode` is a frozen PyO3 class with no Python-side
    constructor, so we can't synthesise arbitrary scores by minting
    real ScoredNodes. The rerank algorithm only reads `.score` and
    `.node.title`, so a tiny dataclass pair satisfies the contract
    without coupling the test to FTS scoring quirks.
    """

    id: int
    title: str
    kind: str = "doc"


@dataclass
class _ScoredStub:
    node: _NodeStub
    score: float


def _scored(node_id: int, title: str, score: float) -> _ScoredStub:
    return _ScoredStub(node=_NodeStub(id=node_id, title=title), score=score)


def test_mmr_reranker_pure_relevance_returns_top_k_by_score() -> None:
    candidates = [
        _scored(1, "apple alpha", score=0.9),
        _scored(2, "banana beta", score=0.5),
        _scored(3, "cherry gamma", score=0.1),
    ]
    embedder = _stub_embedder(
        {"apple": 0, "banana": 1, "cherry": 2, "alpha": 3, "beta": 4, "gamma": 5}
    )
    mmr = MMRReranker(lambda_=1.0)  # pure relevance
    picked = mmr.rerank(candidates, embedder=embedder, k=2)
    assert [c.score for c in picked] == [0.9, 0.5]


def test_mmr_reranker_pure_diversity_avoids_redundant_picks() -> None:
    # Two near-identical docs and one different doc. Pure-diversity
    # (lambda_=0.0) after the top pick should jump to the different one.
    candidates = [
        _scored(1, "apple alpha", score=0.9),
        _scored(2, "apple alpha twin", score=0.85),
        _scored(3, "different domain document", score=0.5),
    ]
    embedder = _stub_embedder(
        {
            "apple": 0,
            "alpha": 1,
            "twin": 2,
            "different": 3,
            "domain": 4,
            "document": 5,
        }
    )
    mmr = MMRReranker(lambda_=0.0)  # pure diversity
    picked = mmr.rerank(candidates, embedder=embedder, k=2)
    assert picked[0].node.title == "apple alpha"
    assert picked[1].node.title == "different domain document"


def test_mmr_reranker_k_zero_returns_empty_list() -> None:
    candidates = [_scored(1, "x first", score=1.0)]
    embedder = _stub_embedder({"x": 0, "first": 1})
    mmr = MMRReranker()
    assert mmr.rerank(candidates, embedder=embedder, k=0) == []


def test_mmr_reranker_empty_candidates_returns_empty_list() -> None:
    mmr = MMRReranker()
    assert mmr.rerank([], embedder=lambda xs: [[0.0]] * len(xs), k=5) == []


def test_mmr_reranker_k_larger_than_candidates_returns_all() -> None:
    candidates = [
        _scored(1, "y solo", score=0.4),
        _scored(2, "z duo", score=0.7),
    ]
    embedder = _stub_embedder({"y": 0, "solo": 1, "z": 2, "duo": 3})
    mmr = MMRReranker(lambda_=1.0)
    picked = mmr.rerank(candidates, embedder=embedder, k=10)
    assert {c.node.id for c in picked} == {c.node.id for c in candidates}


# ── Module surface ─────────────────────────────────────────────────────


def test_rag_module_all_lists_public_surface() -> None:
    expected = {
        "Document",
        "SimpleDocument",
        "IngestSchema",
        "ingest_documents",
        "Retriever",
        "Context",
        "ContextStats",
        "MMRReranker",
        "expand_neighborhood",
    }
    assert expected.issubset(set(rag.__all__))


@pytest.mark.parametrize(
    "name",
    [
        "Document",
        "SimpleDocument",
        "IngestSchema",
        "ingest_documents",
        "Retriever",
        "Context",
        "ContextStats",
        "MMRReranker",
        "expand_neighborhood",
    ],
)
def test_rag_public_symbol_importable(name: str) -> None:
    assert hasattr(rag, name), f"drevo.rag.{name} must resolve via the rag re-export"
