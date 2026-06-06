//! Trigram-based full-text search index.
//!
//! Two layers:
//!
//! - [`tokenizer`] — pure normalization + trigram extraction. Lowercases,
//!   strips punctuation, and emits 3-character sliding windows. CJK
//!   characters additionally emit 2-character bigrams between consecutive
//!   CJK glyphs.
//! - [`index`] — `trigram → Vec<node_id>` inverted index backed by the
//!   [`crate::storage::StorageBackend`]. Each posting is stored as
//!   `fts:{trigram}:{node_id_le} -> empty bytes` so that
//!   [`crate::storage::StorageBackend::scan_prefix`] can retrieve a full
//!   posting list in one call.
//!
//! See [`audit/AUDIT-fts.md`](https://github.com/ice1x/drevo/blob/main/audit/AUDIT-fts.md)
//! for the rules verified against the `drevo-database` and
//! `drevo-tdd` skills and for the recorded refactor follow-ups (BM25
//! strategy trait, broad-query performance mitigations, NFC
//! normalization).

/// Trigram inverted-index storage operations
/// (build / extend / remove / intersect posting lists).
pub mod index;
/// Pure tokenizer: `normalize` → `trigrams` → `extract_trigrams(title,
/// body)`.
pub mod tokenizer;

pub use tokenizer::{extract_raw_trigrams, extract_trigrams, normalize, raw_trigrams, trigrams};
