"""Duck-typed `Document` protocol for `drevo.rag`.

The protocol exists so `drevo.rag.ingest_documents(...)` accepts any
object that quacks like a LangChain / LlamaIndex / Haystack ``Document``
without `drevo-py` taking a hard dependency on any of those frameworks
(RFC §8.1). The two attributes — ``page_content: str`` and
``metadata: dict`` — are exactly the intersection of the three
ecosystems' Document shapes, so a wrapper class is not needed for
interop; pass third-party Documents in directly.

`SimpleDocument` is the bundled reference implementation for tests and
for callers who don't want to pull in a framework just to ingest a few
strings.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Protocol, runtime_checkable


@runtime_checkable
class Document(Protocol):
    """Structural type for ingestable text+metadata records.

    Compatible with `langchain_core.documents.Document`,
    `llama_index.core.schema.Document`, and `haystack.dataclasses.Document`
    — all three expose `page_content: str` + `metadata: dict[str, Any]`.

    `@runtime_checkable` lets `ingest_documents` defensively call
    `isinstance(obj, Document)` to give a useful error before crashing
    inside the loop on a missing attribute.
    """

    page_content: str
    metadata: dict[str, Any]


@dataclass
class SimpleDocument:
    """Bundled `Document` implementation.

    Use this in tests and for one-off ingest jobs where pulling in a
    framework just to make Document instances would be overkill.
    """

    page_content: str
    metadata: dict[str, Any] = field(default_factory=dict)
