use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use grapevine::storage::{Result, StorageBackend, StorageError};

/// Minimal in-memory implementation used to verify the trait contract compiles
/// and behaves correctly. This will be replaced by `MemoryBackend` in task 0003.
struct MockBackend {
    data: Mutex<BTreeMap<Vec<u8>, Vec<u8>>>,
}

impl MockBackend {
    fn new() -> Self {
        Self {
            data: Mutex::new(BTreeMap::new()),
        }
    }
}

impl StorageBackend for MockBackend {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let data = self
            .data
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        Ok(data.get(key).cloned())
    }

    fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let mut data = self
            .data
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        data.insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn delete(&self, key: &[u8]) -> Result<()> {
        let mut data = self
            .data
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        data.remove(key);
        Ok(())
    }

    fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let data = self
            .data
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        let results: Vec<(Vec<u8>, Vec<u8>)> = data
            .range(prefix.to_vec()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Ok(results)
    }

    fn flush(&self) -> Result<()> {
        Ok(())
    }
}

/// Verify that the trait can be used as a trait object behind Arc.
fn backend_as_trait_object() -> Arc<dyn StorageBackend> {
    Arc::new(MockBackend::new())
}

// --- Tests that verify the StorageBackend contract ---

fn run_test(backend: &dyn StorageBackend) {
    // put + get
    backend.put(b"key1", b"value1").unwrap();
    assert_eq!(backend.get(b"key1").unwrap(), Some(b"value1".to_vec()));
}

#[test]
fn test_put_and_get() {
    let backend = MockBackend::new();
    run_test(&backend);
}

#[test]
fn test_get_missing_key_returns_none() {
    let backend = MockBackend::new();
    assert_eq!(backend.get(b"nonexistent").unwrap(), None);
}

#[test]
fn test_put_overwrites_existing() {
    let backend = MockBackend::new();
    backend.put(b"key", b"v1").unwrap();
    backend.put(b"key", b"v2").unwrap();
    assert_eq!(backend.get(b"key").unwrap(), Some(b"v2".to_vec()));
}

#[test]
fn test_delete_existing_key() {
    let backend = MockBackend::new();
    backend.put(b"key", b"value").unwrap();
    backend.delete(b"key").unwrap();
    assert_eq!(backend.get(b"key").unwrap(), None);
}

#[test]
fn test_delete_nonexistent_is_noop() {
    let backend = MockBackend::new();
    // Should not error
    backend.delete(b"nonexistent").unwrap();
}

#[test]
fn test_scan_prefix_returns_matching_sorted() {
    let backend = MockBackend::new();
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
    let backend = MockBackend::new();
    backend.put(b"abc", b"value").unwrap();
    let results = backend.scan_prefix(b"xyz").unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_scan_prefix_empty_store() {
    let backend = MockBackend::new();
    let results = backend.scan_prefix(b"any").unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_flush_does_not_error() {
    let backend = MockBackend::new();
    backend.put(b"key", b"value").unwrap();
    backend.flush().unwrap();
}

#[test]
fn test_empty_key_and_value() {
    let backend = MockBackend::new();
    backend.put(b"", b"").unwrap();
    assert_eq!(backend.get(b"").unwrap(), Some(b"".to_vec()));
}

#[test]
fn test_binary_key_and_value() {
    let backend = MockBackend::new();
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
