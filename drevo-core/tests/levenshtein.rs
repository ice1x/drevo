//! Levenshtein edit distance (issue #593): the number of single-character
//! insertions, deletions and substitutions turning one string into another,
//! counted over Unicode scalar values.

use drevo_core::levenshtein::{distance, distance_within, similarity};

#[test]
fn known_distances() {
    assert_eq!(distance("kitten", "sitting"), 3);
    assert_eq!(distance("flaw", "lawn"), 2);
    assert_eq!(distance("", "abc"), 3);
    assert_eq!(distance("abc", ""), 3);
    assert_eq!(distance("", ""), 0);
    assert_eq!(distance("same", "same"), 0);
    assert_eq!(distance("recieve", "receive"), 2);
}

#[test]
fn characters_not_bytes() {
    // `ё` and `е` are two bytes each in UTF-8 but one character.
    assert_eq!(distance("ёлка", "елка"), 1);
    assert_eq!(distance("日本語", "日本"), 1);
}

#[test]
fn distance_is_symmetric() {
    for (a, b) in [("graph", "giraffe"), ("", "x"), ("abc", "cba")] {
        assert_eq!(distance(a, b), distance(b, a));
    }
}

#[test]
fn bounded_distance_stops_past_the_limit() {
    assert_eq!(distance_within("kitten", "sitting", 3), Some(3));
    assert_eq!(distance_within("kitten", "sitting", 2), None);
    assert_eq!(distance_within("a", "abcdef", 2), None);
    assert_eq!(distance_within("", "", 0), Some(0));
}

#[test]
fn similarity_is_one_minus_distance_over_the_longer_length() {
    assert!((similarity("abc", "abd") - (1.0 - 1.0 / 3.0)).abs() < 1e-12);
    assert!((similarity("same", "same") - 1.0).abs() < 1e-12);
    assert!((similarity("", "") - 1.0).abs() < 1e-12);
    assert!(similarity("abc", "").abs() < 1e-12);
}

/// The bounded form agrees with the full one on random pairs.
#[test]
fn bounded_agrees_with_full() {
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move |bound: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % bound
    };
    let alphabet = ['a', 'b', 'ё'];
    for _ in 0..2000 {
        let mut word = || -> String {
            let n = next(7);
            (0..n)
                .map(|_| alphabet[next(3) as usize])
                .collect::<String>()
        };
        let a = word();
        let b = word();
        let max = next(5) as usize;
        let d = distance(&a, &b);
        let want = (d <= max).then_some(d);
        assert_eq!(distance_within(&a, &b, max), want, "{a:?} {b:?} {max}");
    }
}
