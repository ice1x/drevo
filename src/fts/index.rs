//! FTS index: trigram -> posting list storage (#275, format 3).
//!
//! One KV row per trigram: `fts:{trigram}:` -> packed sorted posting list, plus
//! `ftslen:{node_id}` -> `u32` document length for BM25. Since format 3 (PR-2)
//! each node posting entry is `(node_id: u64, tf: u32)` — the per-document term
//! frequency stored inline — so BM25 scores a candidate straight from the index
//! with no per-candidate node read or re-tokenization. (Edge postings under
//! `edge_index` keep the id-only layout; the shared id-only codec below serves
//! them.)
//!
//! Retrieval is a single `get` per trigram (O(1)); indexing a node is a
//! read-modify-write of each of its trigrams' posting lists (the caller
//! serializes these via drevo's FTS write lock). This replaced the original
//! one-empty-row-per-`(trigram, node)` layout, whose millions of tiny rows on a
//! text-heavy graph inflated the physical file to several× its content (#275).

// Only the no-properties test wrappers below need an empty map.
#[cfg(test)]
use std::collections::HashMap;

use crate::error::Result;
use crate::fts::tokenizer::{extract_raw_trigrams_fields, extract_trigrams_fields};
use crate::model::Properties;
use crate::storage::StorageBackend;

/// Key prefix for FTS posting-list rows: `fts:{trigram}:` -> packed `[node_id]`.
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

/// Build the posting-list key for a trigram: `fts:{trigram}:` -> packed
/// `[node_id]`.
///
/// #275: the FTS index stores **one row per trigram** whose value is the packed
/// sorted posting list of node ids, rather than one empty row per
/// `(trigram, node_id)` pair. On a text-heavy graph that collapses millions of
/// tiny rows to ~one-per-distinct-trigram, cutting redb's per-row/page overhead
/// (the file-bloat driver measured in #275 slice 1). The trailing `:` keeps
/// keys unambiguous when one trigram's bytes are a prefix of another's.
fn fts_key(trigram: &str) -> Vec<u8> {
    let mut key = PREFIX_FTS.to_vec();
    key.extend_from_slice(trigram.as_bytes());
    key.push(b':');
    key
}

/// Encode a **sorted, de-duplicated** posting list as packed little-endian
/// `u64`s. The caller maintains the sorted-unique invariant (see
/// [`merge_posting`] / [`remove_posting`]); decoding is a plain chunked read.
pub(crate) fn encode_postings(ids: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ids.len() * 8);
    for id in ids {
        out.extend_from_slice(&id.to_le_bytes());
    }
    out
}

/// Decode a packed posting list. Trailing bytes that don't form a full `u64`
/// are ignored (defensive; the encoder never produces them).
pub(crate) fn decode_postings(bytes: &[u8]) -> Vec<u64> {
    bytes
        .chunks_exact(8)
        .map(|c| {
            let mut a = [0u8; 8];
            a.copy_from_slice(c);
            u64::from_le_bytes(a)
        })
        .collect()
}

/// Insert `id` into a sorted-unique posting list, preserving the invariant.
/// Returns `true` if it was added (absent before).
pub(crate) fn merge_posting(list: &mut Vec<u64>, id: u64) -> bool {
    match list.binary_search(&id) {
        Ok(_) => false,
        Err(pos) => {
            list.insert(pos, id);
            true
        }
    }
}

/// Remove `id` from a sorted-unique posting list. Returns `true` if removed.
pub(crate) fn remove_posting(list: &mut Vec<u64>, id: u64) -> bool {
    match list.binary_search(&id) {
        Ok(pos) => {
            list.remove(pos);
            true
        }
        Err(_) => false,
    }
}

// ── Node posting entries carry per-document term frequency (format 3, PR-2) ──
//
// A node trigram's posting list is `[(node_id: u64, tf: u32)]`, sorted by
// node_id, 12 bytes per entry. Storing `tf` (the raw occurrence count of the
// trigram in the document) lets BM25 score a candidate straight from the
// index — no per-candidate node read or re-tokenization. Edge postings keep
// the id-only layout above (see `edge_index`), so the shared id-only codec
// stays for them.

/// Bytes per node posting entry: `u64` id + `u32` term frequency.
const NODE_POSTING_ENTRY_LEN: usize = 12;

/// Encode a node posting list — `(id, tf)` entries sorted by id — as packed
/// little-endian `u64` id followed by `u32` tf.
pub(crate) fn encode_node_postings(entries: &[(u64, u32)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(entries.len() * NODE_POSTING_ENTRY_LEN);
    for (id, tf) in entries {
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&tf.to_le_bytes());
    }
    out
}

/// Decode a packed node posting list into `(id, tf)` entries. A trailing
/// partial entry (never produced by the encoder) is ignored.
pub(crate) fn decode_node_postings(bytes: &[u8]) -> Vec<(u64, u32)> {
    bytes
        .chunks_exact(NODE_POSTING_ENTRY_LEN)
        .map(|c| {
            let mut id = [0u8; 8];
            id.copy_from_slice(&c[0..8]);
            let mut tf = [0u8; 4];
            tf.copy_from_slice(&c[8..12]);
            (u64::from_le_bytes(id), u32::from_le_bytes(tf))
        })
        .collect()
}

/// Insert or update `(id, tf)` in a node posting list keyed + sorted by id,
/// preserving the invariant. An existing id's `tf` is replaced (the write path
/// deindexes a node before re-indexing, so a present id is the update case).
pub(crate) fn merge_node_posting(list: &mut Vec<(u64, u32)>, id: u64, tf: u32) {
    match list.binary_search_by_key(&id, |(i, _)| *i) {
        Ok(pos) => list[pos].1 = tf,
        Err(pos) => list.insert(pos, (id, tf)),
    }
}

/// Remove a node id from a node posting list. Returns `true` if removed.
pub(crate) fn remove_node_posting(list: &mut Vec<(u64, u32)>, id: u64) -> bool {
    match list.binary_search_by_key(&id, |(i, _)| *i) {
        Ok(pos) => {
            list.remove(pos);
            true
        }
        Err(_) => false,
    }
}

/// Per-trigram term frequencies of a node's text fields, plus the document
/// length `|d|` (total raw trigram-token count). `tf` is how many times a
/// trigram occurs across `title` + `body` + string properties.
fn trigram_frequencies(fields: &[&str]) -> (std::collections::BTreeMap<String, u32>, u32) {
    let raw = extract_raw_trigrams_fields(fields);
    let doc_len = raw.len() as u32;
    let mut tf: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    for t in raw {
        *tf.entry(t).or_insert(0) += 1;
    }
    (tf, doc_len)
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

/// The distinct (deduplicated) trigram set a node contributes — the same-fields
/// companion of [`node_raw_trigrams`], used by the TF-IDF ranker.
pub(crate) fn node_trigrams(title: &str, body: &str, properties: &Properties) -> Vec<String> {
    let prop_text = collect_property_text(properties);
    let fields = node_fields(title, body, &prop_text);
    extract_trigrams_fields(&fields)
}

/// Index a node by its `title` + `body`. A no-properties convenience used only
/// by the tests (production always has the node's properties, and indexes via
/// [`index_nodes_grouped`]).
#[cfg(test)]
pub(crate) fn index_node(
    backend: &dyn StorageBackend,
    node_id: u64,
    title: &str,
    body: &str,
) -> Result<()> {
    index_nodes_grouped(
        backend,
        &[(node_id, title, body, &Properties(HashMap::new()))],
    )
}

/// Test convenience: index one node with properties.
#[cfg(test)]
pub(crate) fn index_node_with_props(
    backend: &dyn StorageBackend,
    node_id: u64,
    title: &str,
    body: &str,
    properties: &Properties,
) -> Result<()> {
    index_nodes_grouped(backend, &[(node_id, title, body, properties)])
}

/// Index one or many nodes into the posting-list FTS store (#275), with each
/// trigram's posting list read-modified-written **once** across the whole batch
/// (rather than once per node), so a bulk create / import stays fast.
///
/// For each distinct trigram of a node's `title` + `body` + string properties,
/// adds `node_id` to that trigram's posting list, and writes the document length
/// under `ftslen:`.
///
/// **The caller MUST serialize FTS writes** (drevo does this via its FTS write
/// lock): each posting list is a `get` → merge → `put`, so two concurrent
/// indexers of the same trigram would otherwise lose one another's node id.
pub(crate) fn index_nodes_grouped(
    backend: &dyn StorageBackend,
    docs: &[(u64, &str, &str, &Properties)],
) -> Result<()> {
    use std::collections::BTreeMap;

    // Group (node_id, tf) by trigram across the batch; collect doc-length writes.
    let mut by_trigram: BTreeMap<String, Vec<(u64, u32)>> = BTreeMap::new();
    let mut len_writes: Vec<(u64, u32)> = Vec::with_capacity(docs.len());
    for (node_id, title, body, properties) in docs {
        let prop_text = collect_property_text(properties);
        let fields = node_fields(title, body, &prop_text);
        let (tfs, doc_len) = trigram_frequencies(&fields);
        for (trigram, tf) in tfs {
            by_trigram.entry(trigram).or_default().push((*node_id, tf));
        }
        if doc_len > 0 {
            len_writes.push((*node_id, doc_len));
        }
    }

    // Read-modify each distinct trigram's list, then commit EVERY updated
    // posting list + length in ONE put_batch (#275 follow-up). The reads are
    // cheap; issuing an individual `put` per trigram meant one redb
    // commit/fsync each — thousands on a bulk import — which regressed
    // import/shrink ~11x vs the old batched path. The caller holds the FTS
    // write lock, so no concurrent indexer changes a list between the read here
    // and the batched write below.
    let mut writes: Vec<(Vec<u8>, Vec<u8>)> =
        Vec::with_capacity(by_trigram.len() + len_writes.len());
    for (trigram, new_entries) in by_trigram {
        let key = fts_key(&trigram);
        let mut list = match backend.get(&key)? {
            Some(bytes) => decode_node_postings(&bytes),
            None => Vec::new(),
        };
        // Each (node_id, tf) within the batch is distinct per trigram (tf is the
        // node's total for that trigram), so merge inserts/updates in id order.
        for (id, tf) in new_entries {
            merge_node_posting(&mut list, id, tf);
        }
        writes.push((key, encode_node_postings(&list)));
    }
    for (node_id, doc_len) in len_writes {
        writes.push((fts_len_key(node_id), doc_len.to_le_bytes().to_vec()));
    }
    backend.put_batch(&writes)?;
    Ok(())
}

/// Build the **complete** posting-list FTS index for a set of documents as a
/// batch of rows, from scratch — assumes there are no existing `fts:` rows to
/// merge with (the reindex clears them first). One row per distinct trigram
/// (packed sorted posting list) plus one `ftslen:` per document, so the whole
/// index can be written in a single `put_batch` (one commit) instead of a
/// read-modify-write per trigram. Used by the #275 reindex-on-open.
pub(crate) fn build_full_index_batch(
    docs: &[(u64, &str, &str, &Properties)],
) -> Vec<(Vec<u8>, Vec<u8>)> {
    use std::collections::BTreeMap;

    let mut by_trigram: BTreeMap<String, Vec<(u64, u32)>> = BTreeMap::new();
    let mut out: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for (node_id, title, body, properties) in docs {
        let prop_text = collect_property_text(properties);
        let fields = node_fields(title, body, &prop_text);
        let (tfs, doc_len) = trigram_frequencies(&fields);
        for (trigram, tf) in tfs {
            by_trigram.entry(trigram).or_default().push((*node_id, tf));
        }
        if doc_len > 0 {
            out.push((fts_len_key(*node_id), doc_len.to_le_bytes().to_vec()));
        }
    }
    for (trigram, mut entries) in by_trigram {
        entries.sort_by_key(|(id, _)| *id);
        out.push((fts_key(&trigram), encode_node_postings(&entries)));
    }
    out
}

/// Remove FTS index entries for a node. Test convenience (title + body only).
#[cfg(test)]
pub(crate) fn deindex_node(
    backend: &dyn StorageBackend,
    node_id: u64,
    title: &str,
    body: &str,
) -> Result<()> {
    deindex_node_with_props(backend, node_id, title, body, &Properties(HashMap::new()))
}

/// Remove a node's `node_id` from every trigram posting list it contributed to
/// (re-deriving trigrams from `title` + `body` + string properties, #227), and
/// delete its `ftslen:` entry. A trigram whose list becomes empty has its row
/// deleted, so a fully-removed corpus leaves no `fts:` rows.
///
/// Same serialization requirement as [`index_nodes_grouped`].
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
        let key = fts_key(trigram);
        if let Some(bytes) = backend.get(&key)? {
            let mut list = decode_node_postings(&bytes);
            if remove_node_posting(&mut list, node_id) {
                if list.is_empty() {
                    backend.delete(&key)?;
                } else {
                    backend.put(&key, &encode_node_postings(&list))?;
                }
            }
        }
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

// The pure BM25 IDF helper was extracted to the `drevo-core` crate (Phase 7
// slice 7) so the native full-text index (also moving to core) shares the exact
// same scoring. Re-exported here so `crate::fts::index::bm25_idf` keeps resolving
// for this ranker, the keyword extractor, and `db.rs`.
pub(crate) use drevo_core::bm25::bm25_idf;

/// Retrieve all node IDs from the posting list of a single trigram (#275).
///
/// A single `get` of `fts:{trigram}:` (whose value is the packed sorted posting
/// list), decoded — O(1) lookup instead of the former prefix scan. The returned
/// ids are sorted ascending (the write path maintains that invariant).
pub(crate) fn node_ids_for_trigram(
    backend: &dyn StorageBackend,
    trigram: &str,
) -> Result<Vec<u64>> {
    Ok(match backend.get(&fts_key(trigram))? {
        Some(bytes) => decode_node_postings(&bytes)
            .into_iter()
            .map(|(id, _tf)| id)
            .collect(),
        None => Vec::new(),
    })
}

/// Retrieve the `(node_id, tf)` posting entries for a trigram (format 3). The
/// BM25 ranker consumes these directly, so a candidate is scored from the index
/// alone — no per-candidate node read or re-tokenization.
pub(crate) fn postings_for_trigram(
    backend: &dyn StorageBackend,
    trigram: &str,
) -> Result<Vec<(u64, u32)>> {
    Ok(match backend.get(&fts_key(trigram))? {
        Some(bytes) => decode_node_postings(&bytes),
        None => Vec::new(),
    })
}

/// Count how many nodes contain a given trigram (document frequency): the
/// number of entries in its posting list.
pub(crate) fn posting_list_len(backend: &dyn StorageBackend, trigram: &str) -> Result<usize> {
    Ok(match backend.get(&fts_key(trigram))? {
        Some(bytes) => bytes.len() / NODE_POSTING_ENTRY_LEN,
        None => 0,
    })
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn backend() -> MemoryBackend {
        MemoryBackend::new()
    }

    /// A backend that counts individual `put` vs `put_batch` calls, delegating
    /// to an inner `MemoryBackend`. Guards the #275 write-batching fix: bulk FTS
    /// indexing must commit its posting lists in ONE `put_batch`, not a `put`
    /// (= a redb commit/fsync) per trigram, which regressed import ~11x.
    struct CountingBackend {
        inner: MemoryBackend,
        puts: AtomicUsize,
        put_batches: AtomicUsize,
    }
    impl CountingBackend {
        fn new() -> Self {
            Self {
                inner: MemoryBackend::new(),
                puts: AtomicUsize::new(0),
                put_batches: AtomicUsize::new(0),
            }
        }
    }
    impl StorageBackend for CountingBackend {
        fn get(&self, key: &[u8]) -> crate::storage::Result<Option<Vec<u8>>> {
            self.inner.get(key)
        }
        fn put(&self, key: &[u8], value: &[u8]) -> crate::storage::Result<()> {
            self.puts.fetch_add(1, Ordering::Relaxed);
            self.inner.put(key, value)
        }
        fn put_batch(&self, items: &[(Vec<u8>, Vec<u8>)]) -> crate::storage::Result<()> {
            self.put_batches.fetch_add(1, Ordering::Relaxed);
            self.inner.put_batch(items)
        }
        fn delete(&self, key: &[u8]) -> crate::storage::Result<()> {
            self.inner.delete(key)
        }
        fn scan_prefix(&self, prefix: &[u8]) -> crate::storage::Result<Vec<(Vec<u8>, Vec<u8>)>> {
            self.inner.scan_prefix(prefix)
        }
        fn flush(&self) -> crate::storage::Result<()> {
            self.inner.flush()
        }
    }

    #[test]
    fn index_nodes_grouped_commits_in_one_put_batch() {
        let b = CountingBackend::new();
        let props = Properties(HashMap::new());
        let docs: Vec<(u64, &str, &str, &Properties)> = (0..50)
            .map(|i| (i, "t", "shared alpha bravo charlie delta echo", &props))
            .collect();
        index_nodes_grouped(&b, &docs).unwrap();
        // No per-trigram puts; everything in exactly one batched commit.
        assert_eq!(b.puts.load(Ordering::Relaxed), 0, "no individual puts");
        assert_eq!(
            b.put_batches.load(Ordering::Relaxed),
            1,
            "posting lists must commit in a single put_batch"
        );
        // Sanity: the data actually landed.
        assert_eq!(node_ids_for_trigram(&b, "alp").unwrap().len(), 50);
    }

    #[test]
    fn fts_key_format() {
        // #275: one key per trigram, `fts:{trigram}:`, value = packed postings.
        let key = fts_key("hel");
        assert_eq!(key, b"fts:hel:");
    }

    #[test]
    fn postings_encode_decode_roundtrip_and_merge() {
        // The id-only codec now serves the edge index (`efts:`); node postings
        // use the tf-bearing codec below.
        let ids = [1u64, 5, 9, 42];
        assert_eq!(decode_postings(&encode_postings(&ids)), ids);
        let mut list = vec![1u64, 5, 9];
        assert!(merge_posting(&mut list, 7));
        assert_eq!(list, vec![1, 5, 7, 9]); // stays sorted
        assert!(!merge_posting(&mut list, 5)); // already present
        assert!(remove_posting(&mut list, 5));
        assert_eq!(list, vec![1, 7, 9]);
        assert!(!remove_posting(&mut list, 100));
    }

    #[test]
    fn node_postings_tf_encode_decode_roundtrip_and_merge() {
        let entries = vec![(1u64, 3u32), (5, 1), (9, 42)];
        assert_eq!(
            decode_node_postings(&encode_node_postings(&entries)),
            entries
        );

        let mut list = vec![(1u64, 2u32), (5, 1), (9, 4)];
        merge_node_posting(&mut list, 7, 3);
        assert_eq!(list, vec![(1, 2), (5, 1), (7, 3), (9, 4)]); // sorted by id
        merge_node_posting(&mut list, 5, 9); // present id → tf replaced
        assert_eq!(list[1], (5, 9));
        assert!(remove_node_posting(&mut list, 5));
        assert!(!remove_node_posting(&mut list, 100));
    }

    #[test]
    fn index_records_term_frequency() {
        // A trigram repeated across a document gets a higher tf than a single
        // occurrence — the stat BM25 now reads straight from the posting list.
        let b = backend();
        index_node(&b, 1, "rust rust rust", "").unwrap();
        index_node(&b, 2, "rust", "").unwrap();
        let postings = postings_for_trigram(&b, "rus").unwrap();
        let tf1 = postings.iter().find(|(id, _)| *id == 1).unwrap().1;
        let tf2 = postings.iter().find(|(id, _)| *id == 2).unwrap().1;
        assert_eq!(tf2, 1, "single occurrence → tf 1");
        assert!(tf1 > tf2, "repeated term has higher tf ({tf1} vs {tf2})");
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
        // The 2-arg wrapper equals the *_with_props path with empty properties:
        // both write the same posting-list rows.
        let a = backend();
        index_node(&a, 1, "Hello", "World").unwrap();
        let b = backend();
        index_node_with_props(&b, 1, "Hello", "World", &Properties(HashMap::new())).unwrap();
        for tg in ["hel", "ell", "llo", "wor", "orl", "rld"] {
            assert_eq!(
                node_ids_for_trigram(&a, tg).unwrap(),
                node_ids_for_trigram(&b, tg).unwrap()
            );
        }
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
