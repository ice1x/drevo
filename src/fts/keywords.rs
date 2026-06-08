//! Keyword extraction (task `00132`).
//!
//! Picks the top-`k` most *salient* terms from a piece of text by combining
//! three signals already shipped in the FTS layer:
//!
//! 1. **Word tokenization** ([`crate::fts::tokenizer::words`]) — lowercase
//!    word tokens, distinct from the character-trigram index tokenizer.
//! 2. **Stopword removal** ([`crate::fts::stopwords`]) — drop English
//!    function words so grammatical glue never ranks.
//! 3. **BM25 IDF salience** ([`crate::fts::index::bm25_idf`], task `00131`) —
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
use crate::fts::index::{bm25_idf, corpus_stats, intersect_trigrams};
use crate::fts::stemmer::stem;
use crate::fts::stopwords::is_stopword;
use crate::fts::tokenizer::{trigrams, words};
use crate::storage::StorageBackend;

/// Extract the top-`k` salient keywords from `text`.
///
/// * `k` — maximum number of keywords to return; `0` yields an empty list.
/// * `stem_terms` — when `true`, collapse morphological variants onto their
///   Porter stem before counting and ranking (so "running"/"runs" merge).
///
/// Returns an empty list (never an error) when `text` has no rankable terms,
/// mirroring the `similar(...)` precedent (`00077`): a missing or
/// content-free property simply yields no keywords. Genuine storage failures
/// while reading corpus statistics propagate as errors.
pub(crate) fn extract_keywords(
    backend: &dyn StorageBackend,
    text: &str,
    k: usize,
    stem_terms: bool,
) -> Result<Vec<String>> {
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

    // 2. Corpus-wide N (number of indexed documents) for the IDF weight.
    let stats = corpus_stats(backend)?;
    let n = stats.doc_count;

    // 3. Score each distinct term by tf · idf.
    let mut scored: Vec<(String, f32)> = Vec::with_capacity(tf.len());
    for (term, freq) in tf {
        let term_trigrams = trigrams(&term);
        // Estimate document frequency from the trigram index: documents that
        // contain *all* of the term's trigrams. For a term too short to have
        // any trigram (e.g. a 2-char token) we cannot estimate df, so we
        // assign it the minimal IDF (df = N) rather than the maximal one,
        // keeping such low-signal tokens from floating to the top.
        let df = if term_trigrams.is_empty() {
            n
        } else {
            intersect_trigrams(backend, &term_trigrams)?.len() as u64
        };
        // df can never exceed N for a consistent index, but clamp defensively
        // so bm25_idf stays non-negative even against a legacy/over-counted
        // index.
        let idf = bm25_idf(n, df.min(n));
        scored.push((term, freq as f32 * idf));
    }

    // 4. Rank: score descending, then term ascending for deterministic ties.
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.truncate(k);
    Ok(scored.into_iter().map(|(term, _)| term).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fts::index::index_node;
    use crate::storage::MemoryBackend;

    /// Index `count` filler documents whose bodies contain the trigrams of
    /// `common` so that term's document frequency (and thus low salience) is
    /// established, plus one document containing `rare`.
    fn corpus_with(common: &str, common_docs: usize, rare: &str) -> MemoryBackend {
        let backend = MemoryBackend::new();
        let mut id = 1u64;
        for _ in 0..common_docs {
            index_node(&backend, id, &format!("doc{id}"), common).unwrap();
            id += 1;
        }
        index_node(&backend, id, "rare-doc", rare).unwrap();
        backend
    }

    #[test]
    fn k_zero_returns_empty() {
        let backend = MemoryBackend::new();
        assert!(extract_keywords(&backend, "graph database", 0, false)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn empty_and_stopword_only_text_returns_empty() {
        let backend = MemoryBackend::new();
        assert!(extract_keywords(&backend, "", 5, false).unwrap().is_empty());
        assert!(extract_keywords(&backend, "the and of to is", 5, false)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn stopwords_are_dropped() {
        let backend = MemoryBackend::new();
        let kws = extract_keywords(&backend, "the graph and the database", 5, false).unwrap();
        assert!(kws.contains(&"graph".to_string()));
        assert!(kws.contains(&"database".to_string()));
        assert!(!kws.iter().any(|w| w == "the" || w == "and"));
    }

    #[test]
    fn rarer_term_outranks_common_term() {
        // "database" appears in many docs (low IDF); "photosynthesis" in one
        // (high IDF). Both occur once in the query text, so IDF decides.
        let backend = corpus_with("database systems", 8, "photosynthesis chloroplast");
        let kws = extract_keywords(&backend, "database photosynthesis", 2, false).unwrap();
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
        let backend = MemoryBackend::new();
        let kws =
            extract_keywords(&backend, "anxiety anxiety anxiety journaling", 1, false).unwrap();
        assert_eq!(kws, vec!["anxiety"]);
    }

    #[test]
    fn respects_k_limit() {
        let backend = MemoryBackend::new();
        let kws = extract_keywords(&backend, "alpha beta gamma delta epsilon", 3, false).unwrap();
        assert_eq!(kws.len(), 3);
    }

    #[test]
    fn deterministic_tie_break_is_alphabetical() {
        // Empty corpus + all-distinct single-occurrence terms => equal score;
        // alphabetical order must decide, stably.
        let backend = MemoryBackend::new();
        let first = extract_keywords(&backend, "zebra apple mango", 3, false).unwrap();
        let second = extract_keywords(&backend, "mango zebra apple", 3, false).unwrap();
        assert_eq!(first, second);
        assert_eq!(first, vec!["apple", "mango", "zebra"]);
    }

    #[test]
    fn stemming_collapses_variants() {
        // Without stemming "running"/"runs" are distinct; with stemming they
        // merge into one term whose tf is the sum.
        let backend = MemoryBackend::new();
        let unstemmed = extract_keywords(&backend, "running runs running", 5, false).unwrap();
        assert!(unstemmed.contains(&"running".to_string()));
        assert!(unstemmed.contains(&"runs".to_string()));

        let stemmed = extract_keywords(&backend, "running runs running", 1, true).unwrap();
        assert_eq!(stemmed, vec![stem("running")]);
    }

    #[test]
    fn duplicate_keywords_are_collapsed() {
        // A term repeated in the text appears once in the output.
        let backend = MemoryBackend::new();
        let kws = extract_keywords(&backend, "graph graph graph", 5, false).unwrap();
        assert_eq!(kws, vec!["graph"]);
    }
}
