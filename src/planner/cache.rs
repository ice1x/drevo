//! Plan cache — memoise planned queries so a repeated query string skips the
//! plan-building work.
//!
//! [`PlanCache`] is a bounded, thread-safe, least-recently-used cache keyed by
//! the (normalised) query string. It is the third deliverable of the
//! cost-based planner (`00085`): planning is pure given a
//! [`GraphStatistics`](crate::planner::stats::GraphStatistics) snapshot, so
//! caching the resulting [`PlanNode`] is sound until the
//! statistics change — at which point the host calls [`clear`](PlanCache::clear)
//! to invalidate.
//!
//! The cache never panics: a lock poisoned by a panic in another thread is
//! recovered in place (the worst case is slightly stale LRU bookkeeping, which
//! cannot corrupt a result), mirroring the best-effort posture of the MVCC GC
//! worker.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::planner::plan::PlanNode;

/// A bounded LRU cache mapping a query string to its planned [`PlanNode`].
///
/// Construct with [`PlanCache::new`] giving the maximum number of entries
/// (capacity `0` disables caching — every lookup misses and nothing is
/// retained). Thread-safe and cheap to share behind an `Arc`.
#[derive(Debug)]
pub struct PlanCache {
    inner: Mutex<CacheInner>,
    hits: AtomicU64,
    misses: AtomicU64,
}

#[derive(Debug)]
struct CacheInner {
    capacity: usize,
    /// key → (plan, last-access tick).
    entries: HashMap<String, (PlanNode, u64)>,
    /// Monotonic logical clock; the entry with the smallest tick is the LRU
    /// victim.
    clock: u64,
}

impl PlanCache {
    /// Create a cache holding at most `capacity` plans. A capacity of `0`
    /// disables caching.
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(CacheInner {
                capacity,
                entries: HashMap::new(),
                clock: 0,
            }),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Look up the cached plan for `key`, cloning it on a hit and marking it
    /// most-recently-used. Returns `None` (and counts a miss) when absent.
    pub fn get(&self, key: &str) -> Option<PlanNode> {
        let mut inner = self.lock();
        let tick = inner.next_tick();
        match inner.entries.get_mut(key) {
            Some((plan, last)) => {
                *last = tick;
                let plan = plan.clone();
                drop(inner);
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(plan)
            }
            None => {
                drop(inner);
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Insert (or replace) the plan for `key`, evicting the least-recently-used
    /// entry first if the cache is full. A capacity-`0` cache retains nothing.
    pub fn insert(&self, key: impl Into<String>, plan: PlanNode) {
        let mut inner = self.lock();
        if inner.capacity == 0 {
            return;
        }
        let tick = inner.next_tick();
        let key = key.into();
        if !inner.entries.contains_key(&key) && inner.entries.len() >= inner.capacity {
            inner.evict_lru();
        }
        inner.entries.insert(key, (plan, tick));
    }

    /// Return the cached plan for `key`, or compute it with `build`, cache it,
    /// and return it. The ergonomic entry point: one call replaces the
    /// get/miss/insert dance.
    pub fn get_or_insert_with(&self, key: &str, build: impl FnOnce() -> PlanNode) -> PlanNode {
        if let Some(plan) = self.get(key) {
            return plan;
        }
        let plan = build();
        self.insert(key.to_string(), plan.clone());
        plan
    }

    /// Number of plans currently cached.
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// Whether the cache holds no plans.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The configured maximum number of entries.
    pub fn capacity(&self) -> usize {
        self.lock().capacity
    }

    /// Total number of cache hits observed.
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Total number of cache misses observed.
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Drop every cached plan. Call this when the statistics snapshot the
    /// plans were built against changes. Hit/miss counters are left intact.
    pub fn clear(&self) {
        self.lock().entries.clear();
    }

    /// Acquire the inner lock, recovering it in place if poisoned (a poisoned
    /// plan cache cannot yield an incorrect result, only stale LRU order, so
    /// recovery is safe and keeps the cache panic-free).
    fn lock(&self) -> std::sync::MutexGuard<'_, CacheInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl CacheInner {
    /// Advance and return the logical clock.
    fn next_tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Remove the entry with the smallest access tick (the LRU victim).
    fn evict_lru(&mut self) {
        if let Some(victim) = self
            .entries
            .iter()
            .min_by_key(|(_, (_, tick))| *tick)
            .map(|(key, _)| key.clone())
        {
            self.entries.remove(&victim);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::plan::plan_query;
    use crate::planner::stats::GraphStatistics;
    use std::sync::Arc;
    use std::thread;

    fn sample_plan(query: &str) -> PlanNode {
        let ast = crate::cypher::parser::parse(query).expect("parses");
        let mut stats = GraphStatistics::new().with_total_nodes(100);
        stats.set_label_count("Person", 40);
        plan_query(&ast, &stats)
    }

    #[test]
    fn miss_then_hit() {
        let cache = PlanCache::new(8);
        assert!(cache.get("MATCH (n) RETURN n").is_none());
        assert_eq!(cache.misses(), 1);
        cache.insert("MATCH (n) RETURN n", sample_plan("MATCH (n) RETURN n"));
        assert!(cache.get("MATCH (n) RETURN n").is_some());
        assert_eq!(cache.hits(), 1);
    }

    #[test]
    fn get_or_insert_with_builds_once() {
        let cache = PlanCache::new(8);
        let key = "MATCH (n:Person) RETURN n";
        let mut builds = 0;
        let first = cache.get_or_insert_with(key, || {
            builds += 1;
            sample_plan(key)
        });
        let second = cache.get_or_insert_with(key, || {
            builds += 1;
            sample_plan(key)
        });
        assert_eq!(builds, 1, "plan should be built exactly once");
        assert_eq!(first, second);
        assert_eq!(cache.hits(), 1);
    }

    #[test]
    fn evicts_least_recently_used() {
        let cache = PlanCache::new(2);
        cache.insert("a", sample_plan("MATCH (n) RETURN n"));
        cache.insert("b", sample_plan("MATCH (n) RETURN n"));
        // Touch "a" so "b" becomes the LRU.
        assert!(cache.get("a").is_some());
        cache.insert("c", sample_plan("MATCH (n) RETURN n"));
        assert_eq!(cache.len(), 2);
        assert!(cache.get("b").is_none(), "b should have been evicted");
        assert!(cache.get("a").is_some());
        assert!(cache.get("c").is_some());
    }

    #[test]
    fn capacity_zero_disables_caching() {
        let cache = PlanCache::new(0);
        cache.insert("a", sample_plan("MATCH (n) RETURN n"));
        assert!(cache.is_empty());
        assert!(cache.get("a").is_none());
    }

    #[test]
    fn reinsert_does_not_grow_past_capacity() {
        let cache = PlanCache::new(1);
        cache.insert("a", sample_plan("MATCH (n) RETURN n"));
        cache.insert("a", sample_plan("MATCH (n) RETURN n"));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn clear_drops_entries_but_keeps_counters() {
        let cache = PlanCache::new(4);
        cache.insert("a", sample_plan("MATCH (n) RETURN n"));
        let _ = cache.get("a");
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.hits(), 1);
    }

    #[test]
    fn capacity_reports_configured_value() {
        assert_eq!(PlanCache::new(16).capacity(), 16);
    }

    #[test]
    fn concurrent_access_is_safe() {
        let cache = Arc::new(PlanCache::new(32));
        let mut handles = vec![];
        for t in 0..8 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                for i in 0..100 {
                    let key = format!("q{}", (t + i) % 16);
                    cache.get_or_insert_with(&key, || sample_plan("MATCH (n) RETURN n"));
                }
            }));
        }
        for h in handles {
            h.join().expect("thread joins");
        }
        // Bounded regardless of the contention.
        assert!(cache.len() <= 32);
        assert!(cache.hits() + cache.misses() >= 800);
    }
}
