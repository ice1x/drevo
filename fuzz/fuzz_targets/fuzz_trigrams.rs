//! Coverage-guided fuzz target for [`drevo::fts::tokenizer::trigrams`].
//!
//! Phase 9 task `00058`. The function must be total and satisfy:
//!
//! - **Sorted strictly ascending.** The output backs `scan_prefix` lookups
//!   in `src/fts/index.rs`; an unsorted or duplicate-containing list would
//!   corrupt the inverted index.
//! - **Deduplicated.** Implied by the sort assertion above (strict
//!   ascending = no equal adjacent pairs).
//! - **Char-length 2 or 3.** Sliding trigrams contribute 3-char windows;
//!   CJK bigrams contribute 2-char windows. Nothing else is legal.
//! - **Empty on short inputs.** Inputs whose normalized form has fewer
//!   than 2 chars produce an empty trigram list.

#![no_main]

use drevo::fts::tokenizer::{normalize, trigrams};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &str| {
    let result = trigrams(input);

    // 1. Strictly ascending order — combines "sorted" and "deduplicated".
    for pair in result.windows(2) {
        assert!(
            pair[0] < pair[1],
            "trigrams output is not strictly ascending: {:?} before {:?} (input {input:?})",
            pair[0],
            pair[1]
        );
    }

    // 2. Char-length window bound: each entry is a 2-char (CJK bigram)
    //    or 3-char (sliding trigram) window. We count `chars()`, not
    //    bytes, because a CJK code point is 3 bytes in UTF-8 but 1 char.
    for token in &result {
        let n = token.chars().count();
        assert!(
            n == 2 || n == 3,
            "trigram {token:?} has char-length {n}, expected 2 or 3 (input {input:?})"
        );
    }

    // 3. Empty-on-short rule. If the normalized text has < 2 chars the
    //    output must be empty — there are no 2-char or 3-char windows to
    //    extract.
    let norm = normalize(input);
    if norm.chars().count() < 2 {
        assert!(
            result.is_empty(),
            "trigrams produced {result:?} for normalized input {norm:?} (raw {input:?})"
        );
    }
});
