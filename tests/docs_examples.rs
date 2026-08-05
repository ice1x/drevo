//! Documentation verification — Phase 15 task `00102`.
//!
//! The comprehensive docs under [`docs/`](../docs) are only trustworthy if they
//! cannot drift from the code. This suite enforces two contracts:
//!
//! 1. **Every fenced ` ```cypher ` block in [`docs/cypher-reference.md`](../docs/cypher-reference.md)
//!    actually runs.** Each block is extracted, `parse`d, and `execute`d against a
//!    fresh in-memory database with a standard parameter map; a parse error or an
//!    [`ExecError`](drevo::cypher::executor::ExecError) fails the test. If drevo ever
//!    stops supporting a documented construct, CI goes red — the Cypher reference is
//!    executable specification, not prose.
//!
//! 2. **The docs tree is structurally whole.** Every guide promised by the index
//!    exists and is non-trivial, and every relative `docs/*.md` link between guides
//!    resolves to a real file. A broken cross-link or a missing guide fails the test.
//!
//! These are the documentation analogue of the three test layers (`drevo-tdd`): the
//! Cypher blocks are integration tests phrased as documentation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use drevo::cypher::executor::{execute, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;

/// Repository root (the `drevo` crate manifest dir is the workspace root).
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn docs_dir() -> PathBuf {
    repo_root().join("docs")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// A fenced code block lifted from a Markdown file, tagged with its 1-based
/// starting line so a failure points at the exact source location.
struct CodeBlock {
    lang: String,
    start_line: usize,
    body: String,
}

/// Extract every fenced code block from Markdown, recording the info-string
/// (language) and the line the opening fence sits on.
fn extract_code_blocks(markdown: &str) -> Vec<CodeBlock> {
    let mut blocks = Vec::new();
    let mut lines = markdown.lines().enumerate();
    while let Some((idx, line)) = lines.next() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("```") {
            let lang = rest.trim().to_string();
            let start_line = idx + 1;
            let mut body = String::new();
            for (_, inner) in lines.by_ref() {
                if inner.trim_start().starts_with("```") {
                    break;
                }
                body.push_str(inner);
                body.push('\n');
            }
            blocks.push(CodeBlock {
                lang,
                start_line,
                body,
            });
        }
    }
    blocks
}

/// A generous, fixed parameter map so any documented `$param` resolves. Extra
/// unused entries are harmless; missing ones would surface as a real bug in a
/// documented example.
fn doc_params() -> HashMap<String, Value> {
    let mut p = HashMap::new();
    p.insert("name".to_string(), Value::String("Alice".to_string()));
    p.insert("title".to_string(), Value::String("Session 1".to_string()));
    p.insert("kind".to_string(), Value::String("Task".to_string()));
    p.insert("query".to_string(), Value::String("graph".to_string()));
    p.insert("limit".to_string(), Value::Integer(10));
    p.insert("skip".to_string(), Value::Integer(0));
    p.insert("k".to_string(), Value::Integer(3));
    p.insert("threshold".to_string(), Value::Float(0.5));
    p
}

#[test]
fn every_cypher_block_in_the_reference_parses_and_executes() {
    let path = docs_dir().join("cypher-reference.md");
    let markdown = read(&path);
    let blocks = extract_code_blocks(&markdown);

    let cypher_blocks: Vec<&CodeBlock> = blocks.iter().filter(|b| b.lang == "cypher").collect();
    assert!(
        cypher_blocks.len() >= 15,
        "expected the Cypher reference to carry a substantial set of executable \
         examples, found {}",
        cypher_blocks.len()
    );

    for block in cypher_blocks {
        let source = block.body.trim();
        let loc = format!("{}:{}", path.display(), block.start_line);

        let query = parse(source).unwrap_or_else(|e| {
            panic!("PARSE failed for cypher block at {loc}:\n---\n{source}\n---\nerror: {e}")
        });

        // A few documented procedures require *runtime configuration* the bare
        // in-memory database cannot provide (e.g. `drevo.semantic.query` embeds
        // its query text through the server's configured embeddings upstream,
        // which is unset here). Their syntax is still validated by `parse`
        // above; only the execute step is skipped, with an explicit reason —
        // they are exercised end-to-end in their own feature-gated integration
        // tests instead.
        if needs_runtime_config(source) {
            continue;
        }

        // Each example runs against its own pristine database so CREATE/MERGE
        // examples can never collide on drevo's globally-unique node titles.
        let db = Drevo::open_in_memory().expect("open in-memory drevo");
        execute(&query, &db, doc_params()).unwrap_or_else(|e| {
            panic!("EXECUTE failed for cypher block at {loc}:\n---\n{source}\n---\nerror: {e}")
        });
    }
}

/// True for documented examples that parse but cannot *execute* against a bare
/// in-memory database because they depend on runtime configuration absent in
/// this test (an embeddings upstream, an external service, …). Kept as a tiny
/// explicit allowlist so a genuinely broken example can never hide behind it.
fn needs_runtime_config(source: &str) -> bool {
    // `drevo.semantic.query` embeds its query text via the configured
    // embeddings upstream (`DREVO_EMBEDDINGS_UPSTREAM`); without it the call
    // returns "embeddings backend not configured". End-to-end coverage lives in
    // `tests/semantic_query_tests.rs` (feature `embeddings-proxy`).
    //
    // `drevo.semantic.reindex` requires a registered target (and an embedder) to
    // run; on a bare in-memory database it errors "no semantic target
    // registered". Covered in `tests/semantic_reindex_tests.rs`.
    source.contains("drevo.semantic.query") || source.contains("drevo.semantic.reindex")
}

#[test]
fn rust_examples_in_the_reference_are_well_formed() {
    // We do not compile the Rust snippets (they reference a real path on disk),
    // but we guard against the most common doc rot: an empty or truncated block.
    let path = docs_dir().join("cypher-reference.md");
    let markdown = read(&path);
    for block in extract_code_blocks(&markdown) {
        if block.lang == "rust" {
            assert!(
                block.body.contains("Drevo") || block.body.contains("execute"),
                "rust block at {}:{} looks truncated",
                path.display(),
                block.start_line
            );
        }
    }
}

#[test]
fn docs_tree_is_structurally_whole() {
    // Every guide the index promises must exist and be non-trivial.
    let guides = [
        "README.md",
        "user-guide.md",
        "cypher-reference.md",
        "sdk-reference.md",
        "admin-guide.md",
        "migration-guide.md",
    ];
    for guide in guides {
        let path = docs_dir().join(guide);
        assert!(path.is_file(), "missing docs guide: {}", path.display());
        let content = read(&path);
        assert!(
            content.len() > 400,
            "docs guide {} is suspiciously short ({} bytes)",
            guide,
            content.len()
        );
        assert!(
            content.trim_start().starts_with('#'),
            "docs guide {guide} should open with a Markdown heading"
        );
    }
}

#[test]
fn relative_doc_links_resolve() {
    // Every `(foo.md)` / `(foo.md#anchor)` link that points inside docs/ must
    // resolve to a real file; a typo'd cross-link fails here rather than in a
    // reader's browser.
    let dir = docs_dir();
    for entry in std::fs::read_dir(&dir).expect("read docs dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let content = read(&path);
        for target in extract_markdown_link_targets(&content) {
            // Only check links that stay inside docs/ and point at a .md file.
            if target.starts_with("http") || target.starts_with('#') {
                continue;
            }
            let file_part = target.split('#').next().unwrap_or("");
            if !file_part.ends_with(".md") {
                continue;
            }
            let resolved = dir.join(file_part);
            assert!(
                resolved.is_file(),
                "broken doc link in {}: `{}` → {} does not exist",
                path.display(),
                target,
                resolved.display()
            );
        }
    }
}

/// Pull the `target` out of every `[text](target)` Markdown link.
fn extract_markdown_link_targets(markdown: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let bytes = markdown.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b']' && i + 1 < bytes.len() && bytes[i + 1] == b'(' {
            let mut j = i + 2;
            let start = j;
            while j < bytes.len() && bytes[j] != b')' {
                j += 1;
            }
            if j < bytes.len() {
                targets.push(markdown[start..j].trim().to_string());
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    targets
}
