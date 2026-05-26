"""`drevo.rag` — graph-RAG idioms layer (Phase 16 task `00117`).

Pure-Python composition on top of the PyO3 bindings. No FFI calls, no
`unsafe`, no Rust dependency — every function in this package can be
read, edited, and tested with `pytest` alone, without compiling the
cdylib (RFC §2 wheel layout).

Public surface:
- `Document` / `SimpleDocument` — duck-typed Document protocol +
  reference implementation (RFC §8.1).
- `ingest_documents` + `IngestSchema` — batched node creation from a
  Document list (RFC §8.2).
- `Retriever` + `Context` + `ContextStats` — seed-to-context graph
  retrieval (RFC §8.3 + §8.4).
- `MMRReranker` — Maximum Marginal Relevance reranker for
  context-budget-aware re-ranking (RFC §8.5).
- `expand_neighborhood` — bounded BFS with `kind_filter` + `max_nodes`
  (00117 task description).
- `Neighborhood` — frozen dataclass returned by `expand_neighborhood`.

This module is opt-in: `import drevo` does NOT eagerly load `drevo.rag`.
Pull it in explicitly with `from drevo.rag import Retriever` (or
`from drevo import rag`) so `import drevo` stays cheap.
"""

from __future__ import annotations

from ._document import Document, SimpleDocument
from .ingest import IngestSchema, ingest_documents
from .neighborhood import Neighborhood, expand_neighborhood
from .rerank import MMRReranker
from .retriever import Context, ContextStats, Retriever

__all__ = [
    "Context",
    "ContextStats",
    "Document",
    "IngestSchema",
    "MMRReranker",
    "Neighborhood",
    "Retriever",
    "SimpleDocument",
    "expand_neighborhood",
    "ingest_documents",
]
