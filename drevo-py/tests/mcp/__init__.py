"""Phase 10.5 task ``00127`` — MCP-level agentic workload (layer 4).

This package holds the Python harness that drives the real
``drevo-mcp`` stdio server the way a live Claude Code / Cline session
does: spawn it as a child process, speak MCP JSON-RPC 2.0 over its
stdin/stdout, and fire a representative mix of read-only tool calls at
it under sustained load.

It is the *fourth* layer of the five-layer agentic-workload model the
Phase 10.5 hardening band measures independently so a perf regression
can be bisected to the offending layer:

  1. Rust API (raw)        — ``tests/agentic_workload_rust_api.rs`` (00123)
  2. Cypher (parse+exec)   — ``tests/agentic_workload_cypher.rs``  (00128)
  3. Python API (PyO3)     — the ``drevo`` extension module
  4. MCP stdio (this file) — ``drevo-py/tests/mcp/agentic_workload.py``
  5. Bolt wire             — Phase 11

Unlike the other test tiers under ``drevo-py/tests/`` (``unit/``,
``integration/``, ``e2e/``), this directory is deliberately NOT one of
the paths the Python CI matrix (``.github/workflows/python.yml``)
collects — its ``CIBW_TEST_COMMAND`` runs ``pytest tests/unit/ &&
pytest tests/integration/ && pytest tests/e2e/`` explicitly. The MCP
workload needs the *Rust* ``drevo-mcp`` binary on disk, which is not
present inside cibuildwheel's manylinux sandbox, so keeping it out of
those three subdirs gives it zero PR-path cost. It runs on demand /
nightly via ``pytest drevo-py/tests/mcp/`` once the binary is built.
"""
