//! Coverage-guided fuzz target for [`drevo::cypher::lexer::tokenize`].
//!
//! Phase 15 task `00099`. The lexer must be **total** — it never panics on
//! any `&str`, returning either a token stream or a recoverable
//! `LexError`. When it succeeds, the produced tokens satisfy:
//!
//! - **In-bounds spans.** Every token's `span.start <= span.end` and
//!   `span.end <= source.len()` (byte offsets into the input).
//! - **Monotonic spans.** Token spans are non-decreasing by start offset —
//!   the lexer scans left to right and never emits an out-of-order token.
//!
//! Inputs are decoded as `&str` by `libfuzzer-sys` (invalid UTF-8 is
//! filtered before this body runs). The same assertions are mirrored in
//! `tests/cypher_fuzz_harness_tests.rs::assert_lexer_invariants`.

#![no_main]

use drevo::cypher::lexer::tokenize;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &str| {
    let Ok(tokens) = tokenize(input) else {
        // A recoverable LexError is a valid outcome; only a panic fails.
        return;
    };

    let len = input.len();
    let mut prev_start = 0usize;
    for tok in &tokens {
        assert!(
            tok.span.start <= tok.span.end,
            "lexer produced an inverted span {:?} for {input:?}",
            tok.span
        );
        assert!(
            tok.span.end <= len,
            "lexer span {:?} runs past the {len}-byte input {input:?}",
            tok.span
        );
        assert!(
            tok.span.start >= prev_start,
            "lexer emitted a non-monotonic span (start {} < previous {prev_start}) for {input:?}",
            tok.span.start
        );
        prev_start = tok.span.start;
    }
});
