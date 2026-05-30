//! Phase 12 task `00079` — text-level scaffolding tests for the Python
//! embedding-integration surface.
//!
//! Same pattern as `python_rag_idioms_tests.rs` (`00117`) and the other
//! `python_*_tests.rs` scaffolding files: these tests do **not** boot a
//! Python interpreter — they grep the on-disk files so the Rust-only CI
//! runners gate the *shape* of the embedding surface before the wheel
//! build job ever fires `pytest`. The runtime behaviour is exercised by
//! the pytest suites (`drevo-py/tests/unit/` + `tests/integration/`) under
//! the Python CI job and inside cibuildwheel's `CIBW_TEST_COMMAND`.
//!
//! What this file locks:
//!
//! 1. `drevo-py/src/handle.rs` exposes the six vector bridge methods that
//!    surface the `00078` store + `00076` index to Python.
//! 2. `drevo-py/python/drevo/__init__.pyi` declares those six methods so
//!    `mypy --strict` sees the bridge.
//! 3. `drevo-py/python/drevo/rag/embedding.py` ships `Embedder`,
//!    `VectorHit`, `embed_and_store`, `vector_search`.
//! 4. `drevo.rag` (`__init__.py` + `__init__.pyi`) re-exports the new
//!    names in both `__all__` lists.
//! 5. `retriever.py` actually implements `retrieve_with_embedding` (no
//!    longer raises `NotImplementedError`) via `vector_search`.
//! 6. `ingest.py` persists first-class embeddings (`set_embeddings_batch`).
//! 7. The pytest files exist (unit bridge + unit helpers + integration).
//! 8. `drevo-py/CHANGELOG.md` records the `00079` entry.

use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

fn exists(rel: &str) -> bool {
    repo_root().join(rel).exists()
}

fn assert_contains_all(haystack: &str, needles: &[&str], context: &str) {
    for needle in needles {
        assert!(haystack.contains(needle), "{context}: missing `{needle}`");
    }
}

#[test]
fn handle_exposes_six_vector_bridge_methods() {
    let src = read("drevo-py/src/handle.rs");
    assert_contains_all(
        &src,
        &[
            "fn set_embedding(",
            "fn set_embeddings_batch(",
            "fn get_embedding(",
            "fn delete_embedding(",
            "fn embedding_count(",
            "fn vector_search(",
        ],
        "handle.rs vector bridge",
    );
    // The search path rebuilds the persisted index and runs ANN search.
    assert_contains_all(
        &src,
        &[
            "build_vector_index(",
            "use drevo::vector::{HnswConfig, Vector}",
        ],
        "handle.rs vector_search wiring",
    );
}

#[test]
fn type_stub_declares_bridge_methods() {
    let pyi = read("drevo-py/python/drevo/__init__.pyi");
    assert_contains_all(
        &pyi,
        &[
            "def set_embedding(self, node_id: int, embedding: list[float]) -> None",
            "def set_embeddings_batch(",
            "def get_embedding(self, node_id: int) -> Optional[list[float]]",
            "def delete_embedding(self, node_id: int) -> None",
            "def embedding_count(self) -> int",
            "def vector_search(self, query: list[float], k: int) -> list[tuple[int, float]]",
        ],
        "drevo/__init__.pyi bridge stubs",
    );
}

#[test]
fn embedding_module_ships_public_surface() {
    assert!(
        exists("drevo-py/python/drevo/rag/embedding.py"),
        "drevo/rag/embedding.py must exist"
    );
    let m = read("drevo-py/python/drevo/rag/embedding.py");
    assert_contains_all(
        &m,
        &[
            "class Embedder(Protocol)",
            "class VectorHit",
            "def embed_and_store(",
            "def vector_search(",
            "set_embeddings_batch",
            "drevo.vector_search(",
        ],
        "rag/embedding.py surface",
    );
}

#[test]
fn rag_package_reexports_embedding_helpers() {
    let init = read("drevo-py/python/drevo/rag/__init__.py");
    assert_contains_all(
        &init,
        &[
            "from .embedding import Embedder, VectorHit, embed_and_store, vector_search",
            "\"Embedder\"",
            "\"VectorHit\"",
            "\"embed_and_store\"",
            "\"vector_search\"",
        ],
        "rag/__init__.py exports",
    );
    let stub = read("drevo-py/python/drevo/rag/__init__.pyi");
    assert_contains_all(
        &stub,
        &[
            "class Embedder(Protocol)",
            "class VectorHit",
            "def embed_and_store(",
            "def vector_search(",
        ],
        "rag/__init__.pyi exports",
    );
}

#[test]
fn retriever_implements_retrieve_with_embedding() {
    let r = read("drevo-py/python/drevo/rag/retriever.py");
    // The placeholder is gone and the real path calls the bridge.
    assert!(
        !r.contains("raise NotImplementedError"),
        "retrieve_with_embedding must no longer raise NotImplementedError"
    );
    assert_contains_all(
        &r,
        &["self._drevo.vector_search(", "_expand_seeds("],
        "retriever.retrieve_with_embedding wiring",
    );
}

#[test]
fn ingest_persists_first_class_embeddings() {
    let i = read("drevo-py/python/drevo/rag/ingest.py");
    assert_contains_all(
        &i,
        &["set_embeddings_batch(", "first_class"],
        "ingest_documents first-class persistence",
    );
}

#[test]
fn pytest_files_exist() {
    for rel in [
        "drevo-py/tests/unit/test_vector_bridge.py",
        "drevo-py/tests/unit/test_rag_embedding.py",
        "drevo-py/tests/integration/test_vector_embeddings.py",
    ] {
        assert!(exists(rel), "missing pytest file: {rel}");
    }
}

#[test]
fn changelog_records_task_00079() {
    let log = read("drevo-py/CHANGELOG.md");
    assert_contains_all(
        &log,
        &[
            "task `00079`",
            "embedding integration helpers",
            "vector_search",
        ],
        "CHANGELOG 00079 entry",
    );
}
