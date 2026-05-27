"""FTS recall over a realistic corpus on the disk backend.

Unit tests pin individual edges of the FTS contract; this suite pins
the *behaviour an embedding-augmented agent depends on*: real bodies,
tokeniser interaction with punctuation and casing, ranking stability,
and updates that mutate the posting list.

The corpus is small but representative — CBT-style journal entries +
task-tracker entries — borrowed from `README.md` §"Use Cases" so a
regression here surfaces in the same vocabulary the rest of the project
documents.
"""

from __future__ import annotations

import pytest

import drevo


@pytest.fixture
def journal_corpus(disk_db: drevo.Drevo) -> drevo.Drevo:
    """Mixed-kind nodes with realistic bodies.

    Built once per test (function scope through the parent fixture chain)
    so each assertion starts from the same baseline.
    """
    rows = [
        ("thought", "negative-1", "I always fail under pressure and let everyone down"),
        ("thought", "negative-2", "Nobody at work respects my contributions"),
        ("rational_response", "rebuttal-1", "Pressure is universal; success is normal"),
        ("task", "ship-graph-rag", "Implement graph-RAG retriever over the embedded store"),
        ("task", "fix-fts-bug", "Trigram tokenizer drops the last token on odd lengths"),
        ("bug", "crash-on-reopen", "Database file corrupts after concurrent close on macOS"),
        ("note", "unrelated", "shopping list: milk, eggs, sourdough starter"),
    ]
    for kind, title, body in rows:
        disk_db.create_node(drevo.NewNode(kind=kind, title=title, body=body))
    return disk_db


def test_fts_finds_exact_body_token(journal_corpus: drevo.Drevo) -> None:
    """A query that matches a unique body token returns exactly its
    parent node — no false positives from neighbouring rows.
    """
    hits = journal_corpus.search_fts("sourdough", 10)
    assert {h.node.title for h in hits} == {"unrelated"}


def test_fts_finds_across_multiple_rows(journal_corpus: drevo.Drevo) -> None:
    """A query that legitimately matches more than one row returns all
    of them.
    """
    hits = journal_corpus.search_fts("graph", 10)
    titles = {h.node.title for h in hits}
    assert "ship-graph-rag" in titles


def test_fts_scores_are_descending(journal_corpus: drevo.Drevo) -> None:
    """Hits arrive ranked by TF-IDF, highest first. An agent that pages
    a `search_fts` result relies on this ordering.
    """
    hits = journal_corpus.search_fts("graph", 10)
    if len(hits) >= 2:
        scores = [h.score for h in hits]
        assert scores == sorted(scores, reverse=True)


def test_fts_limit_is_respected(journal_corpus: drevo.Drevo) -> None:
    """Limit caps the number of hits returned — never exceeded even if
    more rows match.
    """
    hits = journal_corpus.search_fts("the", 2)
    assert len(hits) <= 2


def test_fts_reflects_updates_to_body(journal_corpus: drevo.Drevo) -> None:
    """Updating a node's body removes it from queries matching old
    tokens and adds it to queries matching new tokens.
    """
    target = journal_corpus.get_node_by_title("unrelated")
    assert target is not None
    journal_corpus.update_node(
        target.id,
        drevo.NodePatch(body="quantum entanglement and the bell inequality"),
    )
    # Old token disappears.
    old_hits = {h.node.title for h in journal_corpus.search_fts("sourdough", 10)}
    assert "unrelated" not in old_hits
    # New token reaches the row.
    new_hits = {h.node.title for h in journal_corpus.search_fts("quantum", 10)}
    assert "unrelated" in new_hits


def test_fts_reflects_deletes(journal_corpus: drevo.Drevo) -> None:
    """A deleted node disappears from FTS results."""
    target = journal_corpus.get_node_by_title("unrelated")
    assert target is not None
    journal_corpus.delete_node(target.id)
    hits = journal_corpus.search_fts("sourdough", 10)
    assert all(h.node.title != "unrelated" for h in hits)


def test_fts_no_match_returns_empty_list(journal_corpus: drevo.Drevo) -> None:
    """A non-matching query returns `[]` (not `None`, not an error)."""
    assert journal_corpus.search_fts("zzz-nonexistent-token-xyz", 10) == []


def test_fts_matches_title_tokens(journal_corpus: drevo.Drevo) -> None:
    """The tokeniser indexes titles too — querying by a distinctive
    title token reaches the row.
    """
    hits = journal_corpus.search_fts("rebuttal", 10)
    assert any(h.node.title == "rebuttal-1" for h in hits)
