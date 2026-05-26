"""Unit tests for `MMRReranker` — closed-form math (00118).

The MMR algorithm has no `Drevo` dependency. The unit tests fix the
embedder (so cosine similarities are determined), the candidate scores
(so relevance is determined), and assert the picking order in closed
form.

`lambda_` semantics — RFC §10 Q-4:
  * 1.0 → pure relevance (highest score first)
  * 0.0 → pure diversity (after the first pick, minimise similarity)
  * (0, 1) → linear blend

We always pin `lambda_` to a definite end of the spectrum in these
tests — the in-between behaviour is left to the integration suite.
"""

from __future__ import annotations

from typing import Any, Callable

import pytest

from drevo.rag import MMRReranker


# ── pure relevance (lambda_ = 1.0) ───────────────────────────────────


def test_pure_relevance_picks_highest_score_first(
    make_scored: Callable[[str, float], Any],
    det_embedder: Callable[[list[str]], list[list[float]]],
) -> None:
    candidates = [make_scored("a", 0.1), make_scored("b", 0.9), make_scored("c", 0.5)]
    picks = MMRReranker(lambda_=1.0).rerank(candidates, embedder=det_embedder, k=1)
    assert picks[0].node.title == "b"


def test_pure_relevance_orders_top_k_by_score(
    make_scored: Callable[[str, float], Any],
    det_embedder: Callable[[list[str]], list[list[float]]],
) -> None:
    candidates = [make_scored("a", 0.1), make_scored("b", 0.9), make_scored("c", 0.5)]
    picks = MMRReranker(lambda_=1.0).rerank(candidates, embedder=det_embedder, k=3)
    assert [p.node.title for p in picks] == ["b", "c", "a"]


# ── pure diversity (lambda_ = 0.0) ───────────────────────────────────


def test_pure_diversity_avoids_identical_titles(
    make_scored: Callable[[str, float], Any],
    orthogonal_embedder: Callable[[list[str]], list[list[float]]],
) -> None:
    """With `lambda_=0.0` and orthogonal embeddings, the picker should
    prefer a *novel* candidate over a duplicate of the first pick.
    """
    candidates = [
        make_scored("alpha", 0.9),  # first pick — highest score
        make_scored("alpha", 0.85),  # near-dup title → near-identical embedding
        make_scored("beta", 0.5),  # orthogonal embedding → maximises diversity
    ]
    picks = MMRReranker(lambda_=0.0).rerank(candidates, embedder=orthogonal_embedder, k=2)
    assert picks[1].node.title == "beta"


def test_pure_diversity_first_pick_is_highest_score(
    make_scored: Callable[[str, float], Any],
    orthogonal_embedder: Callable[[list[str]], list[list[float]]],
) -> None:
    """Even at `lambda_=0.0` the *first* pick is the highest-relevance
    candidate (the MMR objective collapses to relevance with no prior
    selection).
    """
    candidates = [
        make_scored("foo", 0.1),
        make_scored("bar", 0.9),
        make_scored("baz", 0.5),
    ]
    picks = MMRReranker(lambda_=0.0).rerank(candidates, embedder=orthogonal_embedder, k=1)
    assert picks[0].node.title == "bar"


# ── edge cases ───────────────────────────────────────────────────────


def test_k_zero_returns_empty(
    make_scored: Callable[[str, float], Any],
    det_embedder: Callable[[list[str]], list[list[float]]],
) -> None:
    picks = MMRReranker().rerank([make_scored("a", 1.0)], embedder=det_embedder, k=0)
    assert picks == []


def test_k_larger_than_input_returns_all(
    make_scored: Callable[[str, float], Any],
    det_embedder: Callable[[list[str]], list[list[float]]],
) -> None:
    candidates = [make_scored("a", 1.0), make_scored("b", 0.5)]
    picks = MMRReranker().rerank(candidates, embedder=det_embedder, k=10)
    assert {p.node.title for p in picks} == {"a", "b"}


def test_empty_candidates_returns_empty(
    det_embedder: Callable[[list[str]], list[list[float]]],
) -> None:
    """No candidates ⇒ no embedder call ⇒ empty list."""
    calls: list[list[str]] = []

    def spy(texts: list[str]) -> list[list[float]]:
        calls.append(list(texts))
        return [[0.0] for _ in texts]

    picks = MMRReranker().rerank([], embedder=spy, k=3)
    assert picks == []
    assert calls == []  # never reached the embedder


def test_single_candidate_returns_one(
    make_scored: Callable[[str, float], Any],
    det_embedder: Callable[[list[str]], list[list[float]]],
) -> None:
    picks = MMRReranker().rerank([make_scored("solo", 0.7)], embedder=det_embedder, k=5)
    assert len(picks) == 1


def test_embedder_size_mismatch_raises(
    make_scored: Callable[[str, float], Any],
) -> None:
    """An embedder returning the wrong count is a contract bug — raises."""
    candidates = [make_scored("a", 1.0), make_scored("b", 0.5)]

    def bad(texts: list[str]) -> list[list[float]]:
        return [[1.0]]  # only one vector for two candidates

    with pytest.raises(ValueError):
        MMRReranker().rerank(candidates, embedder=bad, k=2)


def test_default_lambda_is_one_half() -> None:
    """`MMRReranker()` default has `lambda_ == 0.5` — the canonical
    Carbonell & Goldstein 1998 starting point.
    """
    assert MMRReranker().lambda_ == pytest.approx(0.5)


def test_lambda_field_round_trips() -> None:
    """`lambda_` is a public field — callers can read what they set."""
    r = MMRReranker(lambda_=0.7)
    assert r.lambda_ == pytest.approx(0.7)


def test_negative_k_returns_empty(
    make_scored: Callable[[str, float], Any],
    det_embedder: Callable[[list[str]], list[list[float]]],
) -> None:
    """`k < 0` is treated like `k == 0` — defensive, matches the
    `if k <= 0` short-circuit in `rerank`.
    """
    picks = MMRReranker().rerank([make_scored("a", 1.0)], embedder=det_embedder, k=-3)
    assert picks == []
