"""Unit tests for `CALL drevo.rag.retrieve` / `CALL drevo.rag.context` from
Python — the graph-RAG pipeline (seeds → expansion → ranking → token-budgeted
text) run inside the database in one statement.

The embedded handle has no text embedder, so the seeds come from BM25 and
`mode` reports `'fts'`.

The fixture is a CBT journal: a person wrote an entry that holds an automatic
thought, which is an instance of a distortion.
"""

from __future__ import annotations

import json

import pytest
from faker import Faker

import drevo
from drevo.rag import Retriever

_CONTEXT = (
    "CALL drevo.rag.context($q, $config) "
    "YIELD text, tokens, format, nodeIds, relIds, truncated, mode, traceId "
    "RETURN text, tokens, format, nodeIds, relIds, truncated, mode, traceId"
)

_RETRIEVE = (
    "CALL drevo.rag.retrieve($q, $config) "
    "YIELD node, seedScore, distance, pprScore, rank, via, truncated "
    "RETURN node.title AS title, seedScore, distance, pprScore, rank, via, truncated"
)


@pytest.fixture
def journal(drevo_db: drevo.Drevo, fake: Faker) -> dict[str, str]:
    """`person -WROTE-> entry -CONTAINS-> thought -IS_A-> distortion`; the
    entry's title holds a made-up word only it carries."""
    word: str = fake.unique.lexify("zq??????").lower()
    names = {
        "word": word,
        "person": fake.unique.first_name(),
        "entry": f"{word} {fake.unique.day_of_week()}",
        "thought": fake.unique.sentence(nb_words=5),
        "distortion": fake.unique.word().capitalize(),
    }
    drevo_db.execute(
        "CREATE (:Person {title: $person})-[:WROTE]->"
        "(:Entry {title: $entry})-[:CONTAINS]->"
        "(:Thought {title: $thought})-[:IS_A]->(:Distortion {title: $distortion})",
        names,
    )
    return names


def _context(db: drevo.Drevo, query: str, config: dict | None = None) -> dict:
    rows = db.execute(_CONTEXT, {"q": query, "config": config}).rows
    assert len(rows) == 1
    return rows[0]


def test_retrieve_returns_the_seed_first_then_the_walk(
    drevo_db: drevo.Drevo, journal: dict[str, str]
) -> None:
    rows = drevo_db.execute(_RETRIEVE, {"q": journal["word"], "config": {"hops": 2}}).rows
    assert rows[0]["title"] == journal["entry"]
    assert rows[0]["distance"] == 0
    assert isinstance(rows[0]["seedScore"], float)
    assert rows[0]["via"] is None
    assert [r["rank"] for r in rows] == list(range(1, len(rows) + 1))
    assert {r["title"] for r in rows} == {
        journal["entry"],
        journal["person"],
        journal["thought"],
        journal["distortion"],
    }
    assert all(r["seedScore"] is None for r in rows[1:])
    assert all(isinstance(r["via"], dict) for r in rows[1:])
    assert abs(sum(r["pprScore"] for r in rows) - 1.0) < 1e-3


def test_context_is_markdown_within_the_budget(
    drevo_db: drevo.Drevo, journal: dict[str, str]
) -> None:
    row = _context(drevo_db, journal["word"])
    assert row["format"] == "markdown"
    assert row["mode"] == "fts"
    assert row["traceId"] is None
    assert row["truncated"] is False
    assert row["text"].startswith(f"## Seeds\n- {journal['entry']} (kind=Entry, id=")
    assert "\n\n## Neighbours\n" in row["text"]
    assert " -[WROTE]-> " in row["text"].split("## Edges\n", 1)[1]
    assert row["tokens"] == -(-len(row["text"]) // 4)
    assert all(isinstance(i, int) for i in row["nodeIds"] + row["relIds"])


def test_each_format_and_tokenizer_holds_the_budget(
    drevo_db: drevo.Drevo, journal: dict[str, str]
) -> None:
    for fmt in ("markdown", "triples", "json"):
        for tokenizer in ("approx", "words"):
            row = _context(
                drevo_db,
                journal["word"],
                {"format": fmt, "tokenizer": tokenizer, "tokenBudget": 50},
            )
            assert row["format"] == fmt
            assert row["tokens"] <= 50
    payload = json.loads(_context(drevo_db, journal["word"], {"format": "json"})["text"])
    assert sorted(payload) == ["nodes", "rels", "stats"]


def test_the_same_question_gives_the_same_text(
    drevo_db: drevo.Drevo, journal: dict[str, str]
) -> None:
    first = _context(drevo_db, journal["word"], {"format": "triples"})
    assert _context(drevo_db, journal["word"], {"format": "triples"}) == first


def test_no_match_gives_an_empty_context(drevo_db: drevo.Drevo, fake: Faker) -> None:
    row = _context(drevo_db, fake.unique.lexify("qx??????"))
    assert row["text"] == ""
    assert row["tokens"] == 0
    assert row["nodeIds"] == [] and row["relIds"] == []


def test_a_bad_config_raises_naming_the_key(drevo_db: drevo.Drevo, journal: dict[str, str]) -> None:
    with pytest.raises(drevo.CypherError, match="tokenBudget"):
        _context(drevo_db, journal["word"], {"tokenBudget": 10})
    with pytest.raises(drevo.CypherError, match="depth"):
        _context(drevo_db, journal["word"], {"depth": 2})


def test_server_and_client_renderings_cover_the_same_graph(
    drevo_db: drevo.Drevo, journal: dict[str, str]
) -> None:
    """The server's markdown and `Context.to_text('markdown')` describe the
    same nodes and relationships for the same seed and depth, each node on
    the same `- title (kind=K, id=N)` line."""
    server = _context(drevo_db, journal["word"], {"hops": 2})
    client = Retriever(drevo_db, hops=2).retrieve(journal["word"])
    client_nodes = {n.id for n in client.seeds + client.neighbours}
    client_edges = {e.id for e in client.edges}
    assert set(server["nodeIds"]) == client_nodes
    assert set(server["relIds"]) == client_edges
    client_text = client.to_text(format="markdown")
    for node in client.seeds + client.neighbours:
        assert node.title in client_text
        assert f"- {node.title} (kind={node.kind}, id={node.id})" in server["text"]


def test_server_markdown_matches_context_to_text(
    drevo_db: drevo.Drevo, journal: dict[str, str]
) -> None:
    """#783 promised that `drevo.rag.context`'s markdown has the same shape
    as `drevo.rag.Context.to_text('markdown')`, so client- and server-side
    renderings match (#808 point 4). For one seed walked two hops (the whole
    journal fits the budget) the two texts must be equal: same headings, same
    lines, same order."""
    server = _context(
        drevo_db, journal["word"], {"hops": 2, "format": "markdown", "tokenBudget": 4000}
    )
    assert server["truncated"] is False
    client = Retriever(drevo_db, hops=2).retrieve(journal["word"])
    expected = client.to_text(format="markdown")
    assert server["text"] == expected, (
        "#808: server markdown differs from Context.to_text('markdown') — "
        f"server:\n{server['text']}\nclient:\n{expected}"
    )
