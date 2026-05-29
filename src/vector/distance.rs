//! Similarity / distance functions over dense `f32` embeddings.
//!
//! Three measures, all operating on plain `&[f32]` slices so callers can
//! pass a [`crate::vector::Vector`], a `Vec<f32>`, or any borrowed slice
//! without an allocation:
//!
//! * [`dot_product`] — raw inner product `Σ aᵢ·bᵢ`.
//! * [`cosine_similarity`] — dot product normalized by both magnitudes,
//!   i.e. `cos θ ∈ [-1, 1]`; the standard relevance score for
//!   normalized embedding models.
//! * [`euclidean_distance`] — L2 distance `√Σ (aᵢ−bᵢ)²`.
//!
//! All three reject a dimension mismatch or an empty input with a
//! [`crate::vector::VectorError`] rather than panicking, because the
//! inputs ultimately come from user-supplied node `properties` (a JSON
//! array of arbitrary length) and a length error there must surface as a
//! recoverable error, not a crash across the FFI / Bolt boundary.
//!
//! ## "SIMD-accelerated"
//!
//! The roadmap calls for SIMD acceleration. Portable SIMD (`std::simd`)
//! is nightly-only and this crate is pinned to stable, so rather than
//! pull in a dependency or a nightly feature the hot loops are written
//! with eight independent lane accumulators over
//! [`slice::chunks_exact`]. That shape is what LLVM's auto-vectorizer
//! needs: a single `f32` accumulator forces a serial dependency chain
//! (floating-point addition is not associative, so the optimizer may not
//! reorder it), whereas eight disjoint accumulators let the back end emit
//! one packed `addps`/`fmla` per iteration on x86-AVX / aarch64-NEON.
//! The horizontal sum at the end folds the lanes together once.

use super::VectorError;

/// Lane count for the accumulator arrays. Eight `f32` lanes map onto a
/// 256-bit AVX register (or two 128-bit NEON registers) — wide enough to
/// saturate common SIMD units without overflowing the register file when
/// the optimizer keeps the whole array in vector registers.
const LANES: usize = 8;

/// Validate that two embeddings can be compared: both non-empty and of
/// equal dimension.
fn ensure_compatible(a: &[f32], b: &[f32]) -> Result<(), VectorError> {
    if a.is_empty() || b.is_empty() {
        return Err(VectorError::Empty);
    }
    if a.len() != b.len() {
        return Err(VectorError::DimensionMismatch {
            left: a.len(),
            right: b.len(),
        });
    }
    Ok(())
}

/// Inner product `Σ aᵢ·bᵢ` of two equal-length embeddings.
///
/// # Errors
///
/// * [`VectorError::Empty`] — either input is empty.
/// * [`VectorError::DimensionMismatch`] — the lengths differ.
pub fn dot_product(a: &[f32], b: &[f32]) -> Result<f32, VectorError> {
    ensure_compatible(a, b)?;
    Ok(dot_unchecked(a, b))
}

/// Cosine similarity `cos θ = (a·b) / (‖a‖·‖b‖)`, in `[-1, 1]`.
///
/// # Errors
///
/// * [`VectorError::Empty`] — either input is empty.
/// * [`VectorError::DimensionMismatch`] — the lengths differ.
/// * [`VectorError::ZeroVector`] — either input has zero magnitude, so
///   the angle is undefined (division by zero).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> Result<f32, VectorError> {
    ensure_compatible(a, b)?;
    let dot = dot_unchecked(a, b);
    let norm_a = dot_unchecked(a, a).sqrt();
    let norm_b = dot_unchecked(b, b).sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return Err(VectorError::ZeroVector);
    }
    Ok(dot / (norm_a * norm_b))
}

/// Euclidean (L2) distance `√Σ (aᵢ−bᵢ)²` between two equal-length
/// embeddings.
///
/// # Errors
///
/// * [`VectorError::Empty`] — either input is empty.
/// * [`VectorError::DimensionMismatch`] — the lengths differ.
pub fn euclidean_distance(a: &[f32], b: &[f32]) -> Result<f32, VectorError> {
    ensure_compatible(a, b)?;
    let mut acc = [0.0f32; LANES];
    let mut ca = a.chunks_exact(LANES);
    let mut cb = b.chunks_exact(LANES);
    for (x, y) in ca.by_ref().zip(cb.by_ref()) {
        for l in 0..LANES {
            let d = x[l] - y[l];
            acc[l] += d * d;
        }
    }
    let mut sum: f32 = acc.iter().sum();
    for (x, y) in ca.remainder().iter().zip(cb.remainder()) {
        let d = x - y;
        sum += d * d;
    }
    Ok(sum.sqrt())
}

/// Inner product without the compatibility check — callers must have
/// already verified equal, non-zero length (or be passing the same slice
/// twice, as the norm computations in [`cosine_similarity`] do).
fn dot_unchecked(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; LANES];
    let mut ca = a.chunks_exact(LANES);
    let mut cb = b.chunks_exact(LANES);
    for (x, y) in ca.by_ref().zip(cb.by_ref()) {
        for l in 0..LANES {
            acc[l] += x[l] * y[l];
        }
    }
    let mut sum: f32 = acc.iter().sum();
    for (x, y) in ca.remainder().iter().zip(cb.remainder()) {
        sum += x * y;
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn dot_product_basic() {
        let a = [1.0, 2.0, 3.0];
        let b = [4.0, 5.0, 6.0];
        // 4 + 10 + 18 = 32
        assert!(approx(dot_product(&a, &b).unwrap(), 32.0));
    }

    #[test]
    fn dot_product_spans_lane_boundary() {
        // 20 elements > LANES (8) so both the chunked loop and the
        // scalar remainder run.
        let a: Vec<f32> = (1..=20).map(|n| n as f32).collect();
        let b = vec![2.0f32; 20];
        let expected: f32 = (1..=20).map(|n| (n as f32) * 2.0).sum();
        assert!(approx(dot_product(&a, &b).unwrap(), expected));
    }

    #[test]
    fn cosine_identical_is_one() {
        let a = [1.0, 2.0, 3.0, 4.0];
        assert!(approx(cosine_similarity(&a, &a).unwrap(), 1.0));
    }

    #[test]
    fn cosine_opposite_is_minus_one() {
        let a = [1.0, 2.0, 3.0];
        let b = [-1.0, -2.0, -3.0];
        assert!(approx(cosine_similarity(&a, &b).unwrap(), -1.0));
    }

    #[test]
    fn cosine_orthogonal_is_zero() {
        let a = [1.0, 0.0];
        let b = [0.0, 1.0];
        assert!(approx(cosine_similarity(&a, &b).unwrap(), 0.0));
    }

    #[test]
    fn cosine_zero_vector_errors() {
        let a = [0.0, 0.0, 0.0];
        let b = [1.0, 2.0, 3.0];
        assert!(matches!(
            cosine_similarity(&a, &b),
            Err(VectorError::ZeroVector)
        ));
    }

    #[test]
    fn euclidean_basic() {
        let a = [0.0, 0.0];
        let b = [3.0, 4.0];
        // sqrt(9 + 16) = 5
        assert!(approx(euclidean_distance(&a, &b).unwrap(), 5.0));
    }

    #[test]
    fn euclidean_identical_is_zero() {
        let a = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert!(approx(euclidean_distance(&a, &a).unwrap(), 0.0));
    }

    #[test]
    fn dimension_mismatch_errors() {
        let a = [1.0, 2.0, 3.0];
        let b = [1.0, 2.0];
        assert!(matches!(
            dot_product(&a, &b),
            Err(VectorError::DimensionMismatch { left: 3, right: 2 })
        ));
        assert!(matches!(
            cosine_similarity(&a, &b),
            Err(VectorError::DimensionMismatch { left: 3, right: 2 })
        ));
        assert!(matches!(
            euclidean_distance(&a, &b),
            Err(VectorError::DimensionMismatch { left: 3, right: 2 })
        ));
    }

    #[test]
    fn empty_errors() {
        let a: [f32; 0] = [];
        let b: [f32; 0] = [];
        assert!(matches!(dot_product(&a, &b), Err(VectorError::Empty)));
        assert!(matches!(cosine_similarity(&a, &b), Err(VectorError::Empty)));
        assert!(matches!(
            euclidean_distance(&a, &b),
            Err(VectorError::Empty)
        ));
    }
}
