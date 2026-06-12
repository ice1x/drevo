//! A REPLICA node — a read-only follower that replays the MAIN node's log.
//!
//! A [`Replica`] wraps its own storage backend and an `applied_lsn` cursor.
//! It serves reads (the "read scaling" half of the feature: point readers at
//! any number of replicas to spread query load off the writable MAIN) and
//! replays [`WalRecord`]s in [`Lsn`] order via [`apply`](Replica::apply). Its
//! write methods exist only to refuse — a replica is read-only by
//! construction, so an accidental write fails loudly with
//! [`ReplicationError::ReadOnly`] rather than diverging from the MAIN.

use crate::replication::error::{ReplicationError, Result};
use crate::replication::wal::{Lsn, WalRecord};
use crate::replication::Role;
use crate::storage::backend::StorageBackend;
use crate::storage::Result as StorageResult;

/// The read-only REPLICA node: a storage backend plus the cursor tracking how
/// far it has replayed the MAIN node's log.
///
/// Reads delegate to the backend; writes are rejected. The replica advances by
/// [`apply`](Replica::apply)ing records pulled from
/// [`Primary::replicate_since`](crate::replication::Primary::replicate_since),
/// enforcing strict in-order replay and idempotent re-application.
#[derive(Debug)]
pub struct Replica<B: StorageBackend> {
    backend: B,
    applied_lsn: Lsn,
}

impl<B: StorageBackend> Replica<B> {
    /// Wrap a fresh (empty) backend as a replica positioned at [`Lsn::ZERO`].
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            applied_lsn: Lsn::ZERO,
        }
    }

    /// This node's role — always [`Role::Replica`].
    #[must_use]
    pub const fn role(&self) -> Role {
        Role::Replica
    }

    /// Borrow the underlying storage backend.
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// The highest [`Lsn`] this replica has replayed ([`Lsn::ZERO`] if none).
    #[must_use]
    pub fn applied_lsn(&self) -> Lsn {
        self.applied_lsn
    }

    /// Read a key from the replica's local backend.
    ///
    /// # Errors
    ///
    /// Propagates any [`StorageError`](crate::storage::StorageError) from the
    /// backend.
    pub fn get(&self, key: &[u8]) -> StorageResult<Option<Vec<u8>>> {
        self.backend.get(key)
    }

    /// Prefix-scan the replica's local backend.
    ///
    /// # Errors
    ///
    /// Propagates any [`StorageError`](crate::storage::StorageError) from the
    /// backend.
    pub fn scan_prefix(&self, prefix: &[u8]) -> StorageResult<Vec<(Vec<u8>, Vec<u8>)>> {
        self.backend.scan_prefix(prefix)
    }

    /// Replay a single record from the MAIN node's log.
    ///
    /// Replay is strictly ordered and idempotent:
    ///
    /// * a record at or below [`applied_lsn`](Replica::applied_lsn) was already
    ///   applied and is silently skipped (returns `Ok(false)`), so a replica
    ///   can safely re-receive overlapping stream windows;
    /// * the very next record (`applied_lsn + 1`) is applied and the cursor
    ///   advances (returns `Ok(true)`);
    /// * any later record exposes a gap — applying it would skip records — and
    ///   is rejected with [`ReplicationError::LsnGap`].
    ///
    /// # Errors
    ///
    /// Returns [`ReplicationError::LsnGap`] when `record.lsn` is beyond the next
    /// expected sequence number, or
    /// [`ReplicationError::Storage`](crate::replication::ReplicationError::Storage)
    /// if the backend rejects the mutation.
    pub fn apply(&mut self, record: &WalRecord) -> Result<bool> {
        if record.lsn <= self.applied_lsn {
            return Ok(false);
        }
        let expected = self.applied_lsn.next();
        if record.lsn != expected {
            return Err(ReplicationError::LsnGap {
                applied: self.applied_lsn,
                expected,
                got: record.lsn,
            });
        }
        record.op.apply_to(&self.backend)?;
        self.applied_lsn = record.lsn;
        Ok(true)
    }

    /// Replay a contiguous stream of records, returning how many were newly
    /// applied (records already seen are skipped, not counted).
    ///
    /// # Errors
    ///
    /// Stops and returns [`ReplicationError::LsnGap`] at the first record that
    /// is not contiguous with what has been applied so far; records before the
    /// gap remain applied. Propagates backend
    /// [`Storage`](crate::replication::ReplicationError::Storage) errors the
    /// same way.
    pub fn apply_stream(&mut self, records: &[WalRecord]) -> Result<usize> {
        let mut applied = 0;
        for record in records {
            if self.apply(record)? {
                applied += 1;
            }
        }
        Ok(applied)
    }

    /// Reject a write — a replica is read-only.
    ///
    /// # Errors
    ///
    /// Always returns [`ReplicationError::ReadOnly`].
    pub fn put(&self, _key: &[u8], _value: &[u8]) -> Result<()> {
        Err(ReplicationError::ReadOnly)
    }

    /// Reject a delete — a replica is read-only.
    ///
    /// # Errors
    ///
    /// Always returns [`ReplicationError::ReadOnly`].
    pub fn delete(&self, _key: &[u8]) -> Result<()> {
        Err(ReplicationError::ReadOnly)
    }

    /// Reject a batch write — a replica is read-only.
    ///
    /// # Errors
    ///
    /// Always returns [`ReplicationError::ReadOnly`].
    pub fn put_batch(&self, _items: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
        Err(ReplicationError::ReadOnly)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replication::wal::WalOp;
    use crate::storage::MemoryBackend;

    fn rec(lsn: u64, key: &[u8], value: &[u8]) -> WalRecord {
        WalRecord {
            lsn: Lsn(lsn),
            op: WalOp::Put {
                key: key.to_vec(),
                value: value.to_vec(),
            },
        }
    }

    fn replica() -> Replica<MemoryBackend> {
        Replica::new(MemoryBackend::new())
    }

    #[test]
    fn role_is_replica_and_starts_at_zero() {
        let r = replica();
        assert_eq!(r.role(), Role::Replica);
        assert_eq!(r.applied_lsn(), Lsn::ZERO);
    }

    #[test]
    fn apply_in_order_advances_cursor_and_data() {
        let mut r = replica();
        assert!(r.apply(&rec(1, b"a", b"1")).unwrap());
        assert!(r.apply(&rec(2, b"b", b"2")).unwrap());
        assert_eq!(r.applied_lsn(), Lsn(2));
        assert_eq!(r.get(b"a").unwrap(), Some(b"1".to_vec()));
        assert_eq!(r.get(b"b").unwrap(), Some(b"2".to_vec()));
    }

    #[test]
    fn re_applying_seen_record_is_idempotent_skip() {
        let mut r = replica();
        r.apply(&rec(1, b"a", b"1")).unwrap();
        // Re-receiving LSN 1 (e.g. overlapping stream window) is a no-op.
        assert!(!r.apply(&rec(1, b"a", b"OVERWRITE")).unwrap());
        assert_eq!(r.applied_lsn(), Lsn(1));
        assert_eq!(r.get(b"a").unwrap(), Some(b"1".to_vec()));
    }

    #[test]
    fn gap_is_rejected() {
        let mut r = replica();
        r.apply(&rec(1, b"a", b"1")).unwrap();
        let err = r.apply(&rec(3, b"c", b"3")).unwrap_err();
        assert!(matches!(
            err,
            ReplicationError::LsnGap {
                applied: Lsn(1),
                expected: Lsn(2),
                got: Lsn(3)
            }
        ));
        // Cursor and data are unchanged by the rejected apply.
        assert_eq!(r.applied_lsn(), Lsn(1));
        assert_eq!(r.get(b"c").unwrap(), None);
    }

    #[test]
    fn apply_stream_counts_only_new_records() {
        let mut r = replica();
        r.apply(&rec(1, b"a", b"1")).unwrap();
        let stream = vec![rec(1, b"a", b"1"), rec(2, b"b", b"2"), rec(3, b"c", b"3")];
        // LSN 1 is skipped, 2 and 3 are new.
        assert_eq!(r.apply_stream(&stream).unwrap(), 2);
        assert_eq!(r.applied_lsn(), Lsn(3));
    }

    #[test]
    fn writes_are_rejected() {
        let r = replica();
        assert!(matches!(r.put(b"k", b"v"), Err(ReplicationError::ReadOnly)));
        assert!(matches!(r.delete(b"k"), Err(ReplicationError::ReadOnly)));
        assert!(matches!(
            r.put_batch(&[(b"k".to_vec(), b"v".to_vec())]),
            Err(ReplicationError::ReadOnly)
        ));
    }
}
