//! How long a failed ingest item waits before it is tried again.
//!
//! The wait doubles with every attempt — `base × 2^attempts`, at most
//! [`MAX_DELAY`] — and a jitter spreads the retries of many items that
//! failed together (a rate limit hit by every worker at once) over the
//! second half of that wait. [`next`] is a pure function of the attempt
//! count, the base and a jitter draw in `[0, 1)`; [`Jitter`] makes the draws,
//! from a seed when a test needs them repeatable.

use std::time::Duration;

/// The longest wait between two attempts: 5 minutes.
pub(crate) const MAX_DELAY: Duration = Duration::from_secs(300);

/// The wait after attempt number `attempts` (1 for the first) failed:
/// `base × 2^attempts`, capped at [`MAX_DELAY`], of which the second half is
/// scaled by `jitter` (clamped to `[0, 1]`) — so the wait lies between half
/// the capped delay and the whole of it.
pub(crate) fn next(attempts: u32, base: Duration, jitter: f64) -> Duration {
    let cap = MAX_DELAY.as_nanos();
    let factor = 1_u128.checked_shl(attempts).unwrap_or(u128::MAX);
    let full = base.as_nanos().saturating_mul(factor).min(cap);
    let jitter = if jitter.is_nan() {
        0.0
    } else {
        jitter.clamp(0.0, 1.0)
    };
    let half = full / 2;
    // `full` is at most 300 s in nanoseconds, well inside u64 and exact in f64.
    let spread = (half as f64 * jitter) as u128;
    let nanos = (full - half) + spread.min(half);
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

/// A small, seedable source of jitter draws (xorshift64*).
#[derive(Debug, Clone)]
pub(crate) struct Jitter(u64);

impl Jitter {
    /// Draws that repeat for the same `seed`.
    pub(crate) fn seeded(seed: u64) -> Self {
        let state = seed ^ 0x9E37_79B9_7F4A_7C15;
        Self(if state == 0 {
            0x2545_F491_4F6C_DD1D
        } else {
            state
        })
    }

    /// The next draw, in `[0, 1)`.
    pub(crate) fn draw(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        let out = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (out >> 11) as f64 / (1_u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: Duration = Duration::from_millis(2000);

    #[test]
    fn the_full_jitter_wait_doubles_with_each_attempt() {
        let waits: Vec<u64> = (1..=6)
            .map(|a| next(a, BASE, 1.0).as_millis() as u64)
            .collect();
        assert_eq!(waits, [4_000, 8_000, 16_000, 32_000, 64_000, 128_000]);
    }

    #[test]
    fn the_wait_is_capped_at_five_minutes() {
        assert_eq!(next(8, BASE, 1.0), MAX_DELAY);
        assert_eq!(next(30, BASE, 1.0), MAX_DELAY);
        // No overflow, however many attempts.
        assert_eq!(next(u32::MAX, BASE, 1.0), MAX_DELAY);
        assert_eq!(next(u32::MAX, BASE, 0.0), MAX_DELAY / 2);
    }

    #[test]
    fn jitter_keeps_the_wait_between_half_and_all_of_the_delay() {
        for attempts in 1..12 {
            let full = next(attempts, BASE, 1.0);
            for jitter in [0.0, 0.25, 0.5, 0.999] {
                let wait = next(attempts, BASE, jitter);
                assert!(
                    wait >= full / 2 && wait <= full,
                    "{attempts} {jitter} {wait:?}"
                );
            }
            assert_eq!(next(attempts, BASE, 0.0), full / 2);
        }
        // Out-of-range draws are clamped.
        assert_eq!(next(1, BASE, -3.0), next(1, BASE, 0.0));
        assert_eq!(next(1, BASE, 7.0), next(1, BASE, 1.0));
        assert_eq!(next(1, BASE, f64::NAN), next(1, BASE, 0.0));
    }

    #[test]
    fn a_zero_attempt_count_waits_the_base() {
        assert_eq!(next(0, BASE, 1.0), BASE);
    }

    #[test]
    fn seeded_draws_repeat_and_stay_in_range() {
        let a: Vec<f64> = {
            let mut j = Jitter::seeded(42);
            (0..100).map(|_| j.draw()).collect()
        };
        let b: Vec<f64> = {
            let mut j = Jitter::seeded(42);
            (0..100).map(|_| j.draw()).collect()
        };
        assert_eq!(a, b);
        assert!(a.iter().all(|d| (0.0..1.0).contains(d)), "{a:?}");
        let distinct: std::collections::BTreeSet<u64> = a.iter().map(|d| d.to_bits()).collect();
        assert!(distinct.len() > 90, "draws barely vary");
        let mut other = Jitter::seeded(7);
        assert_ne!(a[0], other.draw());
        // A zero seed still varies.
        let mut zero = Jitter::seeded(0);
        assert_ne!(zero.draw(), zero.draw());
    }
}
