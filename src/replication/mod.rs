//! Replication — Phase 15 task `00095`.
//!
//! A production graph database rarely runs as a single process: writes funnel
//! to one authoritative node while reads fan out across many followers. This
//! module is the **WAL-based MAIN / REPLICA** substrate that makes that
//! topology possible, and the *read-scaling* story that comes with it.
//!
//! It models replication the way every log-shipping system does — one writable
//! node, an ordered log of its mutations, and followers that replay that log:
//!
//! * [`WriteAheadLog`](crate::replication::WriteAheadLog) — an append-only,
//!   in-memory log of [`WalRecord`](crate::replication::WalRecord)s, each a
//!   [`WalOp`](crate::replication::WalOp) (the write half of
//!   [`StorageBackend`](crate::storage::StorageBackend) — `Put` / `Delete` /
//!   `PutBatch`) stamped with a strictly increasing
//!   [`Lsn`](crate::replication::Lsn). A checkpoint
//!   ([`truncate_through`](crate::replication::WriteAheadLog::truncate_through))
//!   drops a prefix every follower has applied while the high-water LSN keeps
//!   climbing.
//! * [`Primary`](crate::replication::Primary) — the MAIN node. Wraps a backend
//!   and tees every write into the log, returning the assigned LSN. Serves the
//!   log suffix a follower needs via
//!   [`replicate_since`](crate::replication::Primary::replicate_since).
//! * [`Replica`](crate::replication::Replica) — a read-only follower. Serves
//!   reads (point load at any number of replicas to scale read throughput) and
//!   replays records in LSN order via [`apply`](crate::replication::Replica::apply),
//!   enforcing strict ordering, idempotent re-application of overlapping stream
//!   windows, and gap detection. Its write methods exist only to refuse with
//!   [`ReplicationError::ReadOnly`](crate::replication::ReplicationError::ReadOnly).
//!
//! # The sync loop
//!
//! A follower catches up by repeating: ask the MAIN for everything past its
//! cursor, replay it, advance the cursor.
//!
//! ```text
//! loop {
//!     let delta = primary.replicate_since(replica.applied_lsn())?;
//!     replica.apply_stream(&delta)?;
//! }
//! ```
//!
//! Because each [`Replica`](crate::replication::Replica) owns its own
//! [`applied_lsn`](crate::replication::Replica::applied_lsn) cursor, followers
//! can sit at different positions; a slow follower simply pulls a longer delta
//! next round. Once every follower has passed an LSN, the MAIN may
//! [`checkpoint`](crate::replication::Primary::checkpoint) it away — a follower
//! that lagged past the checkpoint is told to full-resync via
//! [`ReplicationError::Truncated`](crate::replication::ReplicationError::Truncated).
//!
//! # Scope
//!
//! Like the planner (`00085`–`00089`), MVCC (`00081`), and authorization
//! (`00094`) engines in their early tasks, this is the replication
//! *substrate* — the log, the roles, and the replay semantics — and is
//! **not yet wired into the executor / HTTP API / Bolt session**, nor does it
//! ship a network transport: [`replicate_since`](crate::replication::Primary::replicate_since)
//! hands back owned records that a caller streams over whatever channel it
//! likes (the Bolt port, an HTTP long-poll, an in-process channel for tests).
//! It is dependency-free, always compiled, and WASM-safe — it touches only
//! `std` collections, holds opaque byte operations, and performs no I/O of its
//! own (the storage backend it wraps owns durability). It keeps its own
//! [`ReplicationError`](crate::replication::ReplicationError) channel rather
//! than widening the crate-wide `DrevoError`.

mod error;
mod primary;
mod replica;
mod wal;

pub use error::{ReplicationError, Result};
pub use primary::Primary;
pub use replica::Replica;
pub use wal::{Lsn, WalOp, WalRecord, WriteAheadLog};

/// The role a node plays in a replication topology.
///
/// A topology has exactly one [`Main`](Role::Main) (the writable
/// [`Primary`]) and any number of [`Replica`](Role::Replica) followers serving
/// reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// The single writable node — accepts writes and produces the log.
    Main,
    /// A read-only follower — serves reads and replays the log.
    Replica,
}

impl Role {
    /// Whether a node in this role accepts writes (only [`Main`](Role::Main)
    /// does).
    #[must_use]
    pub const fn accepts_writes(self) -> bool {
        matches!(self, Role::Main)
    }

    /// A stable lowercase string form (`"main"` / `"replica"`), e.g. for logs
    /// or a status endpoint.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Role::Main => "main",
            Role::Replica => "replica",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_accepts_writes_only_for_main() {
        assert!(Role::Main.accepts_writes());
        assert!(!Role::Replica.accepts_writes());
    }

    #[test]
    fn role_as_str() {
        assert_eq!(Role::Main.as_str(), "main");
        assert_eq!(Role::Replica.as_str(), "replica");
    }
}
