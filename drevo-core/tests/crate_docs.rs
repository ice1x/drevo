//! The published docs (docs.rs crate page, module pages, crates.io README) are
//! written for someone using `drevo-core`, not for the people who extracted it.
//!
//! They must describe what the crate *is* and *does* today — not the long-gone
//! KV store, nor migration bookkeeping (extraction slices, RFC phases, issue
//! numbers) that means nothing to a reader of docs.rs.

use std::fs;
use std::path::Path;

/// Phrases that only make sense inside the drevo repo's history.
const STALE: &[&str] = &[
    "KV",
    "storage-agnostic",
    "Storage-agnostic",
    "do not depend",
    "none of the",
    "extraction slice",
    "later slice",
    "follow-up slice",
    "gated slice",
    "This slice",
    "RFC",
    "Phase ",
    "Drevo::",
    "`Drevo`",
    "main crate",
    "redb",
];

/// `#` followed by three or more digits: an issue/PR number.
fn issue_ref(line: &str) -> bool {
    line.match_indices('#').any(|(i, _)| {
        line[i + 1..]
            .chars()
            .take_while(char::is_ascii_digit)
            .count()
            >= 3
    })
}

fn stale_lines(origin: &str, lines: impl Iterator<Item = String>) -> Vec<String> {
    lines
        .enumerate()
        .filter(|(_, l)| STALE.iter().any(|p| l.contains(p)) || issue_ref(l))
        .map(|(n, l)| format!("{origin}:{}: {}", n + 1, l.trim()))
        .collect()
}

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// The doc comments of a source file — module docs (`//!`) and item docs
/// (`///`) — with every other line blanked so line numbers stay true. Private
/// items are included too: docs.rs hides them, but `--document-private-items`
/// and readers of the source do not.
fn doc_lines(file: &Path) -> Vec<String> {
    let src = fs::read_to_string(file).expect("read source");
    src.lines()
        .map(|l| {
            let t = l.trim_start();
            if t.starts_with("//!") || t.starts_with("///") {
                t.to_string()
            } else {
                String::new()
            }
        })
        .collect()
}

#[test]
fn doc_comments_carry_no_internal_history() {
    let mut hits = Vec::new();
    for entry in fs::read_dir(root().join("src")).expect("src dir") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|e| e == "rs") {
            let name = path.display().to_string();
            hits.extend(stale_lines(&name, doc_lines(&path).into_iter()));
        }
    }
    assert!(hits.is_empty(), "stale doc wording:\n{}", hits.join("\n"));
}

#[test]
fn readme_carries_no_internal_history() {
    let readme = fs::read_to_string(root().join("README.md")).expect("README");
    let hits = stale_lines("README.md", readme.lines().map(str::to_string));
    assert!(
        hits.is_empty(),
        "stale README wording:\n{}",
        hits.join("\n")
    );
}

#[test]
fn package_description_says_what_the_crate_is() {
    let manifest = fs::read_to_string(root().join("Cargo.toml")).expect("Cargo.toml");
    let description = manifest
        .lines()
        .find(|l| l.starts_with("description"))
        .expect("description");
    let hits = stale_lines("Cargo.toml", std::iter::once(description.to_string()));
    assert!(hits.is_empty(), "{}", hits.join("\n"));
}

#[test]
fn crate_page_opens_with_a_runnable_example() {
    let lib = doc_lines(&root().join("src/lib.rs"))
        .into_iter()
        .filter(|l| l.starts_with("//!"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        lib.contains("```") && lib.contains("NativeGraph::new()"),
        "the crate page should show NativeGraph in use"
    );
}
