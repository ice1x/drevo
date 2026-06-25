//! End-to-end Cypher non-deterministic value-function tests — Phase 10
//! follow-up task `00161`.
//!
//! Neo4j 5 exposes two zero-argument *non-deterministic* scalar functions that
//! round out the value-generation side of the function library:
//!
//! * `rand()` — a uniformly-distributed random `Float` in the half-open
//!   interval `[0.0, 1.0)`. Backed by a fast, per-thread, non-cryptographic
//!   generator (mirroring Neo4j's `ThreadLocalRandom`); every evaluation is
//!   independent, so `rand()` re-draws per row.
//! * `randomUUID()` — a randomly-generated version-4 UUID rendered as the
//!   canonical 36-character `8-4-4-4-12` lowercase-hex string. Backed by the
//!   OS CSPRNG (mirroring Neo4j's `SecureRandom`); every evaluation is a fresh,
//!   practically-unique identifier.
//!
//! Because the outputs are non-deterministic, these tests assert the *contract*
//! rather than fixed values: range / format invariants, per-row independence
//! (variation across a scan), composability inside larger expressions, the
//! case-insensitive function names, and the wrong-arity error path. They drive
//! the real parser → executor pipeline across the five drevo target scenario
//! domains (CBT journal, story/book editor, IT task manager, ERP, bug tracker).

use std::collections::HashMap;
use std::collections::HashSet;

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

/// One-row, one-column projection helper.
fn one(source: &str, drevo: &Drevo) -> Value {
    let rows = run(source, drevo);
    assert_eq!(rows.len(), 1, "expected exactly one row from {source:?}");
    rows[0][0].clone()
}

/// Extract the `f64` out of a `Value::Float`, failing loudly otherwise.
fn float_of(v: Value) -> f64 {
    match v {
        Value::Float(f) => f,
        other => panic!("expected Float, got {other:?}"),
    }
}

/// Extract the `String` out of a `Value::String`, failing loudly otherwise.
fn string_of(v: Value) -> String {
    match v {
        Value::String(s) => s,
        other => panic!("expected String, got {other:?}"),
    }
}

/// Assert `s` is a canonical lowercase version-4 UUID (`8-4-4-4-12` hex with the
/// version nibble `4` and the variant nibble in `{8,9,a,b}`).
fn assert_uuid_v4(s: &str) {
    assert_eq!(s.len(), 36, "UUID must be 36 chars: {s:?}");
    let bytes = s.as_bytes();
    for (i, &c) in bytes.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => assert_eq!(c, b'-', "expected '-' at {i} in {s:?}"),
            14 => assert_eq!(c, b'4', "version nibble must be '4' in {s:?}"),
            19 => assert!(
                matches!(c, b'8' | b'9' | b'a' | b'b'),
                "variant nibble must be 8/9/a/b in {s:?}"
            ),
            _ => assert!(
                c.is_ascii_hexdigit() && !c.is_ascii_uppercase(),
                "expected lowercase hex at {i} in {s:?}"
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// rand()
// ---------------------------------------------------------------------------

#[test]
fn rand_returns_float_in_unit_interval() {
    let db = db();
    // A single call must land in [0.0, 1.0).
    for _ in 0..256 {
        let r = float_of(one("RETURN rand()", &db));
        assert!((0.0..1.0).contains(&r), "rand() out of [0,1): {r}");
    }
}

#[test]
fn rand_redraws_per_row() {
    let db = db();
    // 500 independent draws across one scan should not collapse to a single
    // value (a constant-folded `rand()` would). A genuine PRNG yields hundreds
    // of distinct doubles; even a weak one clears "more than one".
    let rows = run("UNWIND range(1, 500) AS i RETURN rand() AS r", &db);
    assert_eq!(rows.len(), 500);
    let distinct: HashSet<u64> = rows
        .into_iter()
        .map(|row| float_of(row[0].clone()).to_bits())
        .collect();
    assert!(
        distinct.len() > 400,
        "expected mostly-distinct draws, got {} distinct of 500",
        distinct.len()
    );
}

#[test]
fn rand_composes_inside_arithmetic() {
    let db = db();
    // The classic dice idiom: toInteger(rand() * 6) + 1 ∈ [1, 6].
    let rows = run(
        "UNWIND range(1, 300) AS i RETURN toInteger(rand() * 6) + 1 AS die",
        &db,
    );
    for row in rows {
        match row[0] {
            Value::Integer(d) => assert!((1..=6).contains(&d), "die out of range: {d}"),
            ref other => panic!("expected Integer die, got {other:?}"),
        }
    }
}

#[test]
fn rand_name_is_case_insensitive() {
    let db = db();
    let r = float_of(one("RETURN RAND()", &db));
    assert!((0.0..1.0).contains(&r), "RAND() out of [0,1): {r}");
}

#[test]
fn rand_rejects_arguments() {
    let db = db();
    match run_err("RETURN rand(1)", &db) {
        ExecError::InvalidFunctionCall { name, .. } => {
            assert_eq!(name.to_lowercase(), "rand")
        }
        other => panic!("expected InvalidFunctionCall, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// randomUUID()
// ---------------------------------------------------------------------------

#[test]
fn randomuuid_has_canonical_v4_format() {
    let db = db();
    for _ in 0..256 {
        let s = string_of(one("RETURN randomUUID()", &db));
        assert_uuid_v4(&s);
    }
}

#[test]
fn randomuuid_is_practically_unique() {
    let db = db();
    let rows = run("UNWIND range(1, 1000) AS i RETURN randomUUID() AS u", &db);
    assert_eq!(rows.len(), 1000);
    let distinct: HashSet<String> = rows
        .into_iter()
        .map(|row| string_of(row[0].clone()))
        .collect();
    assert_eq!(
        distinct.len(),
        1000,
        "randomUUID() collided across 1000 draws"
    );
}

#[test]
fn randomuuid_name_is_case_insensitive() {
    let db = db();
    let s = string_of(one("RETURN RANDOMUUID()", &db));
    assert_uuid_v4(&s);
}

#[test]
fn randomuuid_rejects_arguments() {
    let db = db();
    match run_err("RETURN randomUUID('x')", &db) {
        ExecError::InvalidFunctionCall { name, .. } => {
            assert_eq!(name.to_lowercase(), "randomuuid")
        }
        other => panic!("expected InvalidFunctionCall, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Scenario coverage across the five target domains
// ---------------------------------------------------------------------------

#[test]
fn scenario_bug_tracker_stamps_unique_ticket_ids() {
    let db = db();
    // Bug tracker: mint a stable external id for each newly-filed ticket.
    let rows = run(
        "UNWIND ['login crash', 'slow query', 'typo'] AS title \
         CREATE (b:Bug {title: title, ref: randomUUID()}) RETURN b.ref AS ref",
        &db,
    );
    assert_eq!(rows.len(), 3);
    let mut seen = HashSet::new();
    for row in rows {
        let s = string_of(row[0].clone());
        assert_uuid_v4(&s);
        assert!(seen.insert(s), "ticket refs must be unique");
    }
    // The ids actually persisted on the nodes.
    let stored = run("MATCH (b:Bug) RETURN b.ref AS ref", &db);
    assert_eq!(stored.len(), 3);
    for row in stored {
        assert_uuid_v4(&string_of(row[0].clone()));
    }
}

#[test]
fn scenario_cbt_journal_random_prompt_selection() {
    let db = db();
    // CBT journal: pick one of N reflection prompts at random per session.
    let rows = run(
        "UNWIND range(1, 200) AS session \
         WITH ['What went well?', 'What felt hard?', 'One small win?'] AS prompts \
         RETURN prompts[toInteger(rand() * size(prompts))] AS prompt",
        &db,
    );
    assert_eq!(rows.len(), 200);
    let allowed: HashSet<&str> = ["What went well?", "What felt hard?", "One small win?"]
        .into_iter()
        .collect();
    for row in rows {
        let p = string_of(row[0].clone());
        assert!(allowed.contains(p.as_str()), "unexpected prompt: {p:?}");
    }
}

#[test]
fn scenario_task_manager_jittered_priority() {
    let db = db();
    // IT task manager: add a small random tie-breaker to a base priority so a
    // stable sort doesn't always favour the first-created task.
    let rows = run(
        "UNWIND range(1, 100) AS i \
         RETURN 10 + rand() AS score",
        &db,
    );
    for row in rows {
        let score = float_of(row[0].clone());
        assert!(
            (10.0..11.0).contains(&score),
            "jittered score out of band: {score}"
        );
    }
}

#[test]
fn scenario_erp_and_story_each_row_independent() {
    let db = db();
    // ERP: assign each line item to one of three warehouses by random split;
    // story editor: shuffle-pick a chapter colour. Both rely on per-row redraw.
    let rows = run(
        "UNWIND range(1, 300) AS i \
         RETURN toInteger(rand() * 3) AS warehouse, randomUUID() AS lot",
        &db,
    );
    assert_eq!(rows.len(), 300);
    let mut warehouses = HashSet::new();
    let mut lots = HashSet::new();
    for row in rows {
        match row[0] {
            Value::Integer(w) => {
                assert!((0..3).contains(&w), "warehouse out of range: {w}");
                warehouses.insert(w);
            }
            ref other => panic!("expected Integer warehouse, got {other:?}"),
        }
        assert!(
            lots.insert(string_of(row[1].clone())),
            "lot ids must be unique"
        );
    }
    // Over 300 rows all three warehouse buckets are hit with overwhelming odds.
    assert_eq!(
        warehouses.len(),
        3,
        "expected all three warehouse buckets used"
    );
}
