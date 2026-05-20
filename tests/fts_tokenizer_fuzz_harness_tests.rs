//! Phase 9 task `00058` — stable-CI replay of the `fuzz/seed_corpus/*`
//! inputs against the FTS tokenizer invariants.
//!
//! The full coverage-guided harness lives under `fuzz/` and requires
//! `cargo +nightly fuzz run …`. That toolchain is not always available
//! (nightly toolchain + `cargo-fuzz` install + libFuzzer sanitizer flags)
//! so the seed-corpus files are also replayed here under the stable
//! `cargo test` matrix. The assertion bodies are kept in lock-step with
//! `fuzz/fuzz_targets/*.rs`; if a property changes there, change it
//! here in the same commit — the harness exists precisely so the two
//! cannot silently drift.
//!
//! Cross-links:
//!
//! - `tests/proptest_fts_tokenizer.rs` — `00057` property-based suite.
//! - `tests/fts_audit_tests.rs::tokenizer_extract_trigrams_is_deterministic_on_random_unicode`
//!   — `00108` audit-grade xorshift32 fuzzer.
//! - `audit/AUDIT-fts.md` §"#follow-up-tokenizer-fuzz" — division of
//!   labour between `00057`, `00108`, and this task.

use std::fs;
use std::path::{Path, PathBuf};

use drevo::fts::tokenizer::{extract_trigrams, normalize, trigrams};

/// Root of the fuzz crate, relative to `CARGO_MANIFEST_DIR`. Resolved at
/// runtime so the test passes on any checkout location.
fn fuzz_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz")
}

/// Read every UTF-8 file under `seed_corpus/<target>/` and yield its
/// `(filename, contents)`. Non-UTF-8 files are skipped — libFuzzer's
/// `Arbitrary` impl for `&str` already filters those, so the stable
/// replay does the same.
fn read_utf8_corpus(target: &str) -> Vec<(String, String)> {
    let dir = fuzz_root().join("seed_corpus").join(target);
    let entries = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("missing fuzz seed corpus directory {}: {e}", dir.display()));

    let mut out: Vec<(String, String)> = entries
        .filter_map(|e| {
            let entry = e.ok()?;
            let path = entry.path();
            if !path.is_file() {
                return None;
            }
            let bytes = fs::read(&path).ok()?;
            let s = String::from_utf8(bytes).ok()?;
            let name = path.file_name()?.to_string_lossy().into_owned();
            Some((name, s))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(
        !out.is_empty(),
        "seed corpus {target} is empty — the fuzz harness needs at least one seed"
    );
    out
}

/// Assertions mirrored from `fuzz/fuzz_targets/fuzz_normalize.rs`. Kept
/// in sync by hand; the cross-references at the top of this file flag
/// the requirement.
fn assert_normalize_invariants(label: &str, input: &str) {
    let out = normalize(input);
    assert!(
        !out.starts_with(' '),
        "{label}: normalize leaked a leading space: {out:?} from {input:?}"
    );
    assert!(
        !out.ends_with(' '),
        "{label}: normalize leaked a trailing space: {out:?} from {input:?}"
    );
    assert!(
        !out.contains("  "),
        "{label}: normalize leaked consecutive spaces: {out:?} from {input:?}"
    );

    let twice = normalize(&out);
    assert_eq!(
        twice, out,
        "{label}: normalize is not idempotent: normalize({out:?}) == {twice:?} (input {input:?})"
    );

    for c in out.chars() {
        if c.is_ascii_alphabetic() {
            assert!(
                c.is_ascii_lowercase(),
                "{label}: normalize left uppercase ASCII char {c:?} in {out:?} (input {input:?})"
            );
        }
    }
}

/// Assertions mirrored from `fuzz/fuzz_targets/fuzz_trigrams.rs`.
fn assert_trigrams_invariants(label: &str, input: &str) {
    let result = trigrams(input);

    for pair in result.windows(2) {
        assert!(
            pair[0] < pair[1],
            "{label}: trigrams output is not strictly ascending: {:?} before {:?} (input {input:?})",
            pair[0],
            pair[1]
        );
    }

    for token in &result {
        let n = token.chars().count();
        assert!(
            n == 2 || n == 3,
            "{label}: trigram {token:?} has char-length {n}, expected 2 or 3 (input {input:?})"
        );
    }

    let norm = normalize(input);
    if norm.chars().count() < 2 {
        assert!(
            result.is_empty(),
            "{label}: trigrams produced {result:?} for normalized input {norm:?} (raw {input:?})"
        );
    }
}

/// Assertions mirrored from `fuzz/fuzz_targets/fuzz_extract_trigrams.rs`.
fn assert_extract_trigrams_invariants(label: &str, title: &str, body: &str) {
    let result = extract_trigrams(title, body);

    let combined_expected = trigrams(&join_title_body(title, body));
    assert_eq!(
        result, combined_expected,
        "{label}: extract_trigrams diverged from trigrams(joined) — title={title:?} body={body:?}"
    );

    for pair in result.windows(2) {
        assert!(
            pair[0] < pair[1],
            "{label}: extract_trigrams output not strictly ascending: {:?} before {:?} (title={title:?} body={body:?})",
            pair[0],
            pair[1]
        );
    }
}

fn join_title_body(title: &str, body: &str) -> String {
    if body.is_empty() {
        title.to_string()
    } else if title.is_empty() {
        body.to_string()
    } else {
        format!("{title} {body}")
    }
}

#[test]
fn fuzz_normalize_seed_corpus_holds_invariants() {
    for (name, input) in read_utf8_corpus("fuzz_normalize") {
        assert_normalize_invariants(&name, &input);
    }
}

#[test]
fn fuzz_trigrams_seed_corpus_holds_invariants() {
    for (name, input) in read_utf8_corpus("fuzz_trigrams") {
        assert_trigrams_invariants(&name, &input);
    }
}

#[test]
fn fuzz_extract_trigrams_seed_corpus_holds_invariants() {
    // Each seed file is split in half on a char boundary and fed as
    // `(title, body)`. This produces deterministic, non-trivial pairs
    // (left half / right half) that exercise both single-field branches
    // (when one half is empty) and the join branch.
    for (name, input) in read_utf8_corpus("fuzz_extract_trigrams") {
        let mid = char_floor(&input, input.len() / 2);
        let (title, body) = input.split_at(mid);
        assert_extract_trigrams_invariants(&format!("{name} (mid-split)"), title, body);
        // Also exercise the title-only and body-only branches with the
        // raw input — these match the `title.is_empty()` /
        // `body.is_empty()` short-circuits inside `extract_trigrams`.
        assert_extract_trigrams_invariants(&format!("{name} (title-only)"), &input, "");
        assert_extract_trigrams_invariants(&format!("{name} (body-only)"), "", &input);
    }
}

/// Round `idx` *down* to the nearest UTF-8 char boundary. `str::split_at`
/// panics on a non-boundary; we never want the harness test to fail on
/// the splitter rather than the tokenizer.
fn char_floor(s: &str, idx: usize) -> usize {
    let clamped = idx.min(s.len());
    let mut i = clamped;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[test]
fn char_floor_lands_on_char_boundaries() {
    let s = "Hello 世界";
    for i in 0..=s.len() {
        let f = char_floor(s, i);
        assert!(
            s.is_char_boundary(f),
            "char_floor produced non-boundary {f} for {s:?}"
        );
    }
}

/// Sanity check: every corpus directory referenced by the fuzz crate's
/// `Cargo.toml` `[[bin]]` entries exists on disk. If a new fuzz target
/// is added but its corpus is forgotten, the nightly fuzz run starts
/// from zero coverage; this guard fails first, with a clear message.
#[test]
fn every_fuzz_target_has_a_seed_corpus() {
    for target in ["fuzz_normalize", "fuzz_trigrams", "fuzz_extract_trigrams"] {
        let dir = fuzz_root().join("seed_corpus").join(target);
        assert!(
            dir.is_dir(),
            "fuzz target {target} is missing its seed corpus at {}",
            dir.display()
        );
        let count = fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
            .filter(|e| e.as_ref().is_ok_and(|e| e.path().is_file()))
            .count();
        assert!(
            count > 0,
            "fuzz target {target} has an empty seed corpus at {}",
            dir.display()
        );
    }
}

/// Sanity check: the fuzz crate's `Cargo.toml` exists and declares each
/// target as a `[[bin]]`. This is a lightweight regression guard — if a
/// future refactor renames or removes a target, the stable harness
/// notices in CI before the nightly fuzz job does.
#[test]
fn fuzz_cargo_toml_declares_every_target() {
    let manifest =
        fs::read_to_string(fuzz_root().join("Cargo.toml")).expect("fuzz/Cargo.toml must exist");
    for target in ["fuzz_normalize", "fuzz_trigrams", "fuzz_extract_trigrams"] {
        let needle = format!("name = \"{target}\"");
        assert!(
            manifest.contains(&needle),
            "fuzz/Cargo.toml is missing a [[bin]] entry for {target}"
        );
        let path_needle = format!("fuzz_targets/{target}.rs");
        assert!(
            manifest.contains(&path_needle),
            "fuzz/Cargo.toml is missing the path entry {path_needle}"
        );
    }
}
