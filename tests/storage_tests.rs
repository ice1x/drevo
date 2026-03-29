use std::sync::Arc;

use graphnote_db::storage::{MemoryBackend, StorageBackend, StorageError};

// --- Persist / load integration tests ---

#[test]
fn test_persist_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("roundtrip.db");

    {
        let backend = MemoryBackend::open(&db_path).unwrap();
        backend.put(b"n:1", b"node1").unwrap();
        backend.put(b"n:2", b"node2").unwrap();
        backend.put(b"e:1:likes:2", b"edge_data").unwrap();
        backend.flush().unwrap();
    }

    let backend = MemoryBackend::open(&db_path).unwrap();
    assert_eq!(backend.get(b"n:1").unwrap(), Some(b"node1".to_vec()));
    assert_eq!(backend.get(b"n:2").unwrap(), Some(b"node2".to_vec()));
    assert_eq!(
        backend.get(b"e:1:likes:2").unwrap(),
        Some(b"edge_data".to_vec())
    );
    assert_eq!(backend.get(b"n:999").unwrap(), None);
}

#[test]
fn test_persist_scan_prefix_after_reload() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("scan.db");

    {
        let backend = MemoryBackend::open(&db_path).unwrap();
        backend.put(b"e:42:likes:1", b"d1").unwrap();
        backend.put(b"e:42:likes:2", b"d2").unwrap();
        backend.put(b"e:42:knows:3", b"d3").unwrap();
        backend.put(b"e:99:likes:1", b"other").unwrap();
        backend.flush().unwrap();
    }

    let backend = MemoryBackend::open(&db_path).unwrap();
    let results = backend.scan_prefix(b"e:42:").unwrap();
    assert_eq!(results.len(), 3);
    for (k, _) in &results {
        assert!(k.starts_with(b"e:42:"));
    }
}

#[test]
fn test_persist_via_trait_object() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("trait.db");

    {
        let backend: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::open(&db_path).unwrap());
        backend.put(b"key", b"value").unwrap();
        backend.flush().unwrap();
    }

    let backend = MemoryBackend::open(&db_path).unwrap();
    assert_eq!(backend.get(b"key").unwrap(), Some(b"value".to_vec()));
}

#[test]
fn test_persist_multiple_flush_cycles() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("multi.db");

    let backend = MemoryBackend::open(&db_path).unwrap();

    backend.put(b"k1", b"v1").unwrap();
    backend.flush().unwrap();

    backend.put(b"k2", b"v2").unwrap();
    backend.delete(b"k1").unwrap();
    backend.flush().unwrap();

    drop(backend);

    let backend = MemoryBackend::open(&db_path).unwrap();
    assert_eq!(backend.get(b"k1").unwrap(), None);
    assert_eq!(backend.get(b"k2").unwrap(), Some(b"v2".to_vec()));
}

/// Verify that MemoryBackend can be used as a trait object behind Arc.
fn backend_as_trait_object() -> Arc<dyn StorageBackend> {
    Arc::new(MemoryBackend::new())
}

// --- Tests that verify the StorageBackend contract via MemoryBackend ---

#[test]
fn test_put_and_get() {
    let backend = MemoryBackend::new();
    backend.put(b"key1", b"value1").unwrap();
    assert_eq!(backend.get(b"key1").unwrap(), Some(b"value1".to_vec()));
}

#[test]
fn test_get_missing_key_returns_none() {
    let backend = MemoryBackend::new();
    assert_eq!(backend.get(b"nonexistent").unwrap(), None);
}

#[test]
fn test_put_overwrites_existing() {
    let backend = MemoryBackend::new();
    backend.put(b"key", b"v1").unwrap();
    backend.put(b"key", b"v2").unwrap();
    assert_eq!(backend.get(b"key").unwrap(), Some(b"v2".to_vec()));
}

#[test]
fn test_delete_existing_key() {
    let backend = MemoryBackend::new();
    backend.put(b"key", b"value").unwrap();
    backend.delete(b"key").unwrap();
    assert_eq!(backend.get(b"key").unwrap(), None);
}

#[test]
fn test_delete_nonexistent_is_noop() {
    let backend = MemoryBackend::new();
    backend.delete(b"nonexistent").unwrap();
}

#[test]
fn test_scan_prefix_returns_matching_sorted() {
    let backend = MemoryBackend::new();
    backend.put(b"e:42:likes:1", b"data1").unwrap();
    backend.put(b"e:42:likes:2", b"data2").unwrap();
    backend.put(b"e:42:knows:3", b"data3").unwrap();
    backend.put(b"e:99:likes:1", b"other").unwrap();
    backend.put(b"n:42", b"node").unwrap();

    let results = backend.scan_prefix(b"e:42:").unwrap();
    assert_eq!(results.len(), 3);
    // Verify sorted order
    assert!(results[0].0 < results[1].0);
    assert!(results[1].0 < results[2].0);
    // All keys start with prefix
    for (k, _) in &results {
        assert!(k.starts_with(b"e:42:"));
    }
}

#[test]
fn test_scan_prefix_no_matches() {
    let backend = MemoryBackend::new();
    backend.put(b"abc", b"value").unwrap();
    let results = backend.scan_prefix(b"xyz").unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_scan_prefix_empty_store() {
    let backend = MemoryBackend::new();
    let results = backend.scan_prefix(b"any").unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_flush_does_not_error() {
    let backend = MemoryBackend::new();
    backend.put(b"key", b"value").unwrap();
    backend.flush().unwrap();
}

#[test]
fn test_empty_key_and_value() {
    let backend = MemoryBackend::new();
    backend.put(b"", b"").unwrap();
    assert_eq!(backend.get(b"").unwrap(), Some(b"".to_vec()));
}

#[test]
fn test_binary_key_and_value() {
    let backend = MemoryBackend::new();
    let key = vec![0u8, 1, 255, 128];
    let value = vec![42u8; 1024];
    backend.put(&key, &value).unwrap();
    assert_eq!(backend.get(&key).unwrap(), Some(value));
}

#[test]
fn test_trait_object_works() {
    let backend = backend_as_trait_object();
    backend.put(b"key", b"value").unwrap();
    assert_eq!(backend.get(b"key").unwrap(), Some(b"value".to_vec()));
}

#[test]
fn test_error_display() {
    let err = StorageError::NotFound(b"test_key".to_vec());
    assert!(err.to_string().contains("test_key"));

    let err = StorageError::Serialization("bad format".to_string());
    assert!(err.to_string().contains("bad format"));

    let err = StorageError::Backend("lock poisoned".to_string());
    assert!(err.to_string().contains("lock poisoned"));
}

#[test]
fn test_error_display_binary_key() {
    let err = StorageError::NotFound(vec![0xFF, 0x00, 0xAB]);
    let msg = err.to_string();
    // Non-UTF8 key should still display without panic
    assert!(msg.contains("key not found"));
}
