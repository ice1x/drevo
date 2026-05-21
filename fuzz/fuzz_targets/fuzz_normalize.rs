//! Coverage-guided fuzz target for [`drevo::fts::tokenizer::normalize`].
//!
//! Phase 9 task `00058`. The function must be total (never panics) on any
//! UTF-8 input and must satisfy these post-conditions:
//!
//! - **Idempotent.** `normalize(normalize(s)) == normalize(s)`.
//! - **No leading space.** A run of separators at the start is collapsed.
//! - **No trailing space.** A run of separators at the end is trimmed.
//! - **No consecutive spaces.** Runs of separators are collapsed to one.
//! - **Lowercased.** Every ASCII alphabetic byte in the output is in the
//!   `b'a'..=b'z'` range. We avoid asserting on multi-codepoint case
//!   foldings (e.g. German `ß` → `ss`) because Rust's `char::to_lowercase`
//!   already handles them and re-checking would just re-implement `std`.
//!
//! Inputs are decoded as `&str` via `libfuzzer-sys`'s built-in `Arbitrary`
//! impl, which means the fuzzer reads UTF-8 windows out of the raw byte
//! corpus — invalid UTF-8 is filtered before this body runs.

#![no_main]

use drevo::fts::tokenizer::normalize;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &str| {
    let out = normalize(input);

    // 1. No leading / trailing whitespace, no doubled separators. The
    //    normalizer's invariant is that exactly one ASCII space separates
    //    keepable runs.
    assert!(
        !out.starts_with(' '),
        "normalize leaked a leading space: {out:?} from {input:?}"
    );
    assert!(
        !out.ends_with(' '),
        "normalize leaked a trailing space: {out:?} from {input:?}"
    );
    assert!(
        !out.contains("  "),
        "normalize leaked consecutive spaces: {out:?} from {input:?}"
    );

    // 2. Idempotence — re-normalizing the output is a no-op. This is the
    //    canonical fixed-point property and is the strongest single
    //    invariant we can assert; if it fires, either `normalize` is
    //    losing information or the post-conditions above are wrong.
    let twice = normalize(&out);
    assert_eq!(
        twice, out,
        "normalize is not idempotent: normalize({out:?}) == {twice:?} (input {input:?})"
    );

    // 3. ASCII alphabetic chars are lowercase. We do not test non-ASCII
    //    case folding because Rust's `char::to_lowercase` may emit
    //    multiple code points (Turkish dotless `İ`, German `ß`, …) and
    //    we deliberately delegate to it.
    for c in out.chars() {
        if c.is_ascii_alphabetic() {
            assert!(
                c.is_ascii_lowercase(),
                "normalize left uppercase ASCII char {c:?} in {out:?} (input {input:?})"
            );
        }
    }
});
