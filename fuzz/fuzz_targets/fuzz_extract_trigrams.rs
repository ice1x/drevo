//! Coverage-guided fuzz target for
//! [`drevo::fts::tokenizer::extract_trigrams`].
//!
//! Phase 9 task `00058`. This is the two-field tokenizer that
//! `Drevo::create_node` / `Drevo::update_node` call on every node mutation
//! (`src/db.rs`). It must satisfy:
//!
//! - **Equivalence with `trigrams` on the join.** When both fields are
//!   non-empty, `extract_trigrams(t, b) == trigrams(format!("{t} {b}"))`.
//!   When one is empty, it must equal `trigrams` of the other. This pins
//!   down the documented "join with a space" behaviour against accidental
//!   future refactors.
//! - **Same sort+dedup post-conditions as `trigrams`.** Repeated here
//!   for defence in depth — the helper does its own filter so it could
//!   in principle diverge.
//!
//! The fuzz target reads a `(title, body)` tuple via `libfuzzer-sys`'s
//! `Arbitrary` support for tuples.

#![no_main]

use drevo::fts::tokenizer::{extract_trigrams, trigrams};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: (&str, &str)| {
    let (title, body) = input;
    let result = extract_trigrams(title, body);

    // 1. Equivalence with the documented join. We rebuild the combined
    //    string the same way the helper does (matching the special-case
    //    branches when one side is empty) and compare trigram lists.
    let combined_expected = trigrams(&joined(title, body));
    assert_eq!(
        result, combined_expected,
        "extract_trigrams diverged from trigrams(joined) — title={title:?} body={body:?}"
    );

    // 2. Sort + dedup parity is implied by (1), but assert it directly
    //    so a future refactor that bypasses `trigrams` cannot silently
    //    break the inverted-index contract.
    for pair in result.windows(2) {
        assert!(
            pair[0] < pair[1],
            "extract_trigrams output not strictly ascending: {:?} before {:?} (title={title:?} body={body:?})",
            pair[0],
            pair[1]
        );
    }
});

/// Mirror of the private join inside [`drevo::fts::tokenizer::extract_trigrams`].
/// Kept in sync with `src/fts/tokenizer.rs::extract_trigrams`; if that helper
/// changes its join rule, this function must change with it and the fuzz
/// equivalence assertion above will catch the drift.
fn joined(title: &str, body: &str) -> String {
    if body.is_empty() {
        title.to_string()
    } else if title.is_empty() {
        body.to_string()
    } else {
        format!("{title} {body}")
    }
}
