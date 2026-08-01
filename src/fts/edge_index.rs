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
use crate::fts::index::{collect_property_text, CorpusStats};
use crate::fts::tokenizer::{extract_raw_trigrams_fields, extract_trigrams_fields};
use crate::model::Properties;
use crate::storage::StorageBackend;

/// Posting prefix: `efts:{trigram}:{edge_id_le8}` -> empty.
const PREFIX_EFTS: &[u8] = b"efts:";
/// Per-edge length prefix: `eftslen:{edge_id_le8}` -> `u32` LE trigram count.
const PREFIX_EFTS_LEN: &[u8] = b"eftslen:";

fn efts_key(trigram: &str, edge_id: u64) -> Vec<u8> {
    let mut key = PREFIX_EFTS.to_vec();
    key.extend_from_slice(trigram.as_bytes());
    key.push(b':');
    key.extend_from_slice(&edge_id.to_le_bytes());
    key
}

fn efts_trigram_prefix(trigram: &str) -> Vec<u8> {
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

/// Build (but do not write) the edge FTS entries: one `efts:{trigram}:{edge_id}`
/// posting per distinct trigram plus the `eftslen:{edge_id}` length entry.
pub(crate) fn edge_index_entries(edge_id: u64, properties: &Properties) -> Vec<(Vec<u8>, Vec<u8>)> {
    let text = collect_property_text(properties);
    let fields: Vec<&str> = text.iter().map(String::as_str).collect();
    let trigrams = extract_trigrams_fields(&fields);
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(trigrams.len() + 1);
    for trigram in &trigrams {
        entries.push((efts_key(trigram, edge_id), Vec::new()));
    }
    let doc_len = extract_raw_trigrams_fields(&fields).len();
    if doc_len > 0 {
        entries.push((
            efts_len_key(edge_id),
            (doc_len as u32).to_le_bytes().to_vec(),
        ));
    }
    entries
}

/// Index an edge by its string properties (#227-B).
pub(crate) fn index_edge(
    backend: &dyn StorageBackend,
    edge_id: u64,
    properties: &Properties,
) -> Result<()> {
    for (key, value) in edge_index_entries(edge_id, properties) {
        backend.put(&key, &value)?;
    }
    Ok(())
}

/// Remove an edge's FTS entries, re-deriving trigrams from `properties` so the
/// same postings written by [`index_edge`] are cleaned up.
pub(crate) fn deindex_edge(
    backend: &dyn StorageBackend,
    edge_id: u64,
    properties: &Properties,
) -> Result<()> {
    let text = collect_property_text(properties);
    let fields: Vec<&str> = text.iter().map(String::as_str).collect();
    for trigram in &extract_trigrams_fields(&fields) {
        backend.delete(&efts_key(trigram, edge_id))?;
    }
    backend.delete(&efts_len_key(edge_id))?;
    Ok(())
}

/// Edge ids in the posting list of a single trigram.
pub(crate) fn edge_ids_for_trigram(
    backend: &dyn StorageBackend,
    trigram: &str,
) -> Result<Vec<u64>> {
    let prefix = efts_trigram_prefix(trigram);
    let entries = backend.scan_prefix(&prefix)?;
    let mut ids = Vec::with_capacity(entries.len());
    for (key, _) in entries {
        if key.len() < prefix.len() {
            continue;
        }
        let suffix = &key[prefix.len()..];
        let mut arr = [0u8; 8];
        if suffix.len() == 8 {
            arr.copy_from_slice(suffix);
            ids.push(u64::from_le_bytes(arr));
        }
    }
    Ok(ids)
}

/// Number of edges in a trigram's posting list (document frequency).
pub(crate) fn posting_list_len(backend: &dyn StorageBackend, trigram: &str) -> Result<usize> {
    Ok(backend.scan_prefix(&efts_trigram_prefix(trigram))?.len())
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
