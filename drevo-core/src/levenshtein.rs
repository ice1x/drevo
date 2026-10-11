//! Levenshtein edit distance: the fewest single-character insertions,
//! deletions and substitutions that turn one string into another. Characters
//! are Unicode scalar values, so `"ёлка"` and `"елка"` are one edit apart.
//!
//! ```
//! use drevo_core::levenshtein::{distance, distance_within, similarity};
//!
//! assert_eq!(distance("kitten", "sitting"), 3);
//! // Stop early once the distance is known to exceed a budget.
//! assert_eq!(distance_within("kitten", "sitting", 2), None);
//! assert!((similarity("abc", "abd") - 2.0 / 3.0).abs() < 1e-12);
//! ```

/// The edit distance between `a` and `b`.
#[must_use]
pub fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (diagonal + usize::from(ca != cb))
                .min(above + 1)
                .min(row[j] + 1);
            diagonal = above;
        }
    }
    row[b.len()]
}

/// The edit distance between `a` and `b` if it is at most `max`, else `None`.
/// Gives up as soon as every partial alignment already costs more than `max`.
#[must_use]
pub fn distance_within(a: &str, b: &str, max: usize) -> Option<usize> {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > max {
        return None;
    }
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        let mut best = row[0];
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (diagonal + usize::from(ca != cb))
                .min(above + 1)
                .min(row[j] + 1);
            diagonal = above;
            best = best.min(row[j + 1]);
        }
        if best > max {
            return None;
        }
    }
    let d = row[b.len()];
    (d <= max).then_some(d)
}

/// `1 − distance / max(len(a), len(b))`, in characters: `1.0` for equal
/// strings (two empty strings included), `0.0` when nothing lines up.
#[must_use]
pub fn similarity(a: &str, b: &str) -> f64 {
    let longest = a.chars().count().max(b.chars().count());
    if longest == 0 {
        return 1.0;
    }
    // Lengths of real strings fit an f64's 53-bit mantissa exactly.
    #[allow(clippy::cast_precision_loss)]
    let ratio = distance(a, b) as f64 / longest as f64;
    1.0 - ratio
}
