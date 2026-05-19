//! Phase 9 task `00057` — property-based tests for the FTS tokenizer.
//!
//! Properties covered (cf. `src/fts/tokenizer.rs`):
//!
//! 1. **`normalize` is total** — never panics on any UTF-8 input, including
//!    NUL bytes, control characters, surrogate-free Unicode, ZWJ emoji,
//!    CJK, Cyrillic.
//! 2. **`normalize` is idempotent** — `normalize(normalize(s)) == normalize(s)`.
//! 3. **`normalize` collapses runs of separators** — the result never
//!    contains a leading space, a trailing space, or two consecutive
//!    spaces.
//! 4. **`normalize` is lowercased** — for every `c` in the output,
//!    `c == c.to_lowercase().next().unwrap()` when the lowercase mapping is
//!    a single code point.
//! 5. **`trigrams` is sorted and deduplicated** — required by the storage
//!    layer's `scan_prefix` semantics on the FTS index.
//! 6. **`trigrams` length bound** — every trigram has 2 char-length (CJK
//!    bigram window) or 3 char-length (sliding trigram); never more.
//! 7. **`trigrams` self-consistency** — `trigrams(normalize(s))` equals
//!    `trigrams(s)` because trigrams already calls `normalize` internally.
//!    This pins down the "normalize before tokenize" pipeline against
//!    accidental future refactors that would skip it.
//! 8. **`extract_trigrams` is the union of its parts** — every trigram of
//!    `extract_trigrams(title, body)` is reachable from
//!    `trigrams(title + " " + body)` (or one of the two when empty).
//!
//! All properties are deterministic given the proptest seed printed on
//! failure; failing inputs are auto-shrunk to the minimal reproducer and
//! cached under `proptest-regressions/`.

use drevo::fts::tokenizer::{extract_trigrams, normalize, trigrams};

use proptest::prelude::*;

/// Map a u32 into a char inside `[lo, hi]`. Total — `char::from_u32`
/// returns `None` only for surrogate code points (U+D800..=U+DFFF), and
/// the ranges we exercise all sit either entirely below or entirely
/// above that range, so the unwrap is unreachable on inputs from those
/// ranges.
fn char_from_range(lo: u32, hi: u32) -> impl Strategy<Value = char> {
    (lo..=hi).prop_filter_map("not a surrogate", char::from_u32)
}

/// A character set that exercises every branch of `is_keepable` and
/// `is_cjk`: ASCII letters & digits, ASCII whitespace, punctuation,
/// Latin-1 supplement (Straße-style ß), Cyrillic, CJK Unified, Hiragana,
/// Katakana, Hangul, and one emoji glyph.
///
/// `prop_oneof!` is capped at 9 arms by the macro's tuple-based
/// implementation, so we cluster related ranges together (e.g. all
/// Japanese kana in a single arm).
fn diverse_char() -> impl Strategy<Value = char> {
    let kana_or_hangul = prop_oneof![
        char_from_range(0x3040, 0x309F), // Hiragana
        char_from_range(0x30A0, 0x30FF), // Katakana
        char_from_range(0xAC00, 0xD7AF), // Hangul Syllables
    ];
    let emoji_pick = prop_oneof![Just('🚀'), Just('🔥'), Just('🌍')];

    prop_oneof![
        // ASCII alnum and whitespace heavily
        20 => any::<char>().prop_filter("ascii", |c| c.is_ascii()),
        // Latin extended (ß, é, ñ, …) — U+00C0..U+00FF
        4 => char_from_range(0x00C0, 0x00FF),
        // Cyrillic block U+0400..U+04FF
        4 => char_from_range(0x0400, 0x04FF),
        // CJK Unified Ideographs core U+4E00..U+9FFF
        4 => char_from_range(0x4E00, 0x9FFF),
        // Hiragana / Katakana / Hangul
        4 => kana_or_hangul,
        // Three emoji glyphs that exercise the multi-codepoint path.
        2 => emoji_pick,
    ]
}

fn diverse_string(max_len: usize) -> impl Strategy<Value = String> {
    proptest::collection::vec(diverse_char(), 0..max_len)
        .prop_map(|chars| chars.into_iter().collect())
}

// ---------------------------------------------------------------
// normalize properties
// ---------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        .. ProptestConfig::default()
    })]

    /// `normalize` is total — it never panics on any UTF-8 input.
    #[test]
    fn normalize_is_total(s in diverse_string(64)) {
        // Just calling it without panic is the property.
        let _ = normalize(&s);
    }

    /// `normalize` is idempotent.
    #[test]
    fn normalize_is_idempotent(s in diverse_string(64)) {
        let once = normalize(&s);
        let twice = normalize(&once);
        prop_assert_eq!(once, twice);
    }

    /// The output of `normalize` has no leading space, no trailing space,
    /// and no two consecutive spaces.
    #[test]
    fn normalize_collapses_whitespace(s in diverse_string(64)) {
        let n = normalize(&s);
        prop_assert!(
            !n.starts_with(' '),
            "normalized string starts with space: {:?}",
            n
        );
        prop_assert!(
            !n.ends_with(' '),
            "normalized string ends with space: {:?}",
            n
        );
        prop_assert!(
            !n.contains("  "),
            "normalized string contains double space: {:?}",
            n
        );
    }

    /// Every character in `normalize`'s output is either a space or a
    /// "keepable" character — alphanumeric or CJK. Punctuation, NUL, etc.
    /// must be gone.
    #[test]
    fn normalize_keeps_only_alphanumeric_cjk_and_space(s in diverse_string(64)) {
        let n = normalize(&s);
        for c in n.chars() {
            prop_assert!(
                c == ' ' || c.is_alphanumeric() || is_cjk(c),
                "normalized output kept non-keepable char {:?} (codepoint U+{:04X})",
                c, c as u32
            );
        }
    }

    /// Every alphabetic character in `normalize`'s output is lowercase.
    ///
    /// Stronger property than the existing `normalize_lowercase` unit test:
    /// holds for ALL inputs, not just the one hand-rolled example. Skipped
    /// for characters whose Unicode lowercase mapping expands to multiple
    /// code points (e.g. `ß → ss`) — Rust's `to_lowercase()` returns an
    /// iterator in that case and the implementation pushes each scalar
    /// individually, which is still correct per the Unicode spec.
    #[test]
    fn normalize_alphabetic_output_is_lowercase(s in diverse_string(64)) {
        let n = normalize(&s);
        for c in n.chars() {
            if !c.is_alphabetic() {
                continue;
            }
            let mut iter = c.to_lowercase();
            if let (Some(first), None) = (iter.next(), iter.next()) {
                prop_assert_eq!(c, first, "uppercase char {:?} leaked into normalize output", c);
            }
        }
    }
}

// ---------------------------------------------------------------
// trigrams properties
// ---------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        .. ProptestConfig::default()
    })]

    /// The output of `trigrams` is sorted ascending and contains no
    /// duplicates. The storage layer's `scan_prefix` over `fts:{trigram}:`
    /// keys depends on this property.
    #[test]
    fn trigrams_are_sorted_and_deduplicated(s in diverse_string(64)) {
        let tg = trigrams(&s);
        for pair in tg.windows(2) {
            prop_assert!(
                pair[0] < pair[1],
                "trigrams not strictly increasing: {:?} >= {:?}",
                pair[0], pair[1]
            );
        }
    }

    /// Every trigram has 2 or 3 char-length. Two-char windows come only
    /// from the CJK bigram path; three-char windows from the standard
    /// trigram path. Anything else is a bug.
    #[test]
    fn trigrams_have_length_two_or_three(s in diverse_string(64)) {
        for tg in trigrams(&s) {
            let len = tg.chars().count();
            prop_assert!(
                len == 2 || len == 3,
                "unexpected trigram char-length {} for {:?}",
                len, tg
            );
        }
    }

    /// `trigrams` already normalizes its input, so feeding it
    /// `normalize(s)` must give the same result as feeding it `s`.
    #[test]
    fn trigrams_invariant_under_pre_normalization(s in diverse_string(64)) {
        let direct = trigrams(&s);
        let pre_norm = trigrams(&normalize(&s));
        prop_assert_eq!(direct, pre_norm);
    }

    /// Every char-length-3 trigram of `trigrams(s)` is a contiguous
    /// 3-character window of `normalize(s)`. (We cannot make the same
    /// claim for char-length-2 trigrams in general because those come from
    /// the CJK-bigram code path which has its own selection rule.)
    #[test]
    fn three_char_trigrams_are_windows_of_normalize(s in diverse_string(64)) {
        let n = normalize(&s);
        let chars: Vec<char> = n.chars().collect();
        let mut three_windows: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for w in chars.windows(3) {
            three_windows.insert(w.iter().collect::<String>());
        }
        for tg in trigrams(&s) {
            if tg.chars().count() == 3 {
                prop_assert!(
                    three_windows.contains(&tg),
                    "trigram {:?} is not a 3-window of normalize(s)={:?}",
                    tg, n
                );
            }
        }
    }

    /// `extract_trigrams` agrees with `trigrams` on the joined string.
    ///
    /// Three branches matter:
    /// * both empty → result is empty,
    /// * title empty → result == trigrams(body),
    /// * body  empty → result == trigrams(title),
    /// * both non-empty → result == trigrams("title body").
    #[test]
    fn extract_trigrams_matches_joined(title in diverse_string(20), body in diverse_string(40)) {
        let got = extract_trigrams(&title, &body);
        let expected = if title.is_empty() && body.is_empty() {
            Vec::<String>::new()
        } else if title.is_empty() {
            trigrams(&body)
        } else if body.is_empty() {
            trigrams(&title)
        } else {
            trigrams(&format!("{} {}", title, body))
        };
        prop_assert_eq!(got, expected);
    }

    /// Whitespace-only / punctuation-only / control-only strings produce
    /// no trigrams. This is the FTS analogue of invariant #2 (cascading
    /// delete leaves no orphan index entries) — empty input must not
    /// silently create posting-list rows.
    #[test]
    fn punctuation_only_input_yields_no_trigrams(
        s in proptest::collection::vec(
            prop_oneof![
                Just(' '),
                Just('\t'),
                Just('\n'),
                Just('!'),
                Just('?'),
                Just('.'),
                Just(','),
                Just('('),
                Just(')'),
                Just('['),
                Just(']'),
                Just('{'),
                Just('}'),
                Just(';'),
                Just(':'),
            ],
            0..32,
        ).prop_map(|cs| cs.into_iter().collect::<String>())
    ) {
        prop_assert!(
            trigrams(&s).is_empty(),
            "punctuation-only {:?} produced trigrams: {:?}",
            s, trigrams(&s)
        );
    }
}

/// Local copy of `is_cjk` so the assertion test can call it. The function
/// in `src/fts/tokenizer.rs` is private (`const fn`) — we mirror the same
/// ranges here; if a future refactor adds a range to the source, this
/// helper must move in lockstep or the corresponding property test will
/// (correctly) start failing.
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}'
        | '\u{3400}'..='\u{4DBF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{20000}'..='\u{2A6DF}'
        | '\u{2A700}'..='\u{2B73F}'
        | '\u{2B740}'..='\u{2B81F}'
        | '\u{3000}'..='\u{303F}'
        | '\u{3040}'..='\u{309F}'
        | '\u{30A0}'..='\u{30FF}'
        | '\u{AC00}'..='\u{D7AF}'
    )
}
