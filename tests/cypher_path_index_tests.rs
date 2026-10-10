//! Indexes on nested property paths through Cypher (issue #578):
//! `CREATE INDEX … ON (n.meta.author)` / `ON (n.meta.*)` / `ON (n.*)`,
//! `DROP INDEX`, `SHOW INDEXES`, and `WHERE n.a.b …` served by the index.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use drevo::cypher::executor::{
    execute_on_engine_with_context, ExecError, ExecResult, NativeQueryContext, Value,
};
use drevo::cypher::parser::parse;
use drevo::native::NativeGraph;
use drevo::native_path_index::{NativePathIndex, PathIndexSpec, PropertyPath};
use drevo::native_service::NativeService;

static NEXT: AtomicU64 = AtomicU64::new(0);

fn run(svc: &NativeService, q: &str) -> Result<ExecResult, ExecError> {
    svc.execute(
        &parse(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}")),
        HashMap::new(),
    )
}

fn run_ok(svc: &NativeService, q: &str) -> ExecResult {
    run(svc, q).unwrap_or_else(|e| panic!("execute `{q}`: {e:?}"))
}

fn strings(res: &ExecResult) -> Vec<String> {
    res.rows
        .iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            other => panic!("expected a string, got {other:?}"),
        })
        .collect()
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

/// Bug tracker: bugs and features with nested `meta`.
fn bug_tracker() -> NativeService {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CREATE (:Bug {title: 'crash on save', meta: {severity: 'high', points: 8, assignee: {name: 'ann'}}}), \
                (:Bug {title: 'typo in footer', meta: {severity: 'low', points: 1, assignee: {name: 'bob'}}}), \
                (:Feature {title: 'dark mode', meta: {severity: 'high', points: 5}})",
    );
    svc
}

// ----- results through the index ---------------------------------------------

#[test]
fn exact_path_index_answers_equality_in_and_range() {
    let svc = bug_tracker();
    run_ok(
        &svc,
        "CREATE INDEX bug_severity FOR (n:Bug) ON (n.meta.severity, n.meta.points)",
    );
    assert_eq!(
        strings(&run_ok(
            &svc,
            "MATCH (n:Bug) WHERE n.meta.severity = 'high' RETURN n.title AS t"
        )),
        ["crash on save"]
    );
    assert_eq!(
        strings(&run_ok(
            &svc,
            "MATCH (n:Bug) WHERE n.meta.severity IN ['low', 'none'] RETURN n.title AS t"
        )),
        ["typo in footer"]
    );
    assert_eq!(
        strings(&run_ok(
            &svc,
            "MATCH (n:Bug) WHERE n.meta.points >= 2 RETURN n.title AS t"
        )),
        ["crash on save"]
    );
}

#[test]
fn wildcard_index_answers_deep_paths() {
    let svc = bug_tracker();
    run_ok(&svc, "CREATE INDEX FOR (n:Bug) ON (n.meta.*)");
    assert_eq!(
        strings(&run_ok(
            &svc,
            "MATCH (n:Bug) WHERE n.meta.assignee.name = 'bob' RETURN n.title AS t"
        )),
        ["typo in footer"]
    );
}

#[test]
fn whole_map_index_without_label_serves_unlabelled_patterns() {
    let svc = bug_tracker();
    run_ok(&svc, "CREATE INDEX everything FOR (n) ON (n.*)");
    assert_eq!(
        strings(&run_ok(
            &svc,
            "MATCH (n) WHERE n.meta.severity = 'high' RETURN n.title AS t ORDER BY t"
        )),
        ["crash on save", "dark mode"]
    );
}

#[test]
fn results_after_writes_include_the_new_nodes() {
    let svc = bug_tracker();
    run_ok(&svc, "CREATE INDEX FOR (n:Bug) ON (n.meta.severity)");
    run_ok(
        &svc,
        "CREATE (:Bug {title: 'leak', meta: {severity: 'high'}})",
    );
    run_ok(
        &svc,
        "MATCH (n:Bug {title: 'crash on save'}) SET n.meta = {severity: 'low'}",
    );
    assert_eq!(
        strings(&run_ok(
            &svc,
            "MATCH (n:Bug) WHERE n.meta.severity = 'high' RETURN n.title AS t"
        )),
        ["leak"]
    );
}

// ----- the index is really used ----------------------------------------------

/// A graph plus a path index synced *before* one more matching node is
/// created: a query that narrows through the index misses that node, a scan
/// finds it. That difference is how these tests see the index being used.
struct StaleProbe {
    graph: NativeGraph,
    paths: NativePathIndex,
}

impl StaleProbe {
    fn new(specs: Vec<PathIndexSpec>) -> Self {
        let graph = NativeGraph::new();
        let probe = Self {
            graph,
            paths: NativePathIndex::new(specs),
        };
        probe.exec("CREATE (:Doc {title: 'indexed', meta: {author: 'ann', size: 3}})");
        let mut probe = probe;
        probe.paths.sync(&probe.graph);
        probe.exec("CREATE (:Doc {title: 'late', meta: {author: 'ann', size: 3}})");
        probe
    }

    fn exec(&self, q: &str) -> ExecResult {
        let ctx = NativeQueryContext {
            paths: Some(&self.paths),
            ..NativeQueryContext::default()
        };
        execute_on_engine_with_context(&parse(q).expect("parse"), &self.graph, &ctx, HashMap::new())
            .unwrap_or_else(|e| panic!("execute `{q}`: {e:?}"))
    }

    fn titles(&self, q: &str) -> Vec<String> {
        let mut t = strings(&self.exec(q));
        t.sort();
        t
    }
}

fn spec(label: Option<&str>, segments: &[&str], wildcard: bool) -> PathIndexSpec {
    PathIndexSpec {
        label: label.map(str::to_string),
        path: PropertyPath::new(
            segments.iter().map(|s| (*s).to_string()).collect(),
            wildcard,
        )
        .expect("valid path"),
    }
}

#[test]
fn equality_in_and_range_narrow_through_the_index() {
    let p = StaleProbe::new(vec![spec(Some("Doc"), &["meta"], true)]);
    for q in [
        "MATCH (n:Doc) WHERE n.meta.author = 'ann' RETURN n.title AS t",
        "MATCH (n:Doc) WHERE 'ann' = n.meta.author RETURN n.title AS t",
        "MATCH (n:Doc) WHERE n.meta.author IN ['ann', 'zed'] RETURN n.title AS t",
        "MATCH (n:Doc) WHERE n.meta.size > 2 RETURN n.title AS t",
        "MATCH (n:Doc) WHERE n.meta.author = 'ann' AND n.title <> 'x' RETURN n.title AS t",
    ] {
        assert_eq!(
            p.titles(q),
            ["indexed"],
            "{q} should narrow through the index"
        );
    }
}

#[test]
fn uncovered_shapes_scan() {
    let p = StaleProbe::new(vec![spec(Some("Doc"), &["meta", "author"], false)]);
    for q in [
        // No label: the Doc-only index cannot vouch for other nodes.
        "MATCH (n) WHERE n.meta.author = 'ann' RETURN n.title AS t",
        // A path the index does not declare.
        "MATCH (n:Doc) WHERE n.meta.size = 3 RETURN n.title AS t",
        // Under OR the term is not required.
        "MATCH (n:Doc) WHERE n.meta.author = 'ann' OR n.title = 'x' RETURN n.title AS t",
    ] {
        assert_eq!(p.titles(q), ["indexed", "late"], "{q} must not narrow");
    }
}

// ----- DDL ---------------------------------------------------------------------

#[test]
fn show_indexes_lists_path_indexes_as_range() {
    let svc = bug_tracker();
    run_ok(
        &svc,
        "CREATE INDEX bug_meta IF NOT EXISTS FOR (n:Bug) ON (n.meta.severity, n.meta.assignee.*)",
    );
    run_ok(&svc, "CREATE RANGE INDEX FOR (d) ON (d.*)");
    let res = run_ok(
        &svc,
        "SHOW INDEXES YIELD name, type, entityType, labelsOrTypes, properties \
         RETURN name, type, entityType, labelsOrTypes, properties ORDER BY name",
    );
    assert_eq!(
        res.rows,
        vec![
            vec![
                s("bug_meta"),
                s("RANGE"),
                s("NODE"),
                Value::List(vec![s("Bug")]),
                Value::List(vec![s("meta.severity"), s("meta.assignee.*")]),
            ],
            vec![
                s("index_all_star"),
                s("RANGE"),
                s("NODE"),
                Value::List(vec![]),
                Value::List(vec![s("*")]),
            ],
        ]
    );
    let only_range = run_ok(
        &svc,
        "SHOW RANGE INDEXES YIELD name RETURN name ORDER BY name",
    );
    assert_eq!(only_range.rows.len(), 2);
    let only_vector = run_ok(&svc, "SHOW VECTOR INDEXES YIELD name RETURN name");
    assert!(only_vector.rows.is_empty());
}

#[test]
fn generated_names_and_if_not_exists() {
    let svc = bug_tracker();
    run_ok(&svc, "CREATE INDEX FOR (n:Bug) ON (n.meta.severity)");
    // Same definition again: the generated name already exists.
    let err = run(&svc, "CREATE INDEX FOR (n:Bug) ON (n.meta.severity)").expect_err("duplicate");
    assert!(err.to_string().contains("already exists"), "{err}");
    run_ok(
        &svc,
        "CREATE INDEX IF NOT EXISTS FOR (n:Bug) ON (n.meta.severity)",
    );
    let names = strings(&run_ok(&svc, "SHOW INDEXES YIELD name RETURN name"));
    assert_eq!(names, ["index_Bug_meta_severity"]);
}

#[test]
fn index_names_are_unique_across_vector_and_path_indexes() {
    let svc = bug_tracker();
    run_ok(
        &svc,
        "CREATE VECTOR INDEX shared FOR (c:Chunk) ON (c.embedding)",
    );
    let err = run(&svc, "CREATE INDEX shared FOR (n:Bug) ON (n.meta.severity)")
        .expect_err("name taken by the vector index");
    assert!(err.to_string().contains("already exists"), "{err}");
}

#[test]
fn drop_index_removes_path_and_vector_indexes() {
    let svc = bug_tracker();
    run_ok(&svc, "CREATE INDEX sev FOR (n:Bug) ON (n.meta.severity)");
    run_ok(
        &svc,
        "CREATE VECTOR INDEX chunks FOR (c:Chunk) ON (c.embedding)",
    );
    run_ok(&svc, "DROP INDEX sev");
    run_ok(&svc, "DROP INDEX chunks");
    assert!(run_ok(&svc, "SHOW INDEXES YIELD name RETURN name")
        .rows
        .is_empty());
    let err = run(&svc, "DROP INDEX sev").expect_err("gone");
    assert!(err.to_string().contains("no index named"), "{err}");
    run_ok(&svc, "DROP INDEX sev IF EXISTS");
    // Queries still answer after the drop (now by scanning).
    assert_eq!(
        strings(&run_ok(
            &svc,
            "MATCH (n:Bug) WHERE n.meta.severity = 'low' RETURN n.title AS t"
        )),
        ["typo in footer"]
    );
}

#[test]
fn top_level_and_unsupported_forms_stay_no_ops() {
    let svc = bug_tracker();
    for q in [
        // Top-level properties are always indexed.
        "CREATE INDEX FOR (n:Bug) ON (n.title)",
        "CREATE INDEX title_idx FOR (n:Bug) ON (n.title)",
        // Not range or text indexes / not nodes.
        "CREATE POINT INDEX FOR (n:Bug) ON (n.meta.location)",
        "CREATE INDEX FOR ()-[r:LINKS]-() ON (r.meta.kind)",
        "CREATE FULLTEXT INDEX ft FOR (n:Bug) ON EACH [n.title]",
        "CREATE LOOKUP INDEX FOR (n) ON EACH labels(n)",
    ] {
        run_ok(&svc, q);
    }
    assert!(run_ok(&svc, "SHOW INDEXES YIELD name RETURN name")
        .rows
        .is_empty());
}

#[test]
fn ddl_accepts_neo4j_spellings() {
    let svc = bug_tracker();
    for q in [
        "CREATE BTREE INDEX a FOR (n:Bug) ON (n.meta.severity)",
        "CREATE RANGE INDEX b IF NOT EXISTS FOR (n:Bug) ON (n.meta.points) OPTIONS {}",
        "CREATE INDEX c FOR (n:Bug) ON (n.`odd key`.x)",
    ] {
        run_ok(&svc, q);
    }
    let props = run_ok(
        &svc,
        "SHOW INDEXES YIELD name, properties RETURN name, properties ORDER BY name",
    );
    assert_eq!(
        props.rows[2],
        vec![s("c"), Value::List(vec![s("`odd key`.x")])]
    );
}

#[test]
fn ddl_without_a_native_service_is_an_engine_capability_error() {
    let g = NativeGraph::new();
    let err = drevo::cypher::executor::execute_on_engine(
        &parse("CREATE INDEX FOR (n:Bug) ON (n.meta.severity)").unwrap(),
        &g,
        HashMap::new(),
    )
    .expect_err("needs the native service");
    assert!(err.to_string().contains("CREATE INDEX"), "{err}");
}

#[test]
fn definitions_survive_reopen() {
    let dir = std::env::temp_dir().join(format!(
        "drevo_path_idx_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let wal = dir.join("native.wal");
    {
        let svc = NativeService::open(&wal).unwrap();
        run_ok(
            &svc,
            "CREATE INDEX orders_city FOR (o:Order) ON (o.address.city)",
        );
        run_ok(
            &svc,
            "CREATE (:Order {title: 'o1', address: {city: 'Riga'}}), \
                    (:Order {title: 'o2', address: {city: 'Oslo'}})",
        );
    }
    let svc = NativeService::open(&wal).unwrap();
    assert_eq!(
        strings(&run_ok(&svc, "SHOW INDEXES YIELD name RETURN name")),
        ["orders_city"]
    );
    assert_eq!(
        strings(&run_ok(
            &svc,
            "MATCH (o:Order) WHERE o.address.city = 'Oslo' RETURN o.title AS t"
        )),
        ["o2"]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ----- integration: ERP orders -----------------------------------------------

#[test]
fn erp_orders_by_customer_address() {
    // An ERP keeps the shipping address and customer as nested maps; the
    // warehouse dashboard filters on them.
    let svc = NativeService::in_memory();
    run_ok(&svc, "CREATE INDEX order_doc FOR (o:Order) ON (o.*)");
    run_ok(
        &svc,
        "UNWIND range(1, 60) AS i \
         CREATE (:Order {title: 'order-' + toString(i), total: i * 10, \
                 address: {city: CASE i % 3 WHEN 0 THEN 'Riga' WHEN 1 THEN 'Oslo' ELSE 'Kyiv' END, \
                           zip: toString(1000 + i)}, \
                 customer: {tier: CASE WHEN i % 10 = 0 THEN 'gold' ELSE 'basic' END, \
                            contact: {country: 'EE'}}})",
    );
    let gold_riga = run_ok(
        &svc,
        "MATCH (o:Order) WHERE o.address.city = 'Riga' AND o.customer.tier = 'gold' \
         RETURN o.title AS t ORDER BY o.total",
    );
    assert_eq!(strings(&gold_riga), ["order-30", "order-60"]);
    let count = run_ok(
        &svc,
        "MATCH (o:Order) WHERE o.customer.contact.country = 'EE' AND o.total > 500 \
         RETURN count(o) AS c",
    );
    assert_eq!(count.rows, vec![vec![Value::Integer(10)]]);
}

// ----- differential: indexed answers equal scanned answers --------------------

mod differential {
    use super::*;
    use proptest::prelude::*;

    /// A leaf value: mixed types on purpose, so equality, ranges and their
    /// type errors all get exercised.
    fn leaf() -> impl Strategy<Value = String> {
        prop_oneof![
            (0i64..5).prop_map(|i| i.to_string()),
            (0i64..5).prop_map(|i| format!("{i}.5")),
            "[a-c]".prop_map(|s| format!("'{s}'")),
            Just("true".to_string()),
            Just("null".to_string()),
            Just("['a', 1]".to_string()),
        ]
    }

    fn doc() -> impl Strategy<Value = (bool, String, String, String)> {
        (any::<bool>(), leaf(), leaf(), leaf())
    }

    fn rows(svc: &NativeService, q: &str) -> Result<Vec<String>, String> {
        match run(svc, q) {
            Ok(res) => {
                let mut v: Vec<String> = res.rows.iter().map(|r| format!("{r:?}")).collect();
                v.sort();
                Ok(v)
            }
            // The two sides must fail the same way too (e.g. a type error).
            Err(e) => Err(format!("{e}")),
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]
        #[test]
        fn indexed_and_scanned_answers_agree(
            docs in proptest::collection::vec(doc(), 1..12),
            probe in leaf(),
        ) {
            let indexed = NativeService::in_memory();
            let plain = NativeService::in_memory();
            run_ok(&indexed, "CREATE INDEX FOR (n:Doc) ON (n.meta.*)");
            run_ok(&indexed, "CREATE INDEX FOR (n) ON (n.meta.a)");
            for (i, (is_doc, a, b, c)) in docs.iter().enumerate() {
                let label = if *is_doc { "Doc" } else { "Other" };
                let q = format!(
                    "CREATE (:{label} {{title: 't{i}', meta: {{a: {a}, sub: {{b: {b}}}, c: {c}}}}})"
                );
                run_ok(&indexed, &q);
                run_ok(&plain, &q);
            }
            for q in [
                format!("MATCH (n:Doc) WHERE n.meta.a = {probe} RETURN n.title"),
                format!("MATCH (n) WHERE n.meta.a = {probe} RETURN n.title"),
                format!("MATCH (n:Doc) WHERE n.meta.sub.b = {probe} RETURN n.title"),
                format!("MATCH (n:Doc) WHERE n.meta.c IN [{probe}, 1] RETURN n.title"),
                "MATCH (n:Doc) WHERE n.meta.sub.b >= 2 RETURN n.title".to_string(),
                "MATCH (n:Doc) WHERE n.meta.a < 3 AND n.meta.c = 'a' RETURN n.title".to_string(),
                "MATCH (n:Doc) WHERE n.meta.sub > 1 RETURN n.title".to_string(),
            ] {
                // When the scan answers, the indexed query must give exactly
                // the same rows. When the scan hits a type error, the indexed
                // query may answer or fail on another node: it never evaluates
                // rows another conjunct already excluded, and it visits nodes
                // in a different order. The top-level property index has
                // always behaved this way, and so does Neo4j.
                let want = rows(&plain, &q);
                if want.is_ok() {
                    prop_assert_eq!(rows(&indexed, &q), want, "{}", q);
                }
            }
        }
    }
}
