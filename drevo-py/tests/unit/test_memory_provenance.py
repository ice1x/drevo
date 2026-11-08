"""Unit tests for provenance of long-term memory from Python: sources,
their chunks, cited facts and `drevo.memory.evidence`, through the Cypher API.

A manuscript (`book-1`, title and URI from faker) has two chunks; the
character Dana is mentioned in one and a fact about her cites the other.
"""

from __future__ import annotations

from typing import Any

import pytest
from faker import Faker
from hypothesis import given, settings
from hypothesis import strategies as st

import drevo

_CHUNKS = [
    {"order": 0, "offset": 0, "text": "Dana opened the manuscript at dawn."},
    {"order": 1, "offset": 36, "text": "By noon Dana had joined Acme as an editor."},
]


def _manuscript(db: drevo.Drevo, title: str, uri: str) -> None:
    db.execute(
        "CALL drevo.memory.addSource('book-1', $uri, $title, 'document') YIELD node " "RETURN node",
        {"uri": uri, "title": title},
    )
    db.execute(
        "CALL drevo.memory.addChunks('book-1', $chunks) YIELD node RETURN node",
        {"chunks": _CHUNKS},
    )
    for name, kind in [("Dana", "PERSON"), ("Acme", "ORGANIZATION")]:
        db.execute(
            "CALL drevo.memory.rememberEntity(null, $name, $kind, null) YIELD node " "RETURN node",
            {"name": name, "kind": kind},
        )


@pytest.fixture
def book(drevo_db: drevo.Drevo, fake: Faker) -> dict[str, str]:
    """`book-1` with faker's title and URI; returns them."""
    meta = {"title": fake.sentence(nb_words=4), "uri": fake.uri()}
    _manuscript(drevo_db, meta["title"], meta["uri"])
    return meta


def test_add_source_round_trips_its_fields(drevo_db: drevo.Drevo, book: dict[str, str]) -> None:
    rows = drevo_db.execute(
        "MATCH (s:Source {id: 'book-1'}) RETURN s.title AS title, s.uri AS uri, " "s.kind AS kind"
    ).rows
    assert rows == [{"title": book["title"], "uri": book["uri"], "kind": "document"}]


def test_add_chunks_chains_next_and_is_idempotent(
    drevo_db: drevo.Drevo, book: dict[str, str]
) -> None:
    again = drevo_db.execute(
        "CALL drevo.memory.addChunks('book-1', $chunks) YIELD node " "RETURN node.id AS id",
        {"chunks": _CHUNKS},
    ).rows
    assert [r["id"] for r in again] == ["book-1#0", "book-1#1"]
    counts = drevo_db.execute(
        "MATCH (c:Chunk) WITH count(c) AS chunks "
        "MATCH (:Chunk)-[n:NEXT]->(:Chunk) RETURN chunks, count(n) AS links"
    ).rows
    assert counts == [{"chunks": 2, "links": 1}]


def test_cited_fact_shows_in_facts_at_and_evidence(
    drevo_db: drevo.Drevo, book: dict[str, str]
) -> None:
    drevo_db.execute(
        "CALL drevo.memory.rememberEntity(null, 'Dana', 'PERSON', null, $p) YIELD node "
        "RETURN node",
        {"p": {"chunk": "book-1#0", "surface": "Dana"}},
    )
    fact = drevo_db.execute(
        "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false, $p) YIELD rel "
        "RETURN rel.quote AS quote, rel.offset AS offset, rel.length AS length",
        {"p": {"chunk": "book-1#1", "quote": "joined Acme"}},
    ).rows
    assert fact == [{"quote": "joined Acme", "offset": 17, "length": 11}]

    facts = drevo_db.execute(
        "CALL drevo.memory.factsAt('Dana', null) YIELD relation, source, quote "
        "RETURN relation, source.title AS title, quote"
    ).rows
    assert facts == [{"relation": "WORKS_AT", "title": book["title"], "quote": "joined Acme"}]

    evidence = drevo_db.execute(
        "CALL drevo.memory.evidence('Dana', 5) YIELD chunk, quote, fact "
        "RETURN chunk.id AS chunk, quote, fact IS NULL AS mention"
    ).rows
    assert evidence == [
        {"chunk": "book-1#1", "quote": "joined Acme", "mention": False},
        {"chunk": "book-1#0", "quote": "Dana", "mention": True},
    ]


def test_errors_raise_cypher_error(drevo_db: drevo.Drevo, book: dict[str, str]) -> None:
    with pytest.raises(drevo.CypherError, match="no source `book-9`"):
        drevo_db.execute(
            "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false, " "{source: 'book-9'})"
        )
    with pytest.raises(drevo.CypherError, match="unknown provenance key `page`"):
        drevo_db.execute(
            "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false, {page: 1})"
        )


_TEXT = _CHUNKS[1]["text"]


def _assert_fact(db: drevo.Drevo, quote: str) -> list[dict[str, Any]]:
    return db.execute(
        "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false, $p) YIELD rel "
        "RETURN rel.quote AS quote, rel.offset AS offset, rel.length AS length",
        {"p": {"chunk": "book-1#1", "quote": quote}},
    ).rows


@settings(max_examples=40, deadline=None)
@given(bounds=st.tuples(st.integers(0, len(_TEXT)), st.integers(0, len(_TEXT))))
def test_any_slice_of_the_chunk_is_a_valid_quote(bounds: tuple[int, int]) -> None:
    start, end = sorted(bounds)
    quote = _TEXT[start:end]
    with drevo.Drevo.open_in_memory() as db:
        _manuscript(db, "Book One", "file:///one.md")
        [row] = _assert_fact(db, quote)
    assert row["quote"] == quote
    # The stored offset points at the quote's first occurrence.
    assert _TEXT[row["offset"] : row["offset"] + row["length"]] == quote


@settings(max_examples=40, deadline=None)
@given(quote=st.text(min_size=1, max_size=30).filter(lambda q: q not in _TEXT))
def test_a_quote_not_in_the_chunk_is_refused(quote: str) -> None:
    with drevo.Drevo.open_in_memory() as db:
        _manuscript(db, "Book One", "file:///one.md")
        with pytest.raises(drevo.CypherError, match="is not in the text of chunk `book-1#1`"):
            _assert_fact(db, quote)


@settings(max_examples=40, deadline=None)
@given(bounds=st.tuples(st.integers(0, len(_TEXT) - 1), st.integers(1, len(_TEXT))))
def test_an_explicit_offset_must_point_at_the_quote(bounds: tuple[int, int]) -> None:
    start, end = min(bounds), max(bounds)
    if start == end:
        end += 1
    quote = _TEXT[start:end]
    with drevo.Drevo.open_in_memory() as db:
        _manuscript(db, "Book One", "file:///one.md")
        [row] = db.execute(
            "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false, $p) "
            "YIELD rel RETURN rel.offset AS offset",
            {"p": {"chunk": "book-1#1", "quote": quote, "offset": start}},
        ).rows
        assert row["offset"] == start
        shifted = start + 1
        if _TEXT[shifted : shifted + len(quote)] == quote:
            return  # the text repeats the quote one character later
        with pytest.raises(drevo.CypherError, match=f"is not at offset {shifted} of chunk"):
            db.execute(
                "CALL drevo.memory.retractFact('Dana', 'WORKS_AT', 'Acme') YIELD rel " "RETURN rel"
            )
            db.execute(
                "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false, $p) "
                "YIELD rel RETURN rel",
                {"p": {"chunk": "book-1#1", "quote": quote, "offset": shifted}},
            )
