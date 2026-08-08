//! Edge (relationship) FTS index (#227-B).
//!
//! Mirrors the node FTS index ([`crate::fts::index`]) on a separate `efts:` /
//! `eftslen:` keyspace so a relationship's **string properties** (e.g. graphiti's
//! `name` / `fact` on `:RELATES_TO`) are BM25-searchable via the
//! `fts.searchRelationships` procedure — the edge companion of `fts.search`.
//!
//! Edges have no `title`/`body`, so the indexed text is exactly the string
//! property values (and array string elements) gathered by
//! [`crate::fts::index::collect_property_text`]. The BM25 IDF, corpus-stats
//! shape, and tokenizer are shared with the node index; only the key prefixes
//! and the posting/length scans are edge-specific.

use crate::error::Result;
use crate::fts::index::{
    collect_property_text, decode_postings, encode_postings, merge_posting, remove_posting,
    CorpusStats,
};
use crate::fts::tokenizer::{extract_raw_trigrams_fields, extract_trigrams_fields};
use crate::model::Properties;
use crate::storage::StorageBackend;

/// Posting-list prefix: `efts:{trigram}:` -> packed sorted `[edge_id]` (#275).
const PREFIX_EFTS: &[u8] = b"efts:";
/// Per-edge length prefix: `eftslen:{edge_id_le8}` -> `u32` LE trigram count.
const PREFIX_EFTS_LEN: &[u8] = b"eftslen:";

/// Posting-list key for a trigram: `efts:{trigram}:` -> packed `[edge_id]`
/// (#275, mirrors the node [`crate::fts::index`] layout — one row per trigram).
fn efts_key(trigram: &str) -> Vec<u8> {
    let mut key = PREFIX_EFTS.to_vec();
    key.extend_from_slice(trigram.as_bytes());
    key.push(b':');
    key
}

fn efts_len_key(edge_id: u64) -> Vec<u8> {
    let mut key = PREFIX_EFTS_LEN.to_vec();
    key.extend_from_slice(&edge_id.to_le_bytes());
    key
}

/// The raw (bag) trigrams an edge contributes — from its string properties.
/// Shared by indexing and ranking so query-time term frequency matches the
/// index.
pub(crate) fn edge_raw_trigrams(properties: &Properties) -> Vec<String> {
    let text = collect_property_text(properties);
    let fields: Vec<&str> = text.iter().map(String::as_str).collect();
    extract_raw_trigrams_fields(&fields)
}

/// Index an edge by its string properties (#227-B). Test convenience over
/// [`index_edges_grouped`] for a single edge (production batches via
/// `index_edges_grouped`). Same FTS-write-lock requirement.
#[cfg(test)]
pub(crate) fn index_edge(
    backend: &dyn StorageBackend,
    edge_id: u64,
    properties: &Properties,
) -> Result<()> {
    index_edges_grouped(backend, &[(edge_id, properties)])
}

/// Index many edges into the posting-list `efts:` store (#275), with each
/// trigram's posting list read-modified-written **once** across the batch.
///
/// **The caller MUST serialize FTS writes** (drevo's FTS write lock): each
/// posting list is a `get` → merge → `put`.
pub(crate) fn index_edges_grouped(
    backend: &dyn StorageBackend,
    docs: &[(u64, &Properties)],
) -> Result<()> {
    use std::collections::BTreeMap;

    let mut by_trigram: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut len_writes: Vec<(u64, u32)> = Vec::with_capacity(docs.len());
    for (edge_id, properties) in docs {
        let text = collect_property_text(properties);
        let fields: Vec<&str> = text.iter().map(String::as_str).collect();
        for trigram in extract_trigrams_fields(&fields) {
            by_trigram.entry(trigram).or_default().push(*edge_id);
        }
        let doc_len = extract_raw_trigrams_fields(&fields).len();
        if doc_len > 0 {
            len_writes.push((*edge_id, doc_len as u32));
        }
    }
    // One batched commit for all updated edge posting lists + lengths (see the
    // node index for why: per-trigram puts meant one fsync each, regressing
    // bulk import/shrink). Caller holds the FTS write lock.
    let mut writes: Vec<(Vec<u8>, Vec<u8>)> =
        Vec::with_capacity(by_trigram.len() + len_writes.len());
    for (trigram, mut new_ids) in by_trigram {
        let key = efts_key(&trigram);
        let mut list = match backend.get(&key)? {
            Some(bytes) => decode_postings(&bytes),
            None => Vec::new(),
        };
        new_ids.sort_unstable();
        new_ids.dedup();
        for id in new_ids {
            merge_posting(&mut list, id);
        }
        writes.push((key, encode_postings(&list)));
    }
    for (edge_id, doc_len) in len_writes {
        writes.push((efts_len_key(edge_id), doc_len.to_le_bytes().to_vec()));
    }
    backend.put_batch(&writes)?;
    Ok(())
}

/// Build the complete `efts:` posting-list index for a set of edges as one
/// batch (assumes no existing `efts:` rows), for the #275 reindex-on-open.
pub(crate) fn build_full_edge_index_batch(docs: &[(u64, &Properties)]) -> Vec<(Vec<u8>, Vec<u8>)> {
    use std::collections::BTreeMap;

    let mut by_trigram: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut out: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for (edge_id, properties) in docs {
        let text = collect_property_text(properties);
        let fields: Vec<&str> = text.iter().map(String::as_str).collect();
        for trigram in extract_trigrams_fields(&fields) {
            by_trigram.entry(trigram).or_default().push(*edge_id);
        }
        let doc_len = extract_raw_trigrams_fields(&fields).len();
        if doc_len > 0 {
            out.push((
                efts_len_key(*edge_id),
                (doc_len as u32).to_le_bytes().to_vec(),
            ));
        }
    }
    for (trigram, mut ids) in by_trigram {
        ids.sort_unstable();
        ids.dedup();
        out.push((efts_key(&trigram), encode_postings(&ids)));
    }
    out
}

/// Remove an edge's `edge_id` from every trigram posting list it contributed to
/// (re-deriving trigrams from `properties`) and delete its `eftslen:` entry. A
/// posting list that becomes empty has its row deleted. Same FTS-write-lock
/// requirement as [`index_edges_grouped`].
pub(crate) fn deindex_edge(
    backend: &dyn StorageBackend,
    edge_id: u64,
    properties: &Properties,
) -> Result<()> {
    let text = collect_property_text(properties);
    let fields: Vec<&str> = text.iter().map(String::as_str).collect();
    for trigram in &extract_trigrams_fields(&fields) {
        let key = efts_key(trigram);
        if let Some(bytes) = backend.get(&key)? {
            let mut list = decode_postings(&bytes);
            if remove_posting(&mut list, edge_id) {
                if list.is_empty() {
                    backend.delete(&key)?;
                } else {
                    backend.put(&key, &encode_postings(&list))?;
                }
            }
        }
    }
    backend.delete(&efts_len_key(edge_id))?;
    Ok(())
}

/// Edge ids in the posting list of a single trigram — a single `get` (#275).
pub(crate) fn edge_ids_for_trigram(
    backend: &dyn StorageBackend,
    trigram: &str,
) -> Result<Vec<u64>> {
    Ok(match backend.get(&efts_key(trigram))? {
        Some(bytes) => decode_postings(&bytes),
        None => Vec::new(),
    })
}

/// Number of edges in a trigram's posting list (document frequency).
pub(crate) fn posting_list_len(backend: &dyn StorageBackend, trigram: &str) -> Result<usize> {
    Ok(match backend.get(&efts_key(trigram))? {
        Some(bytes) => bytes.len() / 8,
        None => 0,
    })
}

/// Candidate edge ids: the intersection of every query trigram's posting list.
pub(crate) fn intersect_trigrams(
    backend: &dyn StorageBackend,
    trigrams: &[String],
) -> Result<Vec<u64>> {
    if trigrams.is_empty() {
        return Ok(Vec::new());
    }
    let mut result = edge_ids_for_trigram(backend, &trigrams[0])?;
    for trigram in &trigrams[1..] {
        if result.is_empty() {
            break;
        }
        let ids = edge_ids_for_trigram(backend, trigram)?;
        result.retain(|id| ids.contains(id));
    }
    result.sort_unstable();
    Ok(result)
}

/// One edge's document length `|d|`, or `None` if unindexed.
pub(crate) fn doc_length(backend: &dyn StorageBackend, edge_id: u64) -> Result<Option<u32>> {
    let raw = backend.get(&efts_len_key(edge_id))?;
    Ok(raw.and_then(|bytes| {
        let arr: [u8; 4] = bytes.as_slice().try_into().ok()?;
        Some(u32::from_le_bytes(arr))
    }))
}

/// Corpus statistics (BM25 `N` + total length) over the edge index.
pub(crate) fn corpus_stats(backend: &dyn StorageBackend) -> Result<CorpusStats> {
    let entries = backend.scan_prefix(PREFIX_EFTS_LEN)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryBackend;

    fn props(pairs: &[(&str, serde_json::Value)]) -> Properties {
        Properties(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    #[test]
    fn indexes_and_finds_edge_by_string_property() {
        let b = MemoryBackend::new();
        index_edge(
            &b,
            5,
            &props(&[("fact", serde_json::json!("acquired wolverine corp"))]),
        )
        .unwrap();
        assert_eq!(edge_ids_for_trigram(&b, "wol").unwrap(), vec![5]);
    }

    #[test]
    fn deindex_removes_edge_postings() {
        let b = MemoryBackend::new();
        let p = props(&[("name", serde_json::json!("zebra link"))]);
        index_edge(&b, 5, &p).unwrap();
        deindex_edge(&b, 5, &p).unwrap();
        assert!(edge_ids_for_trigram(&b, "zeb").unwrap().is_empty());
        assert!(doc_length(&b, 5).unwrap().is_none());
    }

    #[test]
    fn edge_and_node_keyspaces_do_not_collide() {
        // efts:/eftslen: must not be caught by the node index's fts:/ftslen:
        // scans (and vice-versa). Index an edge, assert the node index is empty.
        let b = MemoryBackend::new();
        index_edge(&b, 1, &props(&[("name", serde_json::json!("zebra"))])).unwrap();
        assert!(crate::fts::index::node_ids_for_trigram(&b, "zeb")
            .unwrap()
            .is_empty());
        assert_eq!(edge_ids_for_trigram(&b, "zeb").unwrap(), vec![1]);
    }

    #[test]
    fn corpus_stats_counts_indexed_edges() {
        let b = MemoryBackend::new();
        index_edge(&b, 1, &props(&[("name", serde_json::json!("alpha beta"))])).unwrap();
        index_edge(&b, 2, &props(&[("name", serde_json::json!("gamma delta"))])).unwrap();
        let stats = corpus_stats(&b).unwrap();
        assert_eq!(stats.doc_count, 2);
        assert!(stats.total_len > 0);
    }
}
