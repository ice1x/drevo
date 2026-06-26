//! Persistent property index (Phase 14 task `00088`).
//!
//! Maps every `(property key, property value)` pair carried by a node to
//! the set of node ids that hold it, so an equality lookup such as the
//! Cypher pattern `MATCH (n {status: "open"})` resolves through an
//! `O(matches)` prefix scan instead of an `O(N)` full-node scan. This is
//! the durable index foundation Phase 14 (Query Optimization) needs to
//! turn the planner's `NodeIndexSeek` choice into a real fast path; the
//! index is maintained transparently by [`crate::db::Drevo`] on every
//! `create` / `update` / `delete` (mirroring the kind and FTS indexes),
//! and queried through [`crate::db::Drevo::nodes_by_property`].
//!
//! # Key layout
//!
//! ```text
//! prop:{key_len_le32}{key}{val_len_le32}{value}{node_id_le64} -> []
//! ```
//!
//! Both the key and the canonical value bytes are length-prefixed with a
//! fixed-width `u32` (little-endian). Length framing makes the prefix that
//! selects a single `(key, value)` pair unambiguous: because the value's
//! byte length is pinned by `val_len`, the prefix for value `"x"` can
//! never byte-match a stored entry for `"xy"` (their `val_len` fields
//! differ), so the trailing 8 bytes are always exactly the node id. The
//! same trick guards the key field, so a property named `"a"` never
//! collides with one named `"ab"`.
//!
//! # Value canonicalization
//!
//! Values are arbitrary [`serde_json::Value`]s. They are encoded with
//! [`serde_json::to_vec`], which is deterministic here: `drevo` does not
//! enable serde_json's `preserve_order` feature, so object keys serialize
//! in sorted (`BTreeMap`) order — the same normalization
//! [`crate::model::Properties`] relies on for stable bincode output. The
//! index therefore matches on exact canonical-byte equality: a stored
//! integer `5` matches a queried integer `5`, and two objects that differ
//! only in key order still match.

use serde_json::Value;

use crate::error::Result;
use crate::model::Properties;
use crate::storage::StorageBackend;

/// Key prefix for the property index: `prop:{...}` -> empty value.
const PREFIX_PROP: &[u8] = b"prop:";

/// Canonical byte encoding of a property value (sorted-key JSON).
///
/// Deterministic because `drevo` builds `serde_json` without the
/// `preserve_order` feature, so object keys serialize in `BTreeMap` order.
pub fn encode_value(value: &Value) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(value)?)
}

/// The scan prefix selecting every node whose `key` property equals the
/// value encoded as `value_bytes`. Full index keys are this prefix
/// followed by the 8-byte little-endian node id.
fn property_value_prefix(key: &str, value_bytes: &[u8]) -> Vec<u8> {
    let key_bytes = key.as_bytes();
    let mut out = Vec::with_capacity(PREFIX_PROP.len() + 8 + key_bytes.len() + value_bytes.len());
    out.extend_from_slice(PREFIX_PROP);
    out.extend_from_slice(&(key_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(key_bytes);
    out.extend_from_slice(&(value_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(value_bytes);
    out
}

/// Full index key: the `(key, value)` prefix plus the node id suffix.
fn property_key(key: &str, value_bytes: &[u8], node_id: u64) -> Vec<u8> {
    let mut out = property_value_prefix(key, value_bytes);
    out.extend_from_slice(&node_id.to_le_bytes());
    out
}

/// Extract the node id from a full index key given the length of the
/// `(key, value)` prefix it was scanned under. Returns `None` (rather than
/// panicking) for a malformed suffix — `drevo-rust` §"Error handling".
fn id_from_key(key: &[u8], prefix_len: usize) -> Option<u64> {
    let suffix = key.get(prefix_len..)?;
    let arr: [u8; 8] = suffix.try_into().ok()?;
    Some(u64::from_le_bytes(arr))
}

/// Build (but do not write) the property index entries for a node — every
/// key is `node_id`-scoped, so a bulk insert can fold many nodes' entries
/// into one transaction. Shared by [`index_node`] and the batch node-create
/// path ([`crate::db::Drevo::create_nodes`]).
pub fn node_index_entries(
    node_id: u64,
    properties: &Properties,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(properties.len());
    for (key, value) in properties.iter() {
        let value_bytes = encode_value(value)?;
        entries.push((property_key(key, &value_bytes, node_id), Vec::new()));
    }
    Ok(entries)
}

/// Add an index entry for every property of a node. Called by every
/// node-insertion path in [`crate::db::Drevo`] alongside the FTS indexer.
pub fn index_node(
    backend: &dyn StorageBackend,
    node_id: u64,
    properties: &Properties,
) -> Result<()> {
    for (key, value) in node_index_entries(node_id, properties)? {
        backend.put(&key, &value)?;
    }
    Ok(())
}

/// Remove the index entry for every property of a node. Called by every
/// node-removal path in [`crate::db::Drevo`] alongside the FTS deindexer.
pub fn deindex_node(
    backend: &dyn StorageBackend,
    node_id: u64,
    properties: &Properties,
) -> Result<()> {
    for (key, value) in properties.iter() {
        let value_bytes = encode_value(value)?;
        backend.delete(&property_key(key, &value_bytes, node_id))?;
    }
    Ok(())
}

/// Return the ids of all nodes whose `key` property equals `value`,
/// resolved through a single prefix scan. The result is sorted ascending
/// and deduplicated by construction (a node holds a property key once).
pub fn node_ids_for_value(
    backend: &dyn StorageBackend,
    key: &str,
    value: &Value,
) -> Result<Vec<u64>> {
    let value_bytes = encode_value(value)?;
    let prefix = property_value_prefix(key, &value_bytes);
    let entries = backend.scan_prefix(&prefix)?;
    let mut ids = Vec::with_capacity(entries.len());
    for (k, _) in entries {
        if let Some(id) = id_from_key(&k, prefix.len()) {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryBackend;
    use serde_json::json;
    use std::collections::HashMap;

    fn props(pairs: &[(&str, Value)]) -> Properties {
        let mut map = HashMap::new();
        for (k, v) in pairs {
            map.insert((*k).to_string(), v.clone());
        }
        Properties(map)
    }

    #[test]
    fn key_layout_has_prefix_and_trailing_id() {
        let value_bytes = encode_value(&json!("open")).unwrap();
        let key = property_key("status", &value_bytes, 42);
        assert!(key.starts_with(PREFIX_PROP));
        // Last 8 bytes are the little-endian node id.
        let id = id_from_key(&key, key.len() - 8).unwrap();
        assert_eq!(id, 42);
    }

    #[test]
    fn encode_value_is_canonical_across_object_key_order() {
        let a = json!({ "x": 1, "y": 2 });
        let b = json!({ "y": 2, "x": 1 });
        assert_eq!(encode_value(&a).unwrap(), encode_value(&b).unwrap());
    }

    #[test]
    fn index_then_lookup_returns_the_node() {
        let backend = MemoryBackend::new();
        index_node(&backend, 7, &props(&[("status", json!("open"))])).unwrap();
        let ids = node_ids_for_value(&backend, "status", &json!("open")).unwrap();
        assert_eq!(ids, vec![7]);
    }

    #[test]
    fn lookup_returns_all_matching_nodes_sorted() {
        let backend = MemoryBackend::new();
        for id in [9u64, 3, 5] {
            index_node(&backend, id, &props(&[("priority", json!(1))])).unwrap();
        }
        let ids = node_ids_for_value(&backend, "priority", &json!(1)).unwrap();
        assert_eq!(ids, vec![3, 5, 9]);
    }

    #[test]
    fn lookup_distinguishes_values_of_the_same_key() {
        let backend = MemoryBackend::new();
        index_node(&backend, 1, &props(&[("status", json!("open"))])).unwrap();
        index_node(&backend, 2, &props(&[("status", json!("closed"))])).unwrap();
        assert_eq!(
            node_ids_for_value(&backend, "status", &json!("open")).unwrap(),
            vec![1]
        );
        assert_eq!(
            node_ids_for_value(&backend, "status", &json!("closed")).unwrap(),
            vec![2]
        );
    }

    #[test]
    fn length_framing_prevents_value_prefix_collisions() {
        // "x" must not match an entry stored for "xy": their val_len
        // fields differ, so the prefixes cannot byte-overlap.
        let backend = MemoryBackend::new();
        index_node(&backend, 1, &props(&[("tag", json!("x"))])).unwrap();
        index_node(&backend, 2, &props(&[("tag", json!("xy"))])).unwrap();
        assert_eq!(
            node_ids_for_value(&backend, "tag", &json!("x")).unwrap(),
            vec![1]
        );
        assert_eq!(
            node_ids_for_value(&backend, "tag", &json!("xy")).unwrap(),
            vec![2]
        );
    }

    #[test]
    fn length_framing_prevents_key_prefix_collisions() {
        // Property "a" must not collide with property "ab".
        let backend = MemoryBackend::new();
        index_node(&backend, 1, &props(&[("a", json!(true))])).unwrap();
        index_node(&backend, 2, &props(&[("ab", json!(true))])).unwrap();
        assert_eq!(
            node_ids_for_value(&backend, "a", &json!(true)).unwrap(),
            vec![1]
        );
        assert_eq!(
            node_ids_for_value(&backend, "ab", &json!(true)).unwrap(),
            vec![2]
        );
    }

    #[test]
    fn deindex_removes_only_that_nodes_entries() {
        let backend = MemoryBackend::new();
        index_node(&backend, 1, &props(&[("status", json!("open"))])).unwrap();
        index_node(&backend, 2, &props(&[("status", json!("open"))])).unwrap();
        deindex_node(&backend, 1, &props(&[("status", json!("open"))])).unwrap();
        assert_eq!(
            node_ids_for_value(&backend, "status", &json!("open")).unwrap(),
            vec![2]
        );
    }

    #[test]
    fn lookup_on_absent_value_is_empty() {
        let backend = MemoryBackend::new();
        index_node(&backend, 1, &props(&[("status", json!("open"))])).unwrap();
        assert!(node_ids_for_value(&backend, "status", &json!("done"))
            .unwrap()
            .is_empty());
        assert!(node_ids_for_value(&backend, "missing", &json!("open"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn distinct_json_types_are_indexed_independently() {
        let backend = MemoryBackend::new();
        index_node(&backend, 1, &props(&[("v", json!(1))])).unwrap();
        index_node(&backend, 2, &props(&[("v", json!("1"))])).unwrap();
        index_node(&backend, 3, &props(&[("v", json!(true))])).unwrap();
        index_node(&backend, 4, &props(&[("v", json!(null))])).unwrap();
        assert_eq!(
            node_ids_for_value(&backend, "v", &json!(1)).unwrap(),
            vec![1]
        );
        assert_eq!(
            node_ids_for_value(&backend, "v", &json!("1")).unwrap(),
            vec![2]
        );
        assert_eq!(
            node_ids_for_value(&backend, "v", &json!(true)).unwrap(),
            vec![3]
        );
        assert_eq!(
            node_ids_for_value(&backend, "v", &json!(null)).unwrap(),
            vec![4]
        );
    }

    #[test]
    fn unicode_and_emoji_values_round_trip() {
        let backend = MemoryBackend::new();
        index_node(&backend, 1, &props(&[("label", json!("café☕"))])).unwrap();
        index_node(&backend, 2, &props(&[("label", json!("日本語"))])).unwrap();
        assert_eq!(
            node_ids_for_value(&backend, "label", &json!("café☕")).unwrap(),
            vec![1]
        );
        assert_eq!(
            node_ids_for_value(&backend, "label", &json!("日本語")).unwrap(),
            vec![2]
        );
    }
}
