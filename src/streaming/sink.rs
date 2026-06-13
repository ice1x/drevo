//! The sink abstraction — where decoded events land.
//!
//! The consumer decodes broker messages into [`IngestEvent`]s and hands each to
//! a [`IngestSink`]. The trait is the seam between the transport-agnostic engine
//! and whatever actually mutates state: a live [`crate::db::Drevo`] handle, a
//! staging buffer, a metrics tap, or — in tests and lightweight embedders — the
//! reference [`MemoryGraphSink`] in this module.
//!
//! Keeping the sink behind a trait is what lets the engine stay dependency-free
//! and WASM-safe (the same reasoning the replication engine applies by writing
//! through a [`StorageBackend`](crate::storage::StorageBackend)): the engine
//! never names a concrete database type.
//!
//! # Idempotency contract
//!
//! A sink **must** treat upserts as last-writer-wins (create-or-replace) and
//! deletes of absent entities as no-ops. Combined with the at-least-once
//! delivery of [`StreamSource`](crate::streaming::StreamSource), this is what
//! makes re-delivery safe: replaying a window of events converges on the same
//! graph state.

use crate::streaming::event::{EntityKey, EventProperties, IngestEvent};
use std::collections::HashMap;

/// Applies decoded [`IngestEvent`]s to some backing store.
///
/// Implementors map a producer-owned [`EntityKey`](crate::streaming::event::EntityKey)
/// to a concrete graph element and keep that mapping stable across
/// re-deliveries. An error is reported as an opaque [`String`]; the consumer
/// wraps it in [`StreamError::Sink`](crate::streaming::StreamError::Sink),
/// stamping it with the broker offset.
pub trait IngestSink {
    /// Apply one event. Upserts create-or-replace; deletes of absent entities
    /// are no-ops.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message when the event cannot be applied (e.g.
    /// a dangling edge endpoint or a storage failure). The consumer's
    /// [`ErrorPolicy`](crate::streaming::ErrorPolicy) decides whether that halts
    /// ingestion or routes the message to the dead-letter queue.
    fn apply(&mut self, event: &IngestEvent) -> core::result::Result<(), String>;
}

/// A node as materialized by [`MemoryGraphSink`].
#[derive(Debug, Clone, PartialEq)]
pub struct NodeRecord {
    /// Node classification carried by the upsert.
    pub kind: String,
    /// Human-readable title.
    pub title: String,
    /// Raw Markdown body.
    pub body: String,
    /// Arbitrary metadata.
    pub properties: EventProperties,
}

/// An edge as materialized by [`MemoryGraphSink`].
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeRecord {
    /// Key of the source node.
    pub from: EntityKey,
    /// Key of the target node.
    pub to: EntityKey,
    /// Edge classification.
    pub kind: String,
    /// Ranking / traversal weight.
    pub weight: f32,
    /// Arbitrary metadata.
    pub properties: EventProperties,
}

/// An in-memory reference [`IngestSink`] that materializes the event stream into
/// a key-addressed node/edge map.
///
/// It is the canonical, dependency-free sink: it demonstrates the idempotency
/// contract (upsert = replace, delete = remove), lets tests assert the
/// materialized graph state after a run, and serves as a staging buffer an
/// embedder can flush into a real [`crate::db::Drevo`] in one batch.
///
/// By default it accepts every event. To exercise the consumer's error
/// handling, [`reject_keys`](Self::reject_keys) marks specific keys to fail on.
/// To enforce referential integrity, [`require_edge_endpoints`](Self::require_edge_endpoints)
/// makes an edge upsert fail when either endpoint node is unknown.
#[derive(Debug, Default)]
pub struct MemoryGraphSink {
    nodes: HashMap<EntityKey, NodeRecord>,
    edges: HashMap<EntityKey, EdgeRecord>,
    reject: std::collections::HashSet<EntityKey>,
    require_endpoints: bool,
}

impl MemoryGraphSink {
    /// Create an empty sink that accepts every event.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reject any event whose [`key`](IngestEvent::key) is in `keys`, returning
    /// a sink error. Used to drive dead-letter / halt behaviour in tests.
    #[must_use]
    pub fn reject_keys<I, S>(mut self, keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<EntityKey>,
    {
        self.reject = keys.into_iter().map(Into::into).collect();
        self
    }

    /// Require both endpoints of an edge upsert to already exist as nodes;
    /// otherwise the upsert fails with a dangling-endpoint error.
    #[must_use]
    pub fn require_edge_endpoints(mut self) -> Self {
        self.require_endpoints = true;
        self
    }

    /// The materialized node under `key`, if present.
    #[must_use]
    pub fn node(&self, key: &str) -> Option<&NodeRecord> {
        self.nodes.get(key)
    }

    /// The materialized edge under `key`, if present.
    #[must_use]
    pub fn edge(&self, key: &str) -> Option<&EdgeRecord> {
        self.edges.get(key)
    }

    /// The number of nodes currently materialized.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// The number of edges currently materialized.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }
}

impl IngestSink for MemoryGraphSink {
    fn apply(&mut self, event: &IngestEvent) -> core::result::Result<(), String> {
        if self.reject.contains(event.key()) {
            return Err(format!("configured to reject key {}", event.key()));
        }
        match event {
            IngestEvent::UpsertNode {
                key,
                kind,
                title,
                body,
                properties,
            } => {
                self.nodes.insert(
                    key.clone(),
                    NodeRecord {
                        kind: kind.clone(),
                        title: title.clone(),
                        body: body.clone(),
                        properties: properties.clone(),
                    },
                );
            }
            IngestEvent::DeleteNode { key } => {
                self.nodes.remove(key);
            }
            IngestEvent::UpsertEdge {
                key,
                from,
                to,
                kind,
                weight,
                properties,
            } => {
                if self.require_endpoints
                    && (!self.nodes.contains_key(from) || !self.nodes.contains_key(to))
                {
                    return Err(format!(
                        "dangling edge {key}: endpoint(s) {from} -> {to} not present"
                    ));
                }
                self.edges.insert(
                    key.clone(),
                    EdgeRecord {
                        from: from.clone(),
                        to: to.clone(),
                        kind: kind.clone(),
                        weight: *weight,
                        properties: properties.clone(),
                    },
                );
            }
            IngestEvent::DeleteEdge { key } => {
                self.edges.remove(key);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upsert_node(key: &str, title: &str) -> IngestEvent {
        IngestEvent::UpsertNode {
            key: key.into(),
            kind: "note".into(),
            title: title.into(),
            body: String::new(),
            properties: EventProperties::new(),
        }
    }

    #[test]
    fn upsert_then_replace_is_last_writer_wins() {
        let mut sink = MemoryGraphSink::new();
        sink.apply(&upsert_node("n1", "first")).unwrap();
        sink.apply(&upsert_node("n1", "second")).unwrap();
        assert_eq!(sink.node_count(), 1);
        assert_eq!(sink.node("n1").unwrap().title, "second");
    }

    #[test]
    fn reapplying_identical_upsert_is_idempotent() {
        let mut sink = MemoryGraphSink::new();
        let ev = upsert_node("n1", "stable");
        sink.apply(&ev).unwrap();
        let after_first = sink.node("n1").cloned();
        sink.apply(&ev).unwrap();
        assert_eq!(sink.node_count(), 1);
        assert_eq!(sink.node("n1").cloned(), after_first);
    }

    #[test]
    fn delete_is_idempotent_noop_when_absent() {
        let mut sink = MemoryGraphSink::new();
        sink.apply(&IngestEvent::DeleteNode {
            key: "ghost".into(),
        })
        .unwrap();
        assert_eq!(sink.node_count(), 0);
    }

    #[test]
    fn reject_keys_surfaces_a_sink_error() {
        let mut sink = MemoryGraphSink::new().reject_keys(["bad"]);
        let err = sink.apply(&upsert_node("bad", "x")).unwrap_err();
        assert!(err.contains("bad"));
        // A non-rejected key still applies.
        assert!(sink.apply(&upsert_node("ok", "y")).is_ok());
    }

    #[test]
    fn require_endpoints_rejects_dangling_edges() {
        let mut sink = MemoryGraphSink::new().require_edge_endpoints();
        let edge = IngestEvent::UpsertEdge {
            key: "e1".into(),
            from: "a".into(),
            to: "b".into(),
            kind: "links_to".into(),
            weight: 1.0,
            properties: EventProperties::new(),
        };
        assert!(sink.apply(&edge).is_err());
        // Once both endpoints exist, the edge applies.
        sink.apply(&upsert_node("a", "A")).unwrap();
        sink.apply(&upsert_node("b", "B")).unwrap();
        assert!(sink.apply(&edge).is_ok());
        assert_eq!(sink.edge_count(), 1);
    }
}
