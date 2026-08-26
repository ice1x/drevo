//! The `_labels` secondary-label convention.
//!
//! drevo stores a node's primary label in [`crate::model::Node::kind`], but
//! Cypher lets a node carry any number of labels. The extras live in a reserved
//! property, [`crate::labels::SECONDARY_LABELS_KEY`], as a JSON array of strings
//! — never surfaced through user-level `n.<prop>` access (the executor filters it
//! out when it builds the visible property map).
//!
//! [`crate::labels::secondary_labels`] is the single source of truth for parsing that array, so
//! every consumer — the Cypher executor's label matching and the native
//! secondary-label index — reads labels identically and can never drift.

use crate::model::Node;

/// The reserved property key under which a node's secondary labels are stored,
/// as a JSON array of strings.
pub const SECONDARY_LABELS_KEY: &str = "_labels";

/// The secondary labels carried by `node` in its reserved
/// [`SECONDARY_LABELS_KEY`] property (a JSON array of strings), in stored order.
/// Empty when the property is absent or malformed.
pub fn secondary_labels(node: &Node) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(serde_json::Value::Array(arr)) = node.properties.0.get(SECONDARY_LABELS_KEY) {
        for item in arr {
            if let serde_json::Value::String(s) = item {
                out.push(s.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Node, Properties};
    use serde_json::json;
    use std::collections::HashMap;

    fn node_with(props: serde_json::Value) -> Node {
        let mut map = HashMap::new();
        if let serde_json::Value::Object(obj) = props {
            for (k, v) in obj {
                map.insert(k, v);
            }
        }
        Node {
            id: 1,
            uuid: [0u8; 16],
            kind: "thing".into(),
            title: String::new(),
            body: String::new(),
            body_html: String::new(),
            created_at: 0,
            updated_at: 0,
            properties: Properties(map),
        }
    }

    #[test]
    fn reads_the_stored_string_array_in_order() {
        let n = node_with(json!({ SECONDARY_LABELS_KEY: ["A", "B", "C"] }));
        assert_eq!(secondary_labels(&n), vec!["A", "B", "C"]);
    }

    #[test]
    fn absent_property_yields_empty() {
        let n = node_with(json!({ "other": 1 }));
        assert!(secondary_labels(&n).is_empty());
    }

    #[test]
    fn non_string_items_are_skipped() {
        let n = node_with(json!({ SECONDARY_LABELS_KEY: ["A", 2, "C"] }));
        assert_eq!(secondary_labels(&n), vec!["A", "C"]);
    }

    #[test]
    fn non_array_value_yields_empty() {
        let n = node_with(json!({ SECONDARY_LABELS_KEY: "A" }));
        assert!(secondary_labels(&n).is_empty());
    }
}
