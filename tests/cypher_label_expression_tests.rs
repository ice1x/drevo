//! Neo4j 5 label expressions — issue #572.
//!
//! `|` (or), `&` (and), `!` (not), `%` (any label) and parentheses, in node
//! patterns and in the `n:…` predicate (#568). The legacy `:A:B` chain keeps
//! meaning "all of". Precedence, loosest first: `|`, `&`, `!`.

use std::collections::HashMap;

use drevo::cypher::executor::{execute_on_engine as execute, Value};
use drevo::cypher::parser::parse;
use drevo::native::NativeGraph;

fn run(source: &str, db: &NativeGraph) -> Vec<Vec<Value>> {
    let q = parse(source).unwrap_or_else(|e| panic!("parse `{source}`: {e}"));
    execute(&q, db, HashMap::new())
        .unwrap_or_else(|e| panic!("execute `{source}`: {e}"))
        .rows
}

fn titles(source: &str, db: &NativeGraph) -> Vec<String> {
    run(source, db)
        .iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            other => panic!("expected a string, got {other:?}"),
        })
        .collect()
}

/// Bug tracker with an archived bug and a stray comment.
fn bug_tracker() -> NativeGraph {
    let db = NativeGraph::new();
    run(
        "CREATE (:Bug {title: 'crash on save'}), (:Bug:Archived {title: 'old crash'}), \
         (:Feature {title: 'dark mode'}), (:Comment {title: 'me too'})",
        &db,
    );
    db
}

#[test]
fn pattern_disjunction() {
    let db = bug_tracker();
    assert_eq!(
        titles("MATCH (n:Bug|Feature) RETURN n.title AS t ORDER BY t", &db),
        ["crash on save", "dark mode", "old crash"]
    );
}

#[test]
fn pattern_negation() {
    let db = bug_tracker();
    assert_eq!(
        titles("MATCH (n:!Bug) RETURN n.title AS t ORDER BY t", &db),
        ["dark mode", "me too"]
    );
}

#[test]
fn pattern_conjunction_equals_legacy_chain() {
    let db = bug_tracker();
    let amp = titles("MATCH (n:Bug&Archived) RETURN n.title AS t", &db);
    let chain = titles("MATCH (n:Bug:Archived) RETURN n.title AS t", &db);
    assert_eq!(amp, ["old crash"]);
    assert_eq!(amp, chain);
}

#[test]
fn pattern_wildcard_matches_every_labelled_node() {
    let db = bug_tracker();
    assert_eq!(
        run("MATCH (n:%) RETURN count(n) AS c", &db),
        vec![vec![Value::Integer(4)]]
    );
}

#[test]
fn count_of_an_expression_pattern_is_not_the_whole_graph() {
    // `MATCH (n…) RETURN count(n)` has a label-count fast path; an expression
    // with no required label must not fall through to "count every node".
    let db = bug_tracker();
    for (q, want) in [
        ("MATCH (n:!Bug) RETURN count(n) AS c", 2),
        ("MATCH (n:Bug|Feature) RETURN count(*) AS c", 3),
        ("MATCH (n:Bug&!Archived) RETURN count(n) AS c", 1),
    ] {
        assert_eq!(run(q, &db), vec![vec![Value::Integer(want)]], "{q}");
    }
}

#[test]
fn pattern_grouping_and_properties() {
    let db = bug_tracker();
    assert_eq!(
        titles(
            "MATCH (n:(Bug|Feature)&!Archived) RETURN n.title AS t ORDER BY t",
            &db
        ),
        ["crash on save", "dark mode"]
    );
    assert_eq!(
        titles(
            "MATCH (n:Bug|Feature {title: 'dark mode'}) RETURN n.title AS t",
            &db
        ),
        ["dark mode"]
    );
}

#[test]
fn precedence_not_then_and_then_or() {
    let db = bug_tracker();
    // `!Bug|Comment` is `(!Bug)|Comment`: everything except the bugs.
    assert_eq!(
        titles("MATCH (n:!Bug|Comment) RETURN n.title AS t ORDER BY t", &db),
        ["dark mode", "me too"]
    );
    // `Feature|Bug&Archived` is `Feature|(Bug&Archived)`.
    assert_eq!(
        titles(
            "MATCH (n:Feature|Bug&Archived) RETURN n.title AS t ORDER BY t",
            &db
        ),
        ["dark mode", "old crash"]
    );
}

#[test]
fn expressions_in_where() {
    let db = bug_tracker();
    assert_eq!(
        titles(
            "MATCH (n) WHERE n:Bug|Feature AND NOT n:Archived RETURN n.title AS t ORDER BY t",
            &db
        ),
        ["crash on save", "dark mode"]
    );
    assert_eq!(
        titles("MATCH (n) WHERE n:!(Bug|Feature) RETURN n.title AS t", &db),
        ["me too"]
    );
}

#[test]
fn relationship_type_expressions_in_where() {
    // Story editor: chapters linked by FOLLOWS / REFERENCES / DRAFT_OF.
    let db = NativeGraph::new();
    run(
        "CREATE (a:Chapter {title: 'one'})-[:FOLLOWS]->(b:Chapter {title: 'two'}), \
         (b)-[:REFERENCES]->(a), (a)-[:DRAFT_OF]->(b)",
        &db,
    );
    assert_eq!(
        titles(
            "MATCH (x)-[r]->(y) WHERE r:FOLLOWS|REFERENCES RETURN x.title AS t ORDER BY t",
            &db
        ),
        ["one", "two"]
    );
    assert_eq!(
        titles(
            "MATCH (x)-[r]->(y) WHERE r:!FOLLOWS&!REFERENCES RETURN x.title AS t",
            &db
        ),
        ["one"]
    );
}

#[test]
fn list_comprehension_pipe_is_still_the_projection() {
    // Task manager: `|` after a label in a comprehension filter is the
    // projection, not a disjunction; parenthesise to get the disjunction.
    let db = NativeGraph::new();
    run(
        "CREATE (:Task:Blocked {title: 'deploy'}), (:Task {title: 'review'}), (:Epic {title: 'q4'})",
        &db,
    );
    assert_eq!(
        run(
            "MATCH (n) WITH collect(n) AS ns \
             RETURN [x IN ns WHERE x:Blocked | x.title] AS blocked",
            &db
        ),
        vec![vec![Value::List(vec![Value::String("deploy".into())])]]
    );
    let rows = run(
        "MATCH (n) WITH collect(n) AS ns \
         RETURN size([x IN ns WHERE x:(Blocked|Epic) | x.title]) AS c",
        &db,
    );
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn create_and_merge_reject_label_expressions() {
    for q in [
        "CREATE (n:Bug|Feature)",
        "CREATE (n:!Bug)",
        "MERGE (n:Bug|Feature)",
        "MERGE (n:%)",
    ] {
        let e = parse(q).expect_err(q);
        assert!(e.to_string().contains("label expression"), "{q}: {e}");
    }
}

#[test]
fn legacy_forms_keep_working() {
    // ERP: partners that are suppliers, plus SET/REMOVE label syntax.
    let db = NativeGraph::new();
    run(
        "CREATE (:Partner:Supplier {title: 'Acme'}), (:Partner {title: 'Globex'})",
        &db,
    );
    assert_eq!(
        titles("MATCH (n:Partner:Supplier) RETURN n.title AS t", &db),
        ["Acme"]
    );
    run("MATCH (n:Partner {title: 'Globex'}) SET n:Customer", &db);
    assert_eq!(
        titles("MATCH (n) WHERE n:Customer RETURN n.title AS t", &db),
        ["Globex"]
    );
    assert_eq!(
        run("MATCH (a)-[r:A|B]->(b) RETURN count(r) AS c", &db),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn deep_label_expressions_are_rejected_not_stack_overflows() {
    for q in [
        format!("MATCH (n:{}A) RETURN n", "!".repeat(100_000)),
        format!(
            "MATCH (n:{}A{}) RETURN n",
            "(".repeat(100_000),
            ")".repeat(100_000)
        ),
        format!("MATCH (n) WHERE n:{}A RETURN n", "!".repeat(100_000)),
    ] {
        let e = parse(&q).expect_err("too deep");
        assert!(
            matches!(e, drevo::cypher::parser::ParseError::NestingTooDeep { .. }),
            "{e}"
        );
    }
}

mod display_round_trip {
    use drevo::cypher::ast::{Clause, LabelExpr};
    use drevo::cypher::parser::parse;
    use proptest::prelude::*;

    fn label_expr() -> impl Strategy<Value = LabelExpr> {
        let leaf = prop_oneof!["[A-C]".prop_map(LabelExpr::Name), Just(LabelExpr::Any),];
        leaf.prop_recursive(4, 24, 2, |inner| {
            prop_oneof![
                inner.clone().prop_map(|e| LabelExpr::Not(Box::new(e))),
                (inner.clone(), inner.clone())
                    .prop_map(|(a, b)| LabelExpr::And(Box::new(a), Box::new(b))),
                (inner.clone(), inner).prop_map(|(a, b)| LabelExpr::Or(Box::new(a), Box::new(b))),
            ]
        })
    }

    proptest! {
        /// `Display` is what EXPLAIN and error messages print; whatever it
        /// prints must parse back to an expression that matches exactly the
        /// same label sets (a pure `&` chain comes back as plain `labels`).
        #[test]
        fn display_reparses_to_an_equivalent_expression(e in label_expr()) {
            let q = parse(&format!("MATCH (n:{e}) RETURN n")).expect("parse");
            let Clause::Match(m) = &q.parts[0].query.clauses[0] else { panic!("not a MATCH") };
            let head = &m.patterns[0].path.head;
            let parsed = match &head.label_expr {
                Some(expr) => expr.clone(),
                None => head
                    .labels
                    .iter()
                    .cloned()
                    .map(LabelExpr::Name)
                    .reduce(|a, b| LabelExpr::And(Box::new(a), Box::new(b)))
                    .expect("at least one label"),
            };
            for have in [vec![], vec!["A".to_string()], vec!["A".into(), "B".into()], vec!["C".into()]] {
                prop_assert_eq!(parsed.matches(&have), e.matches(&have), "{} on {:?}", e, have);
            }
        }
    }
}
