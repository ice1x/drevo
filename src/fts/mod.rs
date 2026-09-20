//! Trigram-based full-text search index.
//!
//! Two layers:
//!
//! - [`tokenizer`] — pure normalization + trigram extraction. Lowercases,
//!   strips punctuation, and emits 3-character sliding windows. CJK
//!   characters additionally emit 2-character bigrams between consecutive
//!   CJK glyphs.
//! - keyword extraction (`keywords`) and faceting ([`facet`]) — the salience
//!   and grouping layers built on the tokenizer. The trigram inverted index
//!   itself now lives in the native engine (`drevo_core::native_fts`); the KV
//!   `fts:` keyspace index was removed with the KV engine (epic #444).
//!
//! See [`audit/AUDIT-fts.md`](https://github.com/ice1x/drevo/blob/main/audit/AUDIT-fts.md)
//! for the rules verified against the `drevo-database` and
//! `drevo-tdd` skills and for the recorded refactor follow-ups (BM25
//! strategy trait, broad-query performance mitigations, NFC
//! normalization).

/// Keyword-similarity grouping & faceting: collapse near-duplicate
/// keywords (lexical or semantic) into facets (task `00133`).
pub mod facet;
/// Keyword extraction: top-`k` salient terms via word tokenization,
/// stopword removal, and BM25 IDF salience (task `00132`).
pub(crate) mod keywords;
/// Porter stemmer (1980) — pure-Rust, used optionally by keyword extraction.
pub(crate) mod stemmer;
/// English stopword list for keyword extraction.
pub(crate) mod stopwords;
/// Pure tokenizer: `normalize` → `trigrams` → `extract_trigrams(title,
/// body)` plus the word-level `words` tokenizer for keyword extraction.
/// Extracted to the [`drevo-core`](drevo_core) crate (Phase 7 slice 2) and
/// re-exported so `crate::fts::tokenizer::…` / `drevo::fts::tokenizer::…` paths
/// keep resolving.
pub use drevo_core::tokenizer;

pub use facet::{Facet, FacetCollapse};
pub use tokenizer::{
    extract_raw_trigrams, extract_raw_trigrams_fields, extract_trigrams, extract_trigrams_fields,
    normalize, raw_trigrams, trigrams, words,
};
