//! The MAIN node — the single writable replica in a replication topology.
//!
//! A [`Primary`] wraps a storage backend and tees every mutation into a
//! [`WriteAheadLog`] before (logically) acknowledging it: the data write and
//! the log append happen together, so the log is always a faithful record of
//! the backend's history. Read replicas pull the log suffix they have not yet
//! seen via [`replicate_since`](Primary::replicate_since) and replay it.

use crate::replication::error::{ReplicationError, Result};
use crate::replication::wal::{Lsn, WalOp, WalRecord, WriteAheadLog};
use crate::replication::Role;
use crate::storage::backend::StorageBackend;
use crate::storage::Result as StorageResult;

/// The writable MAIN node: a storage backend plus its write-ahead log.
///
/// All writes go through the `Primary`'s [`put`](Primary::put) /
/// [`delete`](Primary::delete) / [`put_batch`](Primary::put_batch) methods,
/// each of which applies the mutation to the backend and appends a matching
/// [`WalRecord`], returning the assigned [`Lsn`]. Reads delegate straight to
/// the backend. A topology has exactly one `Primary`; every other node is a
/// read-only [`Replica`](crate::replication::Replica).
#[derive(Debug)]
pub struct Primary<B: StorageBackend> {
    backend: B,
    wal: WriteAheadLog,
}

impl<B: StorageBackend> Primary<B> {
    /// Wrap a backend as the MAIN node with a fresh, empty log.
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            wal: WriteAheadLog::new(),
        }
    }

    /// This node's role — always [`Role::Main`].
    #[must_use]
    pub const fn role(&self) -> Role {
        Role::Main
    }

    /// Borrow the underlying storage backend (e.g. to construct a graph view
    /// over it).
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Borrow the write-ahead log.
    #[must_use]
    pub fn wal(&self) -> &WriteAheadLog {
        &self.wal
    }

    /// The [`Lsn`] of the most recent write ([`Lsn::ZERO`] if none yet).
    #[must_use]
    pub fn last_lsn(&self) -> Lsn {
        self.wal.last_lsn()
    }

    /// Write a single key, logging the mutation, and return its [`Lsn`].
    ///
    /// # Errors
    ///
    /// Propagates any [`StorageError`](crate::storage::StorageError) from the
    /// backend; the log is only appended after the backend write succeeds.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> StorageResult<Lsn> {
        self.backend.put(key, value)?;
        let record = self.wal.append(WalOp::Put {
            key: key.to_vec(),
            value: value.to_vec(),
        });
        Ok(record.lsn)
    }

    /// Delete a single key, logging the mutation, and return its [`Lsn`].
    ///
    /// # Errors
    ///
    /// Propagates any [`StorageError`](crate::storage::StorageError) from the
    /// backend.
    pub fn delete(&mut self, key: &[u8]) -> StorageResult<Lsn> {
        self.backend.delete(key)?;
        let record = self.wal.append(WalOp::Delete { key: key.to_vec() });
        Ok(record.lsn)
    }

    /// Write many keys in one logical operation, logging a single batch
    /// record, and return its [`Lsn`].
    ///
    /// # Errors
    ///
    /// Propagates any [`StorageError`](crate::storage::StorageError) from the
    /// backend.
    pub fn put_batch(&mut self, items: &[(Vec<u8>, Vec<u8>)]) -> StorageResult<Lsn> {
        self.backend.put_batch(items)?;
        let record = self.wal.append(WalOp::PutBatch {
            items: items.to_vec(),
        });
        Ok(record.lsn)
    }

    /// Read a key straight from the backend.
    ///
    /// # Errors
    ///
    /// Propagates any [`StorageError`](crate::storage::StorageError) from the
    /// backend.
    pub fn get(&self, key: &[u8]) -> StorageResult<Option<Vec<u8>>> {
        self.backend.get(key)
    }

    /// Prefix-scan the backend.
    ///
    /// # Errors
    ///
    /// Propagates any [`StorageError`](crate::storage::StorageError) from the
    /// backend.
    pub fn scan_prefix(&self, prefix: &[u8]) -> StorageResult<Vec<(Vec<u8>, Vec<u8>)>> {
        self.backend.scan_prefix(prefix)
    }

    /// The records a replica positioned at `after` must replay to catch up.
    ///
    /// # Errors
    ///
    /// Returns [`ReplicationError::Truncated`] when a checkpoint has dropped
    /// records the replica has not yet applied — the replica is too far behind
    /// to be served incrementally and must perform a full resync.
    pub fn replicate_since(&self, after: Lsn) -> Result<Vec<WalRecord>> {
        if !self.wal.can_serve_since(after) {
            return Err(ReplicationError::Truncated {
                requested: after,
                earliest: self.wal.earliest_lsn(),
            });
        }
        Ok(self.wal.records_since(after))
    }

    /// Drop the log prefix every replica has durably applied, returning the
    /// number of records released.
    ///
    /// Call once a quorum of replicas reports `applied_lsn >= through`. Frees
    /// memory without affecting [`last_lsn`](Primary::last_lsn); replicas that
    /// still need the dropped records will subsequently fail
    /// [`replicate_since`](Primary::replicate_since) with
    /// [`ReplicationError::Truncated`].
    pub fn checkpoint(&mut self, through: Lsn) -> usize {
        self.wal.truncate_through(through)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryBackend;

    fn primary() -> Primary<MemoryBackend> {
        Primary::new(MemoryBackend::new())
    }

    #[test]
    fn role_is_main() {
        assert_eq!(primary().role(), Role::Main);
    }

    #[test]
    fn writes_tee_into_log_and_return_lsn() {
        let mut p = primary();
        let l1 = p.put(b"a", b"1").unwrap();
        let l2 = p.delete(b"a").unwrap();
        let l3 = p
            .put_batch(&[
                (b"b".to_vec(), b"2".to_vec()),
                (b"c".to_vec(), b"3".to_vec()),
            ])
            .unwrap();
        assert_eq!(l1, Lsn(1));
        assert_eq!(l2, Lsn(2));
        assert_eq!(l3, Lsn(3));
        assert_eq!(p.last_lsn(), Lsn(3));
        assert_eq!(p.wal().len(), 3);
    }

    #[test]
    fn writes_land_in_backend() {
        let mut p = primary();
        p.put(b"k", b"v").unwrap();
        assert_eq!(p.get(b"k").unwrap(), Some(b"v".to_vec()));
        p.delete(b"k").unwrap();
        assert_eq!(p.get(b"k").unwrap(), None);
    }

    #[test]
    fn replicate_since_returns_delta() {
        let mut p = primary();
        p.put(b"a", b"1").unwrap();
        p.put(b"b", b"2").unwrap();
        let delta = p.replicate_since(Lsn(1)).unwrap();
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].lsn, Lsn(2));
    }

    #[test]
    fn checkpoint_then_lagging_replica_is_truncated() {
        let mut p = primary();
        p.put(b"a", b"1").unwrap();
        p.put(b"b", b"2").unwrap();
        p.put(b"c", b"3").unwrap();
        let dropped = p.checkpoint(Lsn(2));
        assert_eq!(dropped, 2);
        // Up-to-date catch-up still works.
        assert_eq!(p.replicate_since(Lsn(2)).unwrap().len(), 1);
        // A replica behind the checkpoint cannot be served.
        let err = p.replicate_since(Lsn(0)).unwrap_err();
        assert!(matches!(
            err,
            ReplicationError::Truncated {
                requested: Lsn(0),
                earliest: Some(Lsn(3))
            }
        ));
        // High-water mark survives the checkpoint.
        assert_eq!(p.last_lsn(), Lsn(3));
    }
}
