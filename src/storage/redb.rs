use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard};

use redb::{Database, ReadableTableMetadata, TableDefinition};

use crate::storage::error::{Result, StorageError};
use crate::storage::StorageBackend;

/// Table definition for the single key-value table.
const DATA_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("data");

/// Metadata table, kept separate from the user-facing [`DATA_TABLE`] so
/// storage-level bookkeeping (currently just the on-disk format version)
/// never collides with graph keys and is invisible to `scan_prefix`.
const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

/// Key under [`META_TABLE`] holding the on-disk format version as a
/// UTF-8 `"MAJOR.MINOR"` string (e.g. `"1.0"`).
const FORMAT_VERSION_KEY: &str = "format_version";

/// Current on-disk format **major** version this build writes and reads.
///
/// Compatibility rule (semver-style): a build opens any file whose major
/// version is `<= FORMAT_MAJOR`. A file with a greater major was written by
/// a newer, layout-incompatible drevo and is refused with
/// [`StorageError::IncompatibleFormat`] instead of being silently misread.
/// Files predating format versioning carry no marker and are treated as the
/// original `1.0` format (then stamped on first open). This is the on-disk
/// durability guarantee for the agent-memory-graph file (issue #48).
///
/// **v2** (#243 slice 2): the adjacency index moved to the kind-in-key layout
/// `out:{from}:{kind}:{edge}`. A v1 file opens (its major `1 <= 2`), but the
/// graph layer detects the old adjacency layout and refuses it with
/// [`crate::error::DrevoError::NeedsMigration`] until
/// [`crate::db::Drevo::migrate_adjacency`] rewrites the index and re-stamps.
pub const FORMAT_MAJOR: u32 = 2;

/// Current on-disk format **minor** version. Bumped for additive,
/// backward-compatible layout changes within a major; purely informational
/// for the compatibility check (any minor of a compatible major opens).
pub const FORMAT_MINOR: u32 = 0;

/// Parse a `"MAJOR.MINOR"` marker string into its numeric parts.
/// Returns `None` for anything that is not two dot-separated `u32`s.
fn parse_format_version(raw: &str) -> Option<(u32, u32)> {
    let (major, minor) = raw.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

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
#[derive(Debug)]
pub struct RedbBackend {
    /// The live redb handle, behind an `RwLock` so an online
    /// [`shrink_rebuild`](StorageBackend::shrink_rebuild) can take the write
    /// lock, quiesce every in-flight operation, and hot-swap the handle onto a
    /// freshly-rebuilt file — no exclusive ownership, no restart. Ordinary
    /// operations take the shared read lock (see [`Self::db`]) and hold it for
    /// the whole transaction (begin → commit) so a swap can never land between
    /// a write's `begin_write` and its `commit`.
    ///
    /// The inner `Arc` is retained (rather than a bare `Database`) purely so
    /// [`compact`](StorageBackend::compact) can keep enforcing its
    /// unique-owner precondition via `Arc::get_mut`.
    db: RwLock<Arc<Database>>,
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
        // Roll forward/back any shrink that a crash interrupted before opening.
        Self::recover_interrupted_shrink(&path)?;
        let db = Database::create(&path)?;
        let backend = Self {
            db: RwLock::new(Arc::new(db)),
            path,
        };
        backend.check_or_stamp_format_version()?;
        Ok(backend)
    }

    /// Recover from a crash mid-[`shrink_rebuild`](StorageBackend::shrink_rebuild).
    ///
    /// The shrink renames the original file to `<name>.backup.redb`, installs the
    /// compacted copy at the live path, self-verifies it, and only then deletes
    /// the backup. If the process dies mid-way it can leave a stale
    /// `.shrinking.redb` temp and/or a `.backup.redb`:
    ///
    /// * live file **missing** but backup present → the crash landed between the
    ///   two renames; restore the backup as the live file (no data is lost).
    /// * live file **present** and backup present → the rebuild was installed and
    ///   is durable (redb committed it before the rename), so the backup is stale
    ///   and is discarded.
    ///
    /// A leftover `.shrinking.redb` temp (an incomplete rebuild) is always
    /// discarded. Idempotent and cheap when nothing was interrupted.
    fn recover_interrupted_shrink(path: &Path) -> Result<()> {
        let tmp = path.with_extension("shrinking.redb");
        if tmp.exists() {
            std::fs::remove_file(&tmp)?;
        }
        let backup = path.with_extension("backup.redb");
        if backup.exists() {
            if path.exists() {
                // Rebuild already installed and durable → backup is stale.
                std::fs::remove_file(&backup)?;
            } else {
                // Interrupted between the renames → put the original back.
                std::fs::rename(&backup, path)?;
            }
        }
        Ok(())
    }

    /// Count `DATA_TABLE` + `META_TABLE` rows in `db` and confirm they match the
    /// `(data, meta)` counts streamed into a [`shrink_rebuild`](StorageBackend::shrink_rebuild)
    /// copy — the post-swap self-diagnostic that decides whether the automatic
    /// backup can be discarded. A table absent from the rebuild counts as zero
    /// rows (it was empty in the source too).
    fn verify_row_counts(db: &Database, expected_data: u64, expected_meta: u64) -> Result<()> {
        let read_txn = db.begin_read()?;
        let data = match read_txn.open_table(DATA_TABLE) {
            Ok(t) => t.len()?,
            Err(redb::TableError::TableDoesNotExist(_)) => 0,
            Err(e) => return Err(e.into()),
        };
        let meta = match read_txn.open_table(META_TABLE) {
            Ok(t) => t.len()?,
            Err(redb::TableError::TableDoesNotExist(_)) => 0,
            Err(e) => return Err(e.into()),
        };
        if (data, meta) != (expected_data, expected_meta) {
            return Err(StorageError::ShrinkVerificationFailed {
                expected: (expected_data, expected_meta),
                got: (data, meta),
            });
        }
        Ok(())
    }

    /// Path of the underlying redb database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Acquire the shared read guard on the live database handle.
    ///
    /// Every ordinary operation goes through this and **holds the returned guard
    /// for the whole transaction** (through `commit`), so an online
    /// [`shrink_rebuild`](StorageBackend::shrink_rebuild) — which takes the
    /// `write` lock — can never swap the handle between a write's `begin_write`
    /// and its `commit`. A poisoned lock is recovered (the guarded `Arc` is
    /// never left in a torn state), mirroring the crate's lock-poisoning policy.
    fn db(&self) -> RwLockReadGuard<'_, Arc<Database>> {
        self.db.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Read the on-disk format version marker, if the file carries one.
    ///
    /// Returns `Ok(None)` for a file written before format versioning
    /// existed (no marker), and `Ok(Some((major, minor)))` otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Redb`] on an underlying redb failure, or
    /// [`StorageError::IncompatibleFormat`] if a marker is present but not a
    /// parseable `MAJOR.MINOR` string.
    pub fn format_version(&self) -> Result<Option<(u32, u32)>> {
        match self.read_raw_format_version()? {
            None => Ok(None),
            Some(raw) => match parse_format_version(&raw) {
                Some(v) => Ok(Some(v)),
                None => Err(StorageError::IncompatibleFormat {
                    found: raw,
                    supported_major: FORMAT_MAJOR,
                }),
            },
        }
    }

    /// Read the raw marker string from [`META_TABLE`], or `None` when the
    /// table or key is absent (a fresh or pre-versioning file).
    fn read_raw_format_version(&self) -> Result<Option<String>> {
        let db = self.db();
        let read_txn = db.begin_read()?;
        let table = match read_txn.open_table(META_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        match table.get(FORMAT_VERSION_KEY)? {
            Some(v) => Ok(Some(String::from_utf8_lossy(v.value()).into_owned())),
            None => Ok(None),
        }
    }

    /// Enforce the on-disk format-version compatibility rule on open.
    ///
    /// - Marker present and major `> FORMAT_MAJOR` (or unparseable) →
    ///   [`StorageError::IncompatibleFormat`].
    /// - Marker present and major `<= FORMAT_MAJOR` → accept.
    /// - Marker absent (fresh or pre-versioning file) → stamp the current
    ///   `FORMAT_MAJOR.FORMAT_MINOR` so the file is versioned from now on.
    fn check_or_stamp_format_version(&self) -> Result<()> {
        match self.read_raw_format_version()? {
            Some(raw) => match parse_format_version(&raw) {
                Some((major, _minor)) if major <= FORMAT_MAJOR => Ok(()),
                _ => Err(StorageError::IncompatibleFormat {
                    found: raw,
                    supported_major: FORMAT_MAJOR,
                }),
            },
            None => self.stamp_format_version(),
        }
    }

    /// Write the current format version marker into [`META_TABLE`].
    fn stamp_format_version(&self) -> Result<()> {
        let marker = format!("{FORMAT_MAJOR}.{FORMAT_MINOR}");
        let db = self.db();
        let write_txn = db.begin_write()?;
        {
            let mut table = write_txn.open_table(META_TABLE)?;
            table.insert(FORMAT_VERSION_KEY, marker.as_bytes())?;
        }
        write_txn.commit()?;
        Ok(())
    }
}

/// Cloning shares the underlying `Arc<Database>` (a new `RwLock` wrapping a
/// clone of the same handle), preserving the pre-`RwLock` semantics: a cloned
/// backend keeps [`compact`](StorageBackend::compact)'s unique-owner check
/// meaningful (outstanding clones ⇒ `Arc::get_mut` fails ⇒
/// [`StorageError::CompactNotExclusive`]). Production never clones a backend;
/// this exists for the compaction tests and any embedder that keeps a copy.
///
/// Note: an online [`shrink_rebuild`](StorageBackend::shrink_rebuild) swaps only
/// the swapping backend's own `Arc`; a pre-existing clone would keep pointing at
/// the old handle. That is fine because the graph layer holds exactly one
/// backend behind its `Box<dyn StorageBackend>`.
impl Clone for RedbBackend {
    fn clone(&self) -> Self {
        Self {
            db: RwLock::new(Arc::clone(&self.db())),
            path: self.path.clone(),
        }
    }
}

impl StorageBackend for RedbBackend {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let db = self.db();
        let read_txn = db.begin_read()?;
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
        let db = self.db();
        let write_txn = db.begin_write()?;
        {
            let mut table = write_txn.open_table(DATA_TABLE)?;
            table.insert(key, value)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    fn put_batch(&self, items: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let db = self.db();
        let write_txn = db.begin_write()?;
        {
            let mut table = write_txn.open_table(DATA_TABLE)?;
            for (key, value) in items {
                table.insert(key.as_slice(), value.as_slice())?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }

    fn delete(&self, key: &[u8]) -> Result<()> {
        let db = self.db();
        let write_txn = db.begin_write()?;
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

    fn delete_batch(&self, keys: &[Vec<u8>]) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        let db = self.db();
        let write_txn = db.begin_write()?;
        {
            let mut table = match write_txn.open_table(DATA_TABLE) {
                Ok(t) => t,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
                Err(e) => return Err(e.into()),
            };
            for key in keys {
                table.remove(key.as_slice())?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }

    fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let db = self.db();
        let read_txn = db.begin_read()?;
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
        let db = self.db();
        let read_txn = db.begin_read()?;
        let table = match read_txn.open_table(DATA_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };

        // Begin strictly after the cursor when it lies within the prefix range;
        // otherwise at the prefix head. The redb range iterator is lazy, so the
        // `break` after `limit` matches stops the scan without materialising
        // the rest of a supernode's adjacency.
        let lower: Bound<&[u8]> = match start_after {
            Some(s) if s >= prefix => Bound::Excluded(s),
            _ => Bound::Included(prefix),
        };
        let mut results = Vec::new();
        let range = table.range::<&[u8]>((lower, Bound::Unbounded))?;
        for entry in range {
            let entry = entry?;
            let key = entry.0.value().to_vec();
            if !key.starts_with(prefix) {
                break;
            }
            results.push((key, entry.1.value().to_vec()));
            if results.len() >= limit {
                break;
            }
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

    /// Sum `key + value` bytes across the whole table with a lazy range
    /// iterator, so a large FTS-heavy keyspace is measured without ever
    /// materialising it (unlike the default `scan_prefix`-based impl).
    fn content_bytes(&self) -> Result<u64> {
        let db = self.db();
        let read_txn = db.begin_read()?;
        let table = match read_txn.open_table(DATA_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        // Full-range lazy scan; matches the inherent `range` used by
        // `scan_prefix` (avoids pulling `ReadableTable` into scope for `iter`).
        use std::ops::Bound;
        let mut total: u64 = 0;
        let lower: Bound<&[u8]> = Bound::Unbounded;
        let range = table.range::<&[u8]>((lower, Bound::Unbounded))?;
        for entry in range {
            let entry = entry?;
            total += (entry.0.value().len() + entry.1.value().len()) as u64;
        }
        Ok(total)
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
        // `&mut self` gives exclusive access to the `RwLock` without locking, so
        // the unique-owner precondition is checked exactly as before: `compact`
        // still requires that no other `Arc<Database>` clone exists.
        let arc = self.db.get_mut().unwrap_or_else(|e| e.into_inner());
        let db = Arc::get_mut(arc).ok_or(StorageError::CompactNotExclusive)?;
        // redb::CompactionError → redb::Error → StorageError::Redb via
        // the existing `From` impls.
        db.compact()
            .map_err(|e| StorageError::Redb(Box::new(e.into())))?;
        Ok(())
    }

    fn format_major(&self) -> Result<Option<u32>> {
        Ok(self.format_version()?.map(|(major, _minor)| major))
    }

    fn set_format_version(&self, major: u32, minor: u32) -> Result<()> {
        let marker = format!("{major}.{minor}");
        let db = self.db();
        let write_txn = db.begin_write()?;
        {
            let mut table = write_txn.open_table(META_TABLE)?;
            table.insert(FORMAT_VERSION_KEY, marker.as_bytes())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    fn shrink_rebuild(&self) -> Result<Option<(u64, u64)>> {
        // Exclusive lock: quiesce every in-flight operation (each holds the
        // shared read guard through its `commit`), so nothing can write to the
        // old file after we snapshot it and nothing observes the swap mid-flight.
        let mut guard = self.db.write().unwrap_or_else(|e| e.into_inner());

        let before = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);

        // Build the compact copy beside the live file; discard any stale temp or
        // backup a crashed prior run left behind first.
        let tmp = self.path.with_extension("shrinking.redb");
        let backup = self.path.with_extension("backup.redb");
        for stale in [&tmp, &backup] {
            if stale.exists() {
                std::fs::remove_file(stale)?;
            }
        }

        // Copy every row of BOTH tables verbatim — the graph never migrates.
        // DATA_TABLE holds nodes/edges/indexes AND drevo's `meta:` keys
        // (counters, semantic registry, fts format marker); META_TABLE holds
        // the redb format-version marker. Streaming via redb range iterators
        // keeps peak memory bounded regardless of database size. Count the rows
        // streamed so the rebuild can be self-verified before its backup is
        // discarded.
        // Keep `fresh` alive past the streaming block: it is the compacted
        // rebuild, and installing this very handle (rather than reopening the
        // file) is what keeps the result small — a `Database::open` of a fresh
        // file re-lays-out its regions and re-grows it.
        let fresh = Database::create(&tmp)?;
        let (src_data, src_meta) = {
            let read_txn = guard.begin_read()?;
            let write_txn = fresh.begin_write()?;
            let mut data_rows = 0u64;
            let mut meta_rows = 0u64;
            match read_txn.open_table(DATA_TABLE) {
                Ok(src) => {
                    let mut dst = write_txn.open_table(DATA_TABLE)?;
                    for entry in src.range::<&[u8]>(..)? {
                        let entry = entry?;
                        dst.insert(entry.0.value(), entry.1.value())?;
                        data_rows += 1;
                    }
                }
                Err(redb::TableError::TableDoesNotExist(_)) => {}
                Err(e) => return Err(e.into()),
            }
            match read_txn.open_table(META_TABLE) {
                Ok(src) => {
                    let mut dst = write_txn.open_table(META_TABLE)?;
                    for entry in src.range::<&str>(..)? {
                        let entry = entry?;
                        dst.insert(entry.0.value(), entry.1.value())?;
                        meta_rows += 1;
                    }
                }
                Err(redb::TableError::TableDoesNotExist(_)) => {}
                Err(e) => return Err(e.into()),
            }
            write_txn.commit()?;
            (data_rows, meta_rows)
        };

        // SELF-DIAGNOSTIC — run it *before* we touch the original. The compacted
        // rebuild must hold exactly the rows we streamed; a mismatch means a
        // duplicate-key collision or a short read silently dropped/merged data.
        // Verifying here means a bad rebuild is discarded with the live file still
        // completely untouched — the safest possible failure.
        if let Err(verify_err) = Self::verify_row_counts(&fresh, src_data, src_meta) {
            drop(fresh);
            let _ = std::fs::remove_file(&tmp);
            return Err(verify_err);
        }

        // AUTO-BACKUP: move the verified-good original aside instead of
        // overwriting it. This is a rename, not a copy — same inode, so it is
        // instant and costs no extra disk (the original's blocks live on under
        // `backup` until it is deleted). Peak disk is unchanged: original-as-backup
        // + rebuild-as-live is the same 2× footprint the build already used. If a
        // crash strikes between these two renames, `recover_interrupted_shrink`
        // rolls the backup forward on the next open.
        std::fs::rename(&self.path, &backup)?;
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            // Could not install the rebuild → put the original straight back.
            std::fs::rename(&backup, &self.path)?;
            return Err(e.into());
        }

        // Swap in the rebuilt handle: `fresh`'s fd follows the inode through the
        // rename, and dropping the old `Arc<Database>` closes the pre-shrink file.
        // The write lock is held throughout, so there is no unavailability window.
        *guard = Arc::new(fresh);

        // Rebuild verified and installed → the automatic backup is no longer
        // needed. (A failure to unlink it is non-fatal: the next open would just
        // treat a live-file-present backup as stale and discard it.)
        std::fs::remove_file(&backup)?;

        let after = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        Ok(Some((before, after)))
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
    fn shrink_rebuild_reclaims_bloat_and_preserves_data() {
        let (backend, _dir) = open_temp_db();
        // Manufacture reclaimable high-water bloat: write many large values,
        // then delete all but a handful. redb frees the pages to its internal
        // freelist but never shrinks the file on its own.
        let big = vec![b'x'; 4096];
        for i in 0..2000u32 {
            backend.put(format!("k{i:04}").as_bytes(), &big).unwrap();
        }
        for i in 10..2000u32 {
            backend.delete(format!("k{i:04}").as_bytes()).unwrap();
        }
        let before = backend.size_bytes().unwrap().unwrap();

        let (b, a) = backend
            .shrink_rebuild()
            .unwrap()
            .expect("a disk-backed store performs an online rebuild");
        assert_eq!(
            b, before,
            "reported before-size must match the pre-shrink file"
        );
        assert!(
            a < before,
            "file must actually shrink: after {a} !< before {before}"
        );
        assert_eq!(
            backend.size_bytes().unwrap().unwrap(),
            a,
            "the live handle now stats the rebuilt file"
        );

        // Data survives verbatim: the 10 kept keys are intact, the deleted ones
        // stay gone, and the redb format marker (META_TABLE) came across.
        for i in 0..10u32 {
            assert_eq!(
                backend.get(format!("k{i:04}").as_bytes()).unwrap(),
                Some(big.clone()),
                "surviving key k{i:04} must be preserved"
            );
        }
        assert_eq!(
            backend.get(b"k0100").unwrap(),
            None,
            "deleted key stays gone"
        );
        assert_eq!(
            backend.format_version().unwrap(),
            Some((FORMAT_MAJOR, FORMAT_MINOR)),
            "format marker preserved across the rebuild"
        );

        // The swapped-in handle is fully writable afterwards.
        backend.put(b"after", b"ok").unwrap();
        assert_eq!(backend.get(b"after").unwrap(), Some(b"ok".to_vec()));
    }

    #[test]
    fn shrink_rebuild_leaves_no_backup_or_temp_on_success() {
        let (backend, _dir) = open_temp_db();
        backend.put(b"k1", b"v1").unwrap();
        backend.put(b"k2", b"v2").unwrap();
        backend
            .shrink_rebuild()
            .unwrap()
            .expect("disk-backed rebuild");
        // Data intact through the backup-verify-swap.
        assert_eq!(backend.get(b"k1").unwrap(), Some(b"v1".to_vec()));
        assert_eq!(backend.get(b"k2").unwrap(), Some(b"v2".to_vec()));
        // The automatic backup and the temp are gone once the self-diagnostic
        // passed — a clean shrink leaves only the live file.
        assert!(
            !backend.path().with_extension("backup.redb").exists(),
            "backup must be deleted after a verified shrink"
        );
        assert!(
            !backend.path().with_extension("shrinking.redb").exists(),
            "temp rebuild file must not linger"
        );
    }

    #[test]
    fn open_restores_orphaned_backup_when_live_file_missing() {
        // Crash mid-shrink: the original was renamed to `.backup.redb` and the
        // process died before the rebuild was installed, so the live path is
        // missing. Reopening must RESTORE the backup, never create an empty DB.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");
        {
            let backend = RedbBackend::open(&path).unwrap();
            backend.put(b"survivor", b"data").unwrap();
        }
        let backup = path.with_extension("backup.redb");
        std::fs::rename(&path, &backup).unwrap();
        assert!(!path.exists() && backup.exists());

        let backend = RedbBackend::open(&path).unwrap();
        assert_eq!(
            backend.get(b"survivor").unwrap(),
            Some(b"data".to_vec()),
            "the interrupted shrink's original must be restored, not lost"
        );
        assert!(!backup.exists(), "the restored backup is consumed");
        assert!(path.exists());
    }

    #[test]
    fn open_discards_stale_backup_when_live_file_present() {
        // Crash after the rebuild was installed but before the backup was
        // deleted: the live file is the durable rebuild, and the leftover
        // `.backup.redb` is stale and must be removed on the next open.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");
        {
            let backend = RedbBackend::open(&path).unwrap();
            backend.put(b"live", b"current").unwrap();
        }
        let backup = path.with_extension("backup.redb");
        std::fs::write(&backup, b"stale").unwrap(); // contents irrelevant; discarded

        let backend = RedbBackend::open(&path).unwrap();
        assert_eq!(backend.get(b"live").unwrap(), Some(b"current".to_vec()));
        assert!(!backup.exists(), "a stale backup must be discarded on open");
    }

    #[test]
    fn verify_row_counts_flags_a_mismatch() {
        let (backend, _dir) = open_temp_db();
        backend.put(b"a", b"1").unwrap(); // DATA_TABLE: 1 row; META_TABLE: format marker
        let db = backend.db();
        // The true counts pass the self-diagnostic.
        RedbBackend::verify_row_counts(&db, 1, 1).unwrap();
        // A wrong expected count is rejected with the typed error.
        let err = RedbBackend::verify_row_counts(&db, 999, 1).unwrap_err();
        assert!(matches!(
            err,
            StorageError::ShrinkVerificationFailed {
                expected: (999, 1),
                got: (1, 1)
            }
        ));
    }

    #[test]
    fn shrink_rebuild_is_none_for_ephemeral_memory_backend() {
        // The in-memory backend has no file to reclaim → online shrink is a
        // no-op reported as `None` (the endpoint turns this into a clear 409).
        let backend = crate::storage::MemoryBackend::new();
        assert!(backend.shrink_rebuild().unwrap().is_none());
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
    fn scan_prefix_limited_bounds_and_paginates() {
        let (backend, _dir) = open_temp_db();
        for i in 0..10u8 {
            backend.put(format!("a:{i}").as_bytes(), b"v").unwrap();
        }
        backend.put(b"b:0", b"other").unwrap();

        let p1 = backend.scan_prefix_limited(b"a:", None, 3).unwrap();
        assert_eq!(p1.len(), 3);
        assert_eq!(p1[0].0, b"a:0");
        assert_eq!(p1[2].0, b"a:2");

        let p2 = backend
            .scan_prefix_limited(b"a:", Some(&p1[2].0), 3)
            .unwrap();
        assert_eq!(p2[0].0, b"a:3");
        assert_eq!(p2.len(), 3);

        let tail = backend
            .scan_prefix_limited(b"a:", Some(b"a:7"), 100)
            .unwrap();
        assert_eq!(tail.len(), 2, "a:8 and a:9, never b:");
        assert!(tail.iter().all(|(k, _)| k.starts_with(b"a:")));

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
    fn scan_prefix_limited_on_empty_store_is_empty() {
        let (backend, _dir) = open_temp_db();
        assert!(backend
            .scan_prefix_limited(b"any", None, 5)
            .unwrap()
            .is_empty());
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

    // --- On-disk format version (issue #48) -------------------------------

    /// Write a raw `meta.format_version` marker directly via redb, bypassing
    /// [`RedbBackend`], so tests can forge foreign / future / malformed files.
    fn write_raw_marker(path: &Path, marker: &[u8]) {
        let db = Database::create(path).unwrap();
        let write_txn = db.begin_write().unwrap();
        {
            let mut table = write_txn.open_table(META_TABLE).unwrap();
            table.insert(FORMAT_VERSION_KEY, marker).unwrap();
        }
        write_txn.commit().unwrap();
    }

    #[test]
    fn fresh_db_is_stamped_with_current_version() {
        let (backend, _dir) = open_temp_db();
        assert_eq!(
            backend.format_version().unwrap(),
            Some((FORMAT_MAJOR, FORMAT_MINOR)),
            "a freshly created file must carry the current format marker"
        );
    }

    #[test]
    fn reopen_same_version_succeeds_and_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("versioned.redb");
        {
            let backend = RedbBackend::open(&db_path).unwrap();
            backend.put(b"k", b"v").unwrap();
        }
        // Second open must accept the marker it stamped itself.
        let backend = RedbBackend::open(&db_path).unwrap();
        assert_eq!(
            backend.format_version().unwrap(),
            Some((FORMAT_MAJOR, FORMAT_MINOR))
        );
        assert_eq!(backend.get(b"k").unwrap(), Some(b"v".to_vec()));
    }

    #[test]
    fn legacy_file_without_marker_opens_and_is_stamped() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("legacy.redb");
        // Simulate a pre-versioning file: a `data` table with content but no
        // `meta` table at all.
        {
            let db = Database::create(&db_path).unwrap();
            let write_txn = db.begin_write().unwrap();
            {
                let mut table = write_txn.open_table(DATA_TABLE).unwrap();
                table.insert(b"old".as_slice(), b"data".as_slice()).unwrap();
            }
            write_txn.commit().unwrap();
        }

        let backend = RedbBackend::open(&db_path).unwrap();
        // Legacy data is preserved and readable.
        assert_eq!(backend.get(b"old").unwrap(), Some(b"data".to_vec()));
        // ...and the file is now stamped as the original 1.0 format.
        assert_eq!(
            backend.format_version().unwrap(),
            Some((FORMAT_MAJOR, FORMAT_MINOR))
        );
    }

    #[test]
    fn open_rejects_incompatible_future_major() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("future.redb");
        write_raw_marker(&db_path, format!("{}.0", FORMAT_MAJOR + 1).as_bytes());

        let err = RedbBackend::open(&db_path).unwrap_err();
        match err {
            StorageError::IncompatibleFormat {
                found,
                supported_major,
            } => {
                assert_eq!(found, format!("{}.0", FORMAT_MAJOR + 1));
                assert_eq!(supported_major, FORMAT_MAJOR);
            }
            other => panic!("expected IncompatibleFormat, got: {other:?}"),
        }
    }

    #[test]
    fn open_accepts_older_or_equal_major_with_any_minor() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("same_major_higher_minor.redb");
        // A future minor within the supported major must still open
        // (additive, backward-compatible).
        write_raw_marker(&db_path, format!("{FORMAT_MAJOR}.999").as_bytes());
        assert!(RedbBackend::open(&db_path).is_ok());
    }

    #[test]
    fn open_rejects_malformed_marker() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("garbage.redb");
        write_raw_marker(&db_path, b"not-a-version");

        match RedbBackend::open(&db_path).unwrap_err() {
            StorageError::IncompatibleFormat { found, .. } => {
                assert_eq!(found, "not-a-version");
            }
            other => panic!("expected IncompatibleFormat, got: {other:?}"),
        }
    }

    #[test]
    fn stamping_does_not_leak_into_data_scan() {
        // The `meta` marker must be invisible to the user-facing data scan.
        let (backend, _dir) = open_temp_db();
        backend.put(b"a:1", b"v").unwrap();
        let all = backend.scan_prefix(b"").unwrap();
        assert_eq!(all.len(), 1, "scan must see only data keys, not meta");
        assert_eq!(all[0].0, b"a:1");
    }
}
