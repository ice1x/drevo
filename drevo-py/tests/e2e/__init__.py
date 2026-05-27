"""Phase 16 task `00120` — Python end-to-end test suite for drevo-py.

Per `audit/RFC-python-api.md` §2 the package's test tree is layered:

    drevo-py/tests/
        ├── unit/         # 00118 — focused, mocked-where-possible cases
        ├── integration/  # 00119 — real redb backend, cross-component
        └── e2e/          # 00120 — five scenarios + graph-RAG scenario  ← here

This tier is the **definition-of-done** suite for Phase 16. Each test
module drives one full end-to-end workflow inside a tempdir, exercises
the entire ``drevo`` + ``drevo.rag`` Python surface in the same shape
an LLM agent would, and asserts observable outputs (created-node
counts, traversal projections, retrieval ordering).

Scenario modules mirror the Rust + Cypher e2e suites under
``tests/scenario_*.rs`` so a regression in one layer surfaces in the
other:

* ``test_cbt_journal.py``      — Cognitive Behavioural Therapy journal
* ``test_story_editor.py``     — long-form fiction outliner
* ``test_task_manager.py``     — IT task / sprint board
* ``test_erp.py``              — enterprise resource planning
* ``test_bug_tracker.py``      — bug + regression report tracker
* ``test_graph_rag.py``        — RAG-specific scenario: ingest →
  retrieve → ``Context.to_text`` stable serialisation

Each scenario uses a deterministic stub embedder (zero network) and
the on-disk redb backend via ``Drevo.open(tempfile)`` so the
durability + GIL contracts inherited from 00119 carry through.

Cites: ``.claude/skills/drevo-tdd/SKILL.md`` §"E2E tests"; mirrors the
Phase 10 DoD requirement that the five scenario suites pass when
expressed in Cypher.
"""
