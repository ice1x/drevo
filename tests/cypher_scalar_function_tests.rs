//! End-to-end Cypher scalar-function tests — Phase 10 follow-up task `00138`.
//!
//! The executor ships a built-in library of standalone scalar functions —
//! string (`toLower` / `toUpper` / `trim` / `substring` / `replace` / `split`
//! / `left` / `right` / `reverse` / `toString`), numeric (`abs` / `ceil` /
//! `floor` / `round` / `sign` / `sqrt` / `toInteger` / `toFloat` /
//! `toBoolean`), and list / scalar (`size` / `length` / `head` / `last` /
//! `tail` / `range` / `coalesce` / `keys` / `labels` / `type` / `id` /
//! `properties`).
//!
//! Every function (except `coalesce`, whose purpose is to skip `NULL`s) is
//! NULL-propagating: a `NULL` argument yields `NULL`, never an error, so a
//! function applied across a heterogeneous scan quietly skips rows whose
//! property is absent. Genuine misuse (wrong arity / type) is a recoverable
//! `ExecError::InvalidFunctionCall`; an unknown function name stays
//! `ExecError::Unsupported`.
//!
//! These cases drive the real parser → executor pipeline across the five
//! drevo target scenario domains (CBT journal, story/book editor, IT task
//! manager, ERP, bug tracker) plus the cross-cutting semantics.

use std::collections::HashMap;

use drevo::cypher::executor::{execute, ExecError, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;

fn db() -> Drevo {
    Drevo::open_in_memory().expect("open in-memory drevo")
}

fn run(source: &str, drevo: &Drevo) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute").rows
}

fn run_err(source: &str, drevo: &Drevo) -> ExecError {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect_err("expected execution error")
}

fn exec(source: &str, drevo: &Drevo) {
    let q = parse(source).expect("parse");
    execute(&q, drevo, HashMap::new()).expect("execute");
}

/// One-row, one-column projection helper.
fn one(source: &str, drevo: &Drevo) -> Value {
    let rows = run(source, drevo);
    assert_eq!(rows.len(), 1, "expected exactly one row from {source:?}");
    rows[0][0].clone()
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

// ===== String functions =====================================================

#[test]
fn case_normalisation_lower_upper_trim() {
    let db = db();
    assert_eq!(one("RETURN toLower('MiXeD') AS v", &db), s("mixed"));
    assert_eq!(one("RETURN toUpper('MiXeD') AS v", &db), s("MIXED"));
    assert_eq!(one("RETURN trim('  spaced  ') AS v", &db), s("spaced"));
    assert_eq!(one("RETURN ltrim('  spaced  ') AS v", &db), s("spaced  "));
    assert_eq!(one("RETURN rtrim('  spaced  ') AS v", &db), s("  spaced"));
}

#[test]
fn substring_replace_split_left_right_reverse() {
    let db = db();
    assert_eq!(one("RETURN substring('drevo', 2) AS v", &db), s("evo"));
    assert_eq!(one("RETURN substring('drevo', 0, 3) AS v", &db), s("dre"));
    assert_eq!(
        one("RETURN replace('a-b-c', '-', '/') AS v", &db),
        s("a/b/c")
    );
    assert_eq!(
        one("RETURN split('x,y,z', ',') AS v", &db),
        Value::List(vec![s("x"), s("y"), s("z")])
    );
    assert_eq!(one("RETURN left('database', 4) AS v", &db), s("data"));
    assert_eq!(one("RETURN right('database', 4) AS v", &db), s("base"));
    assert_eq!(one("RETURN reverse('abc') AS v", &db), s("cba"));
}

// ===== Numeric functions ====================================================

#[test]
fn arithmetic_helpers_preserve_or_widen_type() {
    let db = db();
    assert_eq!(one("RETURN abs(-9) AS v", &db), Value::Integer(9));
    assert_eq!(one("RETURN abs(-1.25) AS v", &db), Value::Float(1.25));
    assert_eq!(one("RETURN ceil(2.1) AS v", &db), Value::Float(3.0));
    assert_eq!(one("RETURN floor(2.9) AS v", &db), Value::Float(2.0));
    assert_eq!(one("RETURN round(2.5) AS v", &db), Value::Float(3.0));
    assert_eq!(one("RETURN sqrt(16) AS v", &db), Value::Float(4.0));
    assert_eq!(one("RETURN sign(-4) AS v", &db), Value::Integer(-1));
}

#[test]
fn type_conversions_are_lenient() {
    let db = db();
    assert_eq!(
        one("RETURN toInteger('100') AS v", &db),
        Value::Integer(100)
    );
    assert_eq!(one("RETURN toInteger(4.7) AS v", &db), Value::Integer(4));
    assert_eq!(one("RETURN toInteger('nope') AS v", &db), Value::Null);
    assert_eq!(one("RETURN toFloat('2.5') AS v", &db), Value::Float(2.5));
    assert_eq!(
        one("RETURN toBoolean('false') AS v", &db),
        Value::Bool(false)
    );
    assert_eq!(one("RETURN toString(7) AS v", &db), s("7"));
}

// ===== List / scalar functions ==============================================

#[test]
fn list_helpers_and_range() {
    let db = db();
    assert_eq!(
        one("RETURN size([10, 20, 30]) AS v", &db),
        Value::Integer(3)
    );
    assert_eq!(
        one("RETURN head([10, 20, 30]) AS v", &db),
        Value::Integer(10)
    );
    assert_eq!(
        one("RETURN last([10, 20, 30]) AS v", &db),
        Value::Integer(30)
    );
    assert_eq!(
        one("RETURN tail([10, 20, 30]) AS v", &db),
        Value::List(vec![Value::Integer(20), Value::Integer(30)])
    );
    assert_eq!(
        one("RETURN range(1, 5) AS v", &db),
        Value::List(vec![
            Value::Integer(1),
            Value::Integer(2),
            Value::Integer(3),
            Value::Integer(4),
            Value::Integer(5),
        ])
    );
}

#[test]
fn coalesce_picks_first_present_value() {
    let db = db();
    // CBT journal: a thought may have no explicit `mood`, fall back to a label.
    exec("CREATE (:Thought {text: 'I failed'})", &db);
    assert_eq!(
        one(
            "MATCH (t:Thought) RETURN coalesce(t.mood, 'unspecified') AS v",
            &db
        ),
        s("unspecified")
    );
    assert_eq!(
        one("RETURN coalesce(null, null, 42) AS v", &db),
        Value::Integer(42)
    );
}

// ===== Graph-aware scalar functions =========================================

#[test]
fn keys_labels_type_id_properties_over_graph() {
    let db = db();
    exec(
        "CREATE (:Task {title: 'Ship', priority: 3})-[:BLOCKS {weight: 2}]->(:Task {title: 'Deploy'})",
        &db,
    );
    // labels() of a node.
    assert_eq!(
        one("MATCH (n:Task {title: 'Ship'}) RETURN labels(n) AS v", &db),
        Value::List(vec![s("Task")])
    );
    // type() of a relationship. The path head must be a named node — the
    // executor resolves each segment's predecessor by variable.
    assert_eq!(
        one(
            "MATCH (a:Task)-[r:BLOCKS]->(b:Task) RETURN type(r) AS v",
            &db
        ),
        s("BLOCKS")
    );
    // id() is an Integer.
    assert!(matches!(
        one("MATCH (n:Task {title: 'Ship'}) RETURN id(n) AS v", &db),
        Value::Integer(_)
    ));
    // keys() includes the user property plus the synthesised title alias.
    match one("MATCH (n:Task {title: 'Ship'}) RETURN keys(n) AS v", &db) {
        Value::List(items) => {
            assert!(items.contains(&s("priority")));
            assert!(items.contains(&s("title")));
        }
        other => panic!("expected List, got {other:?}"),
    }
    // properties() of a relationship.
    match one(
        "MATCH (a:Task)-[r:BLOCKS]->(b:Task) RETURN properties(r) AS v",
        &db,
    ) {
        Value::Map(m) => assert_eq!(m.get("weight"), Some(&Value::Integer(2))),
        other => panic!("expected Map, got {other:?}"),
    }
}

// ===== NULL propagation & error semantics ===================================

#[test]
fn null_argument_propagates_not_errors() {
    let db = db();
    // Story editor: scan chapters, some without a `subtitle` property.
    exec("CREATE (:Chapter {title: 'One', subtitle: 'Dawn'})", &db);
    exec("CREATE (:Chapter {title: 'Two'})", &db);
    let rows = run(
        "MATCH (c:Chapter) RETURN c.title AS t, toUpper(c.subtitle) AS up ORDER BY t",
        &db,
    );
    assert_eq!(rows.len(), 2);
    // Chapter One has a subtitle -> uppercased; Two has none -> NULL, not error.
    assert_eq!(rows[0][1], s("DAWN"));
    assert_eq!(rows[1][1], Value::Null);
}

#[test]
fn wrong_type_is_invalid_call_not_unsupported() {
    let db = db();
    let e = run_err("RETURN size(123) AS v", &db);
    assert!(matches!(e, ExecError::InvalidFunctionCall { .. }), "{e:?}");
}

#[test]
fn unknown_function_remains_unsupported() {
    let db = db();
    let e = run_err("RETURN bogus_fn('x') AS v", &db);
    assert!(matches!(e, ExecError::Unsupported { .. }), "{e:?}");
}

// ===== Composition across clauses ===========================================

#[test]
fn scalar_functions_in_where_filter() {
    let db = db();
    // Bug tracker: match bugs whose normalised severity is 'high'.
    exec("CREATE (:Bug {id: 1, severity: 'HIGH'})", &db);
    exec("CREATE (:Bug {id: 2, severity: 'low'})", &db);
    exec("CREATE (:Bug {id: 3, severity: 'High'})", &db);
    let rows = run(
        "MATCH (b:Bug) WHERE toLower(b.severity) = 'high' RETURN b.id AS id ORDER BY id",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Integer(1));
    assert_eq!(rows[1][0], Value::Integer(3));
}

#[test]
fn scalar_function_as_grouping_key_with_aggregation() {
    let db = db();
    // ERP: line items categorised case-insensitively, summed per category.
    exec("CREATE (:Item {category: 'Hardware', amount: 100})", &db);
    exec("CREATE (:Item {category: 'hardware', amount: 50})", &db);
    exec("CREATE (:Item {category: 'Software', amount: 200})", &db);
    let rows = run(
        "MATCH (i:Item) RETURN toUpper(i.category) AS cat, sum(i.amount) AS total ORDER BY cat",
        &db,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], s("HARDWARE"));
    assert_eq!(rows[0][1], Value::Integer(150));
    assert_eq!(rows[1][0], s("SOFTWARE"));
    assert_eq!(rows[1][1], Value::Integer(200));
}

#[test]
fn nested_scalar_functions_in_with_pipeline() {
    let db = db();
    exec(
        "CREATE (:Note {title: 'note', body: 'the quick brown fox jumps'})",
        &db,
    );
    // size(split(body, ' ')) is the word count, carried through WITH.
    let rows = run(
        "MATCH (n:Note) WITH size(split(n.body, ' ')) AS words RETURN words",
        &db,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Integer(5));
}

#[test]
fn range_drives_row_generation_via_unwind() {
    let db = db();
    // range() + UNWIND (00135) generate a derived row set with no graph reads.
    let rows = run(
        "UNWIND range(1, 3) AS x RETURN x * x AS sq ORDER BY sq",
        &db,
    );
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(4)],
            vec![Value::Integer(9)],
        ]
    );
}

// ===== Vector similarity (issue #202) =======================================
// `cosine_similarity(a, b)` — cosine of the angle between two numeric-list
// vectors, in `[-1, 1]`. Unlike the `similar()` threshold predicate, it
// returns the SCORE, so retrieval can `RETURN … AS score ORDER BY score DESC`.
// NULL-propagating like every other scalar; genuine misuse (arity / type /
// dimension mismatch / zero vector) is a recoverable InvalidFunctionCall.

#[test]
fn cosine_similarity_identical_orthogonal_opposite() {
    let db = db();
    // Identical direction → 1.0.
    assert_eq!(
        one(
            "RETURN cosine_similarity([1.0, 0.0, 0.0], [1.0, 0.0, 0.0]) AS s",
            &db
        ),
        Value::Float(1.0)
    );
    // Orthogonal → 0.0.
    assert_eq!(
        one("RETURN cosine_similarity([1.0, 0.0], [0.0, 1.0]) AS s", &db),
        Value::Float(0.0)
    );
    // Opposite → -1.0.
    assert_eq!(
        one(
            "RETURN cosine_similarity([1.0, 0.0], [-1.0, 0.0]) AS s",
            &db
        ),
        Value::Float(-1.0)
    );
}

#[test]
fn cosine_similarity_parallel_and_integer_lists() {
    let db = db();
    // Parallel (scaled) vectors → 1.0; integer elements accepted (as_number).
    // f32 math means "≈1.0", not bit-exact, so compare within a tolerance.
    let Value::Float(v) = one("RETURN cosine_similarity([1, 2, 3], [2, 4, 6]) AS s", &db) else {
        panic!("expected a Float score");
    };
    assert!((v - 1.0).abs() < 1e-6, "expected ≈1.0, got {v}");
}

#[test]
fn cosine_similarity_null_propagates() {
    let db = db();
    assert_eq!(
        one("RETURN cosine_similarity(null, [1.0, 0.0]) AS s", &db),
        Value::Null
    );
    assert_eq!(
        one("RETURN cosine_similarity([1.0, 0.0], null) AS s", &db),
        Value::Null
    );
}

#[test]
fn cosine_similarity_arity_and_type_and_dimension_errors() {
    let db = db();
    // Wrong arity.
    assert!(matches!(
        run_err("RETURN cosine_similarity([1.0, 0.0]) AS s", &db),
        ExecError::InvalidFunctionCall { .. }
    ));
    // Non-list argument.
    assert!(matches!(
        run_err("RETURN cosine_similarity('nope', [1.0]) AS s", &db),
        ExecError::InvalidFunctionCall { .. }
    ));
    // Dimension mismatch.
    assert!(matches!(
        run_err("RETURN cosine_similarity([1.0, 0.0], [1.0]) AS s", &db),
        ExecError::InvalidFunctionCall { .. }
    ));
}

#[test]
fn cosine_similarity_orders_retrieval_by_score() {
    // The issue's primary use case: score chunks and ORDER BY score DESC.
    let db = db();
    exec(
        "CREATE (:Chunk {title: 'a', embedding: [1.0, 0.0]}), \
                (:Chunk {title: 'b', embedding: [0.9, 0.1]}), \
                (:Chunk {title: 'c', embedding: [0.0, 1.0]})",
        &db,
    );
    let rows = run(
        "MATCH (c:Chunk) \
         RETURN c.title AS title, cosine_similarity(c.embedding, [1.0, 0.0]) AS score \
         ORDER BY score DESC",
        &db,
    );
    let titles: Vec<&str> = rows
        .iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.as_str(),
            other => panic!("title not a string: {other:?}"),
        })
        .collect();
    assert_eq!(titles, vec!["a", "b", "c"]);
}
