//! The repository uses the plain hyphen `-` only.
//!
//! The long dash (U+2014) and the en dash (U+2013) are not allowed in any
//! tracked text file: README, docs, comments, messages, workflows. This test
//! reads every file git tracks and fails with the first offenders.
//!
//! The two characters are written as escapes here, so this file passes its
//! own check.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const LONG_DASH: char = '\u{2014}';
const EN_DASH: char = '\u{2013}';

/// How many offending lines the failure message lists.
const SHOWN: usize = 25;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// 1-based numbers of the lines that contain a forbidden dash.
fn lines_with_a_forbidden_dash(text: &str) -> Vec<usize> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| line.contains(LONG_DASH) || line.contains(EN_DASH))
        .map(|(index, _)| index + 1)
        .collect()
}

/// Every file git tracks, relative to the repository root.
fn tracked_files(root: &Path) -> Vec<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .output()
        .unwrap_or_else(|e| panic!("failed to run `git ls-files`: {e}"));
    assert!(
        out.status.success(),
        "`git ls-files` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

#[test]
fn the_scanner_finds_both_dashes_and_ignores_the_hyphen() {
    let text = format!("plain - hyphen\nlong {LONG_DASH} dash\nrange 1{EN_DASH}63\nagain - fine\n");
    assert_eq!(lines_with_a_forbidden_dash(&text), vec![2, 3]);
    assert!(lines_with_a_forbidden_dash("a - b\n--flag\n").is_empty());
}

#[test]
fn no_tracked_file_contains_a_long_dash_or_an_en_dash() {
    let root = root();
    if !root.join(".git").exists() {
        // A source archive without git metadata: nothing to list.
        return;
    }
    let files = tracked_files(&root);
    assert!(
        files.len() > 100,
        "`git ls-files` returned only {} files, the listing looks broken",
        files.len()
    );

    let mut offenders: Vec<String> = Vec::new();
    let mut offending_files = 0usize;
    for name in &files {
        let Ok(bytes) = fs::read(root.join(name)) else {
            continue; // deleted in the working tree, or a broken link
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue; // a binary file
        };
        let lines = lines_with_a_forbidden_dash(&text);
        if lines.is_empty() {
            continue;
        }
        offending_files += 1;
        offenders.extend(lines.iter().map(|line| format!("{name}:{line}")));
    }

    assert!(
        offenders.is_empty(),
        "{} line(s) in {} tracked file(s) contain a long dash (U+2014) or an en dash (U+2013); \
         use the hyphen `-`. First {}:\n{}",
        offenders.len(),
        offending_files,
        SHOWN.min(offenders.len()),
        offenders
            .iter()
            .take(SHOWN)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
