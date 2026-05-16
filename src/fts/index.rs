//! FTS index: trigram -> posting list storage.
//!
//! Stores inverted index entries as KV pairs:
//! `fts:{trigram}:{node_id}` -> empty bytes.
//!
//! This allows efficient posting list retrieval via `scan_prefix("fts:{trigram}:")`,
//! and efficient add/remove of individual node entries without touching other nodes.

use crate::error::{DrevoError, Result};
use crate::fts::tokenizer::extract_trigrams;
use crate::storage::StorageBackend;

/// Key prefix for FTS index entries: `fts:{trigram}:{node_id}` -> empty.
const PREFIX_FTS: &[u8] = b"fts:";

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

// Note: We cannot efficiently scan all trigrams for a specific node
// because the node_id is at the end of the key. Instead, we re-extract
// trigrams from the text and delete entries one by one.

/// Add FTS index entries for a node.
///
/// Extracts trigrams from the title and body, then stores
/// one `fts:{trigram}:{node_id}` entry per trigram.
pub(crate) fn index_node(
    backend: &dyn StorageBackend,
    node_id: u64,
    title: &str,
    body: &str,
) -> Result<()> {
    let trigrams = extract_trigrams(title, body);
    for trigram in &trigrams {
        backend
            .put(&fts_key(trigram, node_id), &[])
            .map_err(DrevoError::Storage)?;
    }
    Ok(())
}

/// Remove FTS index entries for a node.
///
/// Re-extracts trigrams from the title and body, then deletes
/// the corresponding index entries.
pub(crate) fn deindex_node(
    backend: &dyn StorageBackend,
    node_id: u64,
    title: &str,
    body: &str,
) -> Result<()> {
    let trigrams = extract_trigrams(title, body);
    for trigram in &trigrams {
        backend
            .delete(&fts_key(trigram, node_id))
            .map_err(DrevoError::Storage)?;
    }
    Ok(())
}

/// Retrieve all node IDs from the posting list of a single trigram.
///
/// Scans `fts:{trigram}:` prefix and extracts node IDs from the keys.
pub(crate) fn node_ids_for_trigram(
    backend: &dyn StorageBackend,
    trigram: &str,
) -> Result<Vec<u64>> {
    let prefix = fts_trigram_prefix(trigram);
    let entries = backend.scan_prefix(&prefix).map_err(DrevoError::Storage)?;

    let mut ids = Vec::with_capacity(entries.len());
    for (key, _) in entries {
        let suffix = &key[prefix.len()..];
        if suffix.len() == 8 {
            ids.push(u64::from_le_bytes(suffix.try_into().unwrap()));
        }
    }
    Ok(ids)
}

/// Count how many nodes contain a given trigram (document frequency).
pub(crate) fn posting_list_len(backend: &dyn StorageBackend, trigram: &str) -> Result<usize> {
    let prefix = fts_trigram_prefix(trigram);
    let entries = backend.scan_prefix(&prefix).map_err(DrevoError::Storage)?;
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
