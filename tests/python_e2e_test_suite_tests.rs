//! Phase 16 task `00120` — text-level scaffolding tests for the
//! `drevo-py` end-to-end test suite.
//!
//! Same pattern as `python_unit_test_suite_tests.rs` (00118) and
//! `python_api_scaffolding_tests.rs` (00115): these tests grep the
//! on-disk layout of `drevo-py/tests/e2e/` so the Rust-only CI runners
//! gate the suite's *existence* and *shape* without booting a Python
//! interpreter. The runtime behaviour itself is exercised by pytest
//! inside the Python CI job (00122) and inside cibuildwheel's
//! `CIBW_TEST_COMMAND` for every wheel.
//!
//! What this file locks (per RFC §2 layout + the 00120 task brief):
//!
//! 1. `drevo-py/tests/e2e/__init__.py` exists — the package marker.
//! 2. `drevo-py/tests/e2e/conftest.py` ships the shared e2e fixtures
//!    (`tmp_db_path`, `disk_db`, `deterministic_embedder`).
//! 3. One scenario module per Rust e2e scenario plus the RAG scenario:
//!    `test_cbt_journal.py`, `test_story_editor.py`,
//!    `test_task_manager.py`, `test_erp.py`, `test_bug_tracker.py`,
//!    `test_graph_rag.py`.
//! 4. Every scenario module drives the on-disk redb backend via
//!    `drevo.Drevo.open(...)` (mirrors the 00119 boundary).
//! 5. Each domain scenario mirrors the Rust counterpart's
//!    domain-language node kinds — searching the module text for the
//!    canonical kind strings catches accidental drift between layers.
//! 6. The RAG scenario walks every `drevo.rag` primitive named in RFC
//!    §8 (`Document` / `ingest_documents` / `Retriever` / `Context`),
//!    asserts all three `Context.to_text` formats are exercised, and
//!    confirms a close + reopen cycle.
//! 7. `drevo-py/CHANGELOG.md` records the 00120 entry.
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

fn e2e_dir() -> PathBuf {
    repo_root().join("drevo-py").join("tests").join("e2e")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

fn assert_contains_all(haystack: &str, needles: &[&str], context: &str) {
    for needle in needles {
        assert!(haystack.contains(needle), "{context}: missing `{needle}`");
    }
}

// ── 1. e2e/ directory + package marker ───────────────────────────────

#[test]
fn e2e_dir_exists() {
    let dir = e2e_dir();
    assert!(
        dir.is_dir(),
        "drevo-py/tests/e2e/ must exist — RFC §2 package layout places \
         the 00120 end-to-end suite under tests/e2e/, alongside \
         tests/unit/ (00118) and tests/integration/ (00119)"
    );
}

#[test]
fn e2e_package_marker_exists() {
    let init = e2e_dir().join("__init__.py");
    assert!(
        init.exists(),
        "drevo-py/tests/e2e/__init__.py must exist so pytest can \
         discover the package and so the scaffolding tests below can \
         cite a stable module path"
    );
}

#[test]
fn e2e_package_marker_cites_task_id_and_rfc() {
    let init = read(&e2e_dir().join("__init__.py"));
    assert_contains_all(
        &init,
        &["00120", "RFC", "drevo-tdd", "end-to-end"],
        "drevo-py/tests/e2e/__init__.py must cite task 00120, the \
         RFC, the drevo-tdd skill, and call out the end-to-end tier — \
         the docstring is the entry-point for anyone navigating the \
         test tree",
    );
}

// ── 2. conftest fixtures ─────────────────────────────────────────────

#[test]
fn e2e_conftest_declares_shared_fixtures() {
    let conftest = read(&e2e_dir().join("conftest.py"));
    assert_contains_all(
        &conftest,
        &[
            "def tmp_db_path",
            "def disk_db",
            "def deterministic_embedder",
        ],
        "drevo-py/tests/e2e/conftest.py must declare the three shared \
         e2e fixtures — every scenario module depends on at least one \
         of them, and a missing fixture surfaces only at pytest \
         collection time",
    );
}

#[test]
fn e2e_conftest_uses_tempfile_for_isolation() {
    let conftest = read(&e2e_dir().join("conftest.py"));
    assert!(
        conftest.contains("tempfile.TemporaryDirectory"),
        "drevo-py/tests/e2e/conftest.py must build the temp database \
         path under tempfile.TemporaryDirectory so concurrent test \
         runs don't collide on a shared on-disk path"
    );
}

#[test]
fn e2e_conftest_embedder_is_deterministic_and_offline() {
    let conftest = read(&e2e_dir().join("conftest.py"));
    // The embedder MUST be reproducible (hashlib is the only stdlib
    // hash that fits the contract — no numpy, no network).
    assert!(
        conftest.contains("hashlib"),
        "drevo-py/tests/e2e/conftest.py embedder must be built on a \
         deterministic stdlib hash — the RAG scenario asserts \
         byte-stable Context.to_text output across runs"
    );
}

// ── 3. scenario modules exist ────────────────────────────────────────

const SCENARIO_MODULES: &[&str] = &[
    "test_cbt_journal.py",
    "test_story_editor.py",
    "test_task_manager.py",
    "test_erp.py",
    "test_bug_tracker.py",
    "test_graph_rag.py",
];

#[test]
fn e2e_scenario_modules_all_present() {
    for module in SCENARIO_MODULES {
        let path = e2e_dir().join(module);
        assert!(
            path.exists(),
            "drevo-py/tests/e2e/{module} must exist — task 00120 \
             requires one scenario module per Rust scenario plus the \
             dedicated graph-RAG scenario"
        );
    }
}

#[test]
fn every_scenario_module_drives_the_disk_backend() {
    // Every domain scenario should open the disk DB (either via the
    // `disk_db` fixture or the `tmp_db_path` fixture + `Drevo.open(...)`).
    for module in SCENARIO_MODULES {
        let body = read(&e2e_dir().join(module));
        let uses_disk_db = body.contains("disk_db") || body.contains("drevo.Drevo.open");
        assert!(
            uses_disk_db,
            "drevo-py/tests/e2e/{module} must drive the on-disk redb \
             backend — the e2e tier inherits the boundary 00119 \
             defends, scenarios cannot regress to in-memory shortcuts"
        );
    }
}

#[test]
fn every_scenario_module_asserts_a_reopen_round_trip() {
    // The five domain scenarios + the RAG scenario each ship at least
    // one "close → reopen → re-assert" test so the agentic workload
    // contract (state survives process restarts) is locked.
    for module in SCENARIO_MODULES {
        let body = read(&e2e_dir().join(module));
        assert!(
            body.contains("round_trips_through_reopen") || body.contains("round_trip"),
            "drevo-py/tests/e2e/{module} must include a reopen \
             round-trip test — phase 16 promises an agent can resume \
             work after a process restart and observe the same graph"
        );
    }
}

// ── 4. domain-language drift checks ──────────────────────────────────

#[test]
fn cbt_journal_uses_canonical_kinds() {
    let body = read(&e2e_dir().join("test_cbt_journal.py"));
    assert_contains_all(
        &body,
        &[
            "\"situation\"",
            "\"thought\"",
            "\"emotion\"",
            "\"cognitive_distortion\"",
            "\"rational_response\"",
        ],
        "test_cbt_journal.py must use the same node-kind vocabulary as \
         tests/scenario_cbt_journal.rs — drift here means a fixture \
         change in one layer silently bypasses the other",
    );
}

#[test]
fn story_editor_uses_canonical_kinds() {
    let body = read(&e2e_dir().join("test_story_editor.py"));
    assert_contains_all(
        &body,
        &[
            "\"book\"",
            "\"chapter\"",
            "\"scene\"",
            "\"character\"",
            "\"plot_arc\"",
        ],
        "test_story_editor.py must use the same node-kind vocabulary as \
         tests/scenario_story_editor.rs",
    );
}

#[test]
fn task_manager_uses_canonical_kinds() {
    let body = read(&e2e_dir().join("test_task_manager.py"));
    assert_contains_all(
        &body,
        &[
            "\"project\"",
            "\"sprint\"",
            "\"task\"",
            "\"person\"",
            "\"label\"",
        ],
        "test_task_manager.py must use the same node-kind vocabulary as \
         tests/scenario_task_manager.rs",
    );
}

#[test]
fn erp_uses_canonical_kinds() {
    let body = read(&e2e_dir().join("test_erp.py"));
    assert_contains_all(
        &body,
        &[
            "\"company\"",
            "\"department\"",
            "\"employee\"",
            "\"product\"",
            "\"customer\"",
            "\"purchase_order\"",
        ],
        "test_erp.py must use the same node-kind vocabulary as \
         tests/scenario_erp.rs",
    );
}

#[test]
fn bug_tracker_uses_canonical_kinds() {
    let body = read(&e2e_dir().join("test_bug_tracker.py"));
    assert_contains_all(
        &body,
        &[
            "\"project\"",
            "\"bug\"",
            "\"component\"",
            "\"release\"",
            "\"test_case\"",
        ],
        "test_bug_tracker.py must use the same node-kind vocabulary as \
         tests/scenario_bug_tracker.rs",
    );
}

// ── 5. graph-RAG scenario contract ──────────────────────────────────

#[test]
fn graph_rag_scenario_uses_full_rag_surface() {
    let body = read(&e2e_dir().join("test_graph_rag.py"));
    assert_contains_all(
        &body,
        &[
            "from drevo.rag",
            "ingest_documents",
            "Retriever",
            "Context",
            "SimpleDocument",
        ],
        "test_graph_rag.py must exercise the full drevo.rag surface \
         named in RFC §8 — the scenario is the definition-of-done for \
         the rag layer, every primitive has to be exercised end-to-end",
    );
}

#[test]
fn graph_rag_scenario_covers_all_three_to_text_formats() {
    let body = read(&e2e_dir().join("test_graph_rag.py"));
    assert_contains_all(
        &body,
        &["\"markdown\"", "\"json\"", "\"turtle\""],
        "test_graph_rag.py must assert against every Context.to_text \
         format declared in RFC §8.4 — a format that the e2e suite \
         does not exercise is a format that drifts silently between \
         releases",
    );
}

#[test]
fn graph_rag_scenario_asserts_determinism() {
    let body = read(&e2e_dir().join("test_graph_rag.py"));
    assert!(
        body.contains("deterministic")
            || body.contains("stable_order")
            || body.contains("byte-equal")
            || body.contains("reproducible"),
        "test_graph_rag.py must assert determinism / stable ordering — \
         RFC §8.4 promises byte-stable Context.to_text output and the \
         RAG scenario is where we lock that promise"
    );
}

#[test]
fn graph_rag_scenario_uses_deterministic_embedder_fixture() {
    let body = read(&e2e_dir().join("test_graph_rag.py"));
    assert!(
        body.contains("deterministic_embedder"),
        "test_graph_rag.py must request the `deterministic_embedder` \
         fixture from conftest.py so the per-doc vectors stored under \
         Node.properties stay reproducible across runs"
    );
}

// ── 6. CHANGELOG entry for 00120 ────────────────────────────────────

#[test]
fn changelog_records_00120_entry() {
    let changelog = read(&repo_root().join("drevo-py").join("CHANGELOG.md"));
    assert!(
        changelog.contains("00120"),
        "drevo-py/CHANGELOG.md must record the 00120 entry — every \
         shipped task in Phase 16 is logged here so the wheel changelog \
         stays the canonical \"what changed\" timeline"
    );
}
