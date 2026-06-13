//! Streaming ingestion — Phase 15 task `00096`.
//!
//! A knowledge graph rarely lives in isolation: domain events flow in from
//! message brokers (a Kafka topic of journal entries, a NATS subject of task
//! updates, a CDC tail of an upstream Postgres). This module is the
//! **transport-agnostic ingestion engine** that turns that firehose of change
//! events into graph mutations, with the delivery guarantees a real broker
//! demands.
//!
//! It models streaming ingestion the way every consumer does — pull a batch,
//! decode it, apply it, commit the offset:
//!
//! * [`IngestEvent`](crate::streaming::IngestEvent) — the decoded form of one
//!   broker message: upsert / delete a node or edge, addressed by a
//!   producer-owned [`EntityKey`](crate::streaming::EntityKey) (never drevo's
//!   internal id). Travels as tagged JSON; every upsert carries full state so
//!   replay is idempotent.
//! * [`StreamSource`](crate::streaming::StreamSource) — the broker-partition
//!   abstraction ([`poll`](crate::streaming::StreamSource::poll) /
//!   [`commit`](crate::streaming::StreamSource::commit) /
//!   [`committed`](crate::streaming::StreamSource::committed)) with monotonic
//!   [`Offset`](crate::streaming::Offset)s. drevo ships the in-memory
//!   [`MemorySource`](crate::streaming::MemorySource); a deployment implements
//!   the trait over `rdkafka`, `async-nats`, or an HTTP long-poll.
//! * [`IngestSink`](crate::streaming::IngestSink) — where decoded events land.
//!   The reference [`MemoryGraphSink`](crate::streaming::MemoryGraphSink)
//!   materializes the stream into a key-addressed node/edge map; a deployment
//!   implements the trait over a live [`Drevo`](crate::db::Drevo) handle.
//! * [`IngestConsumer`](crate::streaming::IngestConsumer) — the engine that
//!   drives a source into a sink under an
//!   [`ErrorPolicy`](crate::streaming::ErrorPolicy), tracking offsets for
//!   at-least-once, idempotent ingestion and routing un-ingestable messages to
//!   a [`DeadLetter`](crate::streaming::DeadLetter) queue.
//!
//! # The ingestion loop
//!
//! ```text
//! let mut consumer = IngestConsumer::new().resume_after(source.committed());
//! loop {
//!     let report = consumer.run_once(&mut source, &mut sink)?;
//!     if report.polled == 0 { break; }   // caught up
//! }
//! ```
//!
//! Each round polls a bounded batch, applies the good events, dead-letters the
//! bad ones (or halts, per policy), and commits the high-water offset. After a
//! crash the consumer resumes from the committed offset; the source re-delivers
//! everything past it (at-least-once), and because every
//! [`IngestEvent`](crate::streaming::IngestEvent) is idempotent — upsert is
//! last-writer-wins, delete-of-absent is a no-op — the replayed window
//! converges on the same graph state.
//!
//! # Scope
//!
//! Like the replication (`00095`), authorization (`00094`), and planner
//! (`00085`–`00089`) engines, this is the ingestion *substrate* — the event
//! model, the broker/sink seams, the offset/error/idempotency machinery — and
//! is **not yet wired into the executor / HTTP API / Bolt session**, nor does
//! it bundle a concrete broker client: a deployment supplies a
//! [`StreamSource`](crate::streaming::StreamSource) over its transport of
//! choice (see [`Transport`](crate::streaming::Transport)). It is
//! dependency-free (only `serde`/`serde_json`/`std`), always compiled, and
//! WASM-safe — it holds opaque payloads and performs no I/O of its own. It
//! keeps its own [`StreamError`](crate::streaming::StreamError) channel rather
//! than widening the crate-wide `DrevoError`.

mod consumer;
mod error;
mod event;
mod sink;
mod source;

pub use consumer::{DeadLetter, ErrorPolicy, IngestConsumer, IngestReport};
pub use error::{Result, StreamError};
pub use event::{EntityKey, EventProperties, IngestEvent};
pub use sink::{EdgeRecord, IngestSink, MemoryGraphSink, NodeRecord};
pub use source::{MemorySource, Offset, StreamMessage, StreamSource};

/// The kind of broker transport a [`StreamSource`] represents.
///
/// drevo's engine is transport-agnostic — it consumes any [`StreamSource`] — so
/// this enum carries no behaviour of its own. It exists to label a source for
/// logs, metrics, and a status endpoint ("which broker is this partition
/// from?"), the way [`Role`](crate::replication::Role) labels a replication
/// node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Transport {
    /// Apache Kafka (or an API-compatible broker: Redpanda, Confluent).
    Kafka,
    /// NATS / NATS JetStream.
    Nats,
    /// Change-data-capture tail of an upstream database (e.g. Postgres logical
    /// replication, Debezium).
    Cdc,
    /// An in-process source ([`MemorySource`]) — tests and embedded replay.
    InMemory,
}

impl Transport {
    /// A stable lowercase string form (`"kafka"`, `"nats"`, `"cdc"`,
    /// `"in_memory"`), e.g. for a metrics label or status endpoint.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Transport::Kafka => "kafka",
            Transport::Nats => "nats",
            Transport::Cdc => "cdc",
            Transport::InMemory => "in_memory",
        }
    }

    /// Whether this transport provides durable, replayable offsets (Kafka,
    /// NATS JetStream, and CDC do; a bare in-memory source does not survive a
    /// process restart).
    #[must_use]
    pub const fn is_durable(self) -> bool {
        matches!(self, Transport::Kafka | Transport::Nats | Transport::Cdc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_as_str() {
        assert_eq!(Transport::Kafka.as_str(), "kafka");
        assert_eq!(Transport::Nats.as_str(), "nats");
        assert_eq!(Transport::Cdc.as_str(), "cdc");
        assert_eq!(Transport::InMemory.as_str(), "in_memory");
    }

    #[test]
    fn durable_transports_are_replayable() {
        assert!(Transport::Kafka.is_durable());
        assert!(Transport::Nats.is_durable());
        assert!(Transport::Cdc.is_durable());
        assert!(!Transport::InMemory.is_durable());
    }
}
