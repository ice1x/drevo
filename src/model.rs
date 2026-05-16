//! Data model types for drevo.
//!
//! Defines the core domain types: [`Node`], [`Edge`], and their
//! corresponding creation ([`NewNode`], [`NewEdge`]) and update
//! ([`NodePatch`], [`EdgePatch`]) structs.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// JSON-compatible properties map that works with both bincode and serde_json.
///
/// Bincode v2 does not support `serde_json::Value` directly (no `deserialize_any`).
/// This wrapper serializes properties as a JSON byte string for bincode,
/// while remaining transparent for JSON serialization.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Properties(pub HashMap<String, serde_json::Value>);

impl From<HashMap<String, serde_json::Value>> for Properties {
    fn from(map: HashMap<String, serde_json::Value>) -> Self {
        Self(map)
    }
}

impl std::ops::Deref for Properties {
    type Target = HashMap<String, serde_json::Value>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for Properties {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Serialize for Properties {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            self.0.serialize(serializer)
        } else {
            let json = serde_json::to_vec(&self.0).map_err(serde::ser::Error::custom)?;
            json.serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for Properties {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            let map = HashMap::<String, serde_json::Value>::deserialize(deserializer)?;
            Ok(Self(map))
        } else {
            let json = Vec::<u8>::deserialize(deserializer)?;
            let map = serde_json::from_slice(&json).map_err(serde::de::Error::custom)?;
            Ok(Self(map))
        }
    }
}

/// A node in the graph.
///
/// Nodes represent domain entities (notes, tags, people, tasks, etc.).
/// The `kind` field classifies the node, and `properties` stores
/// arbitrary JSON-compatible metadata without schema migration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Node {
    /// Auto-incremented unique identifier.
    pub id: u64,
    /// UUID v7 — sortable, globally unique.
    pub uuid: [u8; 16],
    /// Classification (e.g. "note", "tag", "person", "concept").
    pub kind: String,
    /// Human-readable title.
    pub title: String,
    /// Raw Markdown body.
    pub body: String,
    /// Rendered HTML (cached).
    pub body_html: String,
    /// Creation timestamp (Unix milliseconds).
    pub created_at: i64,
    /// Last update timestamp (Unix milliseconds).
    pub updated_at: i64,
    /// Arbitrary JSON-compatible metadata.
    pub properties: Properties,
}

/// Input for creating a new node.
///
/// Fields that the system generates (id, uuid, timestamps) are omitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewNode {
    /// Classification (e.g. "note", "tag").
    pub kind: String,
    /// Human-readable title.
    pub title: String,
    /// Raw Markdown body.
    pub body: String,
    /// Rendered HTML (cached). Defaults to empty string if not provided.
    pub body_html: String,
    /// Arbitrary JSON-compatible metadata.
    pub properties: Properties,
}

/// Partial update for an existing node.
///
/// Only `Some` fields are applied; `None` fields are left unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodePatch {
    pub kind: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub body_html: Option<String>,
    pub properties: Option<Properties>,
}

/// A directed edge connecting two nodes.
///
/// Edges represent relationships (links_to, tagged_with, depends_on, etc.).
/// The `weight` field supports ranking and weighted traversal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Edge {
    /// Auto-incremented unique identifier.
    pub id: u64,
    /// UUID v7 — sortable, globally unique.
    pub uuid: [u8; 16],
    /// Source node id.
    pub from_id: u64,
    /// Target node id.
    pub to_id: u64,
    /// Classification (e.g. "links_to", "tagged_with").
    pub kind: String,
    /// Weight for ranking/traversal (default 1.0).
    pub weight: f32,
    /// Creation timestamp (Unix milliseconds).
    pub created_at: i64,
    /// Arbitrary JSON-compatible metadata.
    pub properties: Properties,
}

/// Input for creating a new edge.
///
/// Fields that the system generates (id, uuid, created_at) are omitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewEdge {
    /// Source node id.
    pub from_id: u64,
    /// Target node id.
    pub to_id: u64,
    /// Classification (e.g. "links_to", "tagged_with").
    pub kind: String,
    /// Weight for ranking/traversal (default 1.0).
    pub weight: f32,
    /// Arbitrary JSON-compatible metadata.
    pub properties: Properties,
}

/// Partial update for an existing edge.
///
/// Only `Some` fields are applied; `None` fields are left unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EdgePatch {
    pub kind: Option<String>,
    pub weight: Option<f32>,
    pub properties: Option<Properties>,
}

/// Graph traversal direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Follow edges where `from_id` matches the given node.
    Outgoing,
    /// Follow edges where `to_id` matches the given node.
    Incoming,
    /// Follow edges in both directions.
    Both,
}

/// A search result with a relevance score.
///
/// Returned by [`Drevo::search_fts`] — nodes are ranked by TF-IDF score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoredNode {
    /// The matching node.
    pub node: Node,
    /// Relevance score (higher = more relevant).
    pub score: f32,
}

/// A subgraph extracted by bounded traversal from a root node.
///
/// Contains all nodes and edges within a given radius (depth) of
/// the root. Useful for providing bounded context to AI agents
/// (e.g. via MCP) or exporting a local neighborhood of the graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubGraph {
    /// All nodes within the traversal radius, including the root node.
    pub nodes: Vec<Node>,
    /// All edges that connect nodes within the subgraph.
    pub edges: Vec<Edge>,
}

/// Generate a new UUID v7 as a 16-byte array.
pub fn new_uuid_v7() -> [u8; 16] {
    *uuid::Uuid::now_v7().as_bytes()
}

/// Return the current time as Unix milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_millis() as i64
}

impl Node {
    /// Apply a [`NodePatch`] to this node, updating only the fields that are `Some`.
    /// Automatically updates `updated_at` timestamp.
    pub fn apply_patch(&mut self, patch: NodePatch) {
        if let Some(kind) = patch.kind {
            self.kind = kind;
        }
        if let Some(title) = patch.title {
            self.title = title;
        }
        if let Some(body) = patch.body {
            self.body = body;
        }
        if let Some(body_html) = patch.body_html {
            self.body_html = body_html;
        }
        if let Some(properties) = patch.properties {
            self.properties = properties;
        }
        self.updated_at = now_ms();
    }
}

impl Edge {
    /// Apply an [`EdgePatch`] to this edge, updating only the fields that are `Some`.
    pub fn apply_patch(&mut self, patch: EdgePatch) {
        if let Some(kind) = patch.kind {
            self.kind = kind;
        }
        if let Some(weight) = patch.weight {
            self.weight = weight;
        }
        if let Some(properties) = patch.properties {
            self.properties = properties;
        }
    }
}

impl NewNode {
    /// Create a [`Node`] from this input, assigning the given id and generating uuid/timestamps.
    pub fn into_node(self, id: u64) -> Node {
        let now = now_ms();
        Node {
            id,
            uuid: new_uuid_v7(),
            kind: self.kind,
            title: self.title,
            body: self.body,
            body_html: self.body_html,
            created_at: now,
            updated_at: now,
            properties: self.properties,
        }
    }
}

impl NewEdge {
    /// Create an [`Edge`] from this input, assigning the given id and generating uuid/timestamp.
    pub fn into_edge(self, id: u64) -> Edge {
        Edge {
            id,
            uuid: new_uuid_v7(),
            from_id: self.from_id,
            to_id: self.to_id,
            kind: self.kind,
            weight: self.weight,
            created_at: now_ms(),
            properties: self.properties,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_new_node() -> NewNode {
        let mut props = HashMap::new();
        props.insert("priority".to_string(), json!(1));
        NewNode {
            kind: "note".to_string(),
            title: "Test Node".to_string(),
            body: "# Hello\nWorld".to_string(),
            body_html: "<h1>Hello</h1><p>World</p>".to_string(),
            properties: Properties::from(props),
        }
    }

    fn sample_new_edge() -> NewEdge {
        let mut props = HashMap::new();
        props.insert("label".to_string(), json!("related"));
        NewEdge {
            from_id: 1,
            to_id: 2,
            kind: "links_to".to_string(),
            weight: 1.0,
            properties: Properties::from(props),
        }
    }

    // --- UUID v7 ---

    #[test]
    fn uuid_v7_is_valid() {
        let bytes = new_uuid_v7();
        let uuid = uuid::Uuid::from_bytes(bytes);
        assert_eq!(uuid.get_version(), Some(uuid::Version::SortRand));
    }

    #[test]
    fn uuid_v7_is_unique() {
        let a = new_uuid_v7();
        let b = new_uuid_v7();
        assert_ne!(a, b);
    }

    #[test]
    fn uuid_v7_is_sortable_by_time() {
        let a = new_uuid_v7();
        // Small delay to ensure different timestamp
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = new_uuid_v7();
        assert!(a < b, "UUID v7 should be time-sortable");
    }

    // --- now_ms ---

    #[test]
    fn now_ms_returns_reasonable_timestamp() {
        let ts = now_ms();
        // Should be after 2024-01-01 (1704067200000)
        assert!(ts > 1_704_067_200_000);
        // Should be before 2100-01-01
        assert!(ts < 4_102_444_800_000);
    }

    // --- NewNode::into_node ---

    #[test]
    fn new_node_into_node_sets_id() {
        let node = sample_new_node().into_node(42);
        assert_eq!(node.id, 42);
    }

    #[test]
    fn new_node_into_node_generates_uuid() {
        let node = sample_new_node().into_node(1);
        let uuid = uuid::Uuid::from_bytes(node.uuid);
        assert_eq!(uuid.get_version(), Some(uuid::Version::SortRand));
    }

    #[test]
    fn new_node_into_node_copies_fields() {
        let input = sample_new_node();
        let node = input.clone().into_node(1);
        assert_eq!(node.kind, input.kind);
        assert_eq!(node.title, input.title);
        assert_eq!(node.body, input.body);
        assert_eq!(node.body_html, input.body_html);
        assert_eq!(node.properties, input.properties);
    }

    #[test]
    fn new_node_into_node_sets_timestamps() {
        let before = now_ms();
        let node = sample_new_node().into_node(1);
        let after = now_ms();
        assert!(node.created_at >= before && node.created_at <= after);
        assert_eq!(node.created_at, node.updated_at);
    }

    // --- NewEdge::into_edge ---

    #[test]
    fn new_edge_into_edge_sets_id() {
        let edge = sample_new_edge().into_edge(99);
        assert_eq!(edge.id, 99);
    }

    #[test]
    fn new_edge_into_edge_copies_fields() {
        let input = sample_new_edge();
        let edge = input.clone().into_edge(1);
        assert_eq!(edge.from_id, input.from_id);
        assert_eq!(edge.to_id, input.to_id);
        assert_eq!(edge.kind, input.kind);
        assert_eq!(edge.weight, input.weight);
        assert_eq!(edge.properties, input.properties);
    }

    #[test]
    fn new_edge_into_edge_generates_uuid() {
        let edge = sample_new_edge().into_edge(1);
        let uuid = uuid::Uuid::from_bytes(edge.uuid);
        assert_eq!(uuid.get_version(), Some(uuid::Version::SortRand));
    }

    #[test]
    fn new_edge_into_edge_sets_timestamp() {
        let before = now_ms();
        let edge = sample_new_edge().into_edge(1);
        let after = now_ms();
        assert!(edge.created_at >= before && edge.created_at <= after);
    }

    // --- Node::apply_patch ---

    #[test]
    fn apply_patch_updates_only_some_fields() {
        let mut node = sample_new_node().into_node(1);
        let original_kind = node.kind.clone();
        let original_body = node.body.clone();

        node.apply_patch(NodePatch {
            title: Some("Updated Title".to_string()),
            ..Default::default()
        });

        assert_eq!(node.title, "Updated Title");
        assert_eq!(node.kind, original_kind);
        assert_eq!(node.body, original_body);
    }

    #[test]
    fn apply_patch_updates_all_fields() {
        let mut node = sample_new_node().into_node(1);
        let mut new_props = HashMap::new();
        new_props.insert("status".to_string(), json!("done"));
        let new_props = Properties::from(new_props);

        node.apply_patch(NodePatch {
            kind: Some("task".to_string()),
            title: Some("New Title".to_string()),
            body: Some("New body".to_string()),
            body_html: Some("<p>New body</p>".to_string()),
            properties: Some(new_props.clone()),
        });

        assert_eq!(node.kind, "task");
        assert_eq!(node.title, "New Title");
        assert_eq!(node.body, "New body");
        assert_eq!(node.body_html, "<p>New body</p>");
        assert_eq!(node.properties, new_props);
    }

    #[test]
    fn apply_patch_updates_timestamp() {
        let mut node = sample_new_node().into_node(1);
        let old_updated = node.updated_at;
        std::thread::sleep(std::time::Duration::from_millis(2));

        node.apply_patch(NodePatch {
            title: Some("Changed".to_string()),
            ..Default::default()
        });

        assert!(node.updated_at > old_updated);
    }

    #[test]
    fn apply_empty_patch_only_updates_timestamp() {
        let mut node = sample_new_node().into_node(1);
        let original = node.clone();
        std::thread::sleep(std::time::Duration::from_millis(2));

        node.apply_patch(NodePatch::default());

        assert_eq!(node.kind, original.kind);
        assert_eq!(node.title, original.title);
        assert!(node.updated_at > original.updated_at);
    }

    // --- Edge::apply_patch ---

    #[test]
    fn edge_apply_patch_updates_some_fields() {
        let mut edge = sample_new_edge().into_edge(1);
        let original_kind = edge.kind.clone();

        edge.apply_patch(EdgePatch {
            weight: Some(2.5),
            ..Default::default()
        });

        assert_eq!(edge.weight, 2.5);
        assert_eq!(edge.kind, original_kind);
    }

    #[test]
    fn edge_apply_patch_updates_all_fields() {
        let mut edge = sample_new_edge().into_edge(1);
        let mut new_props = HashMap::new();
        new_props.insert("color".to_string(), json!("red"));
        let new_props = Properties::from(new_props);

        edge.apply_patch(EdgePatch {
            kind: Some("blocks".to_string()),
            weight: Some(0.5),
            properties: Some(new_props.clone()),
        });

        assert_eq!(edge.kind, "blocks");
        assert_eq!(edge.weight, 0.5);
        assert_eq!(edge.properties, new_props);
    }

    // --- Serialization ---

    #[test]
    fn node_serializes_to_bincode_roundtrip() {
        let node = sample_new_node().into_node(1);
        let config = bincode::config::standard();
        let bytes = bincode::serde::encode_to_vec(&node, config).unwrap();
        let (decoded, _): (Node, _) = bincode::serde::decode_from_slice(&bytes, config).unwrap();
        assert_eq!(decoded, node);
    }

    #[test]
    fn edge_serializes_to_bincode_roundtrip() {
        let edge = sample_new_edge().into_edge(1);
        let config = bincode::config::standard();
        let bytes = bincode::serde::encode_to_vec(&edge, config).unwrap();
        let (decoded, _): (Edge, _) = bincode::serde::decode_from_slice(&bytes, config).unwrap();
        assert_eq!(decoded, edge);
    }

    #[test]
    fn node_serializes_to_json_roundtrip() {
        let node = sample_new_node().into_node(1);
        let json = serde_json::to_string(&node).unwrap();
        let decoded: Node = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, node);
    }

    #[test]
    fn edge_serializes_to_json_roundtrip() {
        let edge = sample_new_edge().into_edge(1);
        let json = serde_json::to_string(&edge).unwrap();
        let decoded: Edge = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, edge);
    }

    #[test]
    fn node_with_empty_properties_serializes() {
        let node = NewNode {
            kind: "note".to_string(),
            title: "Empty".to_string(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::default(),
        }
        .into_node(1);

        let config = bincode::config::standard();
        let bytes = bincode::serde::encode_to_vec(&node, config).unwrap();
        let (decoded, _): (Node, _) = bincode::serde::decode_from_slice(&bytes, config).unwrap();
        assert_eq!(decoded, node);
    }

    #[test]
    fn node_with_complex_properties_serializes() {
        let mut props = HashMap::new();
        props.insert("tags".to_string(), json!(["rust", "graph", "db"]));
        props.insert("nested".to_string(), json!({"a": {"b": 42}}));
        props.insert("flag".to_string(), json!(true));
        props.insert("score".to_string(), json!(2.71));
        props.insert("empty".to_string(), json!(null));

        let node = NewNode {
            kind: "note".to_string(),
            title: "Complex".to_string(),
            body: String::new(),
            body_html: String::new(),
            properties: Properties::from(props),
        }
        .into_node(1);

        let config = bincode::config::standard();
        let bytes = bincode::serde::encode_to_vec(&node, config).unwrap();
        let (decoded, _): (Node, _) = bincode::serde::decode_from_slice(&bytes, config).unwrap();
        assert_eq!(decoded, node);
    }

    // --- Direction ---

    #[test]
    fn direction_variants_are_distinct() {
        assert_ne!(Direction::Outgoing, Direction::Incoming);
        assert_ne!(Direction::Outgoing, Direction::Both);
        assert_ne!(Direction::Incoming, Direction::Both);
    }

    #[test]
    fn direction_clone_eq() {
        let d = Direction::Outgoing;
        let d2 = d;
        assert_eq!(d, d2);
    }
}
