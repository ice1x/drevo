//! Keyword extraction (task `00132`).
//!
//! Picks the top-`k` most *salient* terms from a piece of text by combining
//! three signals already shipped in the FTS layer:
//!
//! 1. **Word tokenization** ([`crate::fts::tokenizer::words`]) — lowercase
//!    word tokens, distinct from the character-trigram index tokenizer.
//! 2. **Stopword removal** ([`crate::fts::stopwords`]) — drop English
//!    function words so grammatical glue never ranks.
//! 3. **BM25 IDF salience** (`bm25_idf`, task `00131`) —
//!    weight each surviving term by how *rare* it is across the indexed
//!    corpus. A term's document frequency is estimated from the existing
//!    trigram posting lists (the docs containing all of the term's trigrams),
//!    so no new index is needed.
//!
//! The per-text term frequency multiplies the IDF (classic `tf·idf`), so a
//! term that is both rare in the corpus *and* repeated in the text ranks
//! highest; when every term occurs once this degrades gracefully to a pure
//! IDF ranking. Optional Porter stemming ([`crate::fts::stemmer`]) collapses
//! morphological variants before counting.
//!
//! Surfaced to Cypher as the `keywords(text, k [, stem])` scalar function,
//! which composes in `RETURN` / `WHERE` and per-row over a `MATCH`. The
//! faceted `UNWIND keywords(...) AS kw ... count(*)` group-by is the intended
//! downstream consumer once the executor's `UNWIND` clause lands.
//!
//! Determinism: ties (equal score) break alphabetically, so the output is
//! stable across runs regardless of hash-map iteration order — the Cypher
//! e2e suite depends on stable `RETURN` output.

use std::collections::HashMap;

use crate::error::Result;
use crate::fts::stemmer::stem;
use crate::fts::stopwords::is_stopword;
use crate::fts::tokenizer::{trigrams, words};
use drevo_core::bm25::bm25_idf;

/// Engine-agnostic keyword extraction (#447 native port): the tokenize →
/// stopword → stem → BM25 tf·idf ranking, parameterised by the two corpus
/// statistics an engine must supply — `n` (indexed document count) and `df`
/// (documents containing all of a term's trigrams). The native path
/// (`NativeFtsIndex::doc_count` / `trigram_df`) feeds these, so the ranking is
/// engine-independent.
///
/// # Errors
/// Propagates a `df` lookup failure.
pub(crate) fn extract_keywords_scored<F>(
    text: &str,
    k: usize,
    stem_terms: bool,
    n: u64,
    df: &F,
) -> Result<Vec<String>>
where
    F: Fn(&[String]) -> Result<u64>,
{
    if k == 0 {
        return Ok(Vec::new());
    }

    // 1. Tokenize, drop stopwords, optionally stem, accumulate term frequency.
    let mut tf: HashMap<String, u32> = HashMap::new();
    for word in words(text) {
        if is_stopword(&word) {
            continue;
        }
        let term = if stem_terms { stem(&word) } else { word };
        if term.is_empty() {
            continue;
        }
        *tf.entry(term).or_insert(0) += 1;
    }
    if tf.is_empty() {
        return Ok(Vec::new());
    }

    // 2. Score each distinct term by tf · idf.
    let mut scored: Vec<(String, f32)> = Vec::with_capacity(tf.len());
    for (term, freq) in tf {
        let term_trigrams = trigrams(&term);
        // Estimate document frequency from the trigram index: documents that
        // contain *all* of the term's trigrams. For a term too short to have
        // any trigram (e.g. a 2-char token) we cannot estimate df, so we
        // assign it the minimal IDF (df = N) rather than the maximal one,
        // keeping such low-signal tokens from floating to the top.
        let df_val = if term_trigrams.is_empty() {
            n
        } else {
            df(&term_trigrams)?
        };
        // df can never exceed N for a consistent index, but clamp defensively
        // so bm25_idf stays non-negative even against a legacy/over-counted
        // index.
        let idf = bm25_idf(n, df_val.min(n));
        scored.push((term, freq as f32 * idf));
    }

    // 3. Rank: score descending, then term ascending for deterministic ties.
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.truncate(k);
    Ok(scored.into_iter().map(|(term, _)| term).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Run keyword extraction against an explicit corpus, computing the two
    /// statistics `extract_keywords_scored` needs from `docs`: `n` (document
    /// count) and, per term, the number of documents whose body contains *all*
    /// of the term's trigrams. This exercises exactly the ranking logic the
    /// engines feed (the native path supplies the same two stats from
    /// `NativeFtsIndex`), with no storage backend involved.
    fn keywords(docs: &[&str], text: &str, k: usize, stem_terms: bool) -> Vec<String> {
        let n = docs.len() as u64;
        let doc_trigrams: Vec<HashSet<String>> = docs
            .iter()
            .map(|d| trigrams(d).into_iter().collect())
            .collect();
        extract_keywords_scored(text, k, stem_terms, n, &|term_trigrams| {
            Ok(doc_trigrams
                .iter()
                .filter(|dt| term_trigrams.iter().all(|t| dt.contains(t)))
                .count() as u64)
        })
        .unwrap()
    }

    #[test]
    fn k_zero_returns_empty() {
        assert!(keywords(&[], "graph database", 0, false).is_empty());
    }

    #[test]
    fn empty_and_stopword_only_text_returns_empty() {
        assert!(keywords(&[], "", 5, false).is_empty());
        assert!(keywords(&[], "the and of to is", 5, false).is_empty());
    }

    #[test]
    fn stopwords_are_dropped() {
        let kws = keywords(&[], "the graph and the database", 5, false);
        assert!(kws.contains(&"graph".to_string()));
        assert!(kws.contains(&"database".to_string()));
        assert!(!kws.iter().any(|w| w == "the" || w == "and"));
    }

    #[test]
    fn rarer_term_outranks_common_term() {
        // "database" appears in many docs (low IDF); "photosynthesis" in one
        // (high IDF). Both occur once in the query text, so IDF decides.
        let mut docs: Vec<&str> = vec!["database systems"; 8];
        docs.push("photosynthesis chloroplast");
        let kws = keywords(&docs, "database photosynthesis", 2, false);
        assert_eq!(
            kws.first().map(String::as_str),
            Some("photosynthesis"),
            "rare term should rank first, got {kws:?}"
        );
    }

    #[test]
    fn term_frequency_breaks_toward_repeated_terms() {
        // With an empty corpus every term shares the same IDF, so the more
        // frequent term wins — tf·idf degrades to tf ranking.
        let kws = keywords(&[], "anxiety anxiety anxiety journaling", 1, false);
        assert_eq!(kws, vec!["anxiety"]);
    }

    #[test]
    fn respects_k_limit() {
        let kws = keywords(&[], "alpha beta gamma delta epsilon", 3, false);
        assert_eq!(kws.len(), 3);
    }

    #[test]
    fn deterministic_tie_break_is_alphabetical() {
        // Empty corpus + all-distinct single-occurrence terms => equal score;
        // alphabetical order must decide, stably.
        let first = keywords(&[], "zebra apple mango", 3, false);
        let second = keywords(&[], "mango zebra apple", 3, false);
        assert_eq!(first, second);
        assert_eq!(first, vec!["apple", "mango", "zebra"]);
    }

    #[test]
    fn stemming_collapses_variants() {
        // Without stemming "running"/"runs" are distinct; with stemming they
        // merge into one term whose tf is the sum.
        let unstemmed = keywords(&[], "running runs running", 5, false);
        assert!(unstemmed.contains(&"running".to_string()));
        assert!(unstemmed.contains(&"runs".to_string()));

        let stemmed = keywords(&[], "running runs running", 1, true);
        assert_eq!(stemmed, vec![stem("running")]);
    }

    #[test]
    fn duplicate_keywords_are_collapsed() {
        // A term repeated in the text appears once in the output.
        let kws = keywords(&[], "graph graph graph", 5, false);
        assert_eq!(kws, vec!["graph"]);
    }
}
