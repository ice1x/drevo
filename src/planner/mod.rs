//! Cost-based query planner — Phase 14 task `00085`.
//!
//! Phase 14 keeps Cypher fast on large graphs by replacing O(N) full scans
//! with planned, index-aware execution. This module is its foundation: the
//! three pieces a cost-based planner needs before it can choose a cheaper plan.
//!
//! * [`stats`](crate::planner::stats) — [`GraphStatistics`], the catalogue
//!   figures (node/relationship counts, per-label/-type breakdowns,
//!   per-`(label, property)` distinct-value counts) and the
//!   [`StatisticsCollector`] that tallies them from a scan.
//! * [`cardinality`](crate::planner::cardinality) — the
//!   [`CardinalityEstimator`], which turns those figures into row-count
//!   estimates for scans, expands, and `WHERE` filters.
//! * [`plan`](crate::planner::plan) — the annotated [`PlanNode`] operator tree,
//!   the [`plan_query`]/[`plan_single_query`] builders that produce a naive
//!   left-deep plan from parsed Cypher, and [`PlanNode::explain`], the
//!   `EXPLAIN`-style rendering Phase 14's definition of done is built on.
//! * [`cache`](crate::planner::cache) — the bounded, thread-safe [`PlanCache`]
//!   that memoises planned queries.
//!
//! **Scope (`00085`).** Like the MVCC engine in its early tasks, the planner is
//! a self-contained module: it reads a [`GraphStatistics`] snapshot and parsed
//! [`crate::cypher::ast`] trees, but is **not yet wired into the executor**.
//! Estimates are coarse (documented `DEFAULT_*` constants stand in for missing
//! statistics) and the plan keeps operators in source order. Consuming the
//! plan to actually reorder/optimise execution — and feeding the collector
//! from a live [`crate::db::Drevo`] scan — is the next task (`00086`).
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
//! [`PlanCache`]: crate::planner::cache::PlanCache

pub mod cache;
pub mod cardinality;
pub mod plan;
pub mod stats;

pub use cache::PlanCache;
pub use cardinality::{
    CardinalityEstimator, DEFAULT_EQUALITY_SELECTIVITY, DEFAULT_NULL_SELECTIVITY,
    DEFAULT_PREDICATE_SELECTIVITY, DEFAULT_RANGE_SELECTIVITY, DEFAULT_STRING_MATCH_SELECTIVITY,
};
pub use plan::{plan_query, plan_single_query, Operator, PlanNode};
pub use stats::{GraphStatistics, StatisticsCollector};
