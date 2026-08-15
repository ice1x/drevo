//! Text-level guards for the GitHub Pages documentation site.
//!
//! The site is an [mdBook](https://rust-lang.github.io/mdBook/) built from the
//! existing `docs/` guides and deployed by `.github/workflows/docs.yml` to
//! GitHub Pages (Source = GitHub Actions — no `gh-pages` branch). These tests
//! pin the wiring the same way the other workflow suites do
//! (`docker_publish_ci_tests`, `k8s_manifests_tests`): pure text asserts, no
//! network, no mdbook binary required.

use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    let p = root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("failed to read {}: {e}", p.display()))
}

// ---------------------------------------------------------------------------
// book.toml
// ---------------------------------------------------------------------------

#[test]
fn book_toml_builds_the_docs_dir_into_book() {
    let b = read("book.toml");
    assert!(
        b.contains("src = \"docs\""),
        "the book source must be the existing docs/ guides, not a parallel src/ copy"
    );
    assert!(
        b.contains("build-dir = \"book\""),
        "build output goes to book/ (gitignored; uploaded by the workflow)"
    );
    assert!(
        b.contains("git-repository-url"),
        "the rendered site should link back to the GitHub repository"
    );
    assert!(
        b.contains("site-url = \"/drevo/\""),
        "project Pages are served under /drevo/ — required for correct asset URLs"
    );
}

#[test]
fn book_build_dir_is_gitignored() {
    let g = read(".gitignore");
    assert!(
        g.lines()
            .any(|l| l.trim() == "/book" || l.trim() == "book/"),
        ".gitignore must exclude the mdBook build dir so `mdbook build` output is never committed"
    );
}

// ---------------------------------------------------------------------------
// SUMMARY.md — the book's table of contents
// ---------------------------------------------------------------------------

#[test]
fn summary_lists_every_user_facing_guide() {
    let s = read("docs/SUMMARY.md");
    // README.md is the landing chapter → index.html of the site.
    assert!(
        s.contains("(README.md)"),
        "docs/README.md must be the introduction chapter so the site root is not a 404"
    );
    for guide in [
        "user-guide.md",
        "cypher-reference.md",
        "sdk-reference.md",
        "admin-guide.md",
        "migration-guide.md",
    ] {
        assert!(
            s.contains(&format!("({guide})")),
            "docs/SUMMARY.md must include {guide}"
        );
    }
}

#[test]
fn summary_links_resolve_to_real_files() {
    let s = read("docs/SUMMARY.md");
    for target in s
        .lines()
        .filter_map(|l| l.split_once("](").map(|(_, rest)| rest))
        .filter_map(|rest| rest.split_once(')').map(|(t, _)| t))
    {
        assert!(
            Path::new(&root().join("docs").join(target)).exists(),
            "SUMMARY.md links to docs/{target}, which does not exist"
        );
    }
}

// ---------------------------------------------------------------------------
// .github/workflows/docs.yml
// ---------------------------------------------------------------------------

#[test]
fn docs_workflow_triggers_on_main_docs_changes_and_manual_dispatch() {
    let w = read(".github/workflows/docs.yml");
    assert!(
        w.contains("workflow_dispatch"),
        "manual re-deploy must exist"
    );
    assert!(w.contains("- main"), "deploys from main");
    for path in ["docs/**", "book.toml", ".github/workflows/docs.yml"] {
        assert!(
            w.contains(path),
            "path filter must include {path} so docs edits redeploy and unrelated pushes don't"
        );
    }
}

#[test]
fn docs_workflow_has_minimal_pages_permissions() {
    let w = read(".github/workflows/docs.yml");
    assert!(w.contains("contents: read"));
    assert!(
        w.contains("pages: write") && w.contains("id-token: write"),
        "actions/deploy-pages requires pages:write + id-token:write"
    );
    assert!(
        !w.contains("packages: write") && !w.contains("contents: write"),
        "the docs workflow must not carry publish/write scopes beyond Pages"
    );
}

#[test]
fn docs_workflow_builds_with_pinned_mdbook_and_deploys_via_pages_actions() {
    let w = read(".github/workflows/docs.yml");
    assert!(
        w.contains("rust-lang/mdBook/releases/download/v"),
        "mdBook must be installed from a pinned release tarball (fast, reproducible), not `cargo install`"
    );
    assert!(w.contains("mdbook build"));
    assert!(w.contains("actions/configure-pages"));
    assert!(
        w.contains("actions/upload-pages-artifact") && w.contains("path: book"),
        "the built book/ dir is what gets uploaded"
    );
    assert!(w.contains("actions/deploy-pages"));
}
