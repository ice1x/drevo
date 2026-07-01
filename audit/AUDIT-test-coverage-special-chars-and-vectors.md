# Test-coverage audit — special characters in content & vector functionality

Date: 2026-06-29

Driven by the questions: *do automated tests exist for special characters
inside content? for vector functionality? integration tests? if not — plan
them.*

## Summary verdict

| Area | Unit | Integration | Verdict |
|---|---|---|---|
| Special characters in content | ✅ | ✅ | Already broad; one storage-layer gap **now closed** (see below) |
| Vector functionality | ✅ (41) | ✅ (72 Rust + 29 Python, incl. Cypher `similar()` + graph-RAG e2e) | Comprehensive; no functional gap |

Both areas already had real coverage. The only meaningful missing piece was a
characterization suite pinning the **storage layer's byte-exact round-trip**
of pathological content (NUL bytes, full control-char range, exotic whitespace,
oversized payloads, injection-shaped strings, special-char Cypher parameters).
That suite was added in this change.

## Special characters — existing coverage (where chars are *interpreted*)

- Cypher lexer escaping: `src/cypher/lexer.rs` inline tests + `tests/cypher_lexer_tests.rs` (`\'`, `\"`, `\\`, `\n\r\t`, `\u{...}`, doubled backticks, CJK, error cases).
- Cypher parser: `tests/cypher_parser_tests.rs` (Cyrillic identifiers, emoji in strings, backtick identifiers with spaces).
- FTS tokenization/normalization: `tests/fts_tokenizer_tests.rs`, `tests/fts_recall_tests.rs`, `tests/fts_audit_tests.rs`, `tests/proptest_fts_tokenizer.rs` (emoji, CJK, Cyrillic, Hebrew RTL, Arabic, combining diacritics, zero-width chars, punctuation-only/whitespace-only queries, 100K-char panic guard, total/idempotent normalization property).
- GraphML XML escaping: `tests/graphml_export_tests.rs` (`< > & " '` escaping; Unicode + ZWJ emoji in titles/bodies/property keys).
- Bolt PackStream: `tests/bolt_packstream_tests.rs` (multi-byte UTF-8 byte-length vs char-length).
- FFI: `tests/ffi_tests.rs` (invalid-UTF-8 rejection at the boundary).
- Python property fuzz: `drevo-py/tests/integration/test_property_roundtrip.py` (hypothesis, non-surrogate Unicode round-trip through FFI → redb).

## Special characters — gap closed in this change

New file: `tests/special_chars_content_tests.rs` (16 tests, all green). It
pins the storage/Cypher contract that arbitrary content round-trips verbatim:

- NUL bytes — mid-string, leading/trailing, lone, runs.
- Control characters — explicit C0 set, the full `0x01..=0x1F` + DEL `0x7F` interleaved, C1 `0x80..=0x9F`.
- Unicode whitespace — NBSP, en quad, em space, ideographic space, narrow NBSP, line/paragraph separators, ZWSP.
- Oversized content — ~1.5 MiB title/body; 2000-element property array.
- Injection-shaped content stored as inert data — Cypher/SQL/template/HTML payloads round-trip and create no phantom nodes.
- Quote/backslash-heavy content.
- Edge property special chars.
- Cypher string **parameters** carrying NUL/escape/quote/Unicode — pass through `RETURN $p` and persist verbatim via `CREATE (n {title:$t})`.

Result: drevo preserves all of the above byte-for-byte — no bug found, contract
now regression-guarded.

## Vector functionality — existing coverage (no gap)

- Unit (41): `src/vector/{mod,distance,hnsw,store}.rs` — Vector parsing/JSON, dot/cosine/euclidean + error cases, HNSW insert/search/recall/determinism, store put/get/delete/batch/scan/rebuild.
- Integration (72 Rust): `tests/vector_distance_tests.rs`, `tests/vector_hnsw_tests.rs`, `tests/vector_persistence_tests.rs` (backend-parity Memory+Redb, durability across reopen, cascade delete, graph-RAG e2e), `tests/cypher_similar_tests.rs` (18 — `similar()` semantics, NULL propagation, errors, graph-RAG scenarios), `tests/python_embedding_helpers_tests.rs` (8 — Python API shape).
- Python (29): `drevo-py/tests/unit/test_vector_bridge.py`, `.../test_rag_embedding.py`, `drevo-py/tests/integration/test_vector_embeddings.py`, e2e graph-RAG.

Exposure: vectors reach users via the Rust API, the Python binding (6 methods +
`drevo.rag` helpers) and the Cypher `similar()` predicate. Not exposed over
HTTP (intentional — no server-side embedder) or as a dedicated MCP/Bolt tool.

## Deferred / lower-priority gaps (planned, not yet implemented)

These are LOW severity (the audit ranked them so) and are recorded here as a
backlog rather than implemented now:

- Vector **benchmarks** (no perf assertions today; largest test = 500×384).
- Concurrent/parallel vector operations under contention.
- Cypher escape-sequence corner cases at the integration level (unmatched
  `\u{` brace, truncated hex) — lexer already rejects these in unit tests.
- Case-folding expansion (ß→ss) and ligature normalization in FTS — proptest
  documents the caveat; explicit assertion would clarify intent.
- Unicode-space stripping semantics in FTS normalization (NBSP, en quad) as an
  explicit assertion (currently implied by the alphanumeric-only property).
- Surrogate-code-point rejection as an explicit documented test (proptest
  excludes them today).

<!-- ci docs-only gate smoke test 2026-07-01 -->
