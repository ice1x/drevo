//! English stopword list for keyword extraction (task `00132`).
//!
//! Keyword extraction ([`crate::fts::keywords`]) drops these high-frequency
//! function words *before* IDF ranking so that grammatical glue ("the", "of",
//! "and") never surfaces as a "salient" term. BM25 IDF alone would already
//! down-weight ubiquitous words, but only relative to the indexed corpus —
//! a brand-new or tiny corpus has unreliable document frequencies, so an
//! explicit stoplist keeps the output sensible from the very first document.
//!
//! English-only to start; a language axis is a deliberate follow-up (the
//! roadmap entry for `00132`). The list is the conventional ~120-word
//! English stoplist (Snowball / NLTK lineage), kept **sorted** so lookups
//! are an `O(log n)` binary search. A unit test asserts the sort invariant
//! so an out-of-order edit fails loudly rather than silently breaking
//! membership tests.

/// The sorted English stopword list. MUST stay alphabetically sorted —
/// [`is_stopword`] relies on binary search, and `stopwords_list_is_sorted`
/// guards the invariant.
const STOPWORDS: &[&str] = &[
    "a",
    "about",
    "above",
    "after",
    "again",
    "against",
    "all",
    "also",
    "am",
    "an",
    "and",
    "any",
    "are",
    "as",
    "at",
    "be",
    "because",
    "been",
    "before",
    "being",
    "below",
    "between",
    "both",
    "but",
    "by",
    "can",
    "cannot",
    "could",
    "did",
    "do",
    "does",
    "doing",
    "down",
    "during",
    "each",
    "few",
    "for",
    "from",
    "further",
    "had",
    "has",
    "have",
    "having",
    "he",
    "her",
    "here",
    "hers",
    "herself",
    "him",
    "himself",
    "his",
    "how",
    "i",
    "if",
    "in",
    "into",
    "is",
    "it",
    "its",
    "itself",
    "just",
    "me",
    "more",
    "most",
    "my",
    "myself",
    "no",
    "nor",
    "not",
    "now",
    "of",
    "off",
    "on",
    "once",
    "only",
    "or",
    "other",
    "our",
    "ours",
    "ourselves",
    "out",
    "over",
    "own",
    "same",
    "she",
    "should",
    "so",
    "some",
    "such",
    "than",
    "that",
    "the",
    "their",
    "theirs",
    "them",
    "themselves",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "through",
    "to",
    "too",
    "under",
    "until",
    "up",
    "very",
    "was",
    "we",
    "were",
    "what",
    "when",
    "where",
    "which",
    "while",
    "who",
    "whom",
    "why",
    "will",
    "with",
    "would",
    "you",
    "your",
    "yours",
    "yourself",
    "yourselves",
];

/// Returns `true` if `word` is an English stopword.
///
/// `word` is expected to already be lowercased (keyword extraction
/// lowercases during tokenization), matching the lowercase entries above.
pub(crate) fn is_stopword(word: &str) -> bool {
    STOPWORDS.binary_search(&word).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopwords_list_is_sorted() {
        // Binary search correctness depends on this; assert it explicitly so
        // a careless insert is caught at test time rather than corrupting
        // membership silently.
        for pair in STOPWORDS.windows(2) {
            assert!(
                pair[0] < pair[1],
                "stopword list not strictly sorted: {:?} !< {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn common_words_are_stopwords() {
        for w in [
            "the", "and", "of", "a", "to", "is", "with", "by", "for", "this",
        ] {
            assert!(is_stopword(w), "{w} should be a stopword");
        }
    }

    #[test]
    fn content_words_are_not_stopwords() {
        for w in ["graph", "database", "anxiety", "invoice", "rust", "neo4j"] {
            assert!(!is_stopword(w), "{w} should not be a stopword");
        }
    }

    #[test]
    fn membership_is_case_sensitive_lowercase() {
        // The list is lowercase; callers must lowercase first.
        assert!(is_stopword("the"));
        assert!(!is_stopword("The"));
    }
}
