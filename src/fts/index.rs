//! FTS index: trigram -> posting list storage.
//!
//! Stores inverted index entries as KV pairs:
//! `fts:{trigram}:{node_id}` -> empty bytes.
//!
//! This allows efficient posting list retrieval via `scan_prefix("fts:{trigram}:")`,
//! and efficient add/remove of individual node entries without touching other nodes.

// Only the no-properties test wrappers below need an empty map.
#[cfg(test)]
use std::collections::HashMap;

use crate::error::Result;
use crate::fts::tokenizer::{extract_raw_trigrams_fields, extract_trigrams_fields};
use crate::model::Properties;
use crate::storage::StorageBackend;

/// Key prefix for FTS index entries: `fts:{trigram}:{node_id}` -> empty.
const PREFIX_FTS: &[u8] = b"fts:";

/// Key prefix for per-document FTS length entries: `ftslen:{node_id}` ->
/// `u32` little-endian trigram-token count.
///
/// This is the persisted statistic BM25 (task `00131`) needs that plain
/// TF-IDF did not: the document length `|d|` (number of trigram tokens,
/// counting repetition) feeds the `b` length-normalization term, and the
/// running average document length `avgdl` is derived by scanning this
/// prefix (see [`corpus_stats`]). The prefix is deliberately distinct from
/// [`PREFIX_FTS`] (`fts:` vs `ftslen:`) so neither's `scan_prefix` catches
/// the other.
const PREFIX_FTS_LEN: &[u8] = b"ftslen:";

/// Build a single FTS index key: `fts:{trigram}:{node_id}`.
fn fts_key(trigram: &str, node_id: u64) -> Vec<u8> {
    let mut key = PREFIX_FTS.to_vec();
    key.extend_from_slice(trigram.as_bytes());
    key.push(b':');
    key.extend_from_slice(&node_id.to_le_bytes());
    key
}

/// Build the scan prefix for a trigram: `fts:{trigram}:`.
fn fts_trigram_prefix(trigram: &str) -> Vec<u8> {
    let mut key = PREFIX_FTS.to_vec();
    key.extend_from_slice(trigram.as_bytes());
    key.push(b':');
    key
}

/// Build a per-document length key: `ftslen:{node_id_le}`.
fn fts_len_key(node_id: u64) -> Vec<u8> {
    let mut key = PREFIX_FTS_LEN.to_vec();
    key.extend_from_slice(&node_id.to_le_bytes());
    key
}

// Note: We cannot efficiently scan all trigrams for a specific node
// because the node_id is at the end of the key. Instead, we re-extract
// trigrams from the text and delete entries one by one.

/// Collect every FTS-indexable string from a property map (#227): each
/// `String` value, and each string element of an array value, in property-key
/// order so the index is deterministic regardless of `HashMap` iteration.
/// Non-string scalars, nested objects, and non-string array elements are
/// skipped.
pub(crate) fn collect_property_text(properties: &Properties) -> Vec<String> {
    let mut keys: Vec<&String> = properties.keys().collect();
    keys.sort();
    let mut out = Vec::new();
    for key in keys {
        match properties.get(key) {
            Some(serde_json::Value::String(s)) => out.push(s.clone()),
            Some(serde_json::Value::Array(items)) => {
                for item in items {
                    if let serde_json::Value::String(s) = item {
                        out.push(s.clone());
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The ordered list of text fields indexed for a node: `title`, `body`, then
/// each string drawn from its properties.
fn node_fields<'a>(title: &'a str, body: &'a str, prop_text: &'a [String]) -> Vec<&'a str> {
    let mut fields: Vec<&str> = Vec::with_capacity(2 + prop_text.len());
    fields.push(title);
    fields.push(body);
    fields.extend(prop_text.iter().map(String::as_str));
    fields
}

/// The raw (bag) trigrams a node contributes to the FTS index — `title` +
/// `body` + string properties (#227). The BM25/TF-IDF ranker calls this so
/// query-time term frequency and document length are computed over the **same**
/// text [`index_node_with_props`] indexed (otherwise a node whose text lives
/// only in properties would be a candidate but score 0).
pub(crate) fn node_raw_trigrams(title: &str, body: &str, properties: &Properties) -> Vec<String> {
    let prop_text = collect_property_text(properties);
    let fields = node_fields(title, body, &prop_text);
    extract_raw_trigrams_fields(&fields)
}

/// The distinct (deduplicated) trigram set a node contributes — the same-fields
/// companion of [`node_raw_trigrams`], used by the TF-IDF ranker.
pub(crate) fn node_trigrams(title: &str, body: &str, properties: &Properties) -> Vec<String> {
    let prop_text = collect_property_text(properties);
    let fields = node_fields(title, body, &prop_text);
    extract_trigrams_fields(&fields)
}

/// Build the FTS index entries for a node from its `title` + `body` only.
/// A no-properties convenience over [`node_index_entries_with_props`]; only the
/// tests use it (production always has a node's properties to hand).
#[cfg(test)]
pub(crate) fn node_index_entries(node_id: u64, title: &str, body: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
    node_index_entries_with_props(node_id, title, body, &Properties(HashMap::new()))
}

/// Build the FTS index entries for a node from `title` + `body` + every string
/// property value (#227).
pub(crate) fn node_index_entries_with_props(
    node_id: u64,
    title: &str,
    body: &str,
    properties: &Properties,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let prop_text = collect_property_text(properties);
    let fields = node_fields(title, body, &prop_text);
    let trigrams = extract_trigrams_fields(&fields);
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(trigrams.len() + 1);
    for trigram in &trigrams {
        entries.push((fts_key(trigram, node_id), Vec::new()));
    }

    let doc_len = extract_raw_trigrams_fields(&fields).len();
    if doc_len > 0 {
        entries.push((
            fts_len_key(node_id),
            (doc_len as u32).to_le_bytes().to_vec(),
        ));
    }
    entries
}

/// Index a node by its `title` + `body`. A no-properties convenience used only
/// by the tests (production always has the node's properties).
#[cfg(test)]
pub(crate) fn index_node(
    backend: &dyn StorageBackend,
    node_id: u64,
    title: &str,
    body: &str,
) -> Result<()> {
    index_node_with_props(backend, node_id, title, body, &Properties(HashMap::new()))
}

/// Index a node by its `title` + `body` + string properties (#227).
pub(crate) fn index_node_with_props(
    backend: &dyn StorageBackend,
    node_id: u64,
    title: &str,
    body: &str,
    properties: &Properties,
) -> Result<()> {
    for (key, value) in node_index_entries_with_props(node_id, title, body, properties) {
        backend.put(&key, &value)?;
    }
    Ok(())
}

/// Remove FTS index entries for a node.
///
/// Re-extracts trigrams from the title and body, deletes the corresponding
/// postings, and removes the persisted document-length entry (cascade-on-
/// delete, mirroring the posting cleanup). `delete` of an absent key is a
/// no-op, so this is safe even when the node had no indexable text.
#[cfg(test)]
pub(crate) fn deindex_node(
    backend: &dyn StorageBackend,
    node_id: u64,
    title: &str,
    body: &str,
) -> Result<()> {
    deindex_node_with_props(backend, node_id, title, body, &Properties(HashMap::new()))
}

/// Remove a node's FTS entries, re-deriving trigrams from `title` + `body` +
/// string properties (#227) so the same postings written by
/// [`index_node_with_props`] are cleaned up.
pub(crate) fn deindex_node_with_props(
    backend: &dyn StorageBackend,
    node_id: u64,
    title: &str,
    body: &str,
    properties: &Properties,
) -> Result<()> {
    let prop_text = collect_property_text(properties);
    let fields = node_fields(title, body, &prop_text);
    let trigrams = extract_trigrams_fields(&fields);
    for trigram in &trigrams {
        backend.delete(&fts_key(trigram, node_id))?;
    }

    backend.delete(&fts_len_key(node_id))?;
    Ok(())
}

/// Corpus-level FTS statistics used by BM25 ranking.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CorpusStats {
    /// Number of indexed documents (nodes with at least one trigram token).
    /// This is the BM25 `N`.
    pub doc_count: u64,
    /// Sum of every document's length `|d|`. Divided by `doc_count` it
    /// yields the average document length `avgdl`.
    pub total_len: u64,
}

impl CorpusStats {
    /// Average document length `avgdl`, or `0.0` when the corpus is empty.
    pub fn avgdl(&self) -> f32 {
        if self.doc_count == 0 {
            0.0
        } else {
            self.total_len as f32 / self.doc_count as f32
        }
    }
}

/// Read a single document's length `|d|` (raw trigram-token count).
///
/// Returns `None` if the node has no persisted length (e.g. it was never
/// indexed or had no indexable text).
pub(crate) fn doc_length(backend: &dyn StorageBackend, node_id: u64) -> Result<Option<u32>> {
    let raw = backend.get(&fts_len_key(node_id))?;
    Ok(raw.and_then(|bytes| {
        let arr: [u8; 4] = bytes.as_slice().try_into().ok()?;
        Some(u32::from_le_bytes(arr))
    }))
}

/// Compute corpus statistics by scanning the persisted per-document
/// lengths under `ftslen:`.
///
/// This derives the BM25 `N` (number of indexed documents) and the running
/// average document length from the lengths maintained on every
/// insert/update/delete — a drift-free alternative to a cached counter,
/// at the same `O(N)` cost the previous TF-IDF path already paid to count
/// nodes.
pub(crate) fn corpus_stats(backend: &dyn StorageBackend) -> Result<CorpusStats> {
    let entries = backend.scan_prefix(PREFIX_FTS_LEN)?;
    let mut doc_count: u64 = 0;
    let mut total_len: u64 = 0;
    for (_key, value) in &entries {
        if let Ok(arr) = value.as_slice().try_into() {
            total_len += u32::from_le_bytes(arr) as u64;
            doc_count += 1;
        }
    }
    Ok(CorpusStats {
        doc_count,
        total_len,
    })
}

/// Okapi BM25 inverse document frequency for a term.
///
/// `n` is the total number of documents in the corpus and `df` is the
/// number of documents containing the term. Uses the Robertson–Spärck-
/// Jones form with `+0.5` smoothing, wrapped in `ln(1 + x)` (the
/// Lucene/Elasticsearch default) so the result is always non-negative even
/// when a term appears in more than half the corpus:
///
/// ```text
/// idf(df) = ln(1 + (N − df + 0.5) / (df + 0.5))
/// ```
///
/// This is the exact salience weight the keyword-extraction task (`00132`)
/// reuses to pick candidate terms, which is why it lives here as a shared
/// helper rather than inline in the ranker.
pub(crate) fn bm25_idf(n: u64, df: u64) -> f32 {
    let n = n as f32;
    let df = df as f32;
    ((n - df + 0.5) / (df + 0.5)).ln_1p()
}

/// Retrieve all node IDs from the posting list of a single trigram.
///
/// Scans `fts:{trigram}:` prefix and extracts node IDs from the keys.
pub(crate) fn node_ids_for_trigram(
    backend: &dyn StorageBackend,
    trigram: &str,
) -> Result<Vec<u64>> {
    let prefix = fts_trigram_prefix(trigram);
    let entries = backend.scan_prefix(&prefix)?;

    let mut ids = Vec::with_capacity(entries.len());
    for (key, _) in entries {
        if key.len() < prefix.len() {
            continue;
        }
        let suffix = &key[prefix.len()..];
        // Refactored in `00106` to remove `.unwrap()` from library code
        // (`drevo-rust` §"Error Handling"). `copy_from_slice` into a
        // pre-allocated array is panic-free by construction.
        let mut arr = [0u8; 8];
        if suffix.len() == 8 {
            arr.copy_from_slice(suffix);
            ids.push(u64::from_le_bytes(arr));
        }
    }
    Ok(ids)
}

/// Count how many nodes contain a given trigram (document frequency).
pub(crate) fn posting_list_len(backend: &dyn StorageBackend, trigram: &str) -> Result<usize> {
    let prefix = fts_trigram_prefix(trigram);
    let entries = backend.scan_prefix(&prefix)?;
    Ok(entries.len())
}

/// Intersect posting lists for multiple trigrams.
///
/// Returns node IDs that appear in ALL posting lists.
/// Returns empty if trigrams is empty.
pub(crate) fn intersect_trigrams(
    backend: &dyn StorageBackend,
    trigrams: &[String],
) -> Result<Vec<u64>> {
    if trigrams.is_empty() {
        return Ok(Vec::new());
    }

    // Start with the first trigram's posting list
    let mut result: Vec<u64> = node_ids_for_trigram(backend, &trigrams[0])?;

    // Intersect with each subsequent trigram
    for trigram in &trigrams[1..] {
        if result.is_empty() {
            break;
        }
        let ids = node_ids_for_trigram(backend, trigram)?;
        result.retain(|id| ids.contains(id));
    }

    result.sort_unstable();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryBackend;

    fn backend() -> MemoryBackend {
        MemoryBackend::new()
    }

    #[test]
    fn fts_key_format() {
        let key = fts_key("hel", 42);
        assert!(key.starts_with(b"fts:hel:"));
        let suffix = &key[8..];
        assert_eq!(suffix.len(), 8);
        assert_eq!(u64::from_le_bytes(suffix.try_into().unwrap()), 42);
    }

    #[test]
    fn index_and_retrieve() {
        let b = backend();
        index_node(&b, 1, "Hello World", "").unwrap();

        let ids = node_ids_for_trigram(&b, "hel").unwrap();
        assert_eq!(ids, vec![1]);
    }

    // ── #227: index configurable string properties, not just title/body ──

    fn props(pairs: &[(&str, serde_json::Value)]) -> Properties {
        Properties(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    #[test]
    fn collect_property_text_gathers_strings_and_array_elements() {
        let p = props(&[
            ("name", serde_json::json!("Zebra")),
            ("tags", serde_json::json!(["alpha", "beta"])),
            ("count", serde_json::json!(7)), // non-string scalar → skipped
            ("meta", serde_json::json!({"k": "v"})), // nested object → skipped
        ]);
        let mut text = collect_property_text(&p);
        text.sort();
        assert_eq!(text, vec!["Zebra", "alpha", "beta"]);
    }

    #[test]
    fn index_with_props_makes_a_name_property_searchable() {
        // The exact #227 scenario: text under `name` (not title/body) is indexed.
        let b = backend();
        index_node_with_props(
            &b,
            7,
            "",
            "",
            &props(&[("name", serde_json::json!("Zebra crossing"))]),
        )
        .unwrap();
        assert_eq!(node_ids_for_trigram(&b, "zeb").unwrap(), vec![7]);
    }

    #[test]
    fn index_with_props_indexes_array_string_elements() {
        let b = backend();
        index_node_with_props(
            &b,
            3,
            "",
            "",
            &props(&[("obs", serde_json::json!(["wolverine"]))]),
        )
        .unwrap();
        assert_eq!(node_ids_for_trigram(&b, "wol").unwrap(), vec![3]);
    }

    #[test]
    fn deindex_with_props_removes_property_trigrams() {
        let b = backend();
        let p = props(&[("name", serde_json::json!("Zebra crossing"))]);
        index_node_with_props(&b, 7, "", "", &p).unwrap();
        deindex_node_with_props(&b, 7, "", "", &p).unwrap();
        assert!(node_ids_for_trigram(&b, "zeb").unwrap().is_empty());
    }

    #[test]
    fn non_string_properties_are_not_indexed() {
        let b = backend();
        index_node_with_props(
            &b,
            9,
            "",
            "",
            &props(&[("count", serde_json::json!(12345))]),
        )
        .unwrap();
        assert!(node_ids_for_trigram(&b, "123").unwrap().is_empty());
    }

    #[test]
    fn title_body_wrapper_matches_no_props_call() {
        // Back-compat: the 2-arg wrappers equal the *_with_props path with empty
        // properties, so existing behaviour is unchanged.
        let a = node_index_entries(1, "Hello", "World");
        let b = node_index_entries_with_props(1, "Hello", "World", &Properties(HashMap::new()));
        assert_eq!(a, b);
    }

    #[test]
    fn index_multiple_nodes() {
        let b = backend();
        index_node(&b, 1, "Hello Alice", "").unwrap();
        index_node(&b, 2, "Hello Bob", "").unwrap();

        let ids = node_ids_for_trigram(&b, "hel").unwrap();
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
    }

    #[test]
    fn deindex_removes_entries() {
        let b = backend();
        index_node(&b, 1, "Hello World", "").unwrap();
        deindex_node(&b, 1, "Hello World", "").unwrap();

        let ids = node_ids_for_trigram(&b, "hel").unwrap();
        assert!(ids.is_empty());
    }

    #[test]
    fn deindex_leaves_other_nodes() {
        let b = backend();
        index_node(&b, 1, "Hello Alice", "").unwrap();
        index_node(&b, 2, "Hello Bob", "").unwrap();
        deindex_node(&b, 1, "Hello Alice", "").unwrap();

        let ids = node_ids_for_trigram(&b, "hel").unwrap();
        assert_eq!(ids, vec![2]);
    }

    #[test]
    fn intersect_basic() {
        let b = backend();
        index_node(&b, 1, "Rust programming", "").unwrap();
        index_node(&b, 2, "Rust language", "").unwrap();
        index_node(&b, 3, "Python programming", "").unwrap();

        let ids = intersect_trigrams(&b, &["rus".to_string(), "pro".to_string()]).unwrap();
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn intersect_empty_trigrams() {
        let b = backend();
        index_node(&b, 1, "Hello", "").unwrap();

        let ids = intersect_trigrams(&b, &[]).unwrap();
        assert!(ids.is_empty());
    }

    #[test]
    fn intersect_no_match() {
        let b = backend();
        index_node(&b, 1, "Hello", "").unwrap();

        let ids = intersect_trigrams(&b, &["zzz".to_string()]).unwrap();
        assert!(ids.is_empty());
    }

    #[test]
    fn index_body_text() {
        let b = backend();
        index_node(&b, 1, "Title", "body content here").unwrap();

        let ids = node_ids_for_trigram(&b, "bod").unwrap();
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn index_cjk_bigrams() {
        let b = backend();
        index_node(&b, 1, "你好世界", "").unwrap();

        let ids = node_ids_for_trigram(&b, "你好").unwrap();
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn index_short_text_no_crash() {
        let b = backend();
        index_node(&b, 1, "Hi", "").unwrap();

        // "hi" is too short for trigrams — no entries should exist
        // but it should not crash
        let prefix = b"fts:";
        let entries = b.scan_prefix(prefix).unwrap();
        assert!(entries.is_empty());
    }

    // --- document length & corpus stats (BM25, task 00131) ---

    #[test]
    fn index_node_records_doc_length() {
        let b = backend();
        index_node(&b, 1, "hello world", "").unwrap();
        let len = doc_length(&b, 1).unwrap();
        assert_eq!(len, Some(raw_doc_len("hello world")));
    }

    #[test]
    fn doc_length_counts_repetition() {
        let b = backend();
        // A body that repeats a token has a longer |d| than a unique one.
        index_node(&b, 1, "rust rust rust", "").unwrap();
        index_node(&b, 2, "rust", "").unwrap();
        let l1 = doc_length(&b, 1).unwrap().unwrap();
        let l2 = doc_length(&b, 2).unwrap().unwrap();
        assert!(l1 > l2, "repeated tokens lengthen the document");
    }

    #[test]
    fn short_text_records_no_length() {
        let b = backend();
        index_node(&b, 1, "hi", "").unwrap();
        assert_eq!(doc_length(&b, 1).unwrap(), None);
    }

    #[test]
    fn deindex_removes_doc_length() {
        let b = backend();
        index_node(&b, 1, "hello world", "").unwrap();
        deindex_node(&b, 1, "hello world", "").unwrap();
        assert_eq!(doc_length(&b, 1).unwrap(), None);
    }

    #[test]
    fn corpus_stats_aggregates_lengths() {
        let b = backend();
        index_node(&b, 1, "hello world", "").unwrap();
        index_node(&b, 2, "rust language", "").unwrap();
        let stats = corpus_stats(&b).unwrap();
        assert_eq!(stats.doc_count, 2);
        let expected = raw_doc_len("hello world") as u64 + raw_doc_len("rust language") as u64;
        assert_eq!(stats.total_len, expected);
        assert_eq!(stats.avgdl(), expected as f32 / 2.0);
    }

    #[test]
    fn corpus_stats_empty_corpus() {
        let b = backend();
        let stats = corpus_stats(&b).unwrap();
        assert_eq!(stats.doc_count, 0);
        assert_eq!(stats.total_len, 0);
        assert_eq!(stats.avgdl(), 0.0);
    }

    #[test]
    fn corpus_stats_excludes_short_docs() {
        let b = backend();
        index_node(&b, 1, "hello world", "").unwrap();
        index_node(&b, 2, "hi", "").unwrap(); // no trigrams
        let stats = corpus_stats(&b).unwrap();
        assert_eq!(stats.doc_count, 1, "too-short docs are not counted");
    }

    #[test]
    fn doc_length_key_does_not_collide_with_postings() {
        let b = backend();
        index_node(&b, 1, "hello world", "").unwrap();
        // The `fts:` posting scan must not pick up `ftslen:` length entries.
        let postings = b.scan_prefix(b"fts:").unwrap();
        for (key, _) in &postings {
            assert!(
                !key.starts_with(b"ftslen:"),
                "ftslen entry leaked into fts: posting scan"
            );
        }
    }

    #[test]
    fn bm25_idf_is_non_negative_and_monotonic() {
        // Rarer terms (lower df) score strictly higher; never negative.
        let rare = bm25_idf(100, 1);
        let common = bm25_idf(100, 90);
        assert!(rare > common);
        assert!(
            common >= 0.0,
            "idf must stay non-negative even for df > N/2"
        );
    }

    /// Helper: the raw trigram-token count of a single text field.
    fn raw_doc_len(text: &str) -> u32 {
        crate::fts::tokenizer::extract_raw_trigrams(text, "").len() as u32
    }

    #[test]
    fn reindex_node_updates_trigrams() {
        let b = backend();
        index_node(&b, 1, "Hello World", "").unwrap();
        deindex_node(&b, 1, "Hello World", "").unwrap();
        index_node(&b, 1, "Goodbye World", "").unwrap();

        let ids = node_ids_for_trigram(&b, "hel").unwrap();
        assert!(ids.is_empty(), "old trigrams removed");

        let ids = node_ids_for_trigram(&b, "goo").unwrap();
        assert_eq!(ids, vec![1], "new trigrams present");

        // Shared trigram "wor" should still be present
        let ids = node_ids_for_trigram(&b, "wor").unwrap();
        assert_eq!(ids, vec![1]);
    }
}
