//! Phase 16 task `00118` — text-level scaffolding tests for the
//! `drevo-py` unit-test suite.
//!
//! These tests grep the on-disk layout of `drevo-py/tests/unit/` so the
//! Rust-only CI runners can gate the suite's *existence* and *shape*
//! without booting a Python interpreter. The runtime behaviour itself
//! is exercised by pytest inside the Python CI job (00122) and inside
//! cibuildwheel's `CIBW_TEST_COMMAND` for every wheel.
//!
//! What this file locks (per RFC §2 layout + the 00118 task description):
//!
//! 1. `drevo-py/tests/unit/__init__.py` exists — the package marker.
//! 2. `drevo-py/tests/unit/conftest.py` ships the shared fixtures
//!    (`drevo_db`, `mock_drevo`, `det_embedder`, `orthogonal_embedder`,
//!    `connected_chain`, `mixed_kind_neighbourhood`, `make_scored`).
//! 3. One test module per public surface area: handle, nodes, edges,
//!    traversal, FTS, errors, RAG ingest, RAG neighbourhood, RAG
//!    retriever, RAG context, RAG MMR.
//! 4. Every error variant in `drevo.errors` + the typed hierarchy from
//!    RFC §5.1 is mentioned in `test_errors.py` (at least textually).
//! 5. MMR test file covers both `lambda_=1.0` (pure relevance) and
//!    `lambda_=0.0` (pure diversity) — RFC §10 Q-4 semantics.
//! 6. Context test file covers all three formats (`markdown`, `json`,
//!    `turtle`) — RFC §8.4.
//! 7. `drevo-py/CHANGELOG.md` records the 00118 entry.
//!
//! These checks deliberately stay shallow: a passing test here does not
//! mean the suite is *correct*, only that the agreed file layout
//! survives every PR (rename / delete / cherry-pick mistakes are
//! caught early). The Python suite itself is the source of truth for
//! behavioural assertions.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn unit_dir() -> PathBuf {
    repo_root().join("drevo-py").join("tests").join("unit")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

fn assert_contains_all(haystack: &str, needles: &[&str], context: &str) {
    for needle in needles {
        assert!(haystack.contains(needle), "{context}: missing `{needle}`");
    }
}

// ── 1. unit/ directory exists ───────────────────────────────────────

#[test]
fn unit_dir_exists() {
    let dir = unit_dir();
    assert!(
        dir.is_dir(),
        "drevo-py/tests/unit/ must exist — RFC §2 package layout places \
         the 00118 unit suite under tests/unit/, alongside tests/integration/ \
         (00119) and tests/e2e/ (00120)"
    );
}

#[test]
fn unit_package_marker_exists() {
    let init = unit_dir().join("__init__.py");
    assert!(
        init.exists(),
        "drevo-py/tests/unit/__init__.py must exist so pytest can \
         discover the package and so `from .conftest import ...` \
         resolves under PEP 328 relative imports"
    );
}

// ── 2. shared fixtures ──────────────────────────────────────────────

#[test]
fn unit_conftest_declares_shared_fixtures() {
    let conftest = read(&unit_dir().join("conftest.py"));
    // Each fixture name should appear as `def <name>` (definition) — a
    // `@pytest.fixture` decorator alone is not enough.
    assert_contains_all(
        &conftest,
        &[
            "def drevo_db",
            "def mock_drevo",
            "def det_embedder",
            "def orthogonal_embedder",
            "def connected_chain",
            "def mixed_kind_neighbourhood",
            "def make_scored",
        ],
        "drevo-py/tests/unit/conftest.py must declare every fixture \
         the test modules consume — a missing fixture surfaces as a \
         `fixture not found` error at collect-time which the Python CI \
         job blocks on",
    );
    assert!(
        conftest.contains("@pytest.fixture"),
        "conftest.py must use the @pytest.fixture decorator — bare \
         functions are not picked up as fixtures"
    );
}

// ── 3. one test module per surface area ─────────────────────────────

#[test]
fn unit_handle_test_module_exists() {
    let p = unit_dir().join("test_handle.py");
    assert!(
        p.exists(),
        "drevo-py/tests/unit/test_handle.py must exist — covers \
         Drevo.open / open_in_memory / close / __enter__ / __exit__ / \
         compact / health_check (RFC §3.3, §4.2)"
    );
}

#[test]
fn unit_nodes_test_module_exists() {
    let p = unit_dir().join("test_nodes.py");
    assert!(
        p.exists(),
        "drevo-py/tests/unit/test_nodes.py must exist — covers every \
         node CRUD method on Drevo (RFC §3.3 \"Node CRUD\")"
    );
}

#[test]
fn unit_edges_test_module_exists() {
    let p = unit_dir().join("test_edges.py");
    assert!(
        p.exists(),
        "drevo-py/tests/unit/test_edges.py must exist — covers every \
         edge CRUD method on Drevo (RFC §3.3 \"Edge CRUD\")"
    );
}

#[test]
fn unit_traversal_test_module_exists() {
    let p = unit_dir().join("test_traversal.py");
    assert!(
        p.exists(),
        "drevo-py/tests/unit/test_traversal.py must exist — covers \
         bfs / dfs / shortest_path / subgraph / neighbors"
    );
}

#[test]
fn unit_fts_test_module_exists() {
    let p = unit_dir().join("test_fts.py");
    assert!(
        p.exists(),
        "drevo-py/tests/unit/test_fts.py must exist — covers \
         search_fts (RFC §3.3)"
    );
}

#[test]
fn unit_errors_test_module_exists() {
    let p = unit_dir().join("test_errors.py");
    assert!(
        p.exists(),
        "drevo-py/tests/unit/test_errors.py must exist — every typed \
         exception in RFC §5.1 needs at least one focused test (the \
         00118 task observation explicitly requires this)"
    );
}

#[test]
fn unit_rag_ingest_test_module_exists() {
    let p = unit_dir().join("test_rag_ingest.py");
    assert!(
        p.exists(),
        "drevo-py/tests/unit/test_rag_ingest.py must exist — covers \
         ingest_documents + IngestSchema (RFC §8.2)"
    );
}

#[test]
fn unit_rag_neighborhood_test_module_exists() {
    let p = unit_dir().join("test_rag_neighborhood.py");
    assert!(
        p.exists(),
        "drevo-py/tests/unit/test_rag_neighborhood.py must exist — \
         covers expand_neighborhood (RFC §8 + task 00117 description)"
    );
}

#[test]
fn unit_rag_retriever_test_module_exists() {
    let p = unit_dir().join("test_rag_retriever.py");
    assert!(
        p.exists(),
        "drevo-py/tests/unit/test_rag_retriever.py must exist — covers \
         Retriever.retrieve dispatch + behaviour (RFC §8.3)"
    );
}

#[test]
fn unit_rag_context_test_module_exists() {
    let p = unit_dir().join("test_rag_context.py");
    assert!(
        p.exists(),
        "drevo-py/tests/unit/test_rag_context.py must exist — covers \
         Context.to_text formatting for markdown / json / turtle (RFC §8.4)"
    );
}

#[test]
fn unit_rag_mmr_test_module_exists() {
    let p = unit_dir().join("test_rag_mmr.py");
    assert!(
        p.exists(),
        "drevo-py/tests/unit/test_rag_mmr.py must exist — covers \
         MMRReranker math under both lambda_ semantics (RFC §10 Q-4)"
    );
}

// ── 4. error variants covered ───────────────────────────────────────

#[test]
fn unit_errors_module_mentions_every_drevo_error_variant() {
    let body = read(&unit_dir().join("test_errors.py"));
    // The Phase 16 RFC §5.1 hierarchy — each name must appear at least
    // once so a future PR cannot drop coverage by accident.
    assert_contains_all(
        &body,
        &[
            "DrevoError",
            "NotFoundError",
            "NodeNotFoundError",
            "EdgeNotFoundError",
            "ConflictError",
            "DuplicateTitleError",
            "StorageError",
            "SerializationError",
            "LockedError",
            "PanicError",
            "InvalidWeightError",
        ],
        "drevo-py/tests/unit/test_errors.py must reference every \
         exception variant in RFC §5.1 + §5.3 so each one has at least \
         one focused unit test (the 00118 task description: \"every \
         error mapping (each DrevoError variant has a corresponding \
         Python exception test)\")",
    );
}

// ── 5. MMR test file covers both lambda_ semantics ──────────────────

#[test]
fn unit_mmr_module_covers_both_lambda_semantics() {
    let body = read(&unit_dir().join("test_rag_mmr.py"));
    assert!(
        body.contains("lambda_=1.0"),
        "test_rag_mmr.py must include at least one test pinning \
         lambda_=1.0 (pure relevance) — RFC §10 Q-4 fixes this end \
         of the spectrum"
    );
    assert!(
        body.contains("lambda_=0.0"),
        "test_rag_mmr.py must include at least one test pinning \
         lambda_=0.0 (pure diversity) — RFC §10 Q-4 fixes this end \
         of the spectrum"
    );
}

// ── 6. Context test file covers all three formats ───────────────────

#[test]
fn unit_context_module_covers_all_three_formats() {
    let body = read(&unit_dir().join("test_rag_context.py"));
    assert_contains_all(
        &body,
        &["\"markdown\"", "\"json\"", "\"turtle\""],
        "test_rag_context.py must include at least one assertion per \
         supported Context.to_text format — RFC §8.4 fixes the trio \
         (markdown / json / turtle) and the 00118 task description \
         calls it out explicitly",
    );
}

// ── 7. CHANGELOG records 00118 ──────────────────────────────────────

#[test]
fn changelog_records_00118_entry() {
    let changelog = read(&repo_root().join("drevo-py").join("CHANGELOG.md"));
    assert!(
        changelog.contains("00118"),
        "drevo-py/CHANGELOG.md must record the 00118 entry under a \
         version stanza so wheel consumers can see the unit-test suite \
         landed in the package metadata"
    );
}

// ── 8. Drevo handle methods each have at least one test ─────────────

#[test]
fn unit_handle_module_covers_every_lifecycle_method() {
    let body = read(&unit_dir().join("test_handle.py"));
    // RFC §3.3 enumerates the lifecycle surface — each name must
    // appear so the unit tests do not silently regress one method.
    assert_contains_all(
        &body,
        &[
            "open_in_memory",
            "open",
            "close",
            "__enter__",
            "__exit__",
            "compact",
            "health_check",
        ],
        "test_handle.py must reference every lifecycle method on Drevo \
         (RFC §3.3 \"Lifecycle\") so the per-method coverage requirement \
         in the 00118 task description is observable from on-disk grep",
    );
}
