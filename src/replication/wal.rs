//! Write-ahead log — the ordered record of every mutation a MAIN node
//! applies, and the wire a REPLICA replays to catch up.
//!
//! The log is the single source of truth for replication. A MAIN node tees
//! every write into it ([`crate::replication::Primary`]); a REPLICA pulls the
//! suffix it has not yet seen ([`WriteAheadLog::records_since`]) and replays it
//! in order ([`crate::replication::Replica::apply`]). Every record carries a
//! strictly increasing [`Lsn`] (log sequence number) so a replica can detect
//! gaps, skip records it has already applied, and report exactly how far
//! behind it is.

use crate::storage::backend::StorageBackend;
use crate::storage::Result as StorageResult;
use std::collections::VecDeque;

/// A log sequence number — a strictly increasing identifier stamped on every
/// [`WalRecord`].
///
/// [`Lsn::ZERO`] is the sentinel for "no record yet": a freshly created log
/// has [`WriteAheadLog::last_lsn`] equal to `ZERO`, and a freshly created
/// replica has applied up to `ZERO`. The first appended record is
/// [`Lsn`]`(1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Lsn(pub u64);

impl Lsn {
    /// The sentinel "before any record" sequence number.
    pub const ZERO: Lsn = Lsn(0);

    /// The next sequence number after this one.
    #[must_use]
    pub const fn next(self) -> Lsn {
        Lsn(self.0 + 1)
    }
}

impl std::fmt::Display for Lsn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A single mutation, mirroring the write half of [`StorageBackend`].
///
/// Reads (`get` / `scan_prefix`) never enter the log — only operations that
/// change durable state are replicated. The variants map one-to-one onto the
/// backend's mutating methods so a replica can replay them verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalOp {
    /// Insert or overwrite a single key.
    Put {
        /// The key being written.
        key: Vec<u8>,
        /// The value to store under `key`.
        value: Vec<u8>,
    },
    /// Remove a single key (a no-op on the replica if the key is absent,
    /// matching [`StorageBackend::delete`]).
    Delete {
        /// The key being removed.
        key: Vec<u8>,
    },
    /// Insert or overwrite many keys as one logical operation, mirroring
    /// [`StorageBackend::put_batch`].
    PutBatch {
        /// The key/value pairs to write.
        items: Vec<(Vec<u8>, Vec<u8>)>,
    },
}

impl WalOp {
    /// Replay this operation against a storage backend.
    ///
    /// # Errors
    ///
    /// Propagates any [`StorageError`](crate::storage::StorageError) the
    /// backend raises while applying the mutation.
    pub fn apply_to(&self, backend: &dyn StorageBackend) -> StorageResult<()> {
        match self {
            WalOp::Put { key, value } => backend.put(key, value),
            WalOp::Delete { key } => backend.delete(key),
            WalOp::PutBatch { items } => backend.put_batch(items),
        }
    }

    /// Whether this operation mutates state (always `true` — the log only ever
    /// holds writes). Provided so callers can assert the invariant without
    /// matching every variant.
    #[must_use]
    pub const fn is_write(&self) -> bool {
        true
    }
}

/// A mutation stamped with its [`Lsn`] — the unit of replication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalRecord {
    /// The sequence number assigned when this record was appended.
    pub lsn: Lsn,
    /// The mutation to replay.
    pub op: WalOp,
}

/// An append-only, in-memory write-ahead log.
///
/// Records are appended in [`Lsn`] order and retained as a contiguous suffix:
/// a checkpoint ([`truncate_through`](Self::truncate_through)) drops a prefix
/// of records every replica has durably applied, but the high-water
/// [`last_lsn`](Self::last_lsn) keeps climbing regardless. A replica that has
/// fallen behind the retained suffix can no longer be served incrementally and
/// must perform a full resync — [`can_serve_since`](Self::can_serve_since)
/// reports that condition.
///
/// The log is deliberately storage-agnostic and dependency-free: it holds
/// opaque byte operations, never touches the filesystem itself, and is safe to
/// use on `wasm32-unknown-unknown`. Durability of the log itself (persisting it
/// alongside the data) is a backend concern layered on top, not modelled here.
#[derive(Debug, Default)]
pub struct WriteAheadLog {
    /// The retained suffix of records, in ascending `lsn` order.
    records: VecDeque<WalRecord>,
    /// The highest `lsn` ever assigned — unaffected by truncation.
    last_lsn: Lsn,
}

impl WriteAheadLog {
    /// Create an empty log positioned at [`Lsn::ZERO`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            records: VecDeque::new(),
            last_lsn: Lsn::ZERO,
        }
    }

    /// Append a mutation, assigning it the next [`Lsn`], and return the stored
    /// record.
    pub fn append(&mut self, op: WalOp) -> WalRecord {
        let lsn = self.last_lsn.next();
        self.last_lsn = lsn;
        let record = WalRecord { lsn, op };
        self.records.push_back(record.clone());
        record
    }

    /// The highest [`Lsn`] ever assigned ([`Lsn::ZERO`] if nothing has been
    /// appended). Survives truncation.
    #[must_use]
    pub fn last_lsn(&self) -> Lsn {
        self.last_lsn
    }

    /// The [`Lsn`] of the oldest record still retained, or `None` if the log
    /// holds no records (either nothing was appended, or every record has been
    /// truncated away).
    #[must_use]
    pub fn earliest_lsn(&self) -> Option<Lsn> {
        self.records.front().map(|r| r.lsn)
    }

    /// The number of records currently retained (after any truncation).
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the retained suffix is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Every retained record whose [`Lsn`] is strictly greater than `after`,
    /// in ascending order — the delta a replica positioned at `after` needs.
    ///
    /// Callers that might be lagging behind a checkpoint should consult
    /// [`can_serve_since`](Self::can_serve_since) first; this method silently
    /// returns only what is retained.
    #[must_use]
    pub fn records_since(&self, after: Lsn) -> Vec<WalRecord> {
        self.records
            .iter()
            .filter(|r| r.lsn > after)
            .cloned()
            .collect()
    }

    /// Whether the log can incrementally serve a replica positioned at
    /// `after`.
    ///
    /// `true` when either the replica is already up to date (`after >=
    /// last_lsn`, nothing to stream) or the next record it needs
    /// (`after + 1`) is still retained. `false` when a checkpoint has dropped
    /// records the replica has not yet applied — the replica must full-resync.
    #[must_use]
    pub fn can_serve_since(&self, after: Lsn) -> bool {
        if after >= self.last_lsn {
            return true;
        }
        match self.earliest_lsn() {
            Some(earliest) => after.next() >= earliest,
            None => false,
        }
    }

    /// Drop every retained record whose [`Lsn`] is less than or equal to
    /// `through`, returning the number removed.
    ///
    /// A checkpoint: once every replica reports having applied up to `through`,
    /// those records are no longer needed for catch-up and can be released.
    /// [`last_lsn`](Self::last_lsn) is unaffected.
    pub fn truncate_through(&mut self, through: Lsn) -> usize {
        let mut removed = 0;
        while let Some(front) = self.records.front() {
            if front.lsn <= through {
                self.records.pop_front();
                removed += 1;
            } else {
                break;
            }
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryBackend;

    #[test]
    fn fresh_log_starts_at_zero() {
        let wal = WriteAheadLog::new();
        assert_eq!(wal.last_lsn(), Lsn::ZERO);
        assert_eq!(wal.earliest_lsn(), None);
        assert!(wal.is_empty());
        assert_eq!(wal.len(), 0);
    }

    #[test]
    fn append_assigns_monotonic_lsns_from_one() {
        let mut wal = WriteAheadLog::new();
        let a = wal.append(WalOp::Put {
            key: b"k1".to_vec(),
            value: b"v1".to_vec(),
        });
        let b = wal.append(WalOp::Delete {
            key: b"k2".to_vec(),
        });
        assert_eq!(a.lsn, Lsn(1));
        assert_eq!(b.lsn, Lsn(2));
        assert_eq!(wal.last_lsn(), Lsn(2));
        assert_eq!(wal.earliest_lsn(), Some(Lsn(1)));
        assert_eq!(wal.len(), 2);
    }

    #[test]
    fn records_since_returns_only_newer() {
        let mut wal = WriteAheadLog::new();
        for i in 0..5u8 {
            wal.append(WalOp::Put {
                key: vec![i],
                value: vec![i],
            });
        }
        let delta = wal.records_since(Lsn(2));
        assert_eq!(delta.len(), 3);
        assert_eq!(delta[0].lsn, Lsn(3));
        assert_eq!(delta[2].lsn, Lsn(5));
        assert!(wal.records_since(Lsn(5)).is_empty());
    }

    #[test]
    fn lsn_next_and_display() {
        assert_eq!(Lsn(0).next(), Lsn(1));
        assert_eq!(Lsn(41).next(), Lsn(42));
        assert_eq!(format!("{}", Lsn(7)), "7");
    }

    #[test]
    fn truncate_drops_prefix_but_keeps_high_water() {
        let mut wal = WriteAheadLog::new();
        for i in 0..5u8 {
            wal.append(WalOp::Put {
                key: vec![i],
                value: vec![i],
            });
        }
        let removed = wal.truncate_through(Lsn(3));
        assert_eq!(removed, 3);
        assert_eq!(wal.len(), 2);
        assert_eq!(wal.earliest_lsn(), Some(Lsn(4)));
        // High-water mark is preserved across truncation.
        assert_eq!(wal.last_lsn(), Lsn(5));
        // The still-retained suffix is served correctly.
        assert_eq!(wal.records_since(Lsn(3)).len(), 2);
    }

    #[test]
    fn can_serve_since_detects_truncated_replica() {
        let mut wal = WriteAheadLog::new();
        for i in 0..5u8 {
            wal.append(WalOp::Put {
                key: vec![i],
                value: vec![i],
            });
        }
        wal.truncate_through(Lsn(3));
        // A replica at LSN 3 needs record 4, which is still retained.
        assert!(wal.can_serve_since(Lsn(3)));
        // A replica at LSN 2 needs record 3, which was dropped → must resync.
        assert!(!wal.can_serve_since(Lsn(2)));
        // An up-to-date replica is always serveable (empty delta).
        assert!(wal.can_serve_since(Lsn(5)));
        assert!(wal.can_serve_since(Lsn(99)));
    }

    #[test]
    fn empty_log_can_only_serve_up_to_date() {
        let wal = WriteAheadLog::new();
        assert!(wal.can_serve_since(Lsn::ZERO));
        // After full truncation, a lagging replica cannot be served.
        let mut wal = WriteAheadLog::new();
        wal.append(WalOp::Delete { key: b"x".to_vec() });
        wal.truncate_through(Lsn(1));
        assert!(wal.is_empty());
        assert!(wal.can_serve_since(Lsn(1)));
        assert!(!wal.can_serve_since(Lsn::ZERO));
    }

    #[test]
    fn op_applies_to_backend() {
        let backend = MemoryBackend::new();
        WalOp::Put {
            key: b"a".to_vec(),
            value: b"1".to_vec(),
        }
        .apply_to(&backend)
        .unwrap();
        assert_eq!(backend.get(b"a").unwrap(), Some(b"1".to_vec()));

        WalOp::PutBatch {
            items: vec![
                (b"b".to_vec(), b"2".to_vec()),
                (b"c".to_vec(), b"3".to_vec()),
            ],
        }
        .apply_to(&backend)
        .unwrap();
        assert_eq!(backend.get(b"c").unwrap(), Some(b"3".to_vec()));

        WalOp::Delete { key: b"a".to_vec() }
            .apply_to(&backend)
            .unwrap();
        assert_eq!(backend.get(b"a").unwrap(), None);
    }
}
