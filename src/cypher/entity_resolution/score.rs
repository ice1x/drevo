//! How alike two names (and their embeddings) are, as one score in `[0, 1]`
//! with the reasons behind it.

use super::normalise::{split_initials, words};

/// Words shorter than this never count as shared (`shared_token:` reasons,
/// `'token'` blocking).
pub(crate) const MIN_TOKEN_CHARS: usize = 3;

/// Jaro–Winkler similarity of `a` and `b` over characters: `1` for equal
/// strings, `0` when nothing matches; a common prefix (up to four characters)
/// raises a Jaro similarity above `0.7`.
pub(crate) fn jaro_winkler(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let jaro = jaro(&a, &b);
    if jaro <= 0.7 {
        return jaro;
    }
    let prefix = a.iter().zip(&b).take(4).take_while(|(x, y)| x == y).count();
    jaro + prefix as f64 * 0.1 * (1.0 - jaro)
}

/// Jaro similarity of two character sequences.
fn jaro(a: &[char], b: &[char]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let window = (a.len().max(b.len()) / 2).saturating_sub(1);
    let mut b_used = vec![false; b.len()];
    let mut a_matched: Vec<char> = Vec::new();
    for (i, ca) in a.iter().enumerate() {
        let lo = i.saturating_sub(window);
        let hi = (i + window + 1).min(b.len());
        for j in lo..hi {
            if !b_used[j] && b[j] == *ca {
                b_used[j] = true;
                a_matched.push(*ca);
                break;
            }
        }
    }
    if a_matched.is_empty() {
        return 0.0;
    }
    let b_matched = b
        .iter()
        .zip(&b_used)
        .filter(|(_, used)| **used)
        .map(|(c, _)| *c);
    let transpositions = a_matched
        .iter()
        .zip(b_matched)
        .filter(|(x, y)| **x != *y)
        .count()
        / 2;
    let m = a_matched.len() as f64;
    (m / a.len() as f64 + m / b.len() as f64 + (m - transpositions as f64) / m) / 3.0
}

/// How alike two compared names are: the highest of
/// - their Jaro–Winkler similarity;
/// - the similarity of their words without initials, when the initials do not
///   disagree (one name has none, or both have the same), so «пушкин» matches
///   «а с пушкин»;
/// - when only one name has initials, its similarity to the other with every
///   word it does not share cut to an initial, so «а п смирнова» matches
///   «анна петровна смирнова».
///
/// «а с пушкин» and «л н пушкин» are only as alike as their spelling.
pub(crate) fn name_score(a: &str, b: &str) -> f64 {
    let mut best = jaro_winkler(a, b);
    let (initials_a, rest_a) = split_initials(a);
    let (initials_b, rest_b) = split_initials(b);
    let compatible = initials_a.is_empty() || initials_b.is_empty() || initials_a == initials_b;
    if compatible && !rest_a.is_empty() && !rest_b.is_empty() {
        best = best.max(jaro_winkler(&rest_a.join(" "), &rest_b.join(" ")));
    }
    if !initials_a.is_empty() && initials_b.is_empty() {
        best = best.max(jaro_winkler(a, &abbreviate(b, &rest_a)));
    }
    if initials_a.is_empty() && !initials_b.is_empty() {
        best = best.max(jaro_winkler(&abbreviate(a, &rest_b), b));
    }
    best
}

/// `form` with every word not in `keep` cut to its first character.
fn abbreviate(form: &str, keep: &[&str]) -> String {
    words(form)
        .map(|w| {
            if keep.contains(&w) {
                w.to_string()
            } else {
                w.chars().take(1).collect()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Cosine similarity of two embeddings; `None` when their lengths differ, one
/// is empty or all zeros.
pub(crate) fn vector_score(a: &[f32], b: &[f32]) -> Option<f64> {
    if a.is_empty() || a.len() != b.len() {
        return None;
    }
    crate::vector::cosine_similarity(a, b)
        .ok()
        .map(f64::from)
        .filter(|s| s.is_finite())
}

/// How much each signal counts in the combined score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Weights {
    /// Weight of the name score.
    pub(crate) name: f64,
    /// Weight of the vector score.
    pub(crate) vector: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            name: 0.5,
            vector: 0.5,
        }
    }
}

/// The weighted mean of the signals present: with no vector score it is the
/// name score alone. `None` when the signals present all weigh nothing.
pub(crate) fn combine(name: f64, vector: Option<f64>, weights: Weights) -> Option<f64> {
    match vector {
        None => (weights.name > 0.0).then_some(name),
        Some(_) if weights.vector <= 0.0 => (weights.name > 0.0).then_some(name),
        Some(vector) if weights.name <= 0.0 => Some(vector),
        Some(vector) => {
            Some((weights.name * name + weights.vector * vector) / (weights.name + weights.vector))
        }
    }
}

/// Whether `score` reaches `threshold` (the boundary counts).
pub(crate) fn passes(score: f64, threshold: f64) -> bool {
    score >= threshold
}

/// The words of at least [`MIN_TOKEN_CHARS`] characters both names have,
/// sorted, each once.
pub(crate) fn shared_tokens(a: &str, b: &str) -> Vec<String> {
    let long = |form| -> std::collections::BTreeSet<&str> {
        words(form)
            .filter(|w| w.chars().count() >= MIN_TOKEN_CHARS)
            .collect()
    };
    let theirs = long(b);
    long(a)
        .into_iter()
        .filter(|w| theirs.contains(w))
        .map(str::to_string)
        .collect()
}

/// The reasons of a pair as a reviewer reads them:
/// `['name:0.96', 'vector:0.91', 'shared_token:пушкин']`.
pub(crate) fn reasons(name: f64, vector: Option<f64>, shared: &[String]) -> Vec<String> {
    let mut out = vec![format!("name:{name:.2}")];
    if let Some(vector) = vector {
        out.push(format!("vector:{vector:.2}"));
    }
    out.extend(shared.iter().map(|t| format!("shared_token:{t}")));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn jaro_winkler_reference_values() {
        assert!(close(jaro_winkler("martha", "marhta"), 0.961));
        assert!(close(jaro_winkler("dwayne", "duane"), 0.840));
        assert!(close(jaro_winkler("dixon", "dicksonx"), 0.813));
        assert_eq!(jaro_winkler("пушкин", "пушкин"), 1.0);
        assert_eq!(jaro_winkler("", ""), 1.0);
        assert_eq!(jaro_winkler("abc", ""), 0.0);
        assert_eq!(jaro_winkler("abc", "xyz"), 0.0);
    }

    #[test]
    fn jaro_winkler_counts_characters_not_bytes() {
        // Cyrillic letters are two bytes each; the fifth of six letters
        // differs, exactly as in the ASCII pair.
        let s = jaro_winkler("пушкин", "пушкен");
        assert_eq!(s, jaro_winkler("abcdef", "abcdgf"));
        assert!(s > 0.9 && s < 1.0, "{s}");
    }

    #[test]
    fn jaro_winkler_is_symmetric() {
        for (a, b) in [("пушкин", "пушкина"), ("martha", "marhta"), ("a", "ab")] {
            assert_eq!(jaro_winkler(a, b), jaro_winkler(b, a));
        }
    }

    #[test]
    fn a_name_without_initials_matches_the_name_with_them() {
        assert_eq!(name_score("пушкин", "а с пушкин"), 1.0);
        assert_eq!(name_score("а с пушкин", "пушкин"), 1.0);
        assert_eq!(name_score("а с пушкин", "а с пушкин"), 1.0);
    }

    #[test]
    fn disagreeing_initials_count_as_spelling() {
        let s = name_score("а с пушкин", "л н пушкин");
        assert_eq!(s, jaro_winkler("а с пушкин", "л н пушкин"));
        assert!(s < 0.9, "{s}");
    }

    #[test]
    fn initials_match_the_words_they_stand_for() {
        assert_eq!(name_score("а п смирнова", "анна петровна смирнова"), 1.0);
        assert_eq!(name_score("анна петровна смирнова", "а п смирнова"), 1.0);
        assert!(name_score("а п смирнова", "борис петрович смирнов") < 0.9);
        // Without initials on either side, full names are only spelling.
        let s = name_score("анна смирнова", "алла смирнова");
        assert_eq!(s, jaro_winkler("анна смирнова", "алла смирнова"));
    }

    #[test]
    fn a_case_ending_is_close_but_not_equal() {
        let s = name_score("пушкин", "пушкина");
        assert!(s > 0.95 && s < 1.0, "{s}");
        assert!(name_score("пушкин", "pushkin") < 0.5);
    }

    #[test]
    fn vector_score_is_cosine() {
        assert!(close(vector_score(&[1.0, 0.0], &[1.0, 0.0]).unwrap(), 1.0));
        assert!(close(vector_score(&[1.0, 0.0], &[0.0, 1.0]).unwrap(), 0.0));
        assert!(close(
            vector_score(&[1.0, 1.0], &[1.0, 0.0]).unwrap(),
            std::f64::consts::FRAC_1_SQRT_2
        ));
        assert_eq!(vector_score(&[1.0, 0.0], &[1.0, 0.0, 0.0]), None);
        assert_eq!(vector_score(&[], &[]), None);
        assert_eq!(vector_score(&[0.0, 0.0], &[1.0, 0.0]), None);
    }

    #[test]
    fn a_missing_vector_renormalises_to_the_name_score() {
        let w = Weights::default();
        assert_eq!(combine(0.8, None, w), Some(0.8));
        assert_eq!(
            combine(
                0.9,
                None,
                Weights {
                    name: 0.3,
                    vector: 0.7
                }
            ),
            Some(0.9)
        );
        assert!(close(combine(0.8, Some(0.6), w).unwrap(), 0.7));
        let skewed = Weights {
            name: 0.75,
            vector: 0.25,
        };
        assert!(close(combine(0.8, Some(0.4), skewed).unwrap(), 0.7));
    }

    #[test]
    fn signals_that_weigh_nothing_give_no_score() {
        let vector_only = Weights {
            name: 0.0,
            vector: 1.0,
        };
        assert_eq!(combine(0.8, None, vector_only), None);
        assert_eq!(combine(0.8, Some(0.5), vector_only), Some(0.5));
    }

    #[test]
    fn the_threshold_boundary_passes() {
        assert!(passes(0.9, 0.9));
        assert!(passes(1.0, 0.9));
        assert!(!passes(0.899_999, 0.9));
    }

    #[test]
    fn shared_tokens_skip_short_words() {
        assert_eq!(shared_tokens("а с пушкин", "пушкин"), ["пушкин"]);
        assert_eq!(
            shared_tokens("пушкин александр ал", "ал александр пушкин пушкин"),
            ["александр", "пушкин"]
        );
        assert!(shared_tokens("а с", "а с").is_empty());
        assert!(shared_tokens("пушкин", "пушкина").is_empty());
    }

    #[test]
    fn reasons_read_name_vector_then_tokens() {
        assert_eq!(
            reasons(0.961, Some(0.9149), &["пушкин".to_string()]),
            ["name:0.96", "vector:0.91", "shared_token:пушкин"]
        );
        assert_eq!(reasons(1.0, None, &[]), ["name:1.00"]);
    }
}
