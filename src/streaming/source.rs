//! The broker abstraction — where messages come from and how progress is
//! tracked.
//!
//! drevo does not bundle a Kafka or NATS client (that would drag a heavyweight,
//! non-WASM dependency into an always-compiled crate). Instead it abstracts a
//! broker partition behind the [`StreamSource`] trait: *poll a batch of
//! messages, commit the offset you have durably processed*. A real deployment
//! implements this trait over `rdkafka`, `async-nats`, an HTTP long-poll, or a
//! CDC tail; the engine and its tests drive the in-memory [`MemorySource`].
//!
//! # Offsets and at-least-once delivery
//!
//! Every message carries a monotonically increasing [`Offset`] — exactly the
//! Kafka partition-offset / NATS sequence model. A consumer processes messages
//! and periodically [`commit`](StreamSource::commit)s the highest offset it has
//! durably applied. After a crash it resumes from
//! [`committed`](StreamSource::committed) — which means messages processed but
//! not yet committed are **re-delivered**. This is at-least-once delivery, and
//! it is why every [`IngestEvent`](crate::streaming::IngestEvent) is designed to
//! be idempotent: replaying a window converges on the same graph state.

/// A monotonically increasing position within a stream partition.
///
/// [`Offset::ZERO`] is the sentinel for "before any message": a fresh source
/// has [`committed`](StreamSource::committed) equal to `ZERO`, and the first
/// message produced is [`Offset`]`(1)`. This mirrors the [`Lsn`] convention in
/// the replication engine.
///
/// [`Lsn`]: crate::replication::Lsn
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Offset(pub u64);

impl Offset {
    /// The sentinel "before any message" offset.
    pub const ZERO: Offset = Offset(0);

    /// The next offset after this one.
    #[must_use]
    pub const fn next(self) -> Offset {
        Offset(self.0 + 1)
    }
}

impl std::fmt::Display for Offset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One message pulled from a [`StreamSource`]: an opaque payload stamped with
/// its [`Offset`].
///
/// The payload is the raw broker message body — typically a JSON
/// [`IngestEvent`](crate::streaming::IngestEvent), decoded by the consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamMessage {
    /// The broker offset of this message within its partition.
    pub offset: Offset,
    /// The raw, undecoded message body.
    pub payload: Vec<u8>,
}

/// A pollable, committable source of stream messages — the broker-partition
/// abstraction the ingestion engine consumes.
///
/// Implementors wrap a concrete transport (Kafka, NATS, CDC, an in-process
/// channel). The contract:
///
/// * [`poll`](Self::poll) returns the next un-delivered messages in ascending
///   offset order, at most `max` of them, advancing an internal in-flight
///   cursor. An empty return means "caught up for now".
/// * [`commit`](Self::commit) durably records that everything up to and
///   including `offset` has been processed.
/// * [`committed`](Self::committed) reports the last committed offset; a
///   restarted consumer resumes from here.
pub trait StreamSource {
    /// Pull up to `max` not-yet-delivered messages in ascending offset order.
    /// Returns an empty `Vec` when the partition is drained.
    fn poll(&mut self, max: usize) -> Vec<StreamMessage>;

    /// Durably record that every message up to and including `offset` has been
    /// processed. Offsets at or below the current committed mark are ignored
    /// (commits never move backwards).
    fn commit(&mut self, offset: Offset);

    /// The highest offset durably committed so far ([`Offset::ZERO`] if none).
    fn committed(&self) -> Offset;
}

/// An in-memory [`StreamSource`] backing the engine's tests and any embedder
/// that wants to feed events from a `Vec` (e.g. replaying a captured topic).
///
/// Messages are appended with [`push`](Self::push) / [`push_event`](Self::push_event),
/// assigned ascending offsets starting at `1`. The source models broker
/// semantics faithfully enough to exercise at-least-once delivery:
///
/// * [`poll`](StreamSource::poll) hands out messages after the in-flight
///   cursor and advances it.
/// * [`rewind_to_committed`](Self::rewind_to_committed) resets the in-flight
///   cursor back to the committed mark — simulating a consumer crash /
///   partition rebalance that re-delivers everything not yet committed.
#[derive(Debug, Default)]
pub struct MemorySource {
    messages: Vec<StreamMessage>,
    /// Index into `messages` of the next message `poll` will return.
    cursor: usize,
    committed: Offset,
}

impl MemorySource {
    /// Create an empty source positioned at [`Offset::ZERO`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a raw payload, assigning it the next ascending [`Offset`], and
    /// return that offset.
    pub fn push(&mut self, payload: Vec<u8>) -> Offset {
        let offset = Offset(self.messages.len() as u64 + 1);
        self.messages.push(StreamMessage { offset, payload });
        offset
    }

    /// Append an [`IngestEvent`](crate::streaming::IngestEvent) as a JSON
    /// payload, assigning it the next ascending [`Offset`].
    ///
    /// # Errors
    ///
    /// Propagates a [`serde_json::Error`] if the event fails to serialize
    /// (effectively never).
    pub fn push_event(
        &mut self,
        event: &crate::streaming::IngestEvent,
    ) -> serde_json::Result<Offset> {
        Ok(self.push(event.to_json()?))
    }

    /// Reset the in-flight cursor to the committed mark, so the next
    /// [`poll`](StreamSource::poll) re-delivers every message processed but not
    /// yet committed. Models a consumer restart under at-least-once delivery.
    pub fn rewind_to_committed(&mut self) {
        // The committed offset is 1-based; messages strictly after it are at
        // index `committed.0` onward.
        self.cursor = self.committed.0 as usize;
    }

    /// The total number of messages ever appended (regardless of cursor /
    /// commit position).
    #[must_use]
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Whether no messages have been appended.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

impl StreamSource for MemorySource {
    fn poll(&mut self, max: usize) -> Vec<StreamMessage> {
        let end = (self.cursor + max).min(self.messages.len());
        let batch = self.messages[self.cursor..end].to_vec();
        self.cursor = end;
        batch
    }

    fn commit(&mut self, offset: Offset) {
        if offset > self.committed {
            self.committed = offset;
        }
    }

    fn committed(&self) -> Offset {
        self.committed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_source_is_empty_at_zero() {
        let src = MemorySource::new();
        assert!(src.is_empty());
        assert_eq!(src.len(), 0);
        assert_eq!(src.committed(), Offset::ZERO);
    }

    #[test]
    fn push_assigns_monotonic_offsets_from_one() {
        let mut src = MemorySource::new();
        assert_eq!(src.push(b"a".to_vec()), Offset(1));
        assert_eq!(src.push(b"b".to_vec()), Offset(2));
        assert_eq!(src.len(), 2);
    }

    #[test]
    fn poll_returns_batches_then_drains() {
        let mut src = MemorySource::new();
        for i in 0..5u8 {
            src.push(vec![i]);
        }
        let first = src.poll(3);
        assert_eq!(first.len(), 3);
        assert_eq!(first[0].offset, Offset(1));
        assert_eq!(first[2].offset, Offset(3));
        let second = src.poll(3);
        assert_eq!(second.len(), 2);
        assert_eq!(second[0].offset, Offset(4));
        // Drained.
        assert!(src.poll(3).is_empty());
    }

    #[test]
    fn commit_only_moves_forward() {
        let mut src = MemorySource::new();
        src.commit(Offset(5));
        assert_eq!(src.committed(), Offset(5));
        // A stale commit is ignored.
        src.commit(Offset(3));
        assert_eq!(src.committed(), Offset(5));
        src.commit(Offset(6));
        assert_eq!(src.committed(), Offset(6));
    }

    #[test]
    fn rewind_redelivers_uncommitted_messages() {
        let mut src = MemorySource::new();
        for i in 0..4u8 {
            src.push(vec![i]);
        }
        // Process all four, but commit only the first two.
        let _ = src.poll(4);
        src.commit(Offset(2));
        // Crash + restart: re-deliver everything after the committed mark.
        src.rewind_to_committed();
        let redelivered = src.poll(10);
        assert_eq!(redelivered.len(), 2);
        assert_eq!(redelivered[0].offset, Offset(3));
        assert_eq!(redelivered[1].offset, Offset(4));
    }

    #[test]
    fn offset_next_and_display() {
        assert_eq!(Offset(0).next(), Offset(1));
        assert_eq!(format!("{}", Offset(9)), "9");
    }
}
