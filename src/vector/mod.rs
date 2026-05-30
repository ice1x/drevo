//! Phase 12 task `00075` — dense `f32` vector embeddings and the
//! similarity / distance functions that operate on them.
//!
//! This module is the foundation the rest of Phase 12 builds on: the
//! HNSW index (`00076`) measures candidate distances with the functions
//! in [`distance`](crate::vector::distance), the joint graph+vector
//! Cypher predicate (`00077`) evaluates `similar(n.embedding, $q, 0.85)`
//! through [`cosine_similarity`](crate::vector::distance::cosine_similarity),
//! and the persistence layer (`00078`) stores the same
//! [`Vector`](crate::vector::Vector) payloads.
//!
//! ## Why a newtype over `Vec<f32>`
//!
//! Embeddings live inside node `properties`, which are stored as
//! arbitrary [`serde_json::Value`]. A raw JSON array carries no
//! guarantee that its elements are finite numbers, so the conversion
//! [`Vector::from_json_value`](crate::vector::Vector::from_json_value)
//! validates every element once at the boundary and hands back a
//! [`Vector`](crate::vector::Vector) that the distance functions can
//! trust. [`Vector::to_json_value`](crate::vector::Vector::to_json_value)
//! is the inverse — it serializes back
//! into the JSON shape stored on a node. Keeping that round-trip in one
//! place means callers never sprinkle `as f64` / `as_f64()` casts (and
//! their silent NaN traps) across the codebase.
//!
//! The type is dependency-free and always compiled — no Cargo feature
//! gate — because both the embedded sync path and the future Bolt /
//! Python surfaces need it.

/// Cosine similarity, Euclidean distance, and dot product over `&[f32]`.
pub mod distance;
/// HNSW approximate-nearest-neighbor index over [`Vector`] embeddings
/// (`00076`).
pub mod hnsw;
/// Durable embedding store — redb-backed persistence and a batched
/// insert API for [`Vector`] payloads (`00078`).
pub mod store;

pub use distance::{cosine_similarity, dot_product, euclidean_distance};
pub use hnsw::{HnswConfig, HnswIndex, Metric, Neighbor};

/// Errors raised by vector construction and the distance functions.
///
/// Every failure mode is recoverable: the inputs originate from
/// user-supplied node `properties` (an arbitrary JSON array), so a bad
/// length or a non-numeric element must surface as an error the HTTP /
/// Bolt / FFI layers can map to a client response — never a panic.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VectorError {
    /// The two operands have different lengths, so an element-wise
    /// measure is undefined.
    #[error("vector dimension mismatch: {left} vs {right}")]
    DimensionMismatch {
        /// Length of the left-hand operand.
        left: usize,
        /// Length of the right-hand operand.
        right: usize,
    },

    /// At least one operand is empty; a zero-dimension vector has no
    /// meaningful similarity.
    #[error("vector is empty")]
    Empty,

    /// Cosine similarity is undefined when either operand has zero
    /// magnitude (the angle between a point and the origin is
    /// undefined — it would divide by zero).
    #[error("cosine similarity is undefined for a zero-magnitude vector")]
    ZeroVector,

    /// The JSON value handed to [`Vector::from_json_value`] was not an
    /// array.
    #[error("expected a JSON array of numbers, found {0}")]
    NotAnArray(&'static str),

    /// An element of the JSON array was not a number.
    #[error("vector element at index {index} is not a number")]
    NonNumeric {
        /// Position of the offending element.
        index: usize,
    },

    /// An element was a number but not finite (NaN or ±∞), which a
    /// distance function cannot use and JSON cannot represent.
    #[error("vector element at index {index} is not finite")]
    NonFinite {
        /// Position of the offending element.
        index: usize,
    },
}

/// A dense embedding: a validated, finite `Vec<f32>`.
///
/// Construct one directly from a `Vec<f32>` / slice via the [`From`]
/// impls (used in tests and trusted internal code) or from untrusted
/// JSON via [`Vector::from_json_value`] (used at the API boundary).
/// [`Deref`](std::ops::Deref) to `[f32]` means a `&Vector` is accepted
/// anywhere the distance functions want a `&[f32]`.
///
/// The [`serde`](https://serde.rs) derives let the persistence layer
/// (`00078`) round-trip a `Vector` through `bincode` into the durable
/// embedding store; serialization is the bare `Vec<f32>` (a newtype is
/// transparent), so the on-disk form carries no extra framing.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Vector(pub Vec<f32>);

impl Vector {
    /// Number of dimensions in this embedding.
    #[must_use]
    pub fn dim(&self) -> usize {
        self.0.len()
    }

    /// Borrow the underlying components as a slice.
    #[must_use]
    pub fn as_slice(&self) -> &[f32] {
        &self.0
    }

    /// Parse a [`serde_json::Value`] array into a [`Vector`], validating
    /// that every element is a finite number.
    ///
    /// This is the boundary check for embeddings read out of node
    /// `properties`: storage hands back arbitrary JSON, and this is where
    /// it becomes a trusted vector.
    ///
    /// # Errors
    ///
    /// * [`VectorError::NotAnArray`] — the value is not a JSON array.
    /// * [`VectorError::NonNumeric`] — an element is not a number.
    /// * [`VectorError::NonFinite`] — an element is NaN or ±∞.
    pub fn from_json_value(value: &serde_json::Value) -> Result<Self, VectorError> {
        let arr = value
            .as_array()
            .ok_or_else(|| VectorError::NotAnArray(json_type_name(value)))?;
        let mut out = Vec::with_capacity(arr.len());
        for (index, elem) in arr.iter().enumerate() {
            let n = elem.as_f64().ok_or(VectorError::NonNumeric { index })?;
            if !n.is_finite() {
                return Err(VectorError::NonFinite { index });
            }
            out.push(n as f32);
        }
        Ok(Self(out))
    }

    /// Serialize this embedding back into the [`serde_json::Value`]
    /// array form stored on a node.
    ///
    /// Components are guaranteed finite (the only constructors that admit
    /// non-finite values are the raw [`From`] impls on trusted internal
    /// data), so the `f64 → Number` conversion always succeeds; should a
    /// non-finite value have slipped through, it serializes as JSON
    /// `null` rather than producing invalid JSON.
    #[must_use]
    pub fn to_json_value(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.0
                .iter()
                .map(|&x| {
                    serde_json::Number::from_f64(f64::from(x))
                        .map_or(serde_json::Value::Null, serde_json::Value::Number)
                })
                .collect(),
        )
    }
}

impl From<Vec<f32>> for Vector {
    fn from(v: Vec<f32>) -> Self {
        Self(v)
    }
}

impl From<&[f32]> for Vector {
    fn from(v: &[f32]) -> Self {
        Self(v.to_vec())
    }
}

impl std::ops::Deref for Vector {
    type Target = [f32];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Human-readable JSON type name for the [`VectorError::NotAnArray`]
/// message.
fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn from_json_value_parses_numbers() {
        let v = Vector::from_json_value(&json!([1.0, 2, 3.5])).unwrap();
        assert_eq!(v.as_slice(), &[1.0, 2.0, 3.5]);
        assert_eq!(v.dim(), 3);
    }

    #[test]
    fn from_json_value_rejects_non_array() {
        assert!(matches!(
            Vector::from_json_value(&json!("nope")),
            Err(VectorError::NotAnArray("string"))
        ));
        assert!(matches!(
            Vector::from_json_value(&json!(42)),
            Err(VectorError::NotAnArray("number"))
        ));
    }

    #[test]
    fn from_json_value_rejects_non_numeric_element() {
        assert!(matches!(
            Vector::from_json_value(&json!([1.0, "x", 3.0])),
            Err(VectorError::NonNumeric { index: 1 })
        ));
    }

    #[test]
    fn to_json_value_round_trips() {
        let original = Vector::from(vec![0.5, -1.25, 3.0]);
        let json = original.to_json_value();
        let back = Vector::from_json_value(&json).unwrap();
        assert_eq!(original, back);
    }

    #[test]
    fn deref_feeds_distance_functions() {
        let a = Vector::from(vec![1.0, 2.0, 3.0]);
        let b = Vector::from(vec![1.0, 2.0, 3.0]);
        // &Vector coerces to &[f32] via Deref.
        let sim = cosine_similarity(&a, &b).unwrap();
        assert!((sim - 1.0).abs() < 1e-5);
    }
}
