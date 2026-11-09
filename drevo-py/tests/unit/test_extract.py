"""Unit tests for `drevo.extract.*` from Python, through the Cypher API.

The embedded handle has no chat endpoint, so the model-backed procedures
report the capability error naming `drevo.extract.status()`; config errors
are reported before any model call. `drevo.extract.chunks` needs no model:
its passages are checked against faker-generated manuscripts and, with
hypothesis, against arbitrary text (Cyrillic and emoji included) — offsets
are exact character positions, passages respect the size, never split a
character, and overlap rather than leave a gap.
"""

from __future__ import annotations

from typing import Any

import pytest
from faker import Faker
from hypothesis import given, settings
from hypothesis import strategies as st

import drevo


def _chunks(db: drevo.Drevo, text: str, size: int) -> list[dict[str, Any]]:
    return db.execute(
        "CALL drevo.extract.chunks($text, {chunkChars: $size}) "
        "YIELD position, offset, length, text RETURN position, offset, length, text",
        {"text": text, "size": size},
    ).rows


def _check(text: str, size: int, rows: list[dict[str, Any]]) -> None:
    if not text:
        assert rows == []
        return
    assert rows[0]["offset"] == 0
    assert rows[-1]["offset"] + rows[-1]["length"] == len(text)
    for i, row in enumerate(rows):
        assert row["position"] == i
        assert 1 <= row["length"] <= size
        # Python indexes code points, as drevo counts characters.
        assert text[row["offset"] : row["offset"] + row["length"]] == row["text"]
    for prev, nxt in zip(rows, rows[1:]):
        assert prev["offset"] < nxt["offset"] <= prev["offset"] + prev["length"]


def test_chunks_of_a_manuscript_are_exact(drevo_db: drevo.Drevo, fake: Faker) -> None:
    text = "\n\n".join(fake.paragraph(nb_sentences=6) for _ in range(8))
    rows = _chunks(drevo_db, text, 300)
    assert len(rows) > 2
    _check(text, 300, rows)


@settings(max_examples=60, deadline=None)
@given(
    text=st.text(alphabet=st.sampled_from(list("Дана ушла. Acme 🚀👩‍🚀\n.!? xyzЖ")), max_size=400),
    size=st.integers(min_value=20, max_value=120),
)
def test_chunk_invariants_hold_for_any_text(text: str, size: int) -> None:
    with drevo.Drevo.open_in_memory() as db:
        _check(text, size, _chunks(db, text, size))


def test_model_backed_procedures_need_a_chat_endpoint(drevo_db: drevo.Drevo, fake: Faker) -> None:
    text = fake.paragraph()
    for query in [
        "CALL drevo.extract.entities($text) YIELD name RETURN name",
        "CALL drevo.extract.facts($text) YIELD quote RETURN quote",
        "CALL drevo.extract.ingest($text, {source: 'doc-1'}) YIELD facts RETURN facts",
    ]:
        with pytest.raises(drevo.CypherError, match=r"drevo\.extract\.status\(\)"):
            drevo_db.execute(query, {"text": text})
    status = drevo_db.execute(
        "CALL drevo.extract.status() YIELD configured, lastError RETURN configured, lastError"
    ).rows
    assert status == [{"configured": False, "lastError": None}]


def test_a_bad_config_is_refused_first(drevo_db: drevo.Drevo) -> None:
    with pytest.raises(drevo.CypherError, match="unknown config key `chunk`"):
        drevo_db.execute("CALL drevo.extract.entities('x', {chunk: 1}) YIELD name RETURN name")
    with pytest.raises(drevo.CypherError, match="needs a `source`"):
        drevo_db.execute("CALL drevo.extract.ingest('x', {}) YIELD facts RETURN facts")
