use crate::storage::error::Result;

/// Abstract key-value storage backend.
///
/// All upper layers (graph store, vector engine) interact with storage
/// exclusively through this trait. Concrete implementations (`MemoryBackend`,
/// `RedbBackend`) are injected at initialization time.
///
/// Keys and values are opaque byte slices. The graph layer encodes its own
/// key schema (`n:{id}`, `e:{src}:{type}:{dst}`, etc.) on top of this.
///
/// # Thread Safety
///
/// Backends must be `Send + Sync` so they can be shared behind an `Arc`
/// across threads.
pub trait StorageBackend: Send + Sync {
    /// Retrieve the value associated with `key`.
    ///
    /// Returns `Ok(None)` if the key does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`](super::error::StorageError) on I/O or backend failure.
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// Insert or update a key-value pair.
    ///
    /// If the key already exists, its value is overwritten.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`](super::error::StorageError) on I/O or backend failure.
    fn put(&self, key: &[u8], value: &[u8]) -> Result<()>;

    /// Insert or update many key-value pairs as one logical operation.
    ///
    /// Existing keys are overwritten. The default implementation simply
    /// calls [`put`](Self::put) for each pair, which is correct for any
    /// backend; durable backends are encouraged to override it so the
    /// whole batch commits in a **single** transaction. The redb backend
    /// does exactly that — per-key `put` opens its own write transaction
    /// (one `fsync` each), which is unusable for the bulk vector inserts
    /// Phase 12 task `00078` performs, so [`RedbBackend`](super::RedbBackend)
    /// folds the entire slice into one `begin_write` / `commit`.
    ///
    /// The batch is not required to be atomic for the default
    /// implementation (a mid-loop failure leaves earlier puts applied);
    /// overriding backends that wrap a transaction get all-or-nothing
    /// semantics for free.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`](super::error::StorageError) on I/O or backend failure.
    fn put_batch(&self, items: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
        for (key, value) in items {
            self.put(key, value)?;
        }
        Ok(())
    }

    /// Delete a key-value pair.
    ///
    /// Deleting a non-existent key is a no-op (returns `Ok(())`).
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`](super::error::StorageError) on I/O or backend failure.
    fn delete(&self, key: &[u8]) -> Result<()>;

    /// Return all key-value pairs whose key starts with `prefix`,
    /// sorted by key in lexicographic order.
    ///
    /// Returns an empty `Vec` if no keys match.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`](super::error::StorageError) on I/O or backend failure.
    fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;

    /// Bounded / paginated prefix scan (#243 slice 3).
    ///
    /// Return at most `limit` key-value pairs whose key starts with `prefix`,
    /// in ascending key order, beginning **strictly after** `start_after` when
    /// it is `Some` (a pagination cursor — pass the last key of the previous
    /// page). `limit == 0` returns an empty `Vec`.
    ///
    /// Unlike [`scan_prefix`](Self::scan_prefix), an implementation should stop
    /// reading once `limit` matches are collected, so a supernode with millions
    /// of adjacency entries can be walked in bounded-memory chunks instead of
    /// materialising the whole neighbor set at once.
    ///
    /// A `start_after` that sorts before `prefix` is treated as absent (the
    /// scan still begins at `prefix`), so an out-of-range cursor never skips
    /// the head of the range.
    ///
    /// The default implementation is correct but **not** memory-bounded — it
    /// calls [`scan_prefix`](Self::scan_prefix) and slices the result. Backends
    /// with a native ordered range (redb, the in-memory `BTreeMap`) override it
    /// to read only up to `limit` entries.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`](super::error::StorageError) on I/O or backend failure.
    fn scan_prefix_limited(
        &self,
        prefix: &[u8],
        start_after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let all = self.scan_prefix(prefix)?;
        Ok(all
            .into_iter()
            .filter(|(k, _)| start_after.is_none_or(|s| k.as_slice() > s))
            .take(limit)
            .collect())
    }

    /// Flush any buffered writes to durable storage.
    ///
    /// For in-memory backends this may be a no-op or trigger a snapshot.
    /// For disk-backed stores this ensures data is persisted.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`](super::error::StorageError) on I/O or backend failure.
    fn flush(&self) -> Result<()>;

    /// Report the on-disk size of the backend, in bytes.
    ///
    /// Returns `Ok(None)` when the backend has no measurable on-disk
    /// footprint (the default for ephemeral in-memory backends). Disk-backed
    /// backends return the size of the underlying file. Used by Phase 9
    /// task `00054` (compaction) to populate the `bytes_before` /
    /// `bytes_after` fields of [`crate::db::CompactReport`].
    ///
    /// The default implementation returns `Ok(None)` so existing
    /// non-disk backends do not need to be updated.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`](super::error::StorageError) on I/O or
    /// backend failure.
    fn size_bytes(&self) -> Result<Option<u64>> {
        Ok(None)
    }

    /// Reclaim unused storage. The semantics are backend-specific:
    ///
    /// - **redb**: runs `redb::Database::compact` to release pages whose
    ///   data has been deleted but whose physical slot is still allocated.
    ///   Requires exclusive access — fails with
    ///   [`StorageError::CompactNotExclusive`](super::error::StorageError::CompactNotExclusive)
    ///   if other handles to the same redb `Arc<Database>` exist.
    /// - **persistent memory backend**: rewrites the snapshot file from
    ///   the current `BTreeMap`. Naturally drops bytes for deleted keys
    ///   because the serialisation never re-includes them.
    /// - **ephemeral memory backend**: no-op (returns `Ok(())`).
    ///
    /// The default implementation calls [`flush`](Self::flush) so existing
    /// backends remain correct without changes.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`](super::error::StorageError) on I/O or
    /// backend failure, or
    /// [`StorageError::CompactNotExclusive`](super::error::StorageError::CompactNotExclusive)
    /// when the backend cannot acquire exclusive access to perform
    /// compaction.
    fn compact(&mut self) -> Result<()> {
        self.flush()
    }

    /// Read the persisted on-disk format **major** version, if the backend
    /// tracks one.
    ///
    /// Returns `Ok(None)` for backends without a durable format marker (the
    /// ephemeral in-memory backend) or a file predating format versioning.
    /// The graph layer uses it as one of two signals — alongside sampling an
    /// actual adjacency key — for the #243 slice 2 kind-in-key migration gate
    /// in [`crate::db::Drevo::open`].
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`](super::error::StorageError) on backend failure.
    fn format_major(&self) -> Result<Option<u32>> {
        Ok(None)
    }

    /// Persist the on-disk format version marker `major.minor`.
    ///
    /// The default is a no-op (ephemeral backends have nothing durable to
    /// stamp). The redb backend writes the `meta.format_version` marker so a
    /// completed adjacency migration ([`crate::db::Drevo::migrate_adjacency`])
    /// records the new layout version and old, layout-incompatible builds
    /// refuse the file with
    /// [`StorageError::IncompatibleFormat`](super::error::StorageError::IncompatibleFormat).
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`](super::error::StorageError) on backend failure.
    fn set_format_version(&self, _major: u32, _minor: u32) -> Result<()> {
        Ok(())
    }
}
