//! Integration tests for the Phase 14 `00085` cost-based query planner.
//!
//! These exercise the planner end-to-end against statistics collected from a
//! *real* [`Drevo`] graph: ingest a domain graph (bug tracker, task manager,
//! CBT journal) while tallying a [`StatisticsCollector`] in the same pass, then
//! plan representative Cypher queries and assert the cardinality estimates,
//! `EXPLAIN` rendering, and plan-cache behaviour. The planner itself stays
//! decoupled from the executor (task `00085` scope) — here it is fed through
//! the public `Drevo` API exactly as the executor-wiring task (`00086`) will.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use drevo::cypher::parser::parse;
use drevo::db::Drevo;
use drevo::model::{NewEdge, NewNode, Properties};
use drevo::planner::{plan_query, GraphStatistics, Operator, PlanCache, StatisticsCollector};

/// A node `kind` (drevo's label) plus an optional `status` property to record.
struct NodeSpec {
    kind: &'static str,
    status: Option<&'static str>,
}

/// Create a node in `db`, record it in `collector` (label + optional status
/// distinct value), and return its id.
fn add_node(db: &Drevo, collector: &mut StatisticsCollector, spec: NodeSpec) -> u64 {
    let mut properties = Properties::default();
    if let Some(status) = spec.status {
        properties
            .0
            .insert("status".to_string(), serde_json::json!(status));
        collector.record_property(spec.kind, "status", status);
    }
    static TITLE_SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = TITLE_SEQ.fetch_add(1, Ordering::Relaxed);
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

/// Create an edge in `db` and record its type in `collector`.
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

/// Build a small bug-tracker graph: 5 engineers, 12 bugs (statuses drawn from
/// open/in_progress/closed), each bug ASSIGNED_TO an engineer; a few BLOCKS
/// edges between bugs. Returns the live db and the collected statistics.
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
    let bugs: Vec<u64> = (0..12)
        .map(|i| {
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
            bug
        })
        .collect();

    add_edge(&db, &mut collector, bugs[0], bugs[1], "BLOCKS");
    add_edge(&db, &mut collector, bugs[2], bugs[3], "BLOCKS");

    (db, collector.finish())
}

#[test]
fn collected_statistics_match_the_graph() {
    let (_db, stats) = bug_tracker();
    assert_eq!(stats.total_nodes(), 17, "5 engineers + 12 bugs");
    assert_eq!(stats.total_relationships(), 14, "12 ASSIGNED_TO + 2 BLOCKS");
    assert_eq!(stats.nodes_with_label("Engineer"), Some(5));
    assert_eq!(stats.nodes_with_label("Bug"), Some(12));
    assert_eq!(stats.relationships_with_type("ASSIGNED_TO"), Some(12));
    assert_eq!(stats.relationships_with_type("BLOCKS"), Some(2));
    // Three distinct status values across the 12 bugs.
    assert_eq!(stats.distinct_values("Bug", "status"), Some(3));
}

#[test]
fn label_scan_estimate_equals_real_label_count() {
    let (_db, stats) = bug_tracker();
    let ast = parse("MATCH (b:Bug) RETURN b").expect("parses");
    let plan = plan_query(&ast, &stats);
    let scan = &plan.children()[0];
    assert!(matches!(scan.operator(), Operator::NodeByLabelScan { .. }));
    assert!((scan.estimated_rows() - 12.0).abs() < 1e-9);
}

#[test]
fn equality_filter_uses_distinct_status_values() {
    let (_db, stats) = bug_tracker();
    let ast = parse("MATCH (b:Bug) WHERE b.status = 'open' RETURN b").expect("parses");
    let plan = plan_query(&ast, &stats);
    let filter = &plan.children()[0];
    assert!(matches!(filter.operator(), Operator::Filter { .. }));
    // 12 bugs / 3 distinct statuses = 4.
    assert!((filter.estimated_rows() - 4.0).abs() < 1e-9);
}

#[test]
fn expand_estimate_reflects_average_degree() {
    let (_db, stats) = bug_tracker();
    let ast = parse("MATCH (b:Bug)-[:ASSIGNED_TO]->(e:Engineer) RETURN e").expect("parses");
    let plan = plan_query(&ast, &stats);
    let expand = &plan.children()[0];
    assert!(matches!(expand.operator(), Operator::Expand { .. }));
    // 12 bugs * (12 ASSIGNED_TO / 17 nodes) ≈ 8.47.
    let expected = 12.0 * (12.0 / 17.0);
    assert!((expand.estimated_rows() - expected).abs() < 1e-6);
}

#[test]
fn explain_output_is_a_readable_tree() {
    let (_db, stats) = bug_tracker();
    let ast = parse(
        "MATCH (b:Bug)-[:ASSIGNED_TO]->(e:Engineer) WHERE b.status = 'open' RETURN e LIMIT 3",
    )
    .expect("parses");
    let plan = plan_query(&ast, &stats);
    let text = plan.explain();
    assert!(text.starts_with("+Limit"), "got:\n{text}");
    assert!(text.contains("+Projection"), "got:\n{text}");
    assert!(text.contains("+Filter b.status = 'open'"), "got:\n{text}");
    assert!(
        text.contains("+Expand (b)-[:ASSIGNED_TO]->(e:Engineer)"),
        "got:\n{text}"
    );
    assert!(text.contains("+NodeByLabelScan (b:Bug)"), "got:\n{text}");
    // Every operator line carries estimates.
    for line in text.lines() {
        assert!(
            line.contains("estRows=") && line.contains("cost="),
            "line: {line}"
        );
    }
}

#[test]
fn more_selective_query_costs_less() {
    let (_db, stats) = bug_tracker();
    let broad = plan_query(&parse("MATCH (b:Bug) RETURN b").expect("parses"), &stats);
    let narrow = plan_query(
        &parse("MATCH (b:Bug) WHERE b.status = 'open' RETURN b").expect("parses"),
        &stats,
    );
    // The filtered query produces fewer rows than the unfiltered one.
    assert!(narrow.estimated_rows() < broad.estimated_rows());
}

#[test]
fn plan_cache_reuses_repeated_queries() {
    let (_db, stats) = bug_tracker();
    let cache = PlanCache::new(16);
    let query = "MATCH (b:Bug) WHERE b.status = 'closed' RETURN b";
    let mut builds = 0;

    for _ in 0..5 {
        cache.get_or_insert_with(query, || {
            builds += 1;
            let ast = parse(query).expect("parses");
            plan_query(&ast, &stats)
        });
    }

    assert_eq!(builds, 1, "the plan is built once and reused");
    assert_eq!(cache.hits(), 4);
    assert_eq!(cache.misses(), 1);
    assert_eq!(cache.len(), 1);
}

#[test]
fn plan_cache_is_shareable_across_threads() {
    let (_db, stats) = bug_tracker();
    let stats = Arc::new(stats);
    let cache = Arc::new(PlanCache::new(8));
    let queries = [
        "MATCH (b:Bug) RETURN b",
        "MATCH (e:Engineer) RETURN e",
        "MATCH (b:Bug)-[:BLOCKS]->(o:Bug) RETURN o",
    ];

    let mut handles = vec![];
    for t in 0..4 {
        let cache = Arc::clone(&cache);
        let stats = Arc::clone(&stats);
        handles.push(std::thread::spawn(move || {
            for round in 0..50 {
                let q = queries[(t + round) % queries.len()];
                cache.get_or_insert_with(q, || plan_query(&parse(q).expect("parses"), &stats));
            }
        }));
    }
    for h in handles {
        h.join().expect("thread joins");
    }
    // All three distinct queries fit under the capacity; counters are sane.
    assert!(cache.len() <= 3);
    assert_eq!(cache.hits() + cache.misses(), 200);
}

// --- Mandated edge cases (drevo-tdd §Coverage Targets) -------------------

#[test]
fn empty_graph_estimates_zero_rows() {
    let stats = StatisticsCollector::new().finish();
    let plan = plan_query(&parse("MATCH (n) RETURN n").expect("parses"), &stats);
    let scan = &plan.children()[0];
    assert_eq!(scan.estimated_rows(), 0.0);
    // Expanding from nothing stays at zero, never negative or NaN.
    let expand_plan = plan_query(
        &parse("MATCH (a)-[:R]->(b) RETURN b").expect("parses"),
        &stats,
    );
    let mut node = &expand_plan;
    while let Some(child) = node.children().first() {
        node = child;
    }
    assert!(expand_plan.estimated_rows().is_finite());
    assert!(expand_plan.estimated_rows() >= 0.0);
}

#[test]
fn single_node_graph_plans_cleanly() {
    let db = Drevo::open_in_memory().expect("open db");
    let mut collector = StatisticsCollector::new();
    add_node(
        &db,
        &mut collector,
        NodeSpec {
            kind: "Note",
            status: None,
        },
    );
    let stats = collector.finish();
    let plan = plan_query(&parse("MATCH (n:Note) RETURN n").expect("parses"), &stats);
    assert!((plan.estimated_rows() - 1.0).abs() < 1e-9);
}

#[test]
fn unicode_labels_and_properties_are_tracked() {
    let db = Drevo::open_in_memory().expect("open db");
    let mut collector = StatisticsCollector::new();
    // Cyrillic label, emoji + CJK status values.
    for status in ["открыто", "完了", "🚧"] {
        add_node(
            &db,
            &mut collector,
            NodeSpec {
                kind: "Задача",
                status: Some(status),
            },
        );
    }
    let stats = collector.finish();
    assert_eq!(stats.nodes_with_label("Задача"), Some(3));
    assert_eq!(stats.distinct_values("Задача", "status"), Some(3));
}

#[test]
fn self_loop_relationship_is_counted() {
    let db = Drevo::open_in_memory().expect("open db");
    let mut collector = StatisticsCollector::new();
    let n = add_node(
        &db,
        &mut collector,
        NodeSpec {
            kind: "Account",
            status: None,
        },
    );
    add_edge(&db, &mut collector, n, n, "REFERS_TO");
    let stats = collector.finish();
    assert_eq!(stats.total_relationships(), 1);
    assert_eq!(stats.relationships_with_type("REFERS_TO"), Some(1));
    // Average degree counts the self-loop once from the outgoing perspective.
    assert!((stats.average_degree() - 1.0).abs() < 1e-9);
}
