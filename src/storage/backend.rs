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
}
