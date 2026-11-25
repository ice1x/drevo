//! What the Web UI's graph overview draws: every node and relationship with
//! its kind, its title and its small property values.
//!
//! `GET /export/json` is a backup: it carries bodies, embeddings and every
//! other property, 115 MB for a store of 4582 nodes. The overview draws
//! circles, titles and lines, colours them by a short text property and shows
//! a sample; it needs none of the large values. [`overview`] keeps a property
//! whose JSON text is at most 256 bytes long and names the others
//! under `omitted`, so a client knows there is more to fetch
//! (`GET /nodes/{id}`):
//!
//! ```json
//! {"nodes": [{"id": 1, "kind": "Entity", "title": "drevo",
//!             "properties": {"status": "done"}, "omitted": ["embedding"]}],
//!  "edges": [{"id": 7, "from_id": 1, "to_id": 2, "kind": "USES",
//!             "weight": 1.0, "properties": {}}]}
//! ```
//!
//! Nodes and relationships come in id order. `omitted` is present only when
//! something was left out, with the keys sorted.

use std::io::{self, Write};

use serde_json::{json, Map, Value};

use crate::dump::Dump;
use crate::model::Properties;

/// The longest JSON text of a property value the overview carries, in bytes.
/// Private: a `pub const` here would land in the C header as a bare
/// `#define`.
const MAX_VALUE_BYTES: usize = 256;

/// The overview of `dump` as a JSON value (see the module docs).
pub fn overview(dump: &Dump) -> Value {
    let mut nodes: Vec<_> = dump.nodes.iter().collect();
    nodes.sort_by_key(|n| n.id);
    let mut edges: Vec<_> = dump.edges.iter().collect();
    edges.sort_by_key(|e| e.id);
    let nodes: Vec<Value> = nodes
        .into_iter()
        .map(|n| {
            with_properties(
                json!({"id": n.id, "kind": n.kind, "title": n.title}),
                &n.properties,
            )
        })
        .collect();
    let edges: Vec<Value> = edges
        .into_iter()
        .map(|e| {
            with_properties(
                json!({
                    "id": e.id,
                    "from_id": e.from_id,
                    "to_id": e.to_id,
                    "kind": e.kind,
                    "weight": e.weight,
                }),
                &e.properties,
            )
        })
        .collect();
    json!({"nodes": nodes, "edges": edges})
}

/// `item` with the small values of `properties` under `properties` and the
/// keys of the others, if any, under `omitted`.
fn with_properties(mut item: Value, properties: &Properties) -> Value {
    let all = match serde_json::to_value(properties) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    let mut kept = Map::new();
    let mut omitted = Vec::new();
    for (key, value) in all {
        if is_small(&value) {
            kept.insert(key, value);
        } else {
            omitted.push(Value::String(key));
        }
    }
    omitted.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    if let Value::Object(map) = &mut item {
        map.insert("properties".to_string(), Value::Object(kept));
        if !omitted.is_empty() {
            map.insert("omitted".to_string(), Value::Array(omitted));
        }
    }
    item
}

/// Whether `value` takes at most `MAX_VALUE_BYTES` as compact JSON. The
/// value is not written out in full: counting stops at the limit, so a
/// 1536-number embedding costs as much to refuse as a short list.
fn is_small(value: &Value) -> bool {
    struct Budget(usize);
    impl Write for Budget {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if buf.len() > self.0 {
                return Err(io::Error::other("over the limit"));
            }
            self.0 -= buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(MAX_VALUE_BYTES), value).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_is_small_up_to_the_limit_of_its_json_text() {
        assert!(is_small(&json!(null)));
        assert!(is_small(&json!(true)));
        assert!(is_small(&json!(12345.678)));
        assert!(is_small(&json!("a".repeat(MAX_VALUE_BYTES - 2))));
        assert!(!is_small(&json!("a".repeat(MAX_VALUE_BYTES - 1))));
        assert!(is_small(&json!(["a", "b", {"c": 1}])));
        let vector: Vec<f64> = (0..1536).map(|i| i as f64 / 7.0).collect();
        assert!(!is_small(&json!(vector)));
    }

    #[test]
    fn a_string_is_measured_as_json_with_its_escapes() {
        // 127 quotes are 254 bytes once escaped, 256 with the outer quotes.
        assert!(is_small(&json!("\"".repeat(127))));
        assert!(!is_small(&json!("\"".repeat(128))));
    }

    #[test]
    fn omitted_is_absent_when_everything_fits() {
        let item = with_properties(json!({"id": 1}), &Properties::default());
        assert_eq!(item, json!({"id": 1, "properties": {}}));
    }
}
