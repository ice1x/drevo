//! Integration tests for the Phase 14 memory budget & backpressure (`00089`).
//!
//! These exercise the budget end-to-end the way the executor-wiring task will:
//! plan representative Cypher queries against statistics collected from a *real*
//! [`Drevo`] graph, then (a) admit or refuse the plan up front against a byte
//! ceiling (memory-limited query execution), (b) drive a runtime row-buffering
//! loop through [`MemoryBudget::try_reserve`] and watch the OOM guard refuse the
//! row that would blow the cap, and (c) throttle a streaming producer through
//! the [`Backpressure`] watermarks. A concurrency storm pins that a shared
//! budget can never be jointly overshot. The budget itself stays decoupled from
//! the executor — fed through the public planner API exactly as `00085`–`00088`
//! left it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use drevo::cypher::parser::parse;
use drevo::db::Drevo;
use drevo::model::{NewEdge, NewNode, Properties};
use drevo::planner::{
    estimate_peak_memory, plan_query, Backpressure, BackpressureSignal, BudgetError,
    GraphStatistics, MemoryBudget, PlanNode, StatisticsCollector,
};

// ---- graph fixtures (mirrors tests/planner_tests.rs discipline) ----

struct NodeSpec {
    kind: &'static str,
    status: Option<&'static str>,
}

fn add_node(db: &Drevo, collector: &mut StatisticsCollector, spec: NodeSpec) -> u64 {
    let mut properties = Properties::default();
    if let Some(status) = spec.status {
        properties
            .0
            .insert("status".to_string(), serde_json::json!(status));
        collector.record_property(spec.kind, "status", status);
    }
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let node = db
        .create_node(NewNode {
            kind: spec.kind.to_string(),
            title: format!("{} node {seq}", spec.kind),
            body: String::new(),
            body_html: String::new(),
            properties,
        })
        .expect("create node");
    collector.record_node(&[spec.kind]);
    node.id
}

fn add_edge(db: &Drevo, collector: &mut StatisticsCollector, from: u64, to: u64, kind: &str) {
    db.create_edge(NewEdge {
        from_id: from,
        to_id: to,
        kind: kind.to_string(),
        weight: 1.0,
        properties: Properties::default(),
    })
    .expect("create edge");
    collector.record_relationship(kind);
}

/// A bug tracker: 5 engineers, 12 bugs across 3 statuses, ASSIGNED_TO + BLOCKS.
fn bug_tracker() -> (Drevo, GraphStatistics) {
    let db = Drevo::open_in_memory().expect("open db");
    let mut collector = StatisticsCollector::new();

    let engineers: Vec<u64> = (0..5)
        .map(|_| {
            add_node(
                &db,
                &mut collector,
                NodeSpec {
                    kind: "Engineer",
                    status: None,
                },
            )
        })
        .collect();

    let statuses = ["open", "in_progress", "closed"];
    for i in 0..12 {
        let bug = add_node(
            &db,
            &mut collector,
            NodeSpec {
                kind: "Bug",
                status: Some(statuses[i % statuses.len()]),
            },
        );
        add_edge(
            &db,
            &mut collector,
            bug,
            engineers[i % engineers.len()],
            "ASSIGNED_TO",
        );
    }

    (db, collector.finish())
}

fn plan_for(query: &str, stats: &GraphStatistics) -> PlanNode {
    plan_query(&parse(query).expect("parses"), stats)
}

// ---- memory-limited query execution: plan admission ----

#[test]
fn distinct_report_is_refused_under_a_tight_budget_and_admitted_under_a_generous_one() {
    let (_db, stats) = bug_tracker();
    // A DISTINCT projection must materialise its dedup set — a blocking op.
    let plan = plan_for("MATCH (b:Bug) RETURN DISTINCT b.status", &stats);
    let row_width = 32;
    let needed = estimate_peak_memory(&plan, row_width);
    assert!(
        needed > 0,
        "a DISTINCT projection has a non-zero working set"
    );

    let generous = MemoryBudget::new(needed as usize);
    generous
        .admits(&plan, row_width)
        .expect("a plan that exactly fits is admitted");

    let tight = MemoryBudget::new((needed as usize).saturating_sub(1));
    let err = tight
        .admits(&plan, row_width)
        .expect_err("one byte short is refused before any row is read");
    assert!(matches!(err, BudgetError::MemoryBudgetExceeded { .. }));
}

#[test]
fn streaming_triage_query_is_admitted_under_a_one_byte_budget() {
    let (_db, stats) = bug_tracker();
    // Pure scan → filter → projection streams: zero working set.
    let plan = plan_for("MATCH (b:Bug) WHERE b.status = 'open' RETURN b", &stats);
    assert_eq!(estimate_peak_memory(&plan, 1024), 0);
    MemoryBudget::new(1)
        .admits(&plan, 1024)
        .expect("a fully streaming plan fits any budget");
}

// ---- runtime OOM guard ----

#[test]
fn runtime_row_buffering_hits_the_oom_guard_instead_of_exhausting_memory() {
    // Simulate an operator buffering result rows: reserve per row until the
    // budget refuses, then assert we stopped cleanly with a recoverable error
    // rather than allocating without bound.
    let budget = MemoryBudget::new(1000);
    let row_bytes = 64;
    let mut held = Vec::new();
    let mut refused = false;
    for _ in 0..1000 {
        match budget.try_reserve(row_bytes) {
            Ok(reservation) => held.push(reservation),
            Err(BudgetError::MemoryBudgetExceeded { limit, .. }) => {
                assert_eq!(limit, 1000);
                refused = true;
                break;
            }
        }
    }
    assert!(refused, "the guard eventually refused a row");
    assert_eq!(
        held.len(),
        15,
        "1000 / 64 = 15 rows fit before the 16th is refused"
    );
    assert!(budget.used() <= 1000);

    // Dropping the buffered rows frees the whole budget — no leak.
    held.clear();
    assert_eq!(budget.used(), 0);
}

// ---- backpressure ----

#[test]
fn ingestion_throttles_on_high_watermark_and_resumes_on_low() {
    // A producer fills a budget; backpressure pauses it near the top and only
    // resumes once a consumer has drained it back under the low mark.
    let budget = MemoryBudget::new(1000);
    let bp = Backpressure::from_budget(&budget, 0.3, 0.8); // low=300, high=800

    let mut chunks: Vec<_> = Vec::new();
    let mut paused_at = None;
    for i in 0..20 {
        let r = budget
            .try_reserve(100)
            .expect("100-byte chunk fits under 1000");
        chunks.push(r);
        if bp.observe(budget.used()) == BackpressureSignal::Pause {
            paused_at = Some(i);
            break;
        }
    }
    // 8 × 100 = 800 reaches the high mark → pause on the 8th chunk (index 7).
    assert_eq!(paused_at, Some(7));
    assert!(bp.is_paused());

    // Consumer drains back below the low mark; producer gets the resume signal.
    while budget.used() > 250 {
        chunks.pop();
    }
    assert_eq!(bp.observe(budget.used()), BackpressureSignal::Resume);
    assert!(!bp.is_paused());
}

// ---- concurrency: a shared budget is never jointly overshot ----

#[test]
fn concurrent_reservers_never_exceed_the_shared_limit() {
    const THREADS: usize = 8;
    const PER_THREAD_ATTEMPTS: usize = 500;
    const ROW: usize = 7;
    let budget = MemoryBudget::new(200);
    let barrier = Arc::new(Barrier::new(THREADS));
    let granted = Arc::new(AtomicU64::new(0));

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let budget = budget.clone();
            let barrier = Arc::clone(&barrier);
            let granted = Arc::clone(&granted);
            thread::spawn(move || {
                barrier.wait();
                let mut mine = Vec::new();
                for _ in 0..PER_THREAD_ATTEMPTS {
                    if let Ok(r) = budget.try_reserve(ROW) {
                        // Live usage must never pass the ceiling, mid-storm —
                        // the core no-overshoot invariant under contention.
                        assert!(budget.used() <= 200);
                        granted.fetch_add(1, Ordering::Relaxed);
                        mine.push(r);
                        if mine.len() % 3 == 0 {
                            mine.pop(); // churn: release some back
                        }
                    }
                }
                // `mine` drops here as the thread returns, releasing every
                // reservation this thread still holds.
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread");
    }
    // No leak / no tear: once every thread's reservations have dropped, the
    // shared counter is back to exactly zero.
    assert_eq!(budget.used(), 0);
    assert!(
        granted.load(Ordering::Relaxed) > 0,
        "some reservations were granted"
    );
}

#[test]
fn a_reservation_releases_when_moved_into_and_dropped_by_a_worker_thread() {
    let budget = MemoryBudget::new(500);
    let reservation = budget.try_reserve(300).expect("fits");
    assert_eq!(budget.used(), 300);

    // The RAII guard is Send + 'static: move it into a thread that drops it.
    let handle = thread::spawn(move || {
        let bytes = reservation.bytes();
        drop(reservation);
        bytes
    });
    assert_eq!(handle.join().expect("thread"), 300);
    assert_eq!(budget.used(), 0, "the worker's drop freed the budget");
}

// ---- domain scenario: peak memory grows with the materialised set ----

#[test]
fn distinct_peak_scales_with_the_estimated_result_size() {
    // Two graphs with the same shape but different cardinality: the larger
    // graph's DISTINCT report has the larger estimated peak.
    let small = GraphStatistics::new().with_total_nodes(100);
    let large = GraphStatistics::new().with_total_nodes(100_000);

    let small_plan = plan_for("MATCH (n) RETURN DISTINCT n", &small);
    let large_plan = plan_for("MATCH (n) RETURN DISTINCT n", &large);

    let small_peak = estimate_peak_memory(&small_plan, 64);
    let large_peak = estimate_peak_memory(&large_plan, 64);
    assert!(
        large_peak > small_peak,
        "larger result set ⇒ larger estimated peak ({large_peak} vs {small_peak})"
    );
}
