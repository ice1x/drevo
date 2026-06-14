//! Phase 15 task `00099` — stable-CI replay of the Cypher fuzz invariants.
//!
//! The coverage-guided harness lives under `fuzz/` and requires
//! `cargo +nightly fuzz run …` (nightly toolchain + `cargo-fuzz` install +
//! libFuzzer sanitizer flags). That toolchain is not available on every CI
//! leg, so the seed-corpus files — and the grammar generator itself — are
//! also replayed here under the stable `cargo test` matrix.
//!
//! The assertion bodies are kept in lock-step with `fuzz/fuzz_targets/*.rs`;
//! if a property changes there, change it here in the same commit. The
//! harness exists precisely so the two cannot silently drift.
//!
//! Three targets are mirrored:
//!
//! - `fuzz_cypher_lexer`   → [`assert_lexer_invariants`]   (totality + spans)
//! - `fuzz_cypher_parser`  → [`assert_parser_invariants`]  (totality + shape)
//! - `fuzz_cypher_grammar` → [`assert_grammar_invariants`] (grammar accepted)
//!
//! Cross-link: the FTS-tokenizer equivalent is
//! `tests/fts_tokenizer_fuzz_harness_tests.rs` (Phase 9 task `00058`), which
//! established this stable-replay pattern.

use std::fs;
use std::path::{Path, PathBuf};

use drevo::cypher::lexer::tokenize;
use drevo::cypher::parser::parse;

// The grammar generator, shared verbatim with
// `fuzz/fuzz_targets/fuzz_cypher_grammar.rs`. `include!`ing the same source in
// both consumers is what guarantees the nightly fuzzer and this stable test
// explore the identical grammar.
include!("../fuzz/cypher_grammar.rs");

/// Root of the fuzz crate, relative to `CARGO_MANIFEST_DIR`. Resolved at
/// runtime so the test passes on any checkout location.
fn fuzz_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz")
}

/// Read every UTF-8 file under `seed_corpus/<target>/` and yield its
/// `(filename, contents)`. Non-UTF-8 files are skipped — libFuzzer's
/// `Arbitrary` impl for `&str` already filters those, so the stable replay
/// does the same for the `&str`-typed lexer/parser targets.
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

/// Read every file under `seed_corpus/<target>/` as raw bytes. Used by the
/// grammar target, whose libFuzzer input is `&[u8]` (a choice stream), not a
/// `&str` — so non-UTF-8 seeds are valid and must NOT be filtered.
fn read_raw_corpus(target: &str) -> Vec<(String, Vec<u8>)> {
    let dir = fuzz_root().join("seed_corpus").join(target);
    let entries = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("missing fuzz seed corpus directory {}: {e}", dir.display()));

    let mut out: Vec<(String, Vec<u8>)> = entries
        .filter_map(|e| {
            let entry = e.ok()?;
            let path = entry.path();
            if !path.is_file() {
                return None;
            }
            let bytes = fs::read(&path).ok()?;
            let name = path.file_name()?.to_string_lossy().into_owned();
            Some((name, bytes))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(
        !out.is_empty(),
        "seed corpus {target} is empty — the fuzz harness needs at least one seed"
    );
    out
}

/// Assertions mirrored from `fuzz/fuzz_targets/fuzz_cypher_lexer.rs`. The
/// lexer is total: it never panics, and when it succeeds every token span is
/// in-bounds and the stream is monotonic by start offset.
fn assert_lexer_invariants(label: &str, input: &str) {
    let Ok(tokens) = tokenize(input) else {
        // A recoverable LexError is a valid outcome.
        return;
    };

    let len = input.len();
    let mut prev_start = 0usize;
    for tok in &tokens {
        assert!(
            tok.span.start <= tok.span.end,
            "{label}: lexer produced an inverted span {:?} for {input:?}",
            tok.span
        );
        assert!(
            tok.span.end <= len,
            "{label}: lexer span {:?} runs past the {len}-byte input {input:?}",
            tok.span
        );
        assert!(
            tok.span.start >= prev_start,
            "{label}: lexer emitted a non-monotonic span (start {} < previous {prev_start}) for {input:?}",
            tok.span.start
        );
        prev_start = tok.span.start;
    }
}

/// Assertions mirrored from `fuzz/fuzz_targets/fuzz_cypher_parser.rs`. The
/// parser is total; on success the query is structurally non-empty and the
/// lexer must also accept the same source (the parser consumes the lexer).
fn assert_parser_invariants(label: &str, input: &str) {
    let Ok(query) = parse(input) else {
        // A recoverable ParseError is a valid outcome.
        return;
    };

    assert!(
        !query.parts.is_empty(),
        "{label}: parser returned a query with no parts for {input:?}"
    );
    for part in &query.parts {
        assert!(
            !part.query.clauses.is_empty(),
            "{label}: parser returned a UNION part with no clauses for {input:?}"
        );
    }

    assert!(
        tokenize(input).is_ok(),
        "{label}: parser accepted input the lexer rejects: {input:?}"
    );
}

/// Assertions mirrored from `fuzz/fuzz_targets/fuzz_cypher_grammar.rs`. The
/// generator is total and only emits queries from the supported subset, so
/// every generated query must lex, parse, and be structurally sound.
fn assert_grammar_invariants(label: &str, data: &[u8]) {
    let query = generate_query(data);
    assert!(
        !query.is_empty(),
        "{label}: generate_query returned an empty string for {data:?}"
    );

    assert!(
        tokenize(&query).is_ok(),
        "{label}: generator produced a query the lexer rejects: {query:?} (from {data:?})"
    );

    let parsed = parse(&query).unwrap_or_else(|e| {
        panic!("{label}: generator produced an unparseable query {query:?} (from {data:?}): {e:?}")
    });

    assert!(
        !parsed.parts.is_empty(),
        "{label}: parsed generated query has no parts: {query:?}"
    );
    for part in &parsed.parts {
        assert!(
            !part.query.clauses.is_empty(),
            "{label}: parsed generated query has a UNION part with no clauses: {query:?}"
        );
    }
}

#[test]
fn fuzz_cypher_lexer_seed_corpus_holds_invariants() {
    for (name, input) in read_utf8_corpus("fuzz_cypher_lexer") {
        assert_lexer_invariants(&name, &input);
    }
}

#[test]
fn fuzz_cypher_parser_seed_corpus_holds_invariants() {
    for (name, input) in read_utf8_corpus("fuzz_cypher_parser") {
        assert_parser_invariants(&name, &input);
        // The parser consumes the lexer, so every parser seed is also a
        // valid lexer seed — replay it through the lexer invariants too.
        assert_lexer_invariants(&name, &input);
    }
}

#[test]
fn fuzz_cypher_grammar_seed_corpus_holds_invariants() {
    for (name, data) in read_raw_corpus("fuzz_cypher_grammar") {
        assert_grammar_invariants(&name, &data);
    }
}

/// Exhaustive-ish deterministic sweep over the generator's choice space.
///
/// The seed corpus only pins a handful of representative byte streams; this
/// sweep drives a wide, reproducible range of choice streams through the
/// generator so a parser/grammar drift surfaces in CI without waiting for the
/// nightly fuzzer to rediscover it. Every generated query must parse — that
/// is the whole contract of a grammar-aware generator.
#[test]
fn generator_output_always_parses_over_choice_sweep() {
    // 1) Every single leading-choice byte (selects the top-level clause
    //    shape) crossed with a few tails, so each of the 8 top-level arms is
    //    reached and then steered through its sub-choices.
    let tails: &[&[u8]] = &[
        &[],
        &[0x00],
        &[0xff],
        &[1, 2, 3, 4, 5, 6, 7, 8],
        &[0x55; 16],
        &[0xaa; 16],
        &[3, 1, 4, 1, 5, 9, 2, 6, 5, 3, 5, 8, 9, 7, 9, 3],
    ];
    for lead in 0u8..=255 {
        for tail in tails {
            let mut data = Vec::with_capacity(tail.len() + 1);
            data.push(lead);
            data.extend_from_slice(tail);
            assert_grammar_invariants(&format!("sweep lead={lead}"), &data);
        }
    }

    // 2) A deterministic pseudo-random walk (xorshift32, fixed seed — no
    //    `rand` dependency, fully reproducible) over longer choice streams.
    let mut state: u32 = 0x9e37_79b9;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };
    for i in 0..2_000 {
        let len = (next() % 48) as usize;
        let data: Vec<u8> = (0..len).map(|_| (next() & 0xff) as u8).collect();
        assert_grammar_invariants(&format!("walk #{i}"), &data);
    }
}

/// The generator is deterministic: the same choice stream always yields the
/// same query (no hidden global state, no time/RNG). The stable harness and
/// the nightly fuzzer rely on this to reproduce a crashing input exactly.
#[test]
fn generator_is_deterministic() {
    for seed in [
        &b""[..],
        &b"\x00"[..],
        &b"abcdefgh"[..],
        &b"\xff\xfe\xfd\xfc"[..],
        &[7u8; 32][..],
    ] {
        let a = generate_query(seed);
        let b = generate_query(seed);
        assert_eq!(a, b, "generator is non-deterministic for seed {seed:?}");
    }
}

/// Sanity check: every corpus directory referenced by the fuzz crate's
/// `[[bin]]` entries exists on disk with at least one seed. A forgotten
/// corpus means the nightly fuzz run starts from zero coverage; this guard
/// fails first, with a clear message.
#[test]
fn every_cypher_fuzz_target_has_a_seed_corpus() {
    for target in [
        "fuzz_cypher_lexer",
        "fuzz_cypher_parser",
        "fuzz_cypher_grammar",
    ] {
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

/// Sanity check: the fuzz crate's `Cargo.toml` declares each Cypher target as
/// a `[[bin]]` with the right path. If a refactor renames or drops a target,
/// the stable harness notices in CI before the nightly fuzz job does.
#[test]
fn fuzz_cargo_toml_declares_every_cypher_target() {
    let manifest =
        fs::read_to_string(fuzz_root().join("Cargo.toml")).expect("fuzz/Cargo.toml must exist");
    for target in [
        "fuzz_cypher_lexer",
        "fuzz_cypher_parser",
        "fuzz_cypher_grammar",
    ] {
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
