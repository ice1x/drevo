//! Canonical byte encoding of property values for index keyspaces.
//!
//! Both the KV persistent property index and the native property-value index key
//! their postings on the *bytes* of a property value, so they must encode a
//! value identically. [`crate::value_encoding::encode_value`] is that shared encoder.
//!
//! The encoding is deterministic: drevo builds `serde_json` without the
//! `preserve_order` feature, so object keys serialize in sorted (`BTreeMap`)
//! order — the same normalization [`crate::model::Properties`] relies on for
//! stable output. Two values that are logically equal therefore encode to
//! byte-equal output (e.g. two objects that differ only in key order), and the
//! indexes match on exact canonical-byte equality.

use serde_json::Value;

use crate::error::Result;

/// Canonical byte encoding of a property value (sorted-key JSON).
///
/// # Errors
/// Returns [`crate::error::CoreError::Json`] if `serde_json` cannot serialize
/// the value (in practice unreachable for values reachable from
/// [`crate::model::Properties`], which are always encodable).
pub fn encode_value(value: &Value) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(value)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn is_canonical_across_object_key_order() {
        let a = json!({ "a": 1, "b": 2 });
        let b = json!({ "b": 2, "a": 1 });
        assert_eq!(encode_value(&a).unwrap(), encode_value(&b).unwrap());
    }

    #[test]
    fn encodes_scalars() {
        assert_eq!(encode_value(&json!("open")).unwrap(), b"\"open\"".to_vec());
        assert_eq!(encode_value(&json!(5)).unwrap(), b"5".to_vec());
    }
}
