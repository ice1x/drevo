//! Levenshtein edit distance through Cypher (issue #593):
//! `apoc.text.levenshteinDistance` / `apoc.text.levenshteinSimilarity`, and
//! `CALL drevo.text.fuzzy(indexName, query, maxEdits)` over a text index.

use std::collections::HashMap;

use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

fn run_with(
    svc: &NativeService,
    q: &str,
    params: HashMap<String, Value>,
) -> Result<ExecResult, ExecError> {
    svc.execute(
        &parse(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}")),
        params,
    )
}

fn run(svc: &NativeService, q: &str) -> Result<ExecResult, ExecError> {
    run_with(svc, q, HashMap::new())
}

fn run_ok(svc: &NativeService, q: &str) -> ExecResult {
    run(svc, q).unwrap_or_else(|e| panic!("execute `{q}`: {e:?}"))
}

fn one(svc: &NativeService, q: &str) -> Value {
    run_ok(svc, q).rows[0][0].clone()
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

// ----- scalar functions -------------------------------------------------------

#[test]
fn distance_and_similarity_functions() {
    let svc = NativeService::in_memory();
    assert_eq!(
        one(
            &svc,
            "RETURN apoc.text.levenshteinDistance('kitten', 'sitting')"
        ),
        Value::Integer(3)
    );
    assert_eq!(
        one(&svc, "RETURN apoc.text.levenshteinDistance('ёлка', 'елка')"),
        Value::Integer(1)
    );
    assert_eq!(
        one(
            &svc,
            "RETURN apoc.text.levenshteinSimilarity('abcd', 'abed')"
        ),
        Value::Float(0.75)
    );
    assert_eq!(
        one(&svc, "RETURN apoc.text.levenshteinSimilarity('', '')"),
        Value::Float(1.0)
    );
    // Function names are case-insensitive, like every other function.
    assert_eq!(
        one(&svc, "RETURN APOC.TEXT.LEVENSHTEINDISTANCE('a', 'b')"),
        Value::Integer(1)
    );
}

#[test]
fn null_in_null_out_and_type_errors() {
    let svc = NativeService::in_memory();
    assert_eq!(
        one(&svc, "RETURN apoc.text.levenshteinDistance(null, 'x')"),
        Value::Null
    );
    assert_eq!(
        one(&svc, "RETURN apoc.text.levenshteinSimilarity('x', null)"),
        Value::Null
    );
    let err = run(&svc, "RETURN apoc.text.levenshteinDistance('x', 1)").expect_err("type");
    assert!(err.to_string().contains("String"), "{err}");
    let err = run(&svc, "RETURN apoc.text.levenshteinDistance('x')").expect_err("arity");
    assert!(err.to_string().contains("2 argument"), "{err}");
    // Other dotted names are still unknown functions.
    assert!(run(&svc, "RETURN apoc.text.nosuch('x')").is_err());
}

#[test]
fn distance_filters_rows() {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CREATE (:Word {title: 'w1', text: 'receive'}), (:Word {title: 'w2', text: 'deceive'}), \
                (:Word {title: 'w3', text: 'relieve'}), (:Word {title: 'w4', text: 'graph'})",
    );
    let res = run_ok(
        &svc,
        "MATCH (w:Word) WITH w, apoc.text.levenshteinDistance(w.text, 'recieve') AS d \
         WHERE d <= 2 RETURN w.text, d ORDER BY d, w.text",
    );
    assert_eq!(
        res.rows,
        vec![
            vec![s("relieve"), Value::Integer(1)],
            vec![s("receive"), Value::Integer(2)],
        ]
    );
}

// ----- drevo.text.fuzzy --------------------------------------------------------

/// Bug tracker: titles that are near-duplicates with typos.
fn bugs() -> NativeService {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CREATE TEXT INDEX bug_summary FOR (b:Bug) ON (b.summary)",
    );
    run_ok(
        &svc,
        "CREATE (:Bug {title: 'b1', summary: 'login fails on Safari'}), \
                (:Bug {title: 'b2', summary: 'logn fails on Safari'}), \
                (:Bug {title: 'b3', summary: 'login fails on Safary'}), \
                (:Bug {title: 'b4', summary: 'export is slow'}), \
                (:Task {title: 't1', summary: 'login fails on Safari'})",
    );
    svc
}

fn fuzzy(svc: &NativeService, q: &str) -> Vec<(String, i64)> {
    run_ok(svc, q)
        .rows
        .iter()
        .map(|r| match (&r[0], &r[1]) {
            (Value::String(t), Value::Integer(d)) => (t.clone(), *d),
            other => panic!("unexpected row {other:?}"),
        })
        .collect()
}

#[test]
fn fuzzy_returns_nodes_within_the_budget_nearest_first() {
    let svc = bugs();
    assert_eq!(
        fuzzy(
            &svc,
            "CALL drevo.text.fuzzy('bug_summary', 'login fails on Safari', 1) \
             YIELD node, distance RETURN node.title, distance"
        ),
        [
            ("b1".to_string(), 0),
            ("b2".to_string(), 1),
            ("b3".to_string(), 1)
        ]
    );
    // Only the index's label is searched: the Task is not a Bug.
    assert_eq!(
        fuzzy(
            &svc,
            "CALL drevo.text.fuzzy('bug_summary', 'export is slw', 2) \
             YIELD node, distance RETURN node.title, distance"
        ),
        [("b4".to_string(), 1)]
    );
}

#[test]
fn fuzzy_takes_parameters_and_composes() {
    let svc = bugs();
    let res = run_with(
        &svc,
        "CALL drevo.text.fuzzy($index, $q, $k) YIELD node, distance \
         WHERE distance > 0 RETURN count(node) AS near",
        HashMap::from([
            ("index".to_string(), s("bug_summary")),
            ("q".to_string(), s("login fails on Safari")),
            ("k".to_string(), Value::Integer(1)),
        ]),
    )
    .expect("params");
    assert_eq!(res.rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn fuzzy_sees_writes_made_earlier_in_the_statement() {
    let svc = bugs();
    let got = fuzzy(
        &svc,
        "CREATE (:Bug {title: 'b5', summary: 'export is slooow'}) \
         WITH 1 AS ignored \
         CALL drevo.text.fuzzy('bug_summary', 'export is slow', 2) \
         YIELD node, distance RETURN node.title, distance",
    );
    assert_eq!(got, [("b4".to_string(), 0), ("b5".to_string(), 2)]);
}

#[test]
fn fuzzy_rejects_bad_arguments() {
    let svc = bugs();
    run_ok(&svc, "CREATE INDEX nested FOR (b:Bug) ON (b.meta.severity)");
    for (q, needle) in [
        (
            "CALL drevo.text.fuzzy('nope', 'x', 1) YIELD node RETURN node",
            "no text index",
        ),
        (
            "CALL drevo.text.fuzzy('nested', 'x', 1) YIELD node RETURN node",
            "no text index",
        ),
        (
            "CALL drevo.text.fuzzy('bug_summary', 'x', -1) YIELD node RETURN node",
            "non-negative",
        ),
        (
            "CALL drevo.text.fuzzy('bug_summary', 3, 1) YIELD node RETURN node",
            "String",
        ),
    ] {
        let err = run(&svc, q).expect_err(q);
        assert!(err.to_string().contains(needle), "{q}: {err}");
    }
}

#[test]
fn fuzzy_without_a_native_service_is_an_engine_capability_error() {
    let g = drevo::native::NativeGraph::new();
    let err = drevo::cypher::executor::execute_on_engine(
        &parse("CALL drevo.text.fuzzy('i', 'x', 1) YIELD node RETURN node").unwrap(),
        &g,
        HashMap::new(),
    )
    .expect_err("needs the native service");
    assert!(err.to_string().contains("drevo.text.fuzzy"), "{err}");
}

// ----- integration: CBT journal ------------------------------------------------

#[test]
fn journal_groups_misspelled_emotions() {
    // A CBT journal tags entries with free-typed emotions; the weekly review
    // folds misspellings into the canonical word.
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CREATE TEXT INDEX emotion FOR (e:Entry) ON (e.emotion)",
    );
    run_ok(
        &svc,
        "UNWIND range(1, 40) AS i \
         CREATE (:Entry {title: 'entry-' + toString(i), \
                 emotion: ['anxious', 'anxous', 'anxiuos', 'calm', 'angry', 'anxios'][i % 6]})",
    );
    let res = run_ok(
        &svc,
        "CALL drevo.text.fuzzy('emotion', 'anxious', 2) YIELD node, distance \
         RETURN node.emotion AS spelled, min(distance) AS d, count(*) AS n ORDER BY d, spelled",
    );
    let spelled: Vec<Value> = res.rows.iter().map(|r| r[0].clone()).collect();
    assert_eq!(
        spelled,
        vec![s("anxious"), s("anxios"), s("anxous"), s("anxiuos")]
    );
}

// ----- differential: fuzzy == brute force -------------------------------------

mod differential {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(40))]
        #[test]
        fn fuzzy_matches_a_brute_force_scan(
            words in proptest::collection::vec("[abё]{0,7}", 1..20),
            query in "[abё]{0,7}",
            k in 0i64..4,
        ) {
            let svc = NativeService::in_memory();
            run_ok(&svc, "CREATE TEXT INDEX w FOR (n:W) ON (n.text)");
            for (i, w) in words.iter().enumerate() {
                run_ok(&svc, &format!("CREATE (:W {{title: 't{i}', text: '{w}'}})"));
            }
            let params = || HashMap::from([
                ("q".to_string(), s(&query)),
                ("k".to_string(), Value::Integer(k)),
            ]);
            let fuzzy = run_with(
                &svc,
                "CALL drevo.text.fuzzy('w', $q, $k) YIELD node, distance \
                 RETURN node.title, distance ORDER BY distance, node.title",
                params(),
            ).expect("fuzzy");
            let scan = run_with(
                &svc,
                "MATCH (n:W) WITH n, apoc.text.levenshteinDistance(n.text, $q) AS distance \
                 WHERE distance <= $k RETURN n.title, distance ORDER BY distance, n.title",
                params(),
            ).expect("scan");
            prop_assert_eq!(fuzzy.rows, scan.rows);
        }
    }
}
