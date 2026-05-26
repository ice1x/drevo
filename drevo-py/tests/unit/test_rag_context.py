"""Unit tests for `Context.to_text` formatting (00118).

The 00120 e2e suite asserts byte-equality across runs; this file
asserts the *shape* of each format (markdown headings, JSON validity,
turtle prefixes) so a formatting regression surfaces before the e2e
gates take a full run.
"""

from __future__ import annotations

import json

import pytest

import drevo
from drevo.rag import Retriever


@pytest.fixture
def small_context(drevo_db: drevo.Drevo, connected_chain: dict[str, drevo.Node]):
    """A `Context` covering the full `a → b → c → d` chain.

    Used by every formatting test so the assertion targets the rendering
    layer and not the retrieval layer.
    """
    return Retriever(drevo_db, hops=3).retrieve(connected_chain["a"].id)


# ── format selection ────────────────────────────────────────────────


def test_default_format_is_markdown(small_context) -> None:
    """Calling `.to_text()` with no kwarg returns markdown."""
    out = small_context.to_text()
    assert out.startswith("## Seeds")


def test_unknown_format_raises_value_error(small_context) -> None:
    with pytest.raises(ValueError):
        small_context.to_text(format="yaml")


# ── markdown ─────────────────────────────────────────────────────────


def test_markdown_has_seeds_heading(small_context) -> None:
    out = small_context.to_text(format="markdown")
    assert "## Seeds" in out


def test_markdown_has_neighbours_heading(small_context) -> None:
    out = small_context.to_text(format="markdown")
    assert "## Neighbours" in out


def test_markdown_has_edges_heading(small_context) -> None:
    out = small_context.to_text(format="markdown")
    assert "## Edges" in out


def test_markdown_lists_each_seed_node(small_context) -> None:
    out = small_context.to_text(format="markdown")
    for seed in small_context.seeds:
        assert seed.title in out


def test_markdown_lists_each_neighbour_node(small_context) -> None:
    out = small_context.to_text(format="markdown")
    for n in small_context.neighbours:
        assert n.title in out


def test_markdown_is_deterministic(small_context) -> None:
    """Same context → byte-equal output across calls (RFC §8.4)."""
    assert small_context.to_text(format="markdown") == small_context.to_text(format="markdown")


# ── json ─────────────────────────────────────────────────────────────


def test_json_is_valid_json(small_context) -> None:
    json.loads(small_context.to_text(format="json"))


def test_json_payload_has_top_level_keys(small_context) -> None:
    payload = json.loads(small_context.to_text(format="json"))
    assert {"seeds", "neighbours", "edges", "stats"} <= set(payload.keys())


def test_json_seeds_count_matches_context(small_context) -> None:
    payload = json.loads(small_context.to_text(format="json"))
    assert len(payload["seeds"]) == len(small_context.seeds)


def test_json_neighbours_count_matches_context(small_context) -> None:
    payload = json.loads(small_context.to_text(format="json"))
    assert len(payload["neighbours"]) == len(small_context.neighbours)


def test_json_edges_count_matches_context(small_context) -> None:
    payload = json.loads(small_context.to_text(format="json"))
    assert len(payload["edges"]) == len(small_context.edges)


def test_json_stats_match_context_stats(small_context) -> None:
    payload = json.loads(small_context.to_text(format="json"))
    assert payload["stats"]["seed_count"] == small_context.stats.seed_count


def test_json_is_deterministic(small_context) -> None:
    assert small_context.to_text(format="json") == small_context.to_text(format="json")


def test_json_node_records_carry_uuid_string(small_context) -> None:
    """The JSON renderer surfaces UUIDs as their canonical string form."""
    payload = json.loads(small_context.to_text(format="json"))
    for record in payload["seeds"] + payload["neighbours"]:
        assert isinstance(record["uuid"], str)
        assert len(record["uuid"]) == 36  # canonical UUID textual length


# ── turtle ───────────────────────────────────────────────────────────


def test_turtle_emits_drevo_prefix(small_context) -> None:
    out = small_context.to_text(format="turtle")
    assert "@prefix drevo:" in out


def test_turtle_emits_rdf_prefix(small_context) -> None:
    out = small_context.to_text(format="turtle")
    assert "@prefix rdf:" in out


def test_turtle_includes_each_node_id_uri(small_context) -> None:
    out = small_context.to_text(format="turtle")
    for n in small_context.seeds + small_context.neighbours:
        assert f"drevo:node-{n.id}" in out


def test_turtle_is_deterministic(small_context) -> None:
    assert small_context.to_text(format="turtle") == small_context.to_text(format="turtle")


def test_turtle_escapes_double_quotes_in_title(drevo_db: drevo.Drevo) -> None:
    """Titles containing `"` must be escaped so the Turtle is well-formed."""
    drevo_db.create_node(drevo.NewNode(kind="note", title='hello "world"'))
    ctx = Retriever(drevo_db, hops=0).retrieve('hello "world"')
    out = ctx.to_text(format="turtle")
    assert r"\"world\"" in out


# ── empty context ────────────────────────────────────────────────────


def test_to_text_handles_empty_context(drevo_db: drevo.Drevo) -> None:
    """A Retriever that matches nothing still renders without raising."""
    ctx = Retriever(drevo_db).retrieve("nothing-matches-this-query")
    md = ctx.to_text(format="markdown")
    assert "(none)" in md  # the placeholder bullet for an empty list
