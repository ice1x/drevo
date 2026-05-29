//! Integration tests for vector similarity / distance — Phase 12 task
//! `00075`.
//!
//! Exercises the public `drevo::vector` surface the way the upcoming
//! HNSW index (`00076`) and the joint graph+vector Cypher predicate
//! (`00077`) will: build [`Vector`]s from the JSON shape stored on a
//! node, run cosine / euclidean / dot measures over them, and confirm
//! that bad inputs surface as recoverable errors rather than panics.

use drevo::vector::{cosine_similarity, dot_product, euclidean_distance, Vector, VectorError};
use serde_json::json;

fn approx(a: f32, b: f32) {
    assert!((a - b).abs() < 1e-5, "expected {b}, got {a}");
}

#[test]
fn cosine_ranks_more_similar_higher() {
    // A query embedding and two candidates; the candidate pointing in a
    // closer direction must score higher. This is the core operation the
    // similarity predicate (00077) will rank on.
    let query = Vector::from(vec![1.0, 0.0, 0.0]);
    let near = Vector::from(vec![0.9, 0.1, 0.0]);
    let far = Vector::from(vec![0.0, 1.0, 0.0]);

    let near_score = cosine_similarity(&query, &near).unwrap();
    let far_score = cosine_similarity(&query, &far).unwrap();
    assert!(near_score > far_score);
    approx(far_score, 0.0); // orthogonal
}

#[test]
fn distance_functions_agree_with_hand_computation() {
    let a = Vector::from(vec![1.0, 2.0, 3.0, 4.0]);
    let b = Vector::from(vec![2.0, 0.0, 1.0, 4.0]);

    // dot: 2 + 0 + 3 + 16 = 21
    approx(dot_product(&a, &b).unwrap(), 21.0);
    // euclidean: sqrt(1 + 4 + 4 + 0) = 3
    approx(euclidean_distance(&a, &b).unwrap(), 3.0);
}

#[test]
fn high_dimensional_embedding_crosses_lane_boundary() {
    // 384 dims (a common sentence-embedding size) drives the chunked
    // lane-accumulator loop hard with no scalar remainder, plus a 100-dim
    // case (100 % 8 = 4 leftover) that also exercises the remainder path.
    for dim in [100usize, 384] {
        let a = Vector::from(vec![1.0f32; dim]);
        let b = Vector::from(vec![1.0f32; dim]);
        approx(cosine_similarity(&a, &b).unwrap(), 1.0);
        approx(euclidean_distance(&a, &b).unwrap(), 0.0);
        approx(dot_product(&a, &b).unwrap(), dim as f32);
    }
}

#[test]
fn json_round_trip_preserves_embedding() {
    // Embeddings are stored as a JSON array in node `properties`; the
    // value read back must reconstruct the same vector.
    let stored = json!([0.12, -0.45, 0.78, 1.0, -1.0]);
    let v = Vector::from_json_value(&stored).unwrap();
    let again = v.to_json_value();
    let v2 = Vector::from_json_value(&again).unwrap();
    assert_eq!(v, v2);
}

#[test]
fn integer_json_elements_parse_as_floats() {
    // JSON has no float/int distinction in our property model; a vector
    // serialized with whole numbers must still parse.
    let v = Vector::from_json_value(&json!([1, 2, 3])).unwrap();
    assert_eq!(v.as_slice(), &[1.0, 2.0, 3.0]);
}

#[test]
fn malformed_json_is_a_recoverable_error() {
    assert!(matches!(
        Vector::from_json_value(&json!({"x": 1})),
        Err(VectorError::NotAnArray("object"))
    ));
    assert!(matches!(
        Vector::from_json_value(&json!([1.0, null, 3.0])),
        Err(VectorError::NonNumeric { index: 1 })
    ));
}

#[test]
fn dimension_mismatch_errors_across_all_measures() {
    let a = Vector::from(vec![1.0, 2.0, 3.0]);
    let b = Vector::from(vec![1.0, 2.0]);
    let expected = VectorError::DimensionMismatch { left: 3, right: 2 };
    assert_eq!(cosine_similarity(&a, &b), Err(expected.clone()));
    assert_eq!(euclidean_distance(&a, &b), Err(expected.clone()));
    assert_eq!(dot_product(&a, &b), Err(expected));
}

#[test]
fn zero_vector_has_undefined_cosine() {
    let zero = Vector::from(vec![0.0, 0.0, 0.0]);
    let other = Vector::from(vec![1.0, 2.0, 3.0]);
    assert_eq!(
        cosine_similarity(&zero, &other),
        Err(VectorError::ZeroVector)
    );
    // dot and euclidean remain well-defined against a zero vector.
    approx(dot_product(&zero, &other).unwrap(), 0.0);
    approx(euclidean_distance(&zero, &zero).unwrap(), 0.0);
}

#[test]
fn empty_vector_errors() {
    let empty = Vector::from(Vec::<f32>::new());
    assert_eq!(cosine_similarity(&empty, &empty), Err(VectorError::Empty));
    assert_eq!(dot_product(&empty, &empty), Err(VectorError::Empty));
    assert_eq!(euclidean_distance(&empty, &empty), Err(VectorError::Empty));
}
