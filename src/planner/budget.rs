//! Memory budget & backpressure — Phase 14 task `00089`.
//!
//! The cost-based planner (`00085`–`00088`) tells us *how many rows* an
//! operator is expected to produce; this module turns that into a guard
//! against running out of memory while executing a query on a large graph.
//! Three cooperating pieces, mirroring the standalone-engine discipline the
//! rest of the planner and the MVCC module keep:
//!
//! * [`MemoryBudget`] — a lock-free, clonable byte accountant. A query (or any
//!   producer) calls [`try_reserve`](MemoryBudget::try_reserve) before
//!   buffering rows; the budget either hands back an RAII
//!   [`MemoryReservation`] (which releases the bytes on drop) or refuses with
//!   [`BudgetError::MemoryBudgetExceeded`]. This is the **OOM guard**: a query
//!   that would blow the cap fails with a recoverable error instead of
//!   actually exhausting process memory and aborting.
//! * [`estimate_peak_memory`] / [`MemoryBudget::admits`] — **memory-limited
//!   query execution** at plan time. Given a planned [`PlanNode`] tree and a
//!   per-row width, the model sums the working set of every *blocking*
//!   operator (the ones that must materialise rows — `DISTINCT` projection and
//!   the build side of a cartesian product) and refuses to admit a plan whose
//!   estimated peak exceeds the budget, before a single row is read.
//! * [`Backpressure`] — a high/low-watermark throttle with hysteresis. A
//!   streaming producer consults [`observe`](Backpressure::observe) and pauses
//!   when memory crosses the high mark, resuming only once it falls back below
//!   the low mark, so it never oscillates on the boundary.
//!
//! Like the rest of the planner, this is dependency-free (atomics + `std`
//! only, no spawned threads), always compiled, and WASM-safe. It keeps its own
//! [`BudgetError`] channel rather than touching the crate-wide
//! [`crate::error::DrevoError`]; the budget is a standalone mechanism the
//! executor-wiring task will consume, exactly as `00085`–`00088` left the
//! planner.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use super::plan::{Operator, PlanNode};

/// A coarse default for the in-memory width of one result row, in bytes, used
/// when a caller has no better figure. One of the planner's documented
/// `DEFAULT_*` magic numbers (see [`crate::planner::cardinality`]): a handful
/// of bound values — node ids, short property scalars — at a conservative
/// ~64 bytes each. Callers that know their projection width should pass it
/// directly to [`estimate_peak_memory`] / [`MemoryBudget::admits`].
pub const DEFAULT_ROW_WIDTH_BYTES: usize = 64;

/// Errors raised by the [`MemoryBudget`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BudgetError {
    /// A reservation (or a plan admission) was refused because granting it
    /// would push live usage past the configured limit. The query should fail
    /// with this recoverable error rather than be allowed to exhaust memory —
    /// the **OOM guard**. Carries the offending request alongside the live
    /// `used` total and the `limit` so the caller can report a precise
    /// out-of-budget message.
    #[error(
        "memory budget exceeded: reserving {requested} byte(s) on top of {used} already in use \
         would pass the {limit}-byte limit"
    )]
    MemoryBudgetExceeded {
        /// Bytes the refused request asked for.
        requested: usize,
        /// Bytes already reserved when the request was made.
        used: usize,
        /// The configured budget ceiling.
        limit: usize,
    },
}

/// Convenience alias for fallible budget operations.
pub type Result<T> = std::result::Result<T, BudgetError>;

/// Shared, atomically-updated budget state behind every [`MemoryBudget`] clone
/// and every live [`MemoryReservation`].
#[derive(Debug)]
struct BudgetInner {
    /// The hard ceiling. [`usize::MAX`] marks an unlimited budget.
    limit: usize,
    /// Bytes currently reserved.
    used: AtomicUsize,
    /// High-water mark of `used` over the budget's lifetime (observability).
    peak: AtomicUsize,
}

/// A lock-free, clonable memory accountant — the **OOM guard** for query
/// execution.
///
/// Construct one with a byte [`new`](MemoryBudget::new) ceiling (or
/// [`unlimited`](MemoryBudget::unlimited)). Before buffering rows a producer
/// calls [`try_reserve`](MemoryBudget::try_reserve), which either hands back a
/// [`MemoryReservation`] RAII guard that releases the bytes on drop, or refuses
/// with [`BudgetError::MemoryBudgetExceeded`]. All clones share one underlying
/// counter, so a budget can be handed to several worker threads at once
/// (`Arc`-backed atomics — `Send + Sync`, no lock to poison).
#[derive(Debug, Clone)]
pub struct MemoryBudget {
    inner: Arc<BudgetInner>,
}

impl MemoryBudget {
    /// A budget that refuses to let live usage exceed `limit_bytes`.
    pub fn new(limit_bytes: usize) -> Self {
        Self {
            inner: Arc::new(BudgetInner {
                limit: limit_bytes,
                used: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
            }),
        }
    }

    /// A budget with no ceiling — every reservation succeeds. Modelled as a
    /// [`usize::MAX`] limit; [`is_unlimited`](MemoryBudget::is_unlimited)
    /// reports it.
    pub fn unlimited() -> Self {
        Self::new(usize::MAX)
    }

    /// The configured ceiling in bytes ([`usize::MAX`] when unlimited).
    pub fn limit(&self) -> usize {
        self.inner.limit
    }

    /// `true` when this budget has no effective ceiling.
    pub fn is_unlimited(&self) -> bool {
        self.inner.limit == usize::MAX
    }

    /// Bytes currently reserved across all live reservations.
    pub fn used(&self) -> usize {
        self.inner.used.load(Ordering::Acquire)
    }

    /// Bytes still available before the ceiling (saturating at 0). Always
    /// [`usize::MAX`] for an [`unlimited`](MemoryBudget::unlimited) budget.
    pub fn available(&self) -> usize {
        self.inner.limit.saturating_sub(self.used())
    }

    /// The high-water mark of [`used`](MemoryBudget::used) over this budget's
    /// lifetime — the largest simultaneous reservation total seen so far.
    pub fn peak(&self) -> usize {
        self.inner.peak.load(Ordering::Acquire)
    }

    /// Try to reserve `bytes`. On success returns a [`MemoryReservation`] that
    /// releases the bytes when dropped; on failure returns
    /// [`BudgetError::MemoryBudgetExceeded`] and leaves usage untouched.
    ///
    /// Lock-free: a compare-and-swap loop on the shared counter, so concurrent
    /// reservers can never jointly overshoot the limit (whoever loses the CAS
    /// re-reads the new total and re-checks against the ceiling). A
    /// zero-byte reservation always succeeds.
    pub fn try_reserve(&self, bytes: usize) -> Result<MemoryReservation> {
        let mut current = self.inner.used.load(Ordering::Acquire);
        loop {
            let new = current.saturating_add(bytes);
            if new > self.inner.limit {
                return Err(BudgetError::MemoryBudgetExceeded {
                    requested: bytes,
                    used: current,
                    limit: self.inner.limit,
                });
            }
            match self.inner.used.compare_exchange_weak(
                current,
                new,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.bump_peak(new);
                    return Ok(MemoryReservation {
                        inner: Arc::clone(&self.inner),
                        bytes,
                    });
                }
                Err(actual) => current = actual,
            }
        }
    }

    /// Pre-flight **memory-limited query execution**: estimate the peak memory
    /// `plan` would materialise at `row_width` bytes per row (see
    /// [`estimate_peak_memory`]) and refuse the whole plan up front if that
    /// exceeds the budget's ceiling — before any row is read. A plan that fits
    /// returns `Ok(())`; it does *not* reserve anything (runtime reservation is
    /// [`try_reserve`](MemoryBudget::try_reserve)'s job).
    pub fn admits(&self, plan: &PlanNode, row_width: usize) -> Result<()> {
        let needed = estimate_peak_memory(plan, row_width);
        let limit = self.inner.limit as u128;
        if needed as u128 > limit {
            return Err(BudgetError::MemoryBudgetExceeded {
                requested: saturating_usize(needed),
                used: self.used(),
                limit: self.inner.limit,
            });
        }
        Ok(())
    }

    /// Raise the recorded peak to at least `value` (monotonic max via CAS).
    fn bump_peak(&self, value: usize) {
        let mut peak = self.inner.peak.load(Ordering::Relaxed);
        while value > peak {
            match self.inner.peak.compare_exchange_weak(
                peak,
                value,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => peak = actual,
            }
        }
    }
}

/// An RAII handle to a granted slice of a [`MemoryBudget`]. Holds its bytes
/// reserved for as long as it is alive and releases them on drop, so a query
/// that returns (or unwinds) early never leaks budget. Owns an `Arc` to the
/// shared state, so it is `Send + 'static` and can be moved into a worker
/// thread alongside the work it accounts for.
#[derive(Debug)]
pub struct MemoryReservation {
    inner: Arc<BudgetInner>,
    bytes: usize,
}

impl MemoryReservation {
    /// The number of bytes this reservation holds.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for MemoryReservation {
    fn drop(&mut self) {
        // Release is infallible: we only ever subtract what we added, so the
        // counter cannot underflow. `fetch_sub` wraps on underflow, which a
        // correct accounting never reaches.
        self.inner.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// The throttle decision a [`Backpressure`] hands back from
/// [`observe`](Backpressure::observe).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackpressureSignal {
    /// Keep producing — usage is within the comfortable band.
    Continue,
    /// Memory just crossed the high-water mark: the producer should stop
    /// buffering new work until a later [`Resume`](BackpressureSignal::Resume).
    Pause,
    /// Memory fell back below the low-water mark after a pause: the producer
    /// may resume.
    Resume,
}

/// A high/low-watermark throttle with hysteresis — the **backpressure** signal
/// a streaming producer (ingestion, a paged result feed) consults so it never
/// buffers without bound.
///
/// [`observe`](Backpressure::observe) flips to [`Pause`](BackpressureSignal::Pause)
/// the first time usage reaches the high mark and stays paused — returning
/// [`Continue`](BackpressureSignal::Continue) — until usage falls to or below
/// the low mark, when it emits [`Resume`](BackpressureSignal::Resume) once. The
/// gap between the two marks is the hysteresis band that stops the signal from
/// oscillating every row when usage hovers near a single threshold.
#[derive(Debug)]
pub struct Backpressure {
    low: usize,
    high: usize,
    paused: AtomicBool,
}

impl Backpressure {
    /// A throttle that pauses at `high_watermark` bytes and resumes at or below
    /// `low_watermark` bytes.
    ///
    /// # Panics
    /// Panics if `low_watermark > high_watermark` — an inverted band has no
    /// coherent hysteresis. (Equal marks are allowed: a zero-width band.)
    pub fn new(low_watermark: usize, high_watermark: usize) -> Self {
        assert!(
            low_watermark <= high_watermark,
            "backpressure low watermark ({low_watermark}) must not exceed high watermark \
             ({high_watermark})"
        );
        Self {
            low: low_watermark,
            high: high_watermark,
            paused: AtomicBool::new(false),
        }
    }

    /// Derive watermarks from a fraction of a [`MemoryBudget`]'s ceiling: pause
    /// at `high_fraction` of the limit, resume at `low_fraction`. Fractions are
    /// clamped to `0.0..=1.0` and ordered so `low <= high`. On an
    /// [`unlimited`](MemoryBudget::unlimited) budget the marks land near
    /// [`usize::MAX`], so it effectively never pauses.
    pub fn from_budget(budget: &MemoryBudget, low_fraction: f64, high_fraction: f64) -> Self {
        let limit = budget.limit() as f64;
        let lo = (limit * low_fraction.clamp(0.0, 1.0)) as usize;
        let hi = (limit * high_fraction.clamp(0.0, 1.0)) as usize;
        Self::new(lo.min(hi), lo.max(hi))
    }

    /// The byte level at which production pauses.
    pub fn high_watermark(&self) -> usize {
        self.high
    }

    /// The byte level at which production resumes.
    pub fn low_watermark(&self) -> usize {
        self.low
    }

    /// `true` while the throttle is in its paused state.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// Feed the current `used_bytes` and get the throttle decision, advancing
    /// the internal paused/running state with hysteresis (see the type docs).
    pub fn observe(&self, used_bytes: usize) -> BackpressureSignal {
        if self.paused.load(Ordering::Acquire) {
            if used_bytes <= self.low {
                self.paused.store(false, Ordering::Release);
                BackpressureSignal::Resume
            } else {
                BackpressureSignal::Continue
            }
        } else if used_bytes >= self.high {
            self.paused.store(true, Ordering::Release);
            BackpressureSignal::Pause
        } else {
            BackpressureSignal::Continue
        }
    }
}

/// Estimate the peak bytes a planned query materialises at `row_width` bytes
/// per row — the figure [`MemoryBudget::admits`] checks against the ceiling.
///
/// drevo's executor is pipelined: most operators (scans, expands, filters,
/// `LIMIT`/`SKIP`, plain projection) stream one row at a time and hold only
/// `O(1)` rows. The memory cost comes from *blocking* operators that must
/// buffer a working set:
///
/// * a `DISTINCT` [`Projection`](Operator::Projection) holds its whole
///   deduplicated output (its `estimated_rows`); and
/// * a [`CartesianProduct`](Operator::CartesianProduct) buffers its smaller
///   *build* input (the lesser of the two children's `estimated_rows`) to probe
///   against the streamed side.
///
/// The estimate sums every blocking operator's buffered rows across the tree —
/// a deliberately conservative upper bound (a plan *could* hold several
/// materialisations live at once), which is the right bias for an OOM guard.
/// Streaming-only plans estimate to zero peak working set. Saturates at
/// [`u64::MAX`] rather than overflowing on a pathological cartesian blow-up.
pub fn estimate_peak_memory(plan: &PlanNode, row_width: usize) -> u64 {
    let rows = buffered_rows(plan);
    saturating_u64(rows * row_width as f64)
}

/// Total rows that must be held in memory simultaneously across the plan tree:
/// the sum of each blocking operator's own working set.
fn buffered_rows(node: &PlanNode) -> f64 {
    let own = blocking_working_set(node);
    let children: f64 = node.children().iter().map(buffered_rows).sum();
    own + children
}

/// Rows this single operator must buffer (0 for a streaming operator).
fn blocking_working_set(node: &PlanNode) -> f64 {
    match node.operator() {
        // A DISTINCT projection accumulates its deduplicated output set.
        Operator::Projection { distinct: true, .. } => node.estimated_rows().max(0.0),
        // A cartesian product buffers the smaller (build) side to probe the
        // streamed side against; an unknown/empty side contributes nothing.
        Operator::CartesianProduct => node
            .children()
            .iter()
            .map(|c| c.estimated_rows().max(0.0))
            .fold(None, |acc: Option<f64>, r| {
                Some(acc.map_or(r, |a| a.min(r)))
            })
            .unwrap_or(0.0),
        // Everything else streams: scans, expands, filters, unwind, skip,
        // limit, union, plain (non-distinct) projection, single-row, empty.
        _ => 0.0,
    }
}

/// Clamp a non-negative `f64` to a `u64`, saturating instead of producing an
/// out-of-range / `NaN` cast.
fn saturating_u64(value: f64) -> u64 {
    if value.is_nan() || value <= 0.0 {
        0
    } else if value >= u64::MAX as f64 {
        u64::MAX
    } else {
        value as u64
    }
}

/// Clamp a `u64` to `usize` (saturating on 32-bit targets where `usize` is
/// narrower).
fn saturating_usize(value: u64) -> usize {
    if value > usize::MAX as u64 {
        usize::MAX
    } else {
        value as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::parser::parse;
    use crate::planner::{plan_query, GraphStatistics};

    // ---- MemoryBudget: reservation + OOM guard ----

    #[test]
    fn reservation_within_limit_succeeds_and_tracks_usage() {
        let budget = MemoryBudget::new(1000);
        let r = budget.try_reserve(400).expect("fits");
        assert_eq!(r.bytes(), 400);
        assert_eq!(budget.used(), 400);
        assert_eq!(budget.available(), 600);
    }

    #[test]
    fn reservation_exceeding_limit_is_refused_with_details() {
        let budget = MemoryBudget::new(500);
        let _held = budget.try_reserve(400).expect("first fits");
        let err = budget.try_reserve(200).expect_err("over the cap");
        assert_eq!(
            err,
            BudgetError::MemoryBudgetExceeded {
                requested: 200,
                used: 400,
                limit: 500,
            }
        );
        // The refused request must not have changed live usage.
        assert_eq!(budget.used(), 400);
    }

    #[test]
    fn reservation_releases_on_drop() {
        let budget = MemoryBudget::new(1000);
        {
            let _r = budget.try_reserve(700).expect("fits");
            assert_eq!(budget.used(), 700);
        }
        assert_eq!(budget.used(), 0);
        // The freed budget is reusable.
        let _r2 = budget.try_reserve(900).expect("fits after release");
        assert_eq!(budget.used(), 900);
    }

    #[test]
    fn exact_fit_at_the_limit_succeeds_but_one_more_byte_fails() {
        let budget = MemoryBudget::new(100);
        let _r = budget.try_reserve(100).expect("exact fit");
        assert_eq!(budget.available(), 0);
        assert!(budget.try_reserve(1).is_err());
    }

    #[test]
    fn zero_byte_reservation_always_succeeds() {
        let budget = MemoryBudget::new(0);
        let r = budget.try_reserve(0).expect("zero always fits");
        assert_eq!(r.bytes(), 0);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn unlimited_budget_never_refuses() {
        let budget = MemoryBudget::unlimited();
        assert!(budget.is_unlimited());
        // A fresh unlimited budget offers the whole address space.
        assert_eq!(budget.available(), usize::MAX);
        // Even an absurd reservation is granted; usage tracks it exactly.
        let r = budget.try_reserve(usize::MAX - 1).expect("unlimited fits");
        assert_eq!(budget.used(), usize::MAX - 1);
        assert_eq!(budget.available(), 1);
        drop(r);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn peak_tracks_high_water_mark_across_release() {
        let budget = MemoryBudget::new(1000);
        {
            let _a = budget.try_reserve(300).expect("fits");
            let _b = budget.try_reserve(500).expect("fits");
            assert_eq!(budget.used(), 800);
        }
        // usage is back to 0 but peak remembers the 800 high-water mark.
        assert_eq!(budget.used(), 0);
        assert_eq!(budget.peak(), 800);
    }

    #[test]
    fn clones_share_one_counter() {
        let budget = MemoryBudget::new(1000);
        let clone = budget.clone();
        let _r = budget.try_reserve(600).expect("fits");
        // The clone sees the same live usage and the same shrunken headroom.
        assert_eq!(clone.used(), 600);
        assert!(clone.try_reserve(500).is_err());
    }

    // ---- Backpressure: hysteresis ----

    #[test]
    fn backpressure_pauses_at_high_and_resumes_at_low() {
        let bp = Backpressure::new(40, 80);
        assert_eq!(bp.observe(50), BackpressureSignal::Continue);
        assert_eq!(bp.observe(80), BackpressureSignal::Pause);
        assert!(bp.is_paused());
        // Inside the hysteresis band: stays paused, no thrash.
        assert_eq!(bp.observe(60), BackpressureSignal::Continue);
        assert!(bp.is_paused());
        // Falls to the low mark: resume exactly once.
        assert_eq!(bp.observe(40), BackpressureSignal::Resume);
        assert!(!bp.is_paused());
        assert_eq!(bp.observe(45), BackpressureSignal::Continue);
    }

    #[test]
    fn backpressure_does_not_resume_above_low_mark() {
        let bp = Backpressure::new(40, 80);
        bp.observe(90); // pause
        assert!(bp.is_paused());
        // 41 > low(40): still paused.
        assert_eq!(bp.observe(41), BackpressureSignal::Continue);
        assert!(bp.is_paused());
    }

    #[test]
    fn backpressure_from_budget_fractions() {
        let budget = MemoryBudget::new(1000);
        let bp = Backpressure::from_budget(&budget, 0.5, 0.9);
        assert_eq!(bp.low_watermark(), 500);
        assert_eq!(bp.high_watermark(), 900);
    }

    #[test]
    fn backpressure_from_budget_orders_inverted_fractions() {
        let budget = MemoryBudget::new(1000);
        // Caller passes them backwards; we still produce low <= high.
        let bp = Backpressure::from_budget(&budget, 0.9, 0.5);
        assert_eq!(bp.low_watermark(), 500);
        assert_eq!(bp.high_watermark(), 900);
    }

    #[test]
    #[should_panic(expected = "must not exceed high watermark")]
    fn backpressure_rejects_inverted_band() {
        let _ = Backpressure::new(80, 40);
    }

    #[test]
    fn equal_watermarks_form_a_zero_width_band() {
        let bp = Backpressure::new(50, 50);
        assert_eq!(bp.observe(50), BackpressureSignal::Pause);
        assert_eq!(bp.observe(50), BackpressureSignal::Resume);
    }

    // ---- estimate_peak_memory + admits ----

    fn plan_for(query: &str, stats: &GraphStatistics) -> PlanNode {
        let ast = parse(query).expect("parses");
        plan_query(&ast, stats)
    }

    #[test]
    fn streaming_plan_has_zero_peak_working_set() {
        let stats = GraphStatistics::new().with_total_nodes(1000);
        // A plain scan + projection streams end to end.
        let plan = plan_for("MATCH (n) RETURN n", &stats);
        assert_eq!(estimate_peak_memory(&plan, 64), 0);
    }

    #[test]
    fn distinct_projection_buffers_its_output() {
        let stats = GraphStatistics::new().with_total_nodes(500);
        let plan = plan_for("MATCH (n) RETURN DISTINCT n", &stats);
        // The DISTINCT set is estimated_rows of the projection × row width.
        let rows = plan.estimated_rows();
        assert!(rows > 0.0);
        assert_eq!(estimate_peak_memory(&plan, 10), saturating_u64(rows * 10.0));
    }

    #[test]
    fn admits_accepts_a_plan_that_fits_and_refuses_one_that_does_not() {
        let stats = GraphStatistics::new().with_total_nodes(1000);
        let plan = plan_for("MATCH (n) RETURN DISTINCT n", &stats);
        let needed = estimate_peak_memory(&plan, 64);
        assert!(needed > 0);

        let generous = MemoryBudget::new(needed as usize);
        assert!(generous.admits(&plan, 64).is_ok());

        let tight = MemoryBudget::new(needed as usize - 1);
        assert!(matches!(
            tight.admits(&plan, 64),
            Err(BudgetError::MemoryBudgetExceeded { .. })
        ));
    }

    #[test]
    fn admits_a_streaming_plan_under_any_nonzero_budget() {
        let stats = GraphStatistics::new().with_total_nodes(1_000_000);
        let plan = plan_for("MATCH (n) RETURN n LIMIT 10", &stats);
        // Streams: even a 1-byte budget admits it (zero working set).
        assert!(MemoryBudget::new(1).admits(&plan, 64).is_ok());
    }

    #[test]
    fn saturating_u64_guards_against_overflow_and_nan() {
        assert_eq!(saturating_u64(-5.0), 0);
        assert_eq!(saturating_u64(f64::NAN), 0);
        assert_eq!(saturating_u64(f64::INFINITY), u64::MAX);
        assert_eq!(saturating_u64(42.7), 42);
    }
}
