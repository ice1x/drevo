"""Phase 16 task `00118` — Python unit-test suite for drevo-py.

Per `audit/RFC-python-api.md` §2 the package's test tree is layered:

    drevo-py/tests/
        ├── unit/         # 00118 — ~80 focused, mocked-where-possible cases
        ├── integration/  # 00119 — real redb backend, cross-component
        └── e2e/          # 00120 — five scenarios + graph-RAG scenario

This subpackage holds the unit tier. Each test asserts one thing,
dependencies (storage, embedder) are mocked where the assertion does
not require real I/O, and a single test failure points at exactly one
broken contract.

The legacy runtime suites — `drevo-py/tests/test_shim.py` (00116) and
`drevo-py/tests/test_rag.py` (00117) — stay at the package root so the
contract those tasks shipped does not migrate underneath them. New
fine-grained units belong here.
"""
