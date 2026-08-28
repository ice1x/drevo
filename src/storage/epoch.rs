//! Mutation-epoch wrapper around a storage backend (engine flip, RFC
//! `docs/rfc-native-core.md` #307, Phase 6 slice A).
//!
//! The KV store has no change feed, so a native read mirror
//! ([`crate::native_mirror::NativeMirror`]) cannot tail incremental updates
//! the way the native indexes tail [`crate::native::NativeGraph`]'s WAL. What
//! it *can* do cheaply is detect staleness: [`crate::storage::EpochBackend`] wraps the real
//! backend and bumps an atomic **mutation epoch** on every mutating call
//! (`put` / `put_batch` / `delete` / `delete_batch`). A mirror stamps the
//! epoch it was built at; when the stamps differ, the mirror is stale and
//! reads fall back to the KV engine — always correct, merely slower.
//!
//! Every mutation also holds a shared **quiesce gate** for its duration.
//! [`crate::storage::EpochBackend::quiesce`] takes the gate exclusively, so a caller can read
//! a multi-row snapshot (a full dump export) with no write in flight and no
//! write able to start — the only way an epoch stamp can honestly vouch for
//! a whole snapshot rather than a single row. Reads never touch the gate.
//!
//! Content-preserving maintenance (`flush`, `compact`, `shrink_rebuild`,
//! format-marker stamping) does **not** bump the epoch: the logical graph is
//! unchanged, so a mirror built before it is still valid.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::storage::backend::StorageBackend;
use crate::storage::error::Result;

/// A [`StorageBackend`] decorator that counts mutations and can quiesce them.
///
/// See the [module docs](self) for the role this plays in the engine flip.
pub struct EpochBackend {
    /// The real backend every call delegates to.
    inner: Box<dyn StorageBackend>,
    /// Number of mutating calls applied so far (monotonic).
    epoch: AtomicU64,
    /// Mutations hold this shared; [`Self::quiesce`] holds it exclusively.
    gate: RwLock<()>,
}

impl EpochBackend {
    /// Wrap `inner`, starting the epoch at zero.
    pub fn new(inner: Box<dyn StorageBackend>) -> Self {
        Self {
            inner,
            epoch: AtomicU64::new(0),
            gate: RwLock::new(()),
        }
    }

    /// The current mutation epoch — increases by at least one for every
    /// mutating backend call that has completed.
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Block until no mutation is in flight and keep new ones out while the
    /// returned guard lives. While held, [`Self::epoch`] is stable and any
    /// data read through the backend belongs to that single epoch.
    pub fn quiesce(&self) -> RwLockWriteGuard<'_, ()> {
        self.gate.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Shared-side gate entry for one mutating call.
    fn enter_mutation(&self) -> RwLockReadGuard<'_, ()> {
        self.gate.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Run one mutating call under the gate and bump the epoch — also on
    /// error, because a failed batch may have partially applied.
    fn mutate<T>(&self, op: impl FnOnce(&dyn StorageBackend) -> Result<T>) -> Result<T> {
        let _in_flight = self.enter_mutation();
        let result = op(&*self.inner);
        self.epoch.fetch_add(1, Ordering::SeqCst);
        result
    }
}

impl StorageBackend for EpochBackend {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.inner.get(key)
    }

    fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.mutate(|b| b.put(key, value))
    }

    fn put_batch(&self, items: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        self.mutate(|b| b.put_batch(items))
    }

    fn delete(&self, key: &[u8]) -> Result<()> {
        self.mutate(|b| b.delete(key))
    }

    fn delete_batch(&self, keys: &[Vec<u8>]) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        self.mutate(|b| b.delete_batch(keys))
    }

    fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.inner.scan_prefix(prefix)
    }

    fn scan_prefix_limited(
        &self,
        prefix: &[u8],
        start_after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.inner.scan_prefix_limited(prefix, start_after, limit)
    }

    fn flush(&self) -> Result<()> {
        self.inner.flush()
    }

    fn size_bytes(&self) -> Result<Option<u64>> {
        self.inner.size_bytes()
    }

    fn content_bytes(&self) -> Result<u64> {
        self.inner.content_bytes()
    }

    fn compact(&mut self) -> Result<()> {
        self.inner.compact()
    }

    fn format_major(&self) -> Result<Option<u32>> {
        self.inner.format_major()
    }

    fn set_format_version(&self, major: u32, minor: u32) -> Result<()> {
        self.inner.set_format_version(major, minor)
    }

    fn shrink_rebuild(&self) -> Result<Option<(u64, u64)>> {
        self.inner.shrink_rebuild()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::memory::MemoryBackend;

    fn backend() -> EpochBackend {
        EpochBackend::new(Box::new(MemoryBackend::new()))
    }

    #[test]
    fn epoch_starts_at_zero_and_reads_do_not_bump() {
        let b = backend();
        assert_eq!(b.epoch(), 0);
        assert!(b.get(b"missing").unwrap().is_none());
        assert!(b.scan_prefix(b"").unwrap().is_empty());
        assert!(b.scan_prefix_limited(b"", None, 10).unwrap().is_empty());
        b.flush().unwrap();
        assert_eq!(b.epoch(), 0);
    }

    #[test]
    fn each_mutation_bumps_once() {
        let b = backend();
        b.put(b"a", b"1").unwrap();
        assert_eq!(b.epoch(), 1);
        b.put_batch(&[
            (b"b".to_vec(), b"2".to_vec()),
            (b"c".to_vec(), b"3".to_vec()),
        ])
        .unwrap();
        assert_eq!(b.epoch(), 2, "a batch is one logical mutation");
        b.delete(b"a").unwrap();
        assert_eq!(b.epoch(), 3);
        b.delete_batch(&[b"b".to_vec(), b"c".to_vec()]).unwrap();
        assert_eq!(b.epoch(), 4);
    }

    #[test]
    fn empty_batches_do_not_bump() {
        let b = backend();
        b.put_batch(&[]).unwrap();
        b.delete_batch(&[]).unwrap();
        assert_eq!(b.epoch(), 0);
    }

    #[test]
    fn mutations_still_apply_and_reads_delegate() {
        let b = backend();
        b.put(b"k", b"v").unwrap();
        assert_eq!(b.get(b"k").unwrap().as_deref(), Some(&b"v"[..]));
        b.delete(b"k").unwrap();
        assert!(b.get(b"k").unwrap().is_none());
    }

    #[test]
    fn quiesce_holds_the_epoch_stable() {
        let b = backend();
        b.put(b"k", b"v").unwrap();
        let guard = b.quiesce();
        let stamped = b.epoch();
        // A snapshot read under the guard belongs to `stamped`.
        let rows = b.scan_prefix(b"").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(b.epoch(), stamped);
        drop(guard);
    }
}
