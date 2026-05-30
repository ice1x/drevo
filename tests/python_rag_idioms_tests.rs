//! Phase 16 task `00117` — text-level scaffolding tests for the
//! `drevo.rag` pure-Python idioms layer.
//!
//! Mirror of `python_api_scaffolding_tests.rs` (`00115`) and
//! `python_package_wheels_tests.rs` (`00116`): these tests do **not**
//! invoke any Python interpreter — they grep the on-disk files to lock
//! the layout and contract of every public symbol the task ships, so a
//! reviewer can see in one place what the Rust workspace promises to
//! the Python side.
//!
//! Runtime behaviour of `drevo.rag` (Document protocol, Retriever,
//! Context.to_text formats, MMRReranker math, ingest_documents schema)
//! is exercised by pytest in `drevo-py/tests/test_rag.py`. That pytest
//! suite cannot run on Rust-only CI runners (no Python toolchain), so
//! these grep-tests are the gate that the file structure and exported
//! names exist before the wheel build job ever fires `pytest`.
//!
//! Files locked by this module (relative to repo root):
//!
//! 1. `drevo-py/python/drevo/rag/__init__.py` — module entry point;
//!    re-exports `Document`, `SimpleDocument`, `IngestSchema`,
//!    `ingest_documents`, `Retriever`, `Context`, `ContextStats`,
//!    `MMRReranker`, `expand_neighborhood` and declares `__all__`.
//! 2. `drevo-py/python/drevo/rag/_document.py` — `Document` Protocol
//!    (RFC §8.1) + `SimpleDocument` reference implementation.
//! 3. `drevo-py/python/drevo/rag/ingest.py` — `ingest_documents` +
//!    `IngestSchema` dataclass (RFC §8.2).
//! 4. `drevo-py/python/drevo/rag/retriever.py` — `Retriever`, `Context`,
//!    `ContextStats` (RFC §8.3 + §8.4).
//! 5. `drevo-py/python/drevo/rag/neighborhood.py` — `expand_neighborhood`
//!    free function (00117 task description, bounded BFS with kind
//!    filter + max_nodes cap).
//! 6. `drevo-py/python/drevo/rag/rerank.py` — `MMRReranker` (RFC §8.5).
//! 7. `drevo-py/python/drevo/rag/__init__.pyi` — type stubs for the
//!    full `drevo.rag` public surface, so `mypy --strict` resolves
//!    every name re-exported from `__init__.py`.
//! 8. `drevo-py/pyproject.toml` — `[project.optional-dependencies]`
//!    keeps `langchain` and `llamaindex` extras reserved (the adapter
//!    packages themselves are follow-up tasks; the namespaces must be
//!    claimed at 00117 ship so `pip install drevo-py[langchain]`
//!    autocomplete works today, even if the wrapper modules ship later).
//! 9. `drevo-py/CHANGELOG.md` — records the 00117 entry under a new
//!    version stanza so consumers can see the rag layer in the package
//!    metadata.
//! 10. `drevo-py/python/drevo/__init__.py` does NOT eagerly import
//!     `drevo.rag` — the rag layer is opt-in (`from drevo import rag`
//!     or `from drevo.rag import Retriever`), keeping
//!     `import drevo` itself zero-cost-after-cdylib.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rag_dir() -> PathBuf {
    repo_root()
        .join("drevo-py")
        .join("python")
        .join("drevo")
        .join("rag")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

fn assert_contains_all(haystack: &str, needles: &[&str], context: &str) {
    for needle in needles {
        assert!(haystack.contains(needle), "{context}: missing `{needle}`");
    }
}

// ── 1. rag/__init__.py exists and re-exports the full surface ──────────

#[test]
fn rag_init_module_exists() {
    let init = rag_dir().join("__init__.py");
    assert!(
        init.exists(),
        "drevo-py/python/drevo/rag/__init__.py must exist — RFC §2 \
         package layout puts the graph-RAG idioms layer at drevo.rag"
    );
}

#[test]
fn rag_init_reexports_full_public_surface() {
    let init = read(&rag_dir().join("__init__.py"));
    // Every public name advertised in RFC §8 + the task observation
    // must be importable as `from drevo.rag import <name>`.
    assert_contains_all(
        &init,
        &[
            "Document",
            "SimpleDocument",
            "IngestSchema",
            "ingest_documents",
            "Retriever",
            "Context",
            "ContextStats",
            "MMRReranker",
            "expand_neighborhood",
        ],
        "drevo-py/python/drevo/rag/__init__.py must re-export the full \
         public surface defined by RFC §8 + the 00117 task description",
    );
    assert!(
        init.contains("__all__"),
        "drevo-py/python/drevo/rag/__init__.py must declare __all__ so \
         `from drevo.rag import *` and downstream introspection (00121) \
         see the curated surface, not every implementation detail"
    );
}

// ── 2. rag/_document.py defines Document Protocol ──────────────────────

#[test]
fn rag_document_module_defines_protocol_and_simple_impl() {
    let doc = read(&rag_dir().join("_document.py"));
    assert!(
        doc.contains("from typing import"),
        "_document.py must import from typing (Protocol lives in typing)"
    );
    assert!(
        doc.contains("Protocol"),
        "_document.py must declare a typing.Protocol so the Document \
         contract is structural — RFC §8.1 explicitly rules out \
         inheriting from langchain.Document"
    );
    assert!(
        doc.contains("class Document"),
        "_document.py must declare `class Document(...)` matching the \
         RFC §8.1 protocol shape (page_content: str + metadata: dict)"
    );
    assert!(
        doc.contains("page_content"),
        "Document protocol must expose `page_content` (str) — RFC §8.1; \
         this is the field LangChain/LlamaIndex/Haystack all share"
    );
    assert!(
        doc.contains("metadata"),
        "Document protocol must expose `metadata` (dict) — RFC §8.1"
    );
    assert!(
        doc.contains("runtime_checkable"),
        "Document protocol must be @runtime_checkable so \
         `isinstance(obj, Document)` works inside `ingest_documents` \
         and helps users debug duck-typing failures at the boundary"
    );
    assert!(
        doc.contains("class SimpleDocument") || doc.contains("SimpleDocument"),
        "_document.py must ship a SimpleDocument reference \
         implementation so tests + downstream users have a concrete \
         class to construct without depending on LangChain"
    );
}

// ── 3. rag/ingest.py defines ingest_documents + IngestSchema ───────────

#[test]
fn rag_ingest_module_defines_ingest_and_schema() {
    let ingest = read(&rag_dir().join("ingest.py"));
    assert!(
        ingest.contains("def ingest_documents"),
        "ingest.py must define `def ingest_documents(...)` per RFC §8.2"
    );
    assert!(
        ingest.contains("class IngestSchema"),
        "ingest.py must define `class IngestSchema` (dataclass) per RFC §8.2"
    );
    // Function signature must accept `schema`, `kind`, `embedder` kwargs
    // per RFC §8.2.
    assert_contains_all(
        &ingest,
        &["schema", "kind", "embedder"],
        "ingest_documents signature must include the schema / kind / \
         embedder kwargs from RFC §8.2",
    );
    assert!(
        ingest.contains("page_content"),
        "ingest.py must read `doc.page_content` (Document protocol field)"
    );
    assert!(
        ingest.contains("metadata"),
        "ingest.py must read `doc.metadata` (Document protocol field)"
    );
    // IngestSchema dataclass must cover RFC §8.2 fields.
    assert_contains_all(
        &ingest,
        &["kind_from", "title_from", "property_map"],
        "IngestSchema must expose kind_from / title_from / property_map \
         dataclass fields per RFC §8.2",
    );
}

// ── 4. rag/retriever.py defines Retriever / Context / ContextStats ─────

#[test]
fn rag_retriever_module_defines_retriever_and_context() {
    let r = read(&rag_dir().join("retriever.py"));
    assert!(
        r.contains("class Retriever"),
        "retriever.py must define `class Retriever` per RFC §8.3"
    );
    assert!(
        r.contains("class Context"),
        "retriever.py must define `class Context` per RFC §8.4"
    );
    assert!(
        r.contains("class ContextStats"),
        "retriever.py must define `class ContextStats` — telemetry \
         shape mentioned in RFC §8.4 (hops actually used, dedup count, etc.)"
    );
    assert!(
        r.contains("def retrieve"),
        "Retriever must expose `retrieve(seed, *, limit=...)` per RFC §8.3"
    );
    assert!(
        r.contains("retrieve_with_embedding"),
        "Retriever must declare `retrieve_with_embedding(...)` — RFC §8.3 \
         explicitly names this method (implemented in Phase 12 task 00079)"
    );
    assert!(
        r.contains("def to_text"),
        "Context must expose `to_text(*, format=...)` per RFC §8.4"
    );
    // Format dispatch must cover all three RFC §8.4 formats.
    assert_contains_all(
        &r,
        &["markdown", "json", "turtle"],
        "Context.to_text must handle the three formats from RFC §8.4: \
         markdown (default), json, turtle",
    );
    // Phase 12 task `00079` implemented `retrieve_with_embedding`: it now
    // seeds via the first-class vector store instead of raising
    // NotImplementedError. Lock the wiring to `vector_search` so the method
    // can never silently regress to the placeholder.
    assert!(
        r.contains("vector_search("),
        "retrieve_with_embedding must resolve seeds via Drevo.vector_search \
         now that 00079 has landed the embedding bridge (RFC §8.3)"
    );
}

// ── 5. rag/neighborhood.py defines expand_neighborhood ─────────────────

#[test]
fn rag_neighborhood_module_defines_bounded_bfs() {
    let n = read(&rag_dir().join("neighborhood.py"));
    assert!(
        n.contains("def expand_neighborhood"),
        "neighborhood.py must define `def expand_neighborhood(...)` — \
         00117 task description names this exact free function"
    );
    assert_contains_all(
        &n,
        &["hops", "kind_filter", "max_nodes"],
        "expand_neighborhood signature must accept hops / kind_filter / \
         max_nodes kwargs (task 00117 description)",
    );
    // Bounded BFS must use the existing PyO3 surface for graph
    // traversal — `edges_of`, `get_node`, or `subgraph`.
    assert!(
        n.contains("edges_of") || n.contains("subgraph") || n.contains("neighbors"),
        "expand_neighborhood must call into the existing PyO3 graph \
         surface (edges_of / neighbors / subgraph) — it is a Python \
         orchestration layer, not a re-implementation of BFS in Rust"
    );
}

// ── 6. rag/rerank.py defines MMRReranker ───────────────────────────────

#[test]
fn rag_rerank_module_defines_mmr_reranker() {
    let r = read(&rag_dir().join("rerank.py"));
    assert!(
        r.contains("class MMRReranker"),
        "rerank.py must define `class MMRReranker` per RFC §8.5"
    );
    assert!(
        r.contains("def rerank"),
        "MMRReranker must expose `rerank(candidates, *, embedder, k)` \
         per RFC §8.5"
    );
    assert!(
        r.contains("lambda_"),
        "MMRReranker must take a `lambda_` knob (trailing underscore \
         avoids the Python keyword) — RFC §8.5 + RFC §10 Q-4 fixes \
         semantics: 1.0 = pure relevance, 0.0 = pure diversity"
    );
    assert!(
        r.contains("embedder"),
        "MMRReranker.rerank must accept an `embedder` callable per RFC §8.5"
    );
}

// ── 7. rag/__init__.pyi declares stubs for the full public surface ─────

#[test]
fn rag_type_stubs_declare_public_surface() {
    let pyi = read(&rag_dir().join("__init__.pyi"));
    assert_contains_all(
        &pyi,
        &[
            "class Document",
            "class SimpleDocument",
            "class IngestSchema",
            "def ingest_documents",
            "class Retriever",
            "class Context",
            "class ContextStats",
            "class MMRReranker",
            "def expand_neighborhood",
        ],
        "drevo-py/python/drevo/rag/__init__.pyi must declare class/def \
         stubs for every public symbol re-exported by rag/__init__.py — \
         mypy --strict resolves these signatures (RFC §3.3 + §8)",
    );
    assert!(
        pyi.contains("def to_text"),
        "rag/__init__.pyi must declare `Context.to_text(...)` so \
         mypy sees the method signature without falling through to the \
         runtime class"
    );
    assert!(
        pyi.contains("retrieve_with_embedding"),
        "rag/__init__.pyi must declare Retriever.retrieve_with_embedding \
         — the stub locks the signature for downstream typecheckers"
    );
}

// ── 8. pyproject.toml keeps optional-dependency extras reserved ────────

#[test]
fn pyproject_optional_extras_reserve_langchain_and_llamaindex() {
    let toml = read(&repo_root().join("drevo-py").join("pyproject.toml"));
    assert!(
        toml.contains("[project.optional-dependencies]"),
        "drevo-py/pyproject.toml must declare \
         [project.optional-dependencies] so `pip install drevo-py[X]` \
         autocomplete works — RFC §8.1 names langchain/llamaindex/haystack"
    );
    assert!(
        toml.contains("langchain"),
        "pyproject.toml must reserve the `langchain` extra namespace \
         (RFC §8.1) — adapter package itself is a follow-up task"
    );
    assert!(
        toml.contains("llamaindex") || toml.contains("llama-index"),
        "pyproject.toml must reserve the `llamaindex` extra namespace \
         (RFC §8.1) — adapter package itself is a follow-up task"
    );
}

// ── 9. CHANGELOG.md records 00117 ──────────────────────────────────────

#[test]
fn changelog_documents_00117_rag_idioms_entry() {
    let changelog = read(&repo_root().join("drevo-py").join("CHANGELOG.md"));
    assert!(
        changelog.contains("00117"),
        "drevo-py/CHANGELOG.md must cite task 00117 — graph-RAG idioms \
         landed in the rag/ subpackage"
    );
    assert!(
        changelog.contains("rag") || changelog.contains("RAG") || changelog.contains("Retriever"),
        "drevo-py/CHANGELOG.md must mention the rag layer / Retriever \
         so consumers reading PyPI metadata see the new capability"
    );
}

// ── 10. drevo/__init__.py stays cheap — no eager `import drevo.rag` ────

#[test]
fn drevo_top_level_does_not_eagerly_import_rag() {
    let init = read(
        &repo_root()
            .join("drevo-py")
            .join("python")
            .join("drevo")
            .join("__init__.py"),
    );
    // The rag layer is opt-in: `from drevo import rag` or
    // `from drevo.rag import Retriever` must work, but
    // `import drevo` must not load the rag tree (no LangChain
    // imports, no heavy dataclass construction). The rag/ subpackage
    // is auto-discoverable simply because it lives at
    // drevo-py/python/drevo/rag/ — there is no need for the top-level
    // shim to do an explicit `from . import rag` (and adding one would
    // pay an unnecessary import-cost on every `import drevo`).
    let normalised: String = init
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with('#')
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !normalised.contains("from . import rag")
            && !normalised.contains("from .rag")
            && !normalised.contains("import drevo.rag"),
        "drevo/__init__.py must NOT eagerly import drevo.rag — the rag \
         layer is opt-in. Users explicitly `from drevo.rag import \
         Retriever` when they want it. Keeping `import drevo` cheap is \
         the cdylib-only contract from RFC §2."
    );
}
