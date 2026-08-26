//! Okapi BM25 scoring primitives shared by the KV and native full-text indexes.
//!
//! Only the pure math lives here — no posting lists, no storage — so both the KV
//! trigram index (and the keyword-extraction ranker built on it) and the native
//! `NativeFtsIndex` compute an identical inverse-document-frequency weight.

/// Okapi BM25 inverse document frequency for a term.
///
/// `n` is the total number of documents in the corpus and `df` is the number of
/// documents containing the term. Uses the Robertson–Spärck-Jones form with
/// `+0.5` smoothing, wrapped in `ln(1 + x)` (the Lucene/Elasticsearch default) so
/// the result is always non-negative even when a term appears in more than half
/// the corpus:
///
/// ```text
/// idf(df) = ln(1 + (N − df + 0.5) / (df + 0.5))
/// ```
///
/// This is the exact salience weight the keyword-extraction task (`00132`)
/// reuses to pick candidate terms, which is why it lives here as a shared helper
/// rather than inline in the ranker.
pub fn bm25_idf(n: u64, df: u64) -> f32 {
    let n = n as f32;
    let df = df as f32;
    ((n - df + 0.5) / (df + 0.5)).ln_1p()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idf_is_non_negative_even_for_common_terms() {
        // A term in every document (df == n) still yields a non-negative weight
        // thanks to the `ln(1 + x)` wrapping.
        assert!(bm25_idf(100, 100) >= 0.0);
        assert!(bm25_idf(100, 99) >= 0.0);
    }

    #[test]
    fn rarer_terms_score_higher() {
        // Fewer documents containing the term → higher IDF.
        assert!(bm25_idf(1000, 1) > bm25_idf(1000, 500));
    }

    #[test]
    fn matches_the_closed_form() {
        let (n, df) = (10u64, 3u64);
        let expected = ((n as f32 - df as f32 + 0.5) / (df as f32 + 0.5)).ln_1p();
        assert_eq!(bm25_idf(n, df), expected);
    }
}
