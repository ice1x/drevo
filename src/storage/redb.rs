use std::path::Path;
use std::sync::Arc;

use redb::{Database, TableDefinition};

use crate::storage::error::{Result, StorageError};
use crate::storage::StorageBackend;

/// Table definition for the single key-value table.
const DATA_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("data");

/// ACID-compliant storage backend backed by [redb](https://github.com/cberner/redb).
///
/// Each `get`, `put`, `delete`, and `scan_prefix` call runs inside its own
/// redb transaction, so every operation is durable once it returns.
///
/// Thread-safe: the inner `Database` is wrapped in an `Arc` and redb handles
/// concurrent access internally.
///
/// # Example
///
/// ```no_run
/// use graphnote_db::storage::RedbBackend;
/// use graphnote_db::storage::StorageBackend;
///
/// let backend = RedbBackend::open("/tmp/graphnote.db").unwrap();
/// backend.put(b"key", b"value").unwrap();
/// assert_eq!(backend.get(b"key").unwrap(), Some(b"value".to_vec()));
/// ```
#[derive(Debug, Clone)]
pub struct RedbBackend {
    db: Arc<Database>,
}

impl RedbBackend {
    /// Open or create a redb database at the given path.
    ///
    /// If the file does not exist, it is created. If it exists, it is opened.
    /// The `data` table is created lazily on the first write.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Backend`] if redb cannot open or create the file.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let db = Database::create(path.as_ref()).map_err(map_redb_err)?;
        Ok(Self { db: Arc::new(db) })
    }
}

impl StorageBackend for RedbBackend {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let read_txn = self.db.begin_read().map_err(map_redb_err)?;
        let table = match read_txn.open_table(DATA_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(StorageError::Backend(e.to_string())),
        };
        match table.get(key).map_err(map_redb_err)? {
            Some(value) => Ok(Some(value.value().to_vec())),
            None => Ok(None),
        }
    }

    fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let write_txn = self.db.begin_write().map_err(map_redb_err)?;
        {
            let mut table = write_txn.open_table(DATA_TABLE).map_err(map_redb_err)?;
            table.insert(key, value).map_err(map_redb_err)?;
        }
        write_txn.commit().map_err(map_redb_err)?;
        Ok(())
    }

    fn delete(&self, key: &[u8]) -> Result<()> {
        let write_txn = self.db.begin_write().map_err(map_redb_err)?;
        {
            let mut table = match write_txn.open_table(DATA_TABLE) {
                Ok(t) => t,
                Err(redb::TableError::TableDoesNotExist(_)) => {
                    // Table doesn't exist yet — nothing to delete.
                    return Ok(());
                }
                Err(e) => return Err(StorageError::Backend(e.to_string())),
            };
            table.remove(key).map_err(map_redb_err)?;
        }
        write_txn.commit().map_err(map_redb_err)?;
        Ok(())
    }

    fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let read_txn = self.db.begin_read().map_err(map_redb_err)?;
        let table = match read_txn.open_table(DATA_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(StorageError::Backend(e.to_string())),
        };

        let mut results = Vec::new();
        let range = table.range(prefix..).map_err(map_redb_err)?;
        for entry in range {
            let entry = entry.map_err(map_redb_err)?;
            let key = entry.0.value().to_vec();
            if !key.starts_with(prefix) {
                break;
            }
            let value = entry.1.value().to_vec();
            results.push((key, value));
        }
        Ok(results)
    }

    fn flush(&self) -> Result<()> {
        // redb commits are durable — each put/delete already commits.
        // compact() is available but expensive; flush is a no-op.
        Ok(())
    }
}

/// Map any redb error into a `StorageError::Backend`.
fn map_redb_err(e: impl std::fmt::Display) -> StorageError {
    StorageError::Backend(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_temp_db() -> (RedbBackend, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.redb");
        let backend = RedbBackend::open(&db_path).unwrap();
        (backend, dir)
    }

    #[test]
    fn new_backend_is_empty() {
        let (backend, _dir) = open_temp_db();
        assert_eq!(backend.get(b"any").unwrap(), None);
    }

    #[test]
    fn put_and_get() {
        let (backend, _dir) = open_temp_db();
        backend.put(b"k", b"v").unwrap();
        assert_eq!(backend.get(b"k").unwrap(), Some(b"v".to_vec()));
    }

    #[test]
    fn put_overwrites() {
        let (backend, _dir) = open_temp_db();
        backend.put(b"k", b"v1").unwrap();
        backend.put(b"k", b"v2").unwrap();
        assert_eq!(backend.get(b"k").unwrap(), Some(b"v2".to_vec()));
    }

    #[test]
    fn delete_existing() {
        let (backend, _dir) = open_temp_db();
        backend.put(b"k", b"v").unwrap();
        backend.delete(b"k").unwrap();
        assert_eq!(backend.get(b"k").unwrap(), None);
    }

    #[test]
    fn delete_nonexistent_is_noop() {
        let (backend, _dir) = open_temp_db();
        backend.delete(b"nope").unwrap();
    }

    #[test]
    fn scan_prefix_filters_and_sorts() {
        let (backend, _dir) = open_temp_db();
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
        let (backend, _dir) = open_temp_db();
        backend.put(b"abc", b"v").unwrap();
        assert!(backend.scan_prefix(b"xyz").unwrap().is_empty());
    }

    #[test]
    fn scan_prefix_empty_store() {
        let (backend, _dir) = open_temp_db();
        assert!(backend.scan_prefix(b"any").unwrap().is_empty());
    }

    #[test]
    fn flush_does_not_error() {
        let (backend, _dir) = open_temp_db();
        backend.put(b"key", b"value").unwrap();
        backend.flush().unwrap();
    }

    #[test]
    fn empty_key_and_value() {
        let (backend, _dir) = open_temp_db();
        backend.put(b"", b"").unwrap();
        assert_eq!(backend.get(b"").unwrap(), Some(b"".to_vec()));
    }

    #[test]
    fn binary_key_and_value() {
        let (backend, _dir) = open_temp_db();
        let key = vec![0u8, 1, 255, 128, 64];
        let value = vec![42u8; 512];
        backend.put(&key, &value).unwrap();
        assert_eq!(backend.get(&key).unwrap(), Some(value));
    }

    #[test]
    fn data_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("persist.redb");

        {
            let backend = RedbBackend::open(&db_path).unwrap();
            backend.put(b"k1", b"v1").unwrap();
            backend.put(b"k2", b"v2").unwrap();
        }

        let backend = RedbBackend::open(&db_path).unwrap();
        assert_eq!(backend.get(b"k1").unwrap(), Some(b"v1".to_vec()));
        assert_eq!(backend.get(b"k2").unwrap(), Some(b"v2".to_vec()));
    }

    #[test]
    fn delete_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("del_persist.redb");

        {
            let backend = RedbBackend::open(&db_path).unwrap();
            backend.put(b"k1", b"v1").unwrap();
            backend.put(b"k2", b"v2").unwrap();
            backend.delete(b"k1").unwrap();
        }

        let backend = RedbBackend::open(&db_path).unwrap();
        assert_eq!(backend.get(b"k1").unwrap(), None);
        assert_eq!(backend.get(b"k2").unwrap(), Some(b"v2".to_vec()));
    }

    #[test]
    fn scan_prefix_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("scan_persist.redb");

        {
            let backend = RedbBackend::open(&db_path).unwrap();
            backend.put(b"e:42:likes:1", b"d1").unwrap();
            backend.put(b"e:42:likes:2", b"d2").unwrap();
            backend.put(b"e:42:knows:3", b"d3").unwrap();
            backend.put(b"e:99:likes:1", b"other").unwrap();
        }

        let backend = RedbBackend::open(&db_path).unwrap();
        let results = backend.scan_prefix(b"e:42:").unwrap();
        assert_eq!(results.len(), 3);
        for (k, _) in &results {
            assert!(k.starts_with(b"e:42:"));
        }
    }

    #[test]
    fn trait_object_works() {
        let (backend, _dir) = open_temp_db();
        let backend: Arc<dyn StorageBackend> = Arc::new(backend);
        backend.put(b"key", b"value").unwrap();
        assert_eq!(backend.get(b"key").unwrap(), Some(b"value".to_vec()));
    }
}
