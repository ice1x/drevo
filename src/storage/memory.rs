use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::storage::error::{Result, StorageError};
use crate::storage::StorageBackend;

/// In-memory storage backend backed by a `BTreeMap`.
///
/// Supports two modes:
/// - **Ephemeral** (`new()`): all data lives in memory and is lost on drop.
/// - **Persistent** (`open(path)`): loads existing data from a file on
///   creation and writes a snapshot back on [`flush()`](StorageBackend::flush).
///
/// Thread-safe via interior `Mutex`.
///
/// Persistence format: bincode-encoded `Vec<(Vec<u8>, Vec<u8>)>` — the
/// sorted entries of the BTreeMap. Writes are atomic (temp file + rename).
///
/// This backend is useful for:
/// - Tests and benchmarks (fast, no I/O)
/// - Ephemeral / scratch databases
/// - WASM environments where disk access is unavailable
/// - Small databases that fit in memory but need durability
#[derive(Debug)]
pub struct MemoryBackend {
    data: Mutex<BTreeMap<Vec<u8>, Vec<u8>>>,
    path: Option<PathBuf>,
}

impl MemoryBackend {
    /// Create a new empty in-memory backend (ephemeral, no persistence).
    pub fn new() -> Self {
        Self {
            data: Mutex::new(BTreeMap::new()),
            path: None,
        }
    }

    /// Open a persistent in-memory backend.
    ///
    /// If the file at `path` exists, its contents are loaded into memory.
    /// If the file does not exist, an empty backend is created and the file
    /// will be written on the first [`flush()`](StorageBackend::flush).
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Io`] if the file exists but cannot be read,
    /// or [`StorageError::Serialization`] if the file contents are corrupt.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let data = if path.exists() {
            Self::load_from_file(&path)?
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            data: Mutex::new(data),
            path: Some(path),
        })
    }

    /// Returns the file path if this backend is persistent, `None` if ephemeral.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Load a BTreeMap from a bincode-encoded file.
    fn load_from_file(path: &Path) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
        let bytes = fs::read(path)?;
        let entries: Vec<(Vec<u8>, Vec<u8>)> =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard())
                .map_err(|e| StorageError::Serialization(e.to_string()))?
                .0;
        Ok(entries.into_iter().collect())
    }

    /// Write the BTreeMap to a file atomically (temp file + rename).
    fn save_to_file(data: &BTreeMap<Vec<u8>, Vec<u8>>, path: &Path) -> Result<()> {
        let entries: Vec<(&Vec<u8>, &Vec<u8>)> = data.iter().collect();
        let bytes = bincode::serde::encode_to_vec(&entries, bincode::config::standard())
            .map_err(|e| StorageError::Serialization(e.to_string()))?;

        // Atomic write: write to temp file in the same directory, then rename.
        let parent = path.parent().unwrap_or(Path::new("."));
        let mut tmp = tempfile_in(parent)?;
        tmp.file.write_all(&bytes)?;
        tmp.file.flush()?;
        fs::rename(&tmp.path, path)?;
        tmp.defused = true;
        Ok(())
    }
}

impl Default for MemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl StorageBackend for MemoryBackend {
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
        let Some(ref path) = self.path else {
            return Ok(());
        };
        let data = self
            .data
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        Self::save_to_file(&data, path)
    }
}

// ---------------------------------------------------------------------------
// Minimal temp-file helper (avoids adding a runtime dependency on `tempfile`)
// ---------------------------------------------------------------------------

/// A temporary file that deletes itself on drop unless defused.
struct TempFile {
    file: fs::File,
    path: PathBuf,
    defused: bool,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.defused {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Create a temporary file in `dir` with a unique name.
fn tempfile_in(dir: &Path) -> Result<TempFile> {
    use std::time::SystemTime;

    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    let name = format!(".graphnote-tmp-{pid}-{stamp}");
    let path = dir.join(name);
    let file = fs::File::create(&path)?;
    Ok(TempFile {
        file,
        path,
        defused: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_backend_is_empty() {
        let backend = MemoryBackend::new();
        assert_eq!(backend.get(b"any").unwrap(), None);
    }

    #[test]
    fn default_is_same_as_new() {
        let backend = MemoryBackend::default();
        assert_eq!(backend.get(b"any").unwrap(), None);
    }

    #[test]
    fn put_and_get() {
        let backend = MemoryBackend::new();
        backend.put(b"k", b"v").unwrap();
        assert_eq!(backend.get(b"k").unwrap(), Some(b"v".to_vec()));
    }

    #[test]
    fn put_overwrites() {
        let backend = MemoryBackend::new();
        backend.put(b"k", b"v1").unwrap();
        backend.put(b"k", b"v2").unwrap();
        assert_eq!(backend.get(b"k").unwrap(), Some(b"v2".to_vec()));
    }

    #[test]
    fn delete_existing() {
        let backend = MemoryBackend::new();
        backend.put(b"k", b"v").unwrap();
        backend.delete(b"k").unwrap();
        assert_eq!(backend.get(b"k").unwrap(), None);
    }

    #[test]
    fn delete_nonexistent_is_noop() {
        let backend = MemoryBackend::new();
        backend.delete(b"nope").unwrap();
    }

    #[test]
    fn scan_prefix_filters_and_sorts() {
        let backend = MemoryBackend::new();
        backend.put(b"a:1", b"v1").unwrap();
        backend.put(b"a:2", b"v2").unwrap();
        backend.put(b"b:1", b"v3").unwrap();

        let results = backend.scan_prefix(b"a:").unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, b"a:1");
        assert_eq!(results[1].0, b"a:2");
    }

    #[test]
    fn scan_prefix_empty_result() {
        let backend = MemoryBackend::new();
        backend.put(b"abc", b"v").unwrap();
        assert!(backend.scan_prefix(b"xyz").unwrap().is_empty());
    }

    #[test]
    fn flush_ephemeral_does_not_error() {
        let backend = MemoryBackend::new();
        backend.flush().unwrap();
    }

    #[test]
    fn new_backend_has_no_path() {
        let backend = MemoryBackend::new();
        assert!(backend.path().is_none());
    }

    #[test]
    fn open_nonexistent_creates_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let backend = MemoryBackend::open(&db_path).unwrap();
        assert_eq!(backend.get(b"any").unwrap(), None);
        assert_eq!(backend.path(), Some(db_path.as_path()));
    }

    #[test]
    fn flush_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let backend = MemoryBackend::open(&db_path).unwrap();
        backend.put(b"key", b"value").unwrap();
        backend.flush().unwrap();
        assert!(db_path.exists());
    }

    #[test]
    fn persist_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        // Write data and flush
        {
            let backend = MemoryBackend::open(&db_path).unwrap();
            backend.put(b"k1", b"v1").unwrap();
            backend.put(b"k2", b"v2").unwrap();
            backend.put(b"prefix:a", b"va").unwrap();
            backend.put(b"prefix:b", b"vb").unwrap();
            backend.flush().unwrap();
        }

        // Reload and verify
        {
            let backend = MemoryBackend::open(&db_path).unwrap();
            assert_eq!(backend.get(b"k1").unwrap(), Some(b"v1".to_vec()));
            assert_eq!(backend.get(b"k2").unwrap(), Some(b"v2".to_vec()));
            assert_eq!(backend.get(b"nonexistent").unwrap(), None);

            let prefixed = backend.scan_prefix(b"prefix:").unwrap();
            assert_eq!(prefixed.len(), 2);
        }
    }

    #[test]
    fn persist_overwrite_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        // First write
        {
            let backend = MemoryBackend::open(&db_path).unwrap();
            backend.put(b"k1", b"original").unwrap();
            backend.put(b"k2", b"to_delete").unwrap();
            backend.flush().unwrap();
        }

        // Modify and flush again
        {
            let backend = MemoryBackend::open(&db_path).unwrap();
            backend.put(b"k1", b"updated").unwrap();
            backend.delete(b"k2").unwrap();
            backend.put(b"k3", b"new").unwrap();
            backend.flush().unwrap();
        }

        // Verify final state
        {
            let backend = MemoryBackend::open(&db_path).unwrap();
            assert_eq!(backend.get(b"k1").unwrap(), Some(b"updated".to_vec()));
            assert_eq!(backend.get(b"k2").unwrap(), None);
            assert_eq!(backend.get(b"k3").unwrap(), Some(b"new".to_vec()));
        }
    }

    #[test]
    fn persist_empty_database() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        {
            let backend = MemoryBackend::open(&db_path).unwrap();
            backend.flush().unwrap();
        }

        {
            let backend = MemoryBackend::open(&db_path).unwrap();
            assert_eq!(backend.get(b"any").unwrap(), None);
        }
    }

    #[test]
    fn persist_binary_keys_and_values() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        let key = vec![0u8, 1, 255, 128, 64];
        let value = vec![42u8; 512];

        {
            let backend = MemoryBackend::open(&db_path).unwrap();
            backend.put(&key, &value).unwrap();
            backend.flush().unwrap();
        }

        {
            let backend = MemoryBackend::open(&db_path).unwrap();
            assert_eq!(backend.get(&key).unwrap(), Some(value));
        }
    }

    #[test]
    fn open_corrupt_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        fs::write(&db_path, b"not valid bincode").unwrap();

        let result = MemoryBackend::open(&db_path);
        assert!(result.is_err());
        match result.unwrap_err() {
            StorageError::Serialization(msg) => {
                assert!(!msg.is_empty());
            }
            other => panic!("expected Serialization error, got: {other:?}"),
        }
    }
}
