//! Which pairs of names are compared at all. Only names in the same group
//! (the same `type`) are paired; within a group a blocking strategy keeps the
//! pairs that share a key, and a hard cap refuses a comparison that would
//! grow too large.

use std::collections::{BTreeMap, BTreeSet};

use super::normalise::{split_initials, words};
use super::score::MIN_TOKEN_CHARS;

/// How candidate pairs are chosen within a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Blocking {
    /// Names whose first three characters agree, initials left out
    /// («а с пушкин» and «пушкина» share «пуш»).
    Prefix3,
    /// Names sharing a word of at least three characters.
    Token,
    /// Every pair.
    None,
}

impl Blocking {
    /// The config spelling: `'prefix3'`, `'token'` or `'none'`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Prefix3 => "prefix3",
            Self::Token => "token",
            Self::None => "none",
        }
    }
}

/// A name to pair: its group (pairs never cross groups) and its compared form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Item {
    /// The group key, e.g. the node's `type`.
    pub(crate) group: String,
    /// The compared form of the name.
    pub(crate) name: String,
}

/// The pairs `(i, j)`, `i < j`, of `items` to compare, in order.
///
/// # Errors
///
/// A message naming `maxPairs` and suggesting blocking when more than
/// `max_pairs` pairs would be compared.
pub(crate) fn candidate_pairs(
    items: &[Item],
    blocking: Blocking,
    max_pairs: u64,
) -> Result<Vec<(usize, usize)>, String> {
    if blocking == Blocking::None {
        let mut sizes: BTreeMap<&str, u64> = BTreeMap::new();
        for item in items {
            *sizes.entry(item.group.as_str()).or_insert(0) += 1;
        }
        let total: u64 = sizes
            .values()
            .map(|n| n.saturating_mul(n.saturating_sub(1)) / 2)
            .fold(0, u64::saturating_add);
        if total > max_pairs {
            return Err(format!(
                "blocking 'none' would compare {total} pairs, more than maxPairs = {max_pairs}; \
                 use blocking 'prefix3' or 'token', or raise maxPairs"
            ));
        }
        let mut pairs = Vec::new();
        for i in 0..items.len() {
            for j in i + 1..items.len() {
                if items[i].group == items[j].group {
                    pairs.push((i, j));
                }
            }
        }
        return Ok(pairs);
    }
    let mut buckets: BTreeMap<(&str, String), Vec<usize>> = BTreeMap::new();
    for (i, item) in items.iter().enumerate() {
        for key in keys(&item.name, blocking) {
            buckets
                .entry((item.group.as_str(), key))
                .or_default()
                .push(i);
        }
    }
    let mut pairs: BTreeSet<(usize, usize)> = BTreeSet::new();
    for members in buckets.values() {
        for (n, &i) in members.iter().enumerate() {
            for &j in &members[n + 1..] {
                if i != j && pairs.insert((i.min(j), i.max(j))) && pairs.len() as u64 > max_pairs {
                    return Err(format!(
                        "blocking '{}' would compare more than maxPairs = {max_pairs} pairs; \
                         raise maxPairs or narrow the label",
                        blocking.name()
                    ));
                }
            }
        }
    }
    Ok(pairs.into_iter().collect())
}

/// The blocking keys of a compared name: its first three characters without
/// initials (`prefix3`), or its words of at least [`MIN_TOKEN_CHARS`]
/// characters (`token`). An empty name has none.
fn keys(name: &str, blocking: Blocking) -> Vec<String> {
    match blocking {
        Blocking::Prefix3 => {
            let (_, rest) = split_initials(name);
            let core = if rest.is_empty() {
                name.to_string()
            } else {
                rest.join(" ")
            };
            let key: String = core.chars().take(3).collect();
            if key.is_empty() {
                Vec::new()
            } else {
                vec![key]
            }
        }
        Blocking::Token => {
            let mut tokens: Vec<String> = words(name)
                .filter(|w| w.chars().count() >= MIN_TOKEN_CHARS)
                .map(str::to_string)
                .collect();
            tokens.sort();
            tokens.dedup();
            tokens
        }
        Blocking::None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(entries: &[(&str, &str)]) -> Vec<Item> {
        entries
            .iter()
            .map(|(group, name)| Item {
                group: (*group).to_string(),
                name: (*name).to_string(),
            })
            .collect()
    }

    #[test]
    fn prefix3_pairs_names_sharing_a_prefix_without_initials() {
        let names = items(&[
            ("PERSON", "пушкин"),
            ("PERSON", "пушкина"),
            ("PERSON", "а с пушкин"),
            ("PERSON", "лермонтов"),
        ]);
        assert_eq!(
            candidate_pairs(&names, Blocking::Prefix3, 100).unwrap(),
            [(0, 1), (0, 2), (1, 2)]
        );
    }

    #[test]
    fn prefix3_keys_short_names_by_the_whole_name() {
        let names = items(&[("P", "ли"), ("P", "ли"), ("P", "лиу")]);
        assert_eq!(
            candidate_pairs(&names, Blocking::Prefix3, 100).unwrap(),
            [(0, 1)]
        );
    }

    #[test]
    fn pairs_never_cross_groups() {
        let names = items(&[
            ("PERSON", "пушкин"),
            ("LOCATION", "пушкина"),
            ("PERSON", "а с пушкин"),
        ]);
        for blocking in [Blocking::Prefix3, Blocking::Token, Blocking::None] {
            assert_eq!(
                candidate_pairs(&names, blocking, 100).unwrap(),
                [(0, 2)],
                "{blocking:?}"
            );
        }
    }

    #[test]
    fn token_pairs_names_sharing_a_long_word() {
        let names = items(&[
            ("P", "александр пушкин"),
            ("P", "пушкин"),
            ("P", "ал лермонтов"),
            ("P", "ал толстой"),
        ]);
        // "ал" is too short to count.
        assert_eq!(
            candidate_pairs(&names, Blocking::Token, 100).unwrap(),
            [(0, 1)]
        );
    }

    #[test]
    fn none_pairs_everything_in_a_group() {
        let names = items(&[("P", "a"), ("P", "b"), ("P", "c")]);
        assert_eq!(
            candidate_pairs(&names, Blocking::None, 100).unwrap(),
            [(0, 1), (0, 2), (1, 2)]
        );
    }

    #[test]
    fn none_respects_max_pairs() {
        let names = items(&[("P", "a"), ("P", "b"), ("P", "c")]);
        // Exactly at the cap is allowed.
        assert_eq!(candidate_pairs(&names, Blocking::None, 3).unwrap().len(), 3);
        let err = candidate_pairs(&names, Blocking::None, 2).unwrap_err();
        assert_eq!(
            err,
            "blocking 'none' would compare 3 pairs, more than maxPairs = 2; \
             use blocking 'prefix3' or 'token', or raise maxPairs"
        );
    }

    #[test]
    fn blocked_comparisons_respect_max_pairs_too() {
        let names = items(&[("P", "пушкин"), ("P", "пушкина"), ("P", "пушкины")]);
        let err = candidate_pairs(&names, Blocking::Prefix3, 2).unwrap_err();
        assert_eq!(
            err,
            "blocking 'prefix3' would compare more than maxPairs = 2 pairs; \
             raise maxPairs or narrow the label"
        );
    }

    #[test]
    fn empty_names_are_not_paired_by_blocking() {
        let names = items(&[("P", ""), ("P", "")]);
        assert!(candidate_pairs(&names, Blocking::Prefix3, 100)
            .unwrap()
            .is_empty());
        assert!(candidate_pairs(&names, Blocking::Token, 100)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn blocking_names() {
        assert_eq!(Blocking::Prefix3.name(), "prefix3");
        assert_eq!(Blocking::Token.name(), "token");
        assert_eq!(Blocking::None.name(), "none");
    }
}
