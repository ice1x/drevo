//! Cost-based query planner — Phase 14 task `00085`.
//!
//! Phase 14 keeps Cypher fast on large graphs by replacing O(N) full scans
//! with planned, index-aware execution. This module is its foundation: the
//! three pieces a cost-based planner needs before it can choose a cheaper plan.
//!
//! * [`stats`](crate::planner::stats) — [`GraphStatistics`], the catalogue
//!   figures (node/relationship counts, per-label/-type breakdowns,
//!   per-`(label, property)` distinct-value counts, and — task `00087` — a
//!   coarse degree distribution: maximum degree overall and per label, plus a
//!   supernode threshold/count for spotting hub nodes) and the
//!   [`StatisticsCollector`] that tallies them from a scan.
//! * [`cardinality`](crate::planner::cardinality) — the
//!   [`CardinalityEstimator`], which turns those figures into row-count
//!   estimates for scans, expands, and `WHERE` filters.
//! * [`plan`](crate::planner::plan) — the annotated [`PlanNode`] operator tree,
//!   the [`plan_query`]/[`plan_single_query`] builders that produce a naive
//!   left-deep plan from parsed Cypher, the cost-based
//!   [`optimize_query`]/[`optimize_single_query`] builders (and the
//!   [`PlanOptimizer`] handle) that reorder patterns, select index seeks, and
//!   order joins (task `00086`), and [`PlanNode::explain`], the `EXPLAIN`-style
//!   rendering Phase 14's definition of done is built on.
//! * [`cache`](crate::planner::cache) — the bounded, thread-safe [`PlanCache`]
//!   that memoises planned queries.
//! * [`budget`](crate::planner::budget) — task `00089`'s [`MemoryBudget`] (the
//!   OOM guard), [`estimate_peak_memory`] (memory-limited admission against a
//!   plan's working set), and [`Backpressure`] (a high/low-watermark throttle),
//!   so a query on a large graph fails with a recoverable error rather than
//!   exhausting process memory.
//!
//! **Scope (`00085`–`00089`).** Like the MVCC engine in its early
//! tasks, the planner is a self-contained module: it reads a
//! [`GraphStatistics`] snapshot and parsed [`crate::cypher::ast`] trees, but is
//! **not yet wired into the executor**. `00085` delivered the statistics,
//! cardinality estimates, naive plan, and cache; `00086` adds the cost-based
//! optimiser ([`optimize_query`]/[`optimize_single_query`]); `00087` adds
//! supernode handling — the optimiser avoids anchoring a traversal at a hub
//! node (whose first hop would fan out across its whole degree) and costs any
//! drive-out-of-a-hub with the worst-case fan-out; `00089` adds the memory
//! budget, peak-memory admission, and backpressure. Estimates remain coarse
//! (documented `DEFAULT_*` constants stand in for missing statistics).
//! Consuming the optimised plan to actually drive execution — and feeding the
//! collector from a live [`crate::db::Drevo`] scan — is later-task work.
//!
//! Dependency-free, always compiled, and WASM-safe (`std::sync` only, no
//! spawned threads).
//!
//! [`GraphStatistics`]: crate::planner::stats::GraphStatistics
//! [`StatisticsCollector`]: crate::planner::stats::StatisticsCollector
//! [`CardinalityEstimator`]: crate::planner::cardinality::CardinalityEstimator
//! [`PlanNode`]: crate::planner::plan::PlanNode
//! [`PlanNode::explain`]: crate::planner::plan::PlanNode::explain
//! [`plan_query`]: crate::planner::plan::plan_query
//! [`plan_single_query`]: crate::planner::plan::plan_single_query
//! [`optimize_query`]: crate::planner::plan::optimize_query
//! [`optimize_single_query`]: crate::planner::plan::optimize_single_query
//! [`PlanOptimizer`]: crate::planner::plan::PlanOptimizer
//! [`PlanCache`]: crate::planner::cache::PlanCache
//! [`MemoryBudget`]: crate::planner::budget::MemoryBudget
//! [`estimate_peak_memory`]: crate::planner::budget::estimate_peak_memory
//! [`Backpressure`]: crate::planner::budget::Backpressure

pub mod budget;
pub mod cache;
pub mod cardinality;
pub mod plan;
pub mod stats;

pub use budget::{
    estimate_peak_memory, Backpressure, BackpressureSignal, BudgetError, MemoryBudget,
    MemoryReservation, DEFAULT_ROW_WIDTH_BYTES,
};
pub use cache::PlanCache;
pub use cardinality::{
    CardinalityEstimator, DEFAULT_EQUALITY_SELECTIVITY, DEFAULT_NULL_SELECTIVITY,
    DEFAULT_PREDICATE_SELECTIVITY, DEFAULT_RANGE_SELECTIVITY, DEFAULT_STRING_MATCH_SELECTIVITY,
};
pub use plan::{
    optimize_query, optimize_single_query, plan_query, plan_single_query, Operator, PlanNode,
    PlanOptimizer,
};
pub use stats::{
    GraphStatistics, StatisticsCollector, DEFAULT_SUPERNODE_THRESHOLD_FACTOR,
    MIN_SUPERNODE_THRESHOLD,
};
