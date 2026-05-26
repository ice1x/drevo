"""Maximum Marginal Relevance reranker — `MMRReranker`.

Closed-form pure-Python math. No `drevo.Drevo` dependency: the
reranker works on a list of candidates and an embedder callable. This
is deliberate per RFC §8.5 — the algorithm has zero storage I/O so the
unit tests in `00118` can drive it without spinning up a database.

The semantics of `lambda_` are fixed in RFC §10 Q-4:

* `lambda_ = 1.0` → pure relevance (sort by score, top-k)
* `lambda_ = 0.0` → pure diversity (after the first pick, minimise
  similarity to already-selected items)
* `0 < lambda_ < 1` → linear blend

Matches the convention in the original MMR paper (Carbonell &
Goldstein, 1998).
"""

from __future__ import annotations

import math
from dataclasses import dataclass
from typing import Any, Callable, Sequence


def _cosine(a: Sequence[float], b: Sequence[float]) -> float:
    """Cosine similarity. Returns 0.0 for any zero vector — that matches
    the limit of `dot(a, b) / (|a| * |b|)` as either norm goes to zero
    *and* avoids a NaN propagating into the reranker's max operator."""
    if len(a) != len(b):
        raise ValueError(
            f"_cosine: vector dim mismatch ({len(a)} != {len(b)}) — the "
            f"embedder returned inconsistently-sized vectors"
        )
    dot = sum(x * y for x, y in zip(a, b))
    na = math.sqrt(sum(x * x for x in a))
    nb = math.sqrt(sum(x * x for x in b))
    if na == 0.0 or nb == 0.0:
        return 0.0
    return dot / (na * nb)


@dataclass
class MMRReranker:
    """Maximum Marginal Relevance reranker.

    Given candidate items (anything with `.node.title: str` and
    `.score: float`) and an embedder, picks top-k candidates that
    maximise relevance while minimising redundancy.

    Usage:

        mmr = MMRReranker(lambda_=0.7)
        picks = mmr.rerank(candidates, embedder=my_embedder, k=5)
    """

    lambda_: float = 0.5

    def rerank(
        self,
        candidates: Sequence[Any],
        *,
        embedder: Callable[[list[str]], list[list[float]]],
        k: int,
    ) -> list[Any]:
        """Pick top-`k` candidates by MMR.

        `candidates` may be `drevo.ScoredNode` or any duck-typed object
        with `.node.title: str` and `.score: float`. Returns a list in
        selection order (first pick is always highest relevance).
        """
        if k <= 0 or not candidates:
            return []

        # Embed once up-front. The embedder is the expensive operation
        # in any real retrieval pipeline — we never call it inside the
        # picking loop.
        texts = [str(getattr(c.node, "title", "")) for c in candidates]
        embeddings = embedder(texts)
        if len(embeddings) != len(candidates):
            raise ValueError(
                f"embedder returned {len(embeddings)} vectors for "
                f"{len(candidates)} candidates — counts must match"
            )

        lam = self.lambda_
        remaining = list(range(len(candidates)))
        selected: list[int] = []

        while remaining and len(selected) < k:
            best_idx = remaining[0]
            best_score = -math.inf
            for i in remaining:
                relevance = float(candidates[i].score)
                if not selected:
                    mmr = relevance
                else:
                    max_sim = max(
                        _cosine(embeddings[i], embeddings[j]) for j in selected
                    )
                    mmr = lam * relevance - (1.0 - lam) * max_sim
                if mmr > best_score:
                    best_score = mmr
                    best_idx = i
            selected.append(best_idx)
            remaining.remove(best_idx)

        return [candidates[i] for i in selected]
