//! The published docs (docs.rs crate page, module pages, crates.io README) are
//! written for someone using `drevo-auth`, not for the people who extracted it.
//!
//! They must describe what the crate *is* and *does* today — not the history
//! of the drevo repo it came from (task ids, phases, issue numbers), which
//! means nothing to a reader of docs.rs.

use std::fs;
use std::path::Path;

/// Phrases that only make sense inside the drevo repo's history.
const STALE: &[&str] = &[
    "Phase ",
    "task `",
    "RFC",
    "not yet wired",
    "later-task",
    "later task",
    "main crate",
    "crate::bolt",
    "HELLO",
];

/// `#` followed by three or more digits (an issue/PR number), or a bare
/// five-digit task id in backticks such as `00094`.
fn issue_ref(line: &str) -> bool {
    let issue = line.match_indices('#').any(|(i, _)| {
        line[i + 1..]
            .chars()
            .take_while(char::is_ascii_digit)
            .count()
            >= 3
    });
    let task = line.match_indices('`').any(|(i, _)| {
        let mut rest = line[i + 1..].chars();
        let digits = rest.by_ref().take(5).filter(char::is_ascii_digit).count();
        digits == 5 && rest.next() == Some('`')
    });
    issue || task
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
    let mut dirs = vec![root().join("src")];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).expect("src dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let name = path.display().to_string();
                hits.extend(stale_lines(&name, doc_lines(&path).into_iter()));
            }
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
fn crate_page_is_the_readme_with_a_runnable_example() {
    let lib = fs::read_to_string(root().join("src/lib.rs")).expect("lib.rs");
    assert!(
        lib.contains(r#"include_str!("../README.md")"#),
        "the crate page should be the README, so its example runs as a doctest"
    );
    let readme = fs::read_to_string(root().join("README.md")).expect("README");
    assert!(
        readme.contains("```rust")
            && readme.contains("UserStore::new()")
            && readme.contains("AccessPolicy::new()"),
        "the README should show authentication and authorization in use"
    );
}
