"""Unit tests for `drevo.rag.ingest_documents` + `IngestSchema` (00118).

Where the existing 00117 suite verifies happy-path behaviour, this file
focuses on the *edges* of the mapping logic: empty input, schema with
no keys set, embedder size mismatch, schema fallbacks when a metadata
key is missing.
"""

from __future__ import annotations

from typing import Any, Callable

import pytest

import drevo
from drevo.rag import IngestSchema, SimpleDocument, ingest_documents


# ── Happy paths the deeper-existence tests didn't cover ──────────────


def test_ingest_documents_empty_list_returns_empty_list(drevo_db: drevo.Drevo) -> None:
    """No docs ⇒ no nodes ⇒ no Drevo writes."""
    assert ingest_documents(drevo_db, []) == []


def test_ingest_documents_returns_one_node_per_doc_in_order(
    drevo_db: drevo.Drevo,
) -> None:
    docs = [SimpleDocument(page_content=f"body-{i}", metadata={"order": i}) for i in range(3)]
    out = ingest_documents(drevo_db, docs)
    assert [n.properties["order"] for n in out] == [0, 1, 2]


def test_ingest_documents_stores_full_text_under_text_property(
    drevo_db: drevo.Drevo,
) -> None:
    long = "x" * 500
    [node] = ingest_documents(drevo_db, [SimpleDocument(page_content=long)])
    assert node.properties["text"] == long


def test_ingest_documents_default_kind_is_doc(drevo_db: drevo.Drevo) -> None:
    [node] = ingest_documents(drevo_db, [SimpleDocument(page_content="x")])
    assert node.kind == "doc"


def test_ingest_documents_kind_override_argument(drevo_db: drevo.Drevo) -> None:
    [node] = ingest_documents(drevo_db, [SimpleDocument(page_content="x")], kind="page")
    assert node.kind == "page"


# ── IngestSchema behaviour ───────────────────────────────────────────


def test_ingest_schema_default_field_factories_are_independent() -> None:
    """Two `IngestSchema()` instances must not share the same property_map.

    Locks the `dataclass(field(default_factory=dict))` contract — a
    regression to a class-level mutable default would silently link
    every IngestSchema's map.
    """
    a = IngestSchema()
    b = IngestSchema()
    a.property_map["x"] = "y"
    assert b.property_map == {}


def test_ingest_schema_title_from_uses_metadata_key(drevo_db: drevo.Drevo) -> None:
    schema = IngestSchema(title_from="display_name")
    docs = [SimpleDocument(page_content="body", metadata={"display_name": "Custom Title"})]
    [node] = ingest_documents(drevo_db, docs, schema=schema)
    assert node.title == "Custom Title"


def test_ingest_schema_title_from_falls_back_to_content_when_key_missing(
    drevo_db: drevo.Drevo,
) -> None:
    """The mapped key is absent → fall back to the truncated-content rule."""
    schema = IngestSchema(title_from="nope")
    [node] = ingest_documents(
        drevo_db,
        [SimpleDocument(page_content="fallback body", metadata={"other": "thing"})],
        schema=schema,
    )
    assert node.title == "fallback body"


def test_ingest_schema_kind_from_uses_metadata_key(drevo_db: drevo.Drevo) -> None:
    schema = IngestSchema(kind_from="type")
    [node] = ingest_documents(
        drevo_db,
        [SimpleDocument(page_content="x", metadata={"type": "page"})],
        schema=schema,
    )
    assert node.kind == "page"


def test_ingest_schema_property_map_renames_metadata_keys(drevo_db: drevo.Drevo) -> None:
    schema = IngestSchema(property_map={"src": "source"})
    [node] = ingest_documents(
        drevo_db,
        [SimpleDocument(page_content="x", metadata={"src": "tests"})],
        schema=schema,
    )
    assert node.properties.get("source") == "tests"


def test_ingest_truncates_title_to_200_chars(drevo_db: drevo.Drevo) -> None:
    long = "x" * 300
    [node] = ingest_documents(drevo_db, [SimpleDocument(page_content=long)])
    assert len(node.title) <= 200


def test_ingest_replaces_newlines_in_derived_title(drevo_db: drevo.Drevo) -> None:
    """Multi-line page_content should not pollute the title with `\\n`."""
    [node] = ingest_documents(drevo_db, [SimpleDocument(page_content="line one\nline two")])
    assert "\n" not in node.title


# ── Embedder integration ─────────────────────────────────────────────


def test_ingest_with_embedder_stores_embedding(
    drevo_db: drevo.Drevo,
    det_embedder: Callable[[list[str]], list[list[float]]],
) -> None:
    [node] = ingest_documents(drevo_db, [SimpleDocument(page_content="x")], embedder=det_embedder)
    assert isinstance(node.properties["embedding"], list)


def test_ingest_without_embedder_omits_embedding(drevo_db: drevo.Drevo) -> None:
    [node] = ingest_documents(drevo_db, [SimpleDocument(page_content="x")])
    assert "embedding" not in node.properties


def test_ingest_embedder_size_mismatch_raises(drevo_db: drevo.Drevo) -> None:
    """An embedder that returns the wrong number of vectors is a contract bug."""

    def bad(_: list[str]) -> list[list[float]]:
        return [[0.0, 0.0]]  # always 1, regardless of input length

    with pytest.raises(ValueError):
        ingest_documents(
            drevo_db,
            [SimpleDocument(page_content="a"), SimpleDocument(page_content="b")],
            embedder=bad,
        )


def test_ingest_embedder_called_once_with_all_contents(
    drevo_db: drevo.Drevo,
) -> None:
    """The embedder is called *once* with the full list — RFC §8.2
    promises one batch call to make GPU embedders affordable.
    """
    calls: list[list[str]] = []

    def spy(texts: list[str]) -> list[list[float]]:
        calls.append(list(texts))
        return [[0.0] for _ in texts]

    ingest_documents(
        drevo_db,
        [SimpleDocument(page_content="a"), SimpleDocument(page_content="b")],
        embedder=spy,
    )
    assert calls == [["a", "b"]]


# ── Duck-typed Document Protocol ────────────────────────────────────


def test_ingest_accepts_arbitrary_document_shape(drevo_db: drevo.Drevo) -> None:
    """Any class with `page_content` + `metadata` ingests cleanly — that
    is the entire point of the duck-typed Protocol (RFC §8.1).
    """

    class FakeDoc:
        page_content: str = "duck-typed"
        metadata: dict[str, Any] = {"source": "test"}

    [node] = ingest_documents(drevo_db, [FakeDoc()])
    assert node.properties["text"] == "duck-typed"
