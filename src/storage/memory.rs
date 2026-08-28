use std::collections::BTreeMap;
#[cfg(not(target_arch = "wasm32"))]
use std::fs;
#[cfg(not(target_arch = "wasm32"))]
use std::io::Write;
#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::storage::error::{Result, StorageError};
use crate::storage::StorageBackend;

/// In-memory storage backend backed by a `BTreeMap`.
///
/// Supports two modes:
/// - **Ephemeral** (`new()`): all data lives in memory and is lost on drop.
///   Available on all platforms including WASM.
/// - **Persistent** (`open(path)`): loads existing data from a file on
///   creation and writes a snapshot back on [`flush()`](StorageBackend::flush).
///   Not available on WASM targets (no filesystem access).
///
/// Thread-safe via an interior `RwLock`: reads ([`get`](StorageBackend::get),
/// [`scan_prefix`](StorageBackend::scan_prefix), and the snapshot read inside
/// [`flush`](StorageBackend::flush)) take a shared read lock so any number of
/// readers run concurrently, while writes ([`put`](StorageBackend::put),
/// [`put_batch`](StorageBackend::put_batch),
/// [`delete`](StorageBackend::delete)) take an exclusive write lock. This is
/// the in-memory half of Phase 13 task `00080` "read-write separation"; the
/// redb backend already gets concurrent reads for free from redb's own MVCC
/// (each `begin_read` opens an independent snapshot).
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
    data: RwLock<BTreeMap<Vec<u8>, Vec<u8>>>,
    #[cfg(not(target_arch = "wasm32"))]
    path: Option<PathBuf>,
}

impl MemoryBackend {
    /// Create a new empty in-memory backend (ephemeral, no persistence).
    pub const fn new() -> Self {
        Self {
            data: RwLock::new(BTreeMap::new()),
            #[cfg(not(target_arch = "wasm32"))]
            path: None,
        }
    }

    /// Open a persistent in-memory backend.
    ///
    /// If the file at `path` exists, its contents are loaded into memory.
    /// If the file does not exist, an empty backend is created and the file
    /// will be written on the first [`flush()`](StorageBackend::flush).
    ///
    /// # Availability
    ///
    /// Not available on `wasm32` targets. Use [`new()`](Self::new) instead.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Io`] if the file exists but cannot be read,
    /// or [`StorageError::Decode`] if the file contents are corrupt.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let data = if path.exists() {
            Self::load_from_file(&path)?
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            data: RwLock::new(data),
            path: Some(path),
        })
    }

    /// Returns the file path if this backend is persistent, `None` if ephemeral.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Load a BTreeMap from a bincode-encoded file.
    #[cfg(not(target_arch = "wasm32"))]
    fn load_from_file(path: &Path) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
        let bytes = fs::read(path)?;
        let (entries, _): (Vec<(Vec<u8>, Vec<u8>)>, _) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard())?;
        Ok(entries.into_iter().collect())
    }

    /// Write the BTreeMap to a file atomically (temp file + rename).
    #[cfg(not(target_arch = "wasm32"))]
    fn save_to_file(data: &BTreeMap<Vec<u8>, Vec<u8>>, path: &Path) -> Result<()> {
        let entries: Vec<(&Vec<u8>, &Vec<u8>)> = data.iter().collect();
        let bytes = bincode::serde::encode_to_vec(&entries, bincode::config::standard())?;

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
        let data = self.data.read().map_err(|_| StorageError::LockPoisoned)?;
        Ok(data.get(key).cloned())
    }

    fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let mut data = self.data.write().map_err(|_| StorageError::LockPoisoned)?;
        data.insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn put_batch(&self, items: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
        let mut data = self.data.write().map_err(|_| StorageError::LockPoisoned)?;
        for (key, value) in items {
            data.insert(key.clone(), value.clone());
        }
        Ok(())
    }

    fn delete(&self, key: &[u8]) -> Result<()> {
        let mut data = self.data.write().map_err(|_| StorageError::LockPoisoned)?;
        data.remove(key);
        Ok(())
    }

    fn delete_batch(&self, keys: &[Vec<u8>]) -> Result<()> {
        let mut data = self.data.write().map_err(|_| StorageError::LockPoisoned)?;
        for key in keys {
            data.remove(key);
        }
        Ok(())
    }

    fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let data = self.data.read().map_err(|_| StorageError::LockPoisoned)?;
        let results: Vec<(Vec<u8>, Vec<u8>)> = data
            .range(prefix.to_vec()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Ok(results)
    }

    fn count_prefix(&self, prefix: &[u8]) -> Result<u64> {
        let data = self.data.read().map_err(|_| StorageError::LockPoisoned)?;
        Ok(data
            .range(prefix.to_vec()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .count() as u64)
    }

    fn scan_prefix_limited(
        &self,
        prefix: &[u8],
        start_after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        use std::ops::Bound;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let data = self.data.read().map_err(|_| StorageError::LockPoisoned)?;
        // Start strictly after the cursor when it is within the prefix range;
        // otherwise begin at the prefix head (an out-of-range cursor must not
        // skip the start of the range). The `take_while` bounds the walk to
        // the prefix and `take(limit)` bounds the work — the `BTreeMap` range
        // is lazy, so we never touch more than `limit` matching entries.
        let lower = match start_after {
            Some(s) if s >= prefix => Bound::Excluded(s.to_vec()),
            _ => Bound::Included(prefix.to_vec()),
        };
        let results = data
            .range((lower, Bound::Unbounded))
            .take_while(|(k, _)| k.starts_with(prefix))
            .take(limit)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Ok(results)
    }

    fn flush(&self) -> Result<()> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(ref path) = self.path else {
                return Ok(());
            };
            let data = self.data.read().map_err(|_| StorageError::LockPoisoned)?;
            Self::save_to_file(&data, path)?;
        }
        Ok(())
    }

    /// On-disk size of the snapshot file.
    ///
    /// Returns `Ok(None)` for ephemeral backends and for persistent
    /// backends whose snapshot file does not yet exist. Otherwise returns
    /// the file's length in bytes.
    #[cfg(not(target_arch = "wasm32"))]
    fn size_bytes(&self) -> Result<Option<u64>> {
        let Some(ref path) = self.path else {
            return Ok(None);
        };
        match fs::metadata(path) {
            Ok(meta) => Ok(Some(meta.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StorageError::Io(e)),
        }
    }

    /// Sum `key + value` bytes over the live `BTreeMap` directly, without
    /// cloning the rows the way the default `scan_prefix`-based impl would.
    fn content_bytes(&self) -> Result<u64> {
        let data = self.data.read().map_err(|_| StorageError::LockPoisoned)?;
        Ok(data.iter().map(|(k, v)| (k.len() + v.len()) as u64).sum())
    }

    /// Snapshot-style compaction for the persistent path. Re-serialises
    /// the live `BTreeMap` to disk — naturally drops bytes for keys that
    /// were deleted between the previous flush and now. No-op for
    /// ephemeral backends.
    fn compact(&mut self) -> Result<()> {
        self.flush()
    }
}

// ---------------------------------------------------------------------------
// Minimal temp-file helper (avoids adding a runtime dependency on `tempfile`)
// ---------------------------------------------------------------------------

/// A temporary file that deletes itself on drop unless defused.
#[cfg(not(target_arch = "wasm32"))]
struct TempFile {
    file: fs::File,
    path: PathBuf,
    defused: bool,
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.defused {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Create a temporary file in `dir` with a unique name.
#[cfg(not(target_arch = "wasm32"))]
fn tempfile_in(dir: &Path) -> Result<TempFile> {
    use std::time::SystemTime;

    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    let name = format!(".drevo-tmp-{pid}-{stamp}");
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
    fn scan_prefix_limited_bounds_and_paginates() {
        let backend = MemoryBackend::new();
        for i in 0..10u8 {
            backend.put(format!("a:{i}").as_bytes(), b"v").unwrap();
        }
        backend.put(b"b:0", b"other").unwrap();

        // limit bounds the page; keys are ascending; the other prefix is excluded.
        let p1 = backend.scan_prefix_limited(b"a:", None, 3).unwrap();
        assert_eq!(p1.len(), 3);
        assert_eq!(p1[0].0, b"a:0");
        assert_eq!(p1[2].0, b"a:2");

        // Cursor = last key of the previous page → next page starts strictly after.
        let p2 = backend
            .scan_prefix_limited(b"a:", Some(&p1[2].0), 3)
            .unwrap();
        assert_eq!(p2[0].0, b"a:3");
        assert_eq!(p2.len(), 3);

        // A limit larger than the remainder returns only what's left, no `b:`.
        let tail = backend
            .scan_prefix_limited(b"a:", Some(b"a:7"), 100)
            .unwrap();
        assert_eq!(tail.len(), 2, "a:8 and a:9");
        assert!(tail.iter().all(|(k, _)| k.starts_with(b"a:")));

        // limit 0 is empty; an out-of-range (too-small) cursor still starts at head.
        assert!(backend
            .scan_prefix_limited(b"a:", None, 0)
            .unwrap()
            .is_empty());
        let head = backend.scan_prefix_limited(b"a:", Some(b"a"), 2).unwrap();
        assert_eq!(
            head[0].0, b"a:0",
            "cursor before the prefix does not skip the head"
        );
    }

    #[test]
    fn flush_ephemeral_does_not_error() {
        let backend = MemoryBackend::new();
        backend.flush().unwrap();
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn new_backend_has_no_path() {
        let backend = MemoryBackend::new();
        assert!(backend.path().is_none());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn open_nonexistent_creates_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let backend = MemoryBackend::open(&db_path).unwrap();
        assert_eq!(backend.get(b"any").unwrap(), None);
        assert_eq!(backend.path(), Some(db_path.as_path()));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn flush_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let backend = MemoryBackend::open(&db_path).unwrap();
        backend.put(b"key", b"value").unwrap();
        backend.flush().unwrap();
        assert!(db_path.exists());
    }

    #[cfg(not(target_arch = "wasm32"))]
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

    #[cfg(not(target_arch = "wasm32"))]
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

    #[cfg(not(target_arch = "wasm32"))]
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

    #[cfg(not(target_arch = "wasm32"))]
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

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn open_corrupt_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        fs::write(&db_path, b"not valid bincode").unwrap();

        let result = MemoryBackend::open(&db_path);
        assert!(result.is_err());
        match result.unwrap_err() {
            StorageError::Decode(_) => {}
            other => panic!("expected Decode error, got: {other:?}"),
        }
    }

    #[test]
    fn lock_poisoning_maps_to_lock_poisoned() {
        use std::sync::Arc;
        use std::thread;

        let backend = Arc::new(MemoryBackend::new());
        backend.put(b"k", b"v").unwrap();

        // Spawn a thread that takes the write lock and panics, poisoning it.
        let poisoner = Arc::clone(&backend);
        let handle = thread::spawn(move || {
            let _guard = poisoner.data.write().expect("first lock must succeed");
            panic!("poisoning the rwlock on purpose");
        });
        let _ = handle.join();
        assert!(backend.data.is_poisoned(), "rwlock must be poisoned");

        // Every data accessor must surface LockPoisoned, not Backend(_).
        // `flush()` short-circuits on an ephemeral backend before taking the
        // lock, so it is covered separately by the persistent path below.
        for result in [
            backend.get(b"k").map(|_| ()),
            backend.put(b"k", b"v2"),
            backend.delete(b"k"),
            backend.scan_prefix(b"").map(|_| ()),
        ] {
            match result {
                Err(StorageError::LockPoisoned) => {}
                Err(other) => panic!("expected LockPoisoned, got: {other:?}"),
                Ok(()) => panic!("expected LockPoisoned, got Ok"),
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn lock_poisoning_maps_to_lock_poisoned_on_flush() {
        use std::sync::Arc;
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("poison.db");
        let backend = Arc::new(MemoryBackend::open(&db_path).unwrap());
        backend.put(b"k", b"v").unwrap();

        let poisoner = Arc::clone(&backend);
        let handle = thread::spawn(move || {
            let _guard = poisoner.data.write().expect("first lock must succeed");
            panic!("poisoning the rwlock on purpose");
        });
        let _ = handle.join();

        match backend.flush() {
            Err(StorageError::LockPoisoned) => {}
            Err(other) => panic!("expected LockPoisoned, got: {other:?}"),
            Ok(()) => panic!("expected LockPoisoned, got Ok"),
        }
    }

    // ------------------------------------------------------------------
    // Read-write separation (Phase 13 task 00080)
    // ------------------------------------------------------------------

    /// The defining property of the `RwLock` swap: multiple readers hold the
    /// lock at the same time. With the previous `Mutex` this deadlocked —
    /// a second `lock()` while the first guard is alive blocks forever.
    #[test]
    fn concurrent_readers_share_the_lock() {
        let backend = MemoryBackend::new();
        backend.put(b"k", b"v").unwrap();

        let first = backend.data.read().expect("first read guard");
        // A second shared read must succeed *while the first is still held*.
        let second = backend
            .data
            .try_read()
            .expect("second concurrent read guard must be granted");

        assert_eq!(first.get(b"k".as_slice()), Some(&b"v".to_vec()));
        assert_eq!(second.get(b"k".as_slice()), Some(&b"v".to_vec()));
    }

    /// The exclusion half: a held write lock blocks every reader. `try_read`
    /// must fail (would-block) rather than observe a half-applied mutation.
    #[test]
    fn writer_excludes_readers() {
        let backend = MemoryBackend::new();

        let writer = backend.data.write().expect("write guard");
        assert!(
            backend.data.try_read().is_err(),
            "a reader must not acquire the lock while a writer holds it"
        );
        drop(writer);

        // Once the writer releases, readers flow again.
        assert!(backend.data.try_read().is_ok());
    }

    /// End-to-end through the public `StorageBackend` API: many threads read
    /// the same shared backend at once and every read observes the committed
    /// value. Exercises the `Send + Sync` contract under real contention.
    #[test]
    fn many_threads_read_concurrently() {
        use std::sync::Arc;
        use std::thread;

        let backend = Arc::new(MemoryBackend::new());
        for i in 0..100u32 {
            backend
                .put(format!("k:{i}").as_bytes(), &i.to_le_bytes())
                .unwrap();
        }

        let mut handles = Vec::new();
        for _ in 0..16 {
            let b = Arc::clone(&backend);
            handles.push(thread::spawn(move || {
                for _ in 0..50 {
                    for i in 0..100u32 {
                        let got = b.get(format!("k:{i}").as_bytes()).unwrap();
                        assert_eq!(got, Some(i.to_le_bytes().to_vec()));
                    }
                }
            }));
        }
        for h in handles {
            h.join().expect("reader thread must not panic");
        }
    }

    /// A panic *inside a reader* must NOT poison the lock. `std::sync::RwLock`
    /// only poisons on a write-mode panic, so a misbehaving read query cannot
    /// take the whole backend down with it — surviving readers and writers
    /// keep working. This is a real reliability win of the read-write split,
    /// so pin it with a test.
    #[test]
    fn reader_panic_does_not_poison() {
        use std::sync::Arc;
        use std::thread;

        let backend = Arc::new(MemoryBackend::new());
        backend.put(b"k", b"v").unwrap();

        let reader = Arc::clone(&backend);
        let handle = thread::spawn(move || {
            let _guard = reader.data.read().expect("read guard");
            panic!("a read query blew up");
        });
        let _ = handle.join();

        assert!(
            !backend.data.is_poisoned(),
            "a reader panic must not poison the lock"
        );
        // The backend is still fully usable afterwards.
        assert_eq!(backend.get(b"k").unwrap(), Some(b"v".to_vec()));
        backend.put(b"k2", b"v2").unwrap();
        assert_eq!(backend.get(b"k2").unwrap(), Some(b"v2".to_vec()));
    }
}
