use std::path::{Path, PathBuf};
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
/// use drevo::storage::RedbBackend;
/// use drevo::storage::StorageBackend;
///
/// let backend = RedbBackend::open("/tmp/drevo.db").unwrap();
/// backend.put(b"key", b"value").unwrap();
/// assert_eq!(backend.get(b"key").unwrap(), Some(b"value".to_vec()));
/// ```
#[derive(Debug, Clone)]
pub struct RedbBackend {
    db: Arc<Database>,
    /// Path to the underlying redb file — retained so
    /// [`StorageBackend::size_bytes`] can `stat(2)` the file without
    /// pulling the path through every call site. Set by [`Self::open`].
    path: PathBuf,
}

impl RedbBackend {
    /// Open or create a redb database at the given path.
    ///
    /// If the file does not exist, it is created. If it exists, it is opened.
    /// The `data` table is created lazily on the first write.
    ///
    /// # Errors
    ///
    /// Returns [`crate::storage::StorageError::Redb`] if redb cannot open or
    /// create the file.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let db = Database::create(&path)?;
        Ok(Self {
            db: Arc::new(db),
            path,
        })
    }

    /// Path of the underlying redb database file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl StorageBackend for RedbBackend {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(DATA_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        match table.get(key)? {
            Some(value) => Ok(Some(value.value().to_vec())),
            None => Ok(None),
        }
    }

    fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(DATA_TABLE)?;
            table.insert(key, value)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    fn delete(&self, key: &[u8]) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = match write_txn.open_table(DATA_TABLE) {
                Ok(t) => t,
                Err(redb::TableError::TableDoesNotExist(_)) => {
                    // Table doesn't exist yet — nothing to delete.
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            };
            table.remove(key)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(DATA_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };

        let mut results = Vec::new();
        let range = table.range(prefix..)?;
        for entry in range {
            let entry = entry?;
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

    /// Stat the underlying redb file. Returns `Ok(None)` only when the
    /// file disappears between open and the call (e.g. concurrent deletion
    /// in a test harness) — every other I/O failure surfaces as `Err`.
    fn size_bytes(&self) -> Result<Option<u64>> {
        match std::fs::metadata(&self.path) {
            Ok(meta) => Ok(Some(meta.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StorageError::Io(e)),
        }
    }

    /// Reclaim free pages inside the redb file.
    ///
    /// redb's compactor takes `&mut Database`. The wrapper holds the
    /// database behind `Arc` so it can be shared with the higher-level
    /// graph layer; calling `compact` therefore requires that no other
    /// `Arc<Database>` clone exists. The exclusivity is enforced via
    /// `Arc::get_mut` and surfaced through
    /// [`StorageError::CompactNotExclusive`] when the caller still holds
    /// a clone.
    ///
    /// `redb::Database::compact` itself never returns
    /// [`StorageError::CompactNotExclusive`] — only the unique-Arc
    /// pre-check does. The compactor may decline work (no free pages,
    /// outstanding savepoint, live read txn) and report that through
    /// `redb::CompactionError`, which funnels into
    /// [`StorageError::Redb`].
    fn compact(&mut self) -> Result<()> {
        let db = Arc::get_mut(&mut self.db).ok_or(StorageError::CompactNotExclusive)?;
        // redb::CompactionError → redb::Error → StorageError::Redb via
        // the existing `From` impls.
        db.compact()
            .map_err(|e| StorageError::Redb(Box::new(e.into())))?;
        Ok(())
    }
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
