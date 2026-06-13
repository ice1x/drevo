//! The change-event model — what a broker message decodes into.
//!
//! A streaming producer (a Kafka topic, a NATS subject, a CDC pipeline) emits
//! a flat sequence of *change events*: "this entity now looks like this",
//! "this entity is gone". This module defines the typed form of those events
//! ([`IngestEvent`]) and the JSON wire format they travel in.
//!
//! # Keys, not ids
//!
//! Events reference entities by a **producer-owned key** ([`EntityKey`] — an
//! arbitrary opaque string such as a Kafka message key, a source-system primary
//! key, or a UUID), never by drevo's internal auto-increment node/edge id. The
//! producer cannot know what id drevo will assign, and the same logical entity
//! must map to the same graph element across re-deliveries. The sink
//! ([`crate::streaming::IngestSink`]) owns the key → graph mapping; the engine
//! only routes events.
//!
//! Edges reference their endpoints by the *node* keys [`from`](IngestEvent) and
//! [`to`](IngestEvent), so an edge event is self-describing without first
//! resolving its endpoints to ids.
//!
//! # Wire format
//!
//! Each message body is a JSON object tagged by an `op` field:
//!
//! ```json
//! {"op":"upsert_node","key":"note-42","kind":"note","title":"…","body":"…","properties":{}}
//! {"op":"delete_node","key":"note-42"}
//! {"op":"upsert_edge","key":"link-7","from":"note-42","to":"tag-cbt","kind":"tagged_with","weight":1.0,"properties":{}}
//! {"op":"delete_edge","key":"link-7"}
//! ```
//!
//! Upserts carry the *full* desired state of the entity (last-writer-wins);
//! there is no partial-patch event, because a stream consumer that missed an
//! earlier message cannot reconstruct a patch's base. This makes every upsert
//! idempotent: replaying it converges on the same state.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A producer-owned, opaque entity identifier carried in every event.
///
/// drevo never interprets the bytes — it is the *sink's* job to map a key to a
/// concrete node or edge and to keep that mapping stable across re-deliveries.
pub type EntityKey = String;

/// JSON-compatible property bag carried on upsert events.
///
/// Mirrors the shape of [`crate::model::Properties`] on the wire (a plain JSON
/// object) without depending on its bincode-specific serialization, since
/// events are always human-readable JSON.
pub type EventProperties = HashMap<String, serde_json::Value>;

/// A single change event — the decoded form of one broker message.
///
/// The four variants mirror the create/delete half of the graph data model:
/// nodes and edges, each upserted (last-writer-wins, idempotent) or deleted
/// (idempotent — deleting an absent entity is a no-op).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum IngestEvent {
    /// Create the node under `key`, or replace it if it already exists.
    UpsertNode {
        /// Producer-owned stable identity for this node.
        key: EntityKey,
        /// Node classification (e.g. `"note"`, `"task"`, `"person"`).
        kind: String,
        /// Human-readable title.
        title: String,
        /// Raw Markdown body. Defaults to empty when omitted.
        #[serde(default)]
        body: String,
        /// Arbitrary metadata. Defaults to empty when omitted.
        #[serde(default)]
        properties: EventProperties,
    },

    /// Remove the node under `key` if present; a no-op otherwise.
    DeleteNode {
        /// Producer-owned stable identity of the node to remove.
        key: EntityKey,
    },

    /// Create the edge under `key`, or replace it if it already exists.
    UpsertEdge {
        /// Producer-owned stable identity for this edge.
        key: EntityKey,
        /// Key of the source node.
        from: EntityKey,
        /// Key of the target node.
        to: EntityKey,
        /// Edge classification (e.g. `"links_to"`, `"tagged_with"`).
        kind: String,
        /// Ranking / traversal weight. Defaults to `1.0` when omitted.
        #[serde(default = "default_weight")]
        weight: f32,
        /// Arbitrary metadata. Defaults to empty when omitted.
        #[serde(default)]
        properties: EventProperties,
    },

    /// Remove the edge under `key` if present; a no-op otherwise.
    DeleteEdge {
        /// Producer-owned stable identity of the edge to remove.
        key: EntityKey,
    },
}

/// The default edge weight (`1.0`), matching [`crate::model::NewEdge`].
fn default_weight() -> f32 {
    1.0
}

impl IngestEvent {
    /// The producer-owned [`EntityKey`] this event targets, regardless of
    /// variant.
    #[must_use]
    pub fn key(&self) -> &EntityKey {
        match self {
            IngestEvent::UpsertNode { key, .. }
            | IngestEvent::DeleteNode { key }
            | IngestEvent::UpsertEdge { key, .. }
            | IngestEvent::DeleteEdge { key } => key,
        }
    }

    /// Whether this event removes an entity (rather than upserting one).
    #[must_use]
    pub const fn is_delete(&self) -> bool {
        matches!(
            self,
            IngestEvent::DeleteNode { .. } | IngestEvent::DeleteEdge { .. }
        )
    }

    /// Whether this event targets an edge (rather than a node).
    #[must_use]
    pub const fn is_edge(&self) -> bool {
        matches!(
            self,
            IngestEvent::UpsertEdge { .. } | IngestEvent::DeleteEdge { .. }
        )
    }

    /// Decode an event from a JSON message payload.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`serde_json::Error`] when the bytes are not a
    /// valid tagged event object. The streaming consumer maps this into
    /// [`StreamError::Parse`](crate::streaming::StreamError::Parse), stamping it
    /// with the broker offset.
    pub fn from_json(bytes: &[u8]) -> serde_json::Result<Self> {
        serde_json::from_slice(bytes)
    }

    /// Encode this event to a JSON message payload.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`serde_json::Error`] if serialization fails
    /// (effectively never for this type, but propagated rather than panicked).
    pub fn to_json(&self) -> serde_json::Result<Vec<u8>> {
        serde_json::to_vec(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_node_round_trips_through_json() {
        let ev = IngestEvent::UpsertNode {
            key: "note-1".into(),
            kind: "note".into(),
            title: "Hello".into(),
            body: "body text".into(),
            properties: HashMap::from([("mood".into(), serde_json::json!("calm"))]),
        };
        let bytes = ev.to_json().unwrap();
        let back = IngestEvent::from_json(&bytes).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn body_and_properties_default_when_omitted() {
        let ev =
            IngestEvent::from_json(br#"{"op":"upsert_node","key":"k","kind":"note","title":"T"}"#)
                .unwrap();
        match ev {
            IngestEvent::UpsertNode {
                body, properties, ..
            } => {
                assert_eq!(body, "");
                assert!(properties.is_empty());
            }
            other => panic!("expected UpsertNode, got {other:?}"),
        }
    }

    #[test]
    fn edge_weight_defaults_to_one() {
        let ev = IngestEvent::from_json(
            br#"{"op":"upsert_edge","key":"e","from":"a","to":"b","kind":"links_to"}"#,
        )
        .unwrap();
        match ev {
            IngestEvent::UpsertEdge { weight, .. } => assert_eq!(weight, 1.0),
            other => panic!("expected UpsertEdge, got {other:?}"),
        }
    }

    #[test]
    fn key_accessor_covers_every_variant() {
        assert_eq!(IngestEvent::DeleteNode { key: "n".into() }.key(), "n");
        assert_eq!(IngestEvent::DeleteEdge { key: "e".into() }.key(), "e");
    }

    #[test]
    fn classification_predicates() {
        let upsert_node = IngestEvent::UpsertNode {
            key: "k".into(),
            kind: "note".into(),
            title: "T".into(),
            body: String::new(),
            properties: EventProperties::new(),
        };
        assert!(!upsert_node.is_delete());
        assert!(!upsert_node.is_edge());

        let del_edge = IngestEvent::DeleteEdge { key: "e".into() };
        assert!(del_edge.is_delete());
        assert!(del_edge.is_edge());
    }

    #[test]
    fn malformed_payload_is_an_error_not_a_panic() {
        assert!(IngestEvent::from_json(b"not json").is_err());
        // Unknown op tag is rejected.
        assert!(IngestEvent::from_json(br#"{"op":"frobnicate","key":"k"}"#).is_err());
        // Missing required field (title) is rejected.
        assert!(
            IngestEvent::from_json(br#"{"op":"upsert_node","key":"k","kind":"note"}"#).is_err()
        );
    }
}
