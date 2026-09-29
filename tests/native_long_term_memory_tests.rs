//! Native long-term agent memory over the native engine (issue #533, the
//! POLE+O / temporal-validity layer of the context graph).
//!
//! - `CALL drevo.memory.rememberEntity(session, name, type, description)` —
//!   upsert a POLE+O entity (`PERSON` / `OBJECT` / `LOCATION` / `EVENT` /
//!   `ORGANIZATION`) as `:Entity:<Type> { id, name, type, description,
//!   created_at }`, keyed on `(name, type)`, and link it `:MENTIONS` from the
//!   session's newest message.
//! - `CALL drevo.memory.assertFact(subject, relation, object, exclusive)` — a
//!   fact between two entities as `-[:RELATED_TO { type, valid_from,
//!   valid_until }]->`; `exclusive` closes the subject's other currently-valid
//!   facts of that relation (a supersession: "works at Globex" ends "works at
//!   Acme").
//! - `CALL drevo.memory.retractFact(subject, relation, object)` — end a fact.
//! - `CALL drevo.memory.factsAt(name, asOf)` — the entity's facts valid at
//!   `asOf` (an ISO-8601 UTC timestamp, `null` = now).
//!
//! The schema is the one `neo4j-agent-memory` writes (`:Entity` + type label,
//! `MERGE` on `(name, type)`, `RELATED_TO {type, valid_from, valid_until}`,
//! `(:Message)-[:MENTIONS]->(:Entity)`), with the temporal fields actually
//! maintained and queried.

use std::collections::HashMap;

use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

fn run(svc: &NativeService, q: &str) -> Result<ExecResult, ExecError> {
    svc.execute(&parse(q).expect("parse"), HashMap::new())
}

fn run_ok(svc: &NativeService, q: &str) -> ExecResult {
    run(svc, q).unwrap_or_else(|e| panic!("execute `{q}`: {e:?}"))
}

fn strings(res: &ExecResult, col: usize) -> Vec<String> {
    res.rows
        .iter()
        .map(|r| match &r[col] {
            Value::String(s) => s.clone(),
            other => panic!("expected a string, got {other:?}"),
        })
        .collect()
}

fn remember(svc: &NativeService, session: &str, name: &str, ty: &str) -> ExecResult {
    run_ok(
        svc,
        &format!(
            "CALL drevo.memory.rememberEntity('{session}', '{name}', '{ty}', null) YIELD node \
             RETURN node.name AS name"
        ),
    )
}

fn assert_fact(svc: &NativeService, s: &str, rel: &str, o: &str, exclusive: bool) -> ExecResult {
    run_ok(
        svc,
        &format!(
            "CALL drevo.memory.assertFact('{s}', '{rel}', '{o}', {exclusive}) YIELD rel \
             RETURN rel.type AS relation, rel.valid_from AS valid_from, \
             rel.valid_until AS valid_until"
        ),
    )
}

/// Current facts (`asOf = null`) of `name`, as sorted `subject relation object`.
fn facts_now(svc: &NativeService, name: &str) -> Vec<String> {
    facts_at(svc, name, "null")
}

fn facts_at(svc: &NativeService, name: &str, as_of: &str) -> Vec<String> {
    let res = run_ok(
        svc,
        &format!(
            "CALL drevo.memory.factsAt('{name}', {as_of}) \
             YIELD subject, relation, object \
             RETURN subject.name + ' ' + relation + ' ' + object.name AS fact ORDER BY fact"
        ),
    );
    strings(&res, 0)
}

// ----- entities ---------------------------------------------------------------

#[test]
fn remember_entity_creates_a_pole_entity() {
    let svc = NativeService::in_memory();
    let res = run_ok(
        &svc,
        "CALL drevo.memory.rememberEntity('s', 'Alice', 'person', 'staff engineer') YIELD node \
         RETURN node.name AS name, node.type AS type, node.description AS description, \
         labels(node) AS labels, node.id IS NOT NULL AS has_id, \
         node.created_at IS NOT NULL AS has_ts",
    );
    let row = &res.rows[0];
    assert_eq!(row[0], Value::String("Alice".into()));
    // The type is normalised to the upper-case POLE+O name…
    assert_eq!(row[1], Value::String("PERSON".into()));
    assert_eq!(row[2], Value::String("staff engineer".into()));
    // …and also carried as a PascalCase label next to :Entity.
    match &row[3] {
        Value::List(labels) => {
            assert!(
                labels.contains(&Value::String("Entity".into())),
                "{labels:?}"
            );
            assert!(
                labels.contains(&Value::String("Person".into())),
                "{labels:?}"
            );
        }
        other => panic!("expected a label list, got {other:?}"),
    }
    assert_eq!(row[4], Value::Bool(true));
    assert_eq!(row[5], Value::Bool(true));
    assert_eq!(res.stats.nodes_created, 1);
}

#[test]
fn remember_entity_upserts_on_name_and_type() {
    let svc = NativeService::in_memory();
    remember(&svc, "s", "Acme", "ORGANIZATION");
    let again = run_ok(
        &svc,
        "CALL drevo.memory.rememberEntity('s', 'Acme', 'ORGANIZATION', 'a rocket maker') \
         YIELD node RETURN node.description AS d",
    );
    assert_eq!(
        again.stats.nodes_created, 0,
        "same (name, type) is the same entity"
    );
    assert_eq!(
        strings(&again, 0),
        vec!["a rocket maker"],
        "description updated"
    );

    // A null description never erases a known one.
    let keep = run_ok(
        &svc,
        "CALL drevo.memory.rememberEntity('s', 'Acme', 'ORGANIZATION', null) \
         YIELD node RETURN node.description AS d",
    );
    assert_eq!(strings(&keep, 0), vec!["a rocket maker"]);

    // Same name, different type → a different entity.
    let place = remember(&svc, "s", "Acme", "LOCATION");
    assert_eq!(place.stats.nodes_created, 1);
    let count = run_ok(&svc, "MATCH (e:Entity {name: 'Acme'}) RETURN count(e) AS n");
    assert_eq!(count.rows[0][0], Value::Integer(2));
}

#[test]
fn remember_entity_links_a_mention_from_the_latest_message_once() {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CALL drevo.memory.addMessage('s', 'user', 'I had lunch with Alice') YIELD node RETURN node",
    );
    let first = remember(&svc, "s", "Alice", "PERSON");
    assert_eq!(
        first.stats.relationships_created, 1,
        ":MENTIONS from the message"
    );
    let second = remember(&svc, "s", "Alice", "PERSON");
    assert_eq!(
        second.stats.relationships_created, 0,
        "never a duplicate :MENTIONS"
    );

    let res = run_ok(
        &svc,
        "MATCH (m:Message)-[:MENTIONS]->(e:Entity) RETURN m.text AS text, e.name AS name",
    );
    assert_eq!(strings(&res, 0), vec!["I had lunch with Alice"]);
    assert_eq!(strings(&res, 1), vec!["Alice"]);
}

#[test]
fn remember_entity_without_a_message_or_session_links_nothing() {
    let svc = NativeService::in_memory();
    let res = run_ok(
        &svc,
        "CALL drevo.memory.rememberEntity(null, 'Paris', 'LOCATION', null) YIELD node \
         RETURN node.name AS name",
    );
    assert_eq!(strings(&res, 0), vec!["Paris"]);
    assert_eq!(res.stats.relationships_created, 0);
}

#[test]
fn remember_entity_rejects_a_non_pole_type() {
    let svc = NativeService::in_memory();
    let err = run(
        &svc,
        "CALL drevo.memory.rememberEntity('s', 'R2', 'ROBOT', null) YIELD node RETURN node",
    )
    .expect_err("ROBOT is not a POLE+O type");
    let msg = format!("{err}");
    assert!(
        msg.contains("PERSON") && msg.contains("ORGANIZATION"),
        "{msg}"
    );
}

// ----- temporal facts ---------------------------------------------------------

fn seed_people(svc: &NativeService) {
    remember(svc, "s", "Alice", "PERSON");
    remember(svc, "s", "Bob", "PERSON");
    remember(svc, "s", "Acme", "ORGANIZATION");
    remember(svc, "s", "Globex", "ORGANIZATION");
}

#[test]
fn assert_fact_creates_a_currently_valid_related_to_edge() {
    let svc = NativeService::in_memory();
    seed_people(&svc);
    let res = assert_fact(&svc, "Alice", "WORKS_AT", "Acme", false);
    assert_eq!(res.stats.relationships_created, 1);
    assert_eq!(res.rows[0][0], Value::String("WORKS_AT".into()));
    assert!(
        matches!(res.rows[0][1], Value::String(_)),
        "valid_from is set"
    );
    assert_eq!(res.rows[0][2], Value::Null, "valid_until is open");

    let edge = run_ok(
        &svc,
        "MATCH (:Entity {name: 'Alice'})-[r]->(:Entity {name: 'Acme'}) RETURN type(r) AS t",
    );
    assert_eq!(strings(&edge, 0), vec!["RELATED_TO"]);
    assert_eq!(facts_now(&svc, "Alice"), vec!["Alice WORKS_AT Acme"]);
}

#[test]
fn assert_fact_is_idempotent_while_the_fact_holds() {
    let svc = NativeService::in_memory();
    seed_people(&svc);
    assert_fact(&svc, "Alice", "WORKS_AT", "Acme", false);
    let again = assert_fact(&svc, "Alice", "WORKS_AT", "Acme", false);
    assert_eq!(again.stats.relationships_created, 0);
    assert_eq!(again.rows.len(), 1, "yields the existing fact");
    let n = run_ok(
        &svc,
        "MATCH (:Entity {name: 'Alice'})-[r:RELATED_TO]->() RETURN count(r) AS n",
    );
    assert_eq!(n.rows[0][0], Value::Integer(1));
}

#[test]
fn non_exclusive_facts_accumulate() {
    let svc = NativeService::in_memory();
    seed_people(&svc);
    assert_fact(&svc, "Alice", "KNOWS", "Bob", false);
    remember(&svc, "s", "Carol", "PERSON");
    assert_fact(&svc, "Alice", "KNOWS", "Carol", false);
    assert_eq!(
        facts_now(&svc, "Alice"),
        vec!["Alice KNOWS Bob", "Alice KNOWS Carol"]
    );
}

#[test]
fn exclusive_fact_supersedes_and_history_stays_queryable() {
    let svc = NativeService::in_memory();
    seed_people(&svc);
    let acme = assert_fact(&svc, "Alice", "WORKS_AT", "Acme", true);
    let acme_from = strings(&acme, 1).remove(0);
    // Timestamps are millisecond ISO-8601 strings; make the two facts land on
    // distinct instants so "as of the first" is unambiguous.
    std::thread::sleep(std::time::Duration::from_millis(5));
    let globex = assert_fact(&svc, "Alice", "WORKS_AT", "Globex", true);
    let globex_from = strings(&globex, 1).remove(0);
    assert!(globex_from > acme_from);

    // Now: only Globex.
    assert_eq!(facts_now(&svc, "Alice"), vec!["Alice WORKS_AT Globex"]);
    // The old fact was closed at the supersession instant, not deleted.
    let closed = run_ok(
        &svc,
        "MATCH (:Entity {name: 'Alice'})-[r:RELATED_TO]->(:Entity {name: 'Acme'}) \
         RETURN r.valid_until AS until",
    );
    assert_eq!(strings(&closed, 0), vec![globex_from.clone()]);
    // As of Acme's start: only Acme.
    assert_eq!(
        facts_at(&svc, "Alice", &format!("'{acme_from}'")),
        vec!["Alice WORKS_AT Acme"]
    );
    // Before anything was known: nothing.
    assert!(facts_at(&svc, "Alice", "'1970-01-01T00:00:00.000Z'").is_empty());
}

#[test]
fn exclusive_supersession_is_per_relation() {
    let svc = NativeService::in_memory();
    seed_people(&svc);
    assert_fact(&svc, "Alice", "KNOWS", "Bob", false);
    assert_fact(&svc, "Alice", "WORKS_AT", "Acme", true);
    assert_fact(&svc, "Alice", "WORKS_AT", "Globex", true);
    // KNOWS is untouched by a WORKS_AT supersession.
    assert_eq!(
        facts_now(&svc, "Alice"),
        vec!["Alice KNOWS Bob", "Alice WORKS_AT Globex"]
    );
}

#[test]
fn retract_fact_ends_it() {
    let svc = NativeService::in_memory();
    seed_people(&svc);
    assert_fact(&svc, "Alice", "WORKS_AT", "Acme", false);
    let res = run_ok(
        &svc,
        "CALL drevo.memory.retractFact('Alice', 'WORKS_AT', 'Acme') YIELD rel \
         RETURN rel.valid_until IS NOT NULL AS closed",
    );
    assert_eq!(res.rows, vec![vec![Value::Bool(true)]]);
    assert!(facts_now(&svc, "Alice").is_empty());
    // Nothing left to retract.
    let again = run_ok(
        &svc,
        "CALL drevo.memory.retractFact('Alice', 'WORKS_AT', 'Acme') YIELD rel RETURN rel",
    );
    assert!(again.rows.is_empty());
}

#[test]
fn facts_at_covers_both_directions() {
    let svc = NativeService::in_memory();
    seed_people(&svc);
    assert_fact(&svc, "Bob", "MANAGES", "Alice", false);
    assert_fact(&svc, "Alice", "WORKS_AT", "Acme", false);
    assert_eq!(
        facts_now(&svc, "Alice"),
        vec!["Alice WORKS_AT Acme", "Bob MANAGES Alice"]
    );
}

#[test]
fn assert_fact_on_an_unknown_entity_errors() {
    let svc = NativeService::in_memory();
    seed_people(&svc);
    let err = run(
        &svc,
        "CALL drevo.memory.assertFact('Alice', 'WORKS_AT', 'Nowhere Inc', false) \
         YIELD rel RETURN rel",
    )
    .expect_err("unknown object entity");
    assert!(format!("{err}").contains("Nowhere Inc"), "{err}");
}

#[test]
fn a_fact_on_an_ambiguous_name_errors() {
    let svc = NativeService::in_memory();
    remember(&svc, "s", "Jordan", "PERSON");
    remember(&svc, "s", "Jordan", "LOCATION");
    remember(&svc, "s", "Acme", "ORGANIZATION");
    let err = run(
        &svc,
        "CALL drevo.memory.assertFact('Jordan', 'WORKS_AT', 'Acme', false) YIELD rel RETURN rel",
    )
    .expect_err("two entities are named Jordan");
    assert!(
        format!("{err}").contains("2 entities are named `Jordan`"),
        "{err}"
    );
}

// ----- the whole context graph, across a restart ------------------------------

static NEXT_DIR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn tmp_wal() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "drevo_ltm_{}_{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("native.wal")
}

/// A realistic agent flow over all three memory layers, spanning a server
/// restart: short-term turns, long-term entities mentioned by them, a fact
/// that is later superseded, and a reasoning trace — then, after the reopen,
/// the session's context (who/what it mentioned) and the fact history are
/// still answerable, and memory keeps growing.
#[test]
fn context_graph_across_a_restart() {
    let wal = tmp_wal();
    let first_job_from;
    {
        let svc = NativeService::open(&wal).unwrap();
        run_ok(
            &svc,
            "CALL drevo.memory.addMessage('onboarding', 'user', 'Dana just joined Acme') \
             YIELD node RETURN node",
        );
        remember(&svc, "onboarding", "Dana", "PERSON");
        remember(&svc, "onboarding", "Acme", "ORGANIZATION");
        let fact = assert_fact(&svc, "Dana", "WORKS_AT", "Acme", true);
        first_job_from = strings(&fact, 1).remove(0);
        run_ok(
            &svc,
            "CALL drevo.memory.recordReasoning('onboarding', 'stored employment fact', null, 'ok') \
             YIELD node RETURN node",
        );
    }

    std::thread::sleep(std::time::Duration::from_millis(5));
    let svc = NativeService::open(&wal).unwrap();
    run_ok(
        &svc,
        "CALL drevo.memory.addMessage('onboarding', 'user', 'Dana moved to Globex') \
         YIELD node RETURN node",
    );
    remember(&svc, "onboarding", "Globex", "ORGANIZATION");
    remember(&svc, "onboarding", "Dana", "PERSON");
    assert_fact(&svc, "Dana", "WORKS_AT", "Globex", true);

    // Current state and history.
    assert_eq!(facts_now(&svc, "Dana"), vec!["Dana WORKS_AT Globex"]);
    assert_eq!(
        facts_at(&svc, "Dana", &format!("'{first_job_from}'")),
        vec!["Dana WORKS_AT Acme"]
    );

    // Session context: every entity this conversation mentioned.
    let ctx = run_ok(
        &svc,
        "MATCH (m:Message {session: 'onboarding'})-[:MENTIONS]->(e:Entity) \
         RETURN DISTINCT e.name AS name ORDER BY name",
    );
    assert_eq!(strings(&ctx, 0), vec!["Acme", "Dana", "Globex"]);

    // Dana is mentioned by both turns, as one entity.
    let dana = run_ok(
        &svc,
        "MATCH (m:Message)-[:MENTIONS]->(e:Entity {name: 'Dana'}) RETURN count(m) AS n",
    );
    assert_eq!(dana.rows[0][0], Value::Integer(2));
    let _ = std::fs::remove_dir_all(wal.parent().unwrap());
}
