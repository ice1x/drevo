//! End-to-end Cypher `=~` regex-match tests — Phase 10 follow-up task
//! `00140`.
//!
//! The parser already produces the `=~` binary operator (`00062`); this task
//! makes the executor evaluate it with Neo4j-compatible semantics:
//!
//! * The right-hand side is compiled as a regular expression and the **entire**
//!   left-hand string must match (Java `Matcher::matches`, not `find`).
//! * `NULL =~ x` and `x =~ NULL` yield `NULL` (three-valued logic), so a row
//!   with a `NULL` operand never satisfies a `WHERE` predicate.
//! * Both operands must be strings; any other type is a recoverable
//!   `ExecError::TypeMismatch`.
//! * A malformed pattern (or a pathological one that blows the matcher's
//!   complexity budget) is a recoverable `ExecError::InvalidRegex`.
//!
//! These cases drive the real parser → executor pipeline across the five
//! drevo target scenario domains (CBT journal, story / book editor, IT task
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

fn run_with(source: &str, drevo: &Drevo, params: HashMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse(source).expect("parse");
    execute(&q, drevo, params).expect("execute").rows
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

fn b(v: bool) -> Value {
    Value::Bool(v)
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

// ===== Core semantics — projecting the boolean ==============================

#[test]
fn full_match_is_anchored_at_both_ends() {
    let d = db();
    assert_eq!(one("RETURN 'hello' =~ 'hello'", &d), b(true));
    // `find` would match the substring; `=~` requires the whole string.
    assert_eq!(one("RETURN 'hello world' =~ 'hello'", &d), b(false));
    assert_eq!(one("RETURN 'say hello' =~ 'hello'", &d), b(false));
}

#[test]
fn dot_star_matches_whole_string() {
    let d = db();
    assert_eq!(one("RETURN 'anything at all' =~ '.*'", &d), b(true));
    assert_eq!(one("RETURN 'hello world' =~ 'hello.*'", &d), b(true));
    assert_eq!(one("RETURN 'hello world' =~ '.*world'", &d), b(true));
    assert_eq!(one("RETURN 'hello world' =~ '.*xyz'", &d), b(false));
}

#[test]
fn character_classes_and_quantifiers() {
    let d = db();
    assert_eq!(one("RETURN '2026' =~ '[0-9]+'", &d), b(true));
    assert_eq!(one("RETURN '2026' =~ '\\\\d{4}'", &d), b(true));
    assert_eq!(one("RETURN '20x6' =~ '\\\\d{4}'", &d), b(false));
    assert_eq!(one("RETURN 'abc123' =~ '[a-z]+[0-9]+'", &d), b(true));
}

#[test]
fn alternation_and_groups() {
    let d = db();
    assert_eq!(one("RETURN 'cat' =~ 'cat|dog'", &d), b(true));
    assert_eq!(one("RETURN 'dog' =~ 'cat|dog'", &d), b(true));
    assert_eq!(one("RETURN 'cow' =~ 'cat|dog'", &d), b(false));
    assert_eq!(one("RETURN 'cats' =~ '(cat|dog)s?'", &d), b(true));
}

#[test]
fn case_insensitive_inline_flag() {
    let d = db();
    assert_eq!(one("RETURN 'HELLO' =~ '(?i)hello'", &d), b(true));
    assert_eq!(one("RETURN 'Hello' =~ '(?i)hello'", &d), b(true));
    assert_eq!(one("RETURN 'HELLO' =~ 'hello'", &d), b(false));
}

#[test]
fn negation_via_not() {
    let d = db();
    assert_eq!(one("RETURN NOT ('hello' =~ 'world')", &d), b(true));
    assert_eq!(one("RETURN NOT ('hello' =~ 'hello')", &d), b(false));
}

// ===== Three-valued logic (NULL propagation) ================================

#[test]
fn null_operand_yields_null() {
    let d = db();
    assert_eq!(one("RETURN null =~ 'abc'", &d), Value::Null);
    assert_eq!(one("RETURN 'abc' =~ null", &d), Value::Null);
}

#[test]
fn null_match_does_not_satisfy_where() {
    let d = db();
    // A node whose `code` property is absent (NULL) must be filtered out: a
    // NULL `=~` result is not TRUE.
    exec("CREATE (:Item {name: 'has-code', code: 'A12'})", &d);
    exec("CREATE (:Item {name: 'no-code'})", &d);
    let rows = run(
        "MATCH (i:Item) WHERE i.code =~ '[A-Z][0-9]+' RETURN i.name",
        &d,
    );
    assert_eq!(rows, vec![vec![s("has-code")]]);
}

// ===== Type errors ==========================================================

#[test]
fn non_string_subject_is_type_mismatch() {
    let d = db();
    let e = run_err("RETURN 42 =~ '[0-9]+'", &d);
    assert!(matches!(e, ExecError::TypeMismatch { .. }), "{e:?}");
}

#[test]
fn non_string_pattern_is_type_mismatch() {
    let d = db();
    let e = run_err("RETURN 'abc' =~ 123", &d);
    assert!(matches!(e, ExecError::TypeMismatch { .. }), "{e:?}");
}

// ===== Invalid pattern ======================================================

#[test]
fn malformed_pattern_is_invalid_regex() {
    let d = db();
    let e = run_err("RETURN 'abc' =~ '(unclosed'", &d);
    assert!(matches!(e, ExecError::InvalidRegex { .. }), "{e:?}");
}

#[test]
fn invalid_regex_error_has_span() {
    let d = db();
    let e = run_err("RETURN 'abc' =~ '[z-a]'", &d);
    assert!(matches!(e, ExecError::InvalidRegex { .. }), "{e:?}");
    assert!(e.span().is_some(), "InvalidRegex should carry a span");
}

// ===== Parameterised patterns ===============================================

#[test]
fn pattern_from_parameter() {
    let d = db();
    let mut params = HashMap::new();
    params.insert("pat".to_string(), s("h.*o"));
    let rows = run_with("RETURN 'hello' =~ $pat", &d, params);
    assert_eq!(rows, vec![vec![b(true)]]);
}

// ===== Scenario: CBT journal ================================================

#[test]
fn cbt_filter_thoughts_mentioning_should_statements() {
    let d = db();
    exec(
        "CREATE (:Thought {text: 'I should have done better', distortion: 'should'})",
        &d,
    );
    exec(
        "CREATE (:Thought {text: 'Today went well', distortion: 'none'})",
        &d,
    );
    let rows = run(
        "MATCH (t:Thought) WHERE t.text =~ '(?i).*should.*' RETURN t.distortion",
        &d,
    );
    assert_eq!(rows, vec![vec![s("should")]]);
}

// ===== Scenario: story / book editor ========================================

#[test]
fn story_match_chapter_titles_by_pattern() {
    let d = db();
    exec("CREATE (:Chapter {title: 'Chapter 1'})", &d);
    exec("CREATE (:Chapter {title: 'Chapter 12'})", &d);
    exec("CREATE (:Chapter {title: 'Epilogue'})", &d);
    let rows = run(
        "MATCH (c:Chapter) WHERE c.title =~ 'Chapter \\\\d+' RETURN c.title ORDER BY c.title",
        &d,
    );
    assert_eq!(rows, vec![vec![s("Chapter 1")], vec![s("Chapter 12")]]);
}

// ===== Scenario: IT task manager ============================================

#[test]
fn task_filter_by_ticket_id_format() {
    let d = db();
    exec("CREATE (:Task {ref: 'JIRA-1234', title: 'Fix login'})", &d);
    exec("CREATE (:Task {ref: 'adhoc note', title: 'Tidy desk'})", &d);
    let rows = run(
        "MATCH (t:Task) WHERE t.ref =~ '[A-Z]+-[0-9]+' RETURN t.title",
        &d,
    );
    assert_eq!(rows, vec![vec![s("Fix login")]]);
}

// ===== Scenario: ERP ========================================================

#[test]
fn erp_match_sku_codes() {
    let d = db();
    exec("CREATE (:Product {sku: 'SKU-0001-A', name: 'Widget'})", &d);
    exec("CREATE (:Product {sku: 'SKU-0002-B', name: 'Gadget'})", &d);
    exec("CREATE (:Product {sku: 'misc', name: 'Sample'})", &d);
    let rows = run(
        "MATCH (p:Product) WHERE p.sku =~ 'SKU-\\\\d{4}-[A-Z]' RETURN p.name ORDER BY p.name",
        &d,
    );
    assert_eq!(rows, vec![vec![s("Gadget")], vec![s("Widget")]]);
}

// ===== Scenario: bug tracker ================================================

#[test]
fn bug_tracker_match_emails_in_reporter() {
    let d = db();
    exec(
        "CREATE (:Bug {reporter: 'alice@example.com', summary: 'Crash on save'})",
        &d,
    );
    exec(
        "CREATE (:Bug {reporter: 'anonymous', summary: 'Typo in footer'})",
        &d,
    );
    let rows = run(
        "MATCH (b:Bug) WHERE b.reporter =~ '[\\\\w.]+@[\\\\w.]+' RETURN b.summary",
        &d,
    );
    assert_eq!(rows, vec![vec![s("Crash on save")]]);
}

// ===== Use inside RETURN with WITH chaining =================================

#[test]
fn regex_match_projected_alongside_other_columns() {
    let d = db();
    exec("CREATE (:Doc {name: 'report-2026.pdf'})", &d);
    let rows = run("MATCH (d:Doc) RETURN d.name, d.name =~ '.*\\\\.pdf'", &d);
    assert_eq!(rows, vec![vec![s("report-2026.pdf"), b(true)]]);
}
