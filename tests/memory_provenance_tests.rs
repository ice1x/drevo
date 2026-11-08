//! Provenance for long-term memory: where an entity or a fact was read from.
//!
//! - `CALL drevo.memory.addSource(id, uri, title, kind) YIELD node` upserts a
//!   `:Source {id, uri, title, kind, created_at}` keyed on `id`.
//! - `CALL drevo.memory.addChunks(sourceId, chunks) YIELD node` upserts the
//!   source's `:Chunk {id, source_id, order, offset, length, text, body,
//!   created_at}` passages, each `-[:FROM]->` the source and chained
//!   `-[:NEXT]->` in `order`.
//! - `rememberEntity` / `assertFact` take an optional fifth `provenance` map
//!   `{source, chunk, quote, offset, length, surface}`: a mention becomes
//!   `(:Entity)-[:MENTIONED_IN {offset, length, surface}]->(:Chunk)`, a fact
//!   carries `source_id, chunk_id, quote, offset, length`.
//! - `CALL drevo.memory.evidence(name, k)` lists the chunks behind an entity.
//! - `drevo.memory.factsAt` gains trailing `source, quote` columns.
//!
//! The scenario is a book editor: two manuscripts whose passages mention
//! characters and back facts about them.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use drevo::bolt::packstream::Value as Bolt;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

fn run(svc: &NativeService, q: &str) -> Result<ExecResult, ExecError> {
    svc.execute(&parse(q).expect("parse"), HashMap::new())
}

fn run_ok(svc: &NativeService, q: &str) -> ExecResult {
    run(svc, q).unwrap_or_else(|e| panic!("execute `{q}`: {e:?}"))
}

/// The `InvalidProcedureCall` message `q` fails with.
fn invalid_call(svc: &NativeService, q: &str) -> String {
    match run(svc, q) {
        Err(ExecError::InvalidProcedureCall { message, .. }) => message,
        other => panic!("`{q}`: expected InvalidProcedureCall, got {other:?}"),
    }
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

fn count(svc: &NativeService, q: &str) -> i64 {
    match run_ok(svc, q).rows[0][0] {
        Value::Integer(n) => n,
        ref other => panic!("expected a count, got {other:?}"),
    }
}

const CH0: &str = "Dana opened the manuscript; Dana read till dawn.";
const CH1: &str = "By noon Dana had joined Acme as an editor.";
const CH2: &str = "Acme printed the first edition in May.";

/// `book-1` with three chunks (orders 0, 1, 2) and the entities Dana / Acme.
fn manuscript(svc: &NativeService) {
    run_ok(
        svc,
        "CALL drevo.memory.addSource('book-1', 'file:///books/one.md', 'Book One', 'document') \
         YIELD node RETURN node",
    );
    run_ok(
        svc,
        &format!(
            "CALL drevo.memory.addChunks('book-1', [\
               {{order: 0, offset: 0, length: 48, text: '{CH0}'}}, \
               {{order: 1, offset: 49, length: 42, text: '{CH1}'}}, \
               {{order: 2, offset: 92, length: 38, text: '{CH2}'}}]) \
             YIELD node RETURN node"
        ),
    );
    run_ok(
        svc,
        "CALL drevo.memory.rememberEntity(null, 'Dana', 'PERSON', null) YIELD node RETURN node",
    );
    run_ok(
        svc,
        "CALL drevo.memory.rememberEntity(null, 'Acme', 'ORGANIZATION', null) YIELD node \
         RETURN node",
    );
}

// ----- addSource ----------------------------------------------------------------

#[test]
fn add_source_creates_a_source() {
    let svc = NativeService::in_memory();
    let res = run_ok(
        &svc,
        "CALL drevo.memory.addSource('book-1', 'file:///books/one.md', 'Book One', 'document') \
         YIELD node RETURN node.id, node.uri, node.title, node.kind, labels(node), \
         node.created_at IS NOT NULL",
    );
    assert_eq!(
        res.rows[0],
        vec![
            Value::String("book-1".into()),
            Value::String("file:///books/one.md".into()),
            Value::String("Book One".into()),
            Value::String("document".into()),
            Value::List(vec![Value::String("Source".into())]),
            Value::Bool(true),
        ]
    );
    assert_eq!(res.stats.nodes_created, 1);
}

#[test]
fn add_source_is_an_upsert_on_id() {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CALL drevo.memory.addSource('book-1', 'file:///one.md', 'Draft', 'document') \
         YIELD node RETURN node",
    );
    let res = run_ok(
        &svc,
        "CALL drevo.memory.addSource('book-1', null, 'Book One', null) \
         YIELD node RETURN node.uri, node.title, node.kind",
    );
    assert_eq!(res.stats.nodes_created, 0);
    // A null field keeps the stored value; a given one replaces it.
    assert_eq!(
        res.rows[0],
        vec![
            Value::String("file:///one.md".into()),
            Value::String("Book One".into()),
            Value::String("document".into()),
        ]
    );
    assert_eq!(count(&svc, "MATCH (s:Source) RETURN count(s)"), 1);
}

#[test]
fn add_source_needs_a_string_id() {
    let svc = NativeService::in_memory();
    let err = invalid_call(
        &svc,
        "CALL drevo.memory.addSource(null, null, null, null) YIELD node RETURN node",
    );
    assert!(err.contains("the source id must be a string"), "{err}");
}

// ----- addChunks ----------------------------------------------------------------

#[test]
fn add_chunks_links_from_and_next_in_order() {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CALL drevo.memory.addSource('book-1', null, null, null) YIELD node RETURN node",
    );
    // Listed out of order, with gaps.
    let res = run_ok(
        &svc,
        "CALL drevo.memory.addChunks('book-1', [\
           {order: 5, text: 'five'}, {order: 0, text: 'zero'}, {id: 'c2', order: 2, text: 'two'}]) \
         YIELD node RETURN node.id, node.order, node.source_id, node.body",
    );
    // Yielded in reading order; an id defaults to `<source>#<order>`.
    assert_eq!(strings(&res, 0), vec!["book-1#0", "c2", "book-1#5"]);
    assert_eq!(strings(&res, 3), vec!["zero", "two", "five"]);
    assert!(res
        .rows
        .iter()
        .all(|r| r[2] == Value::String("book-1".into())));
    let chain = run_ok(
        &svc,
        "MATCH (a:Chunk)-[:NEXT]->(b:Chunk) RETURN a.text + '>' + b.text AS step ORDER BY step",
    );
    assert_eq!(strings(&chain, 0), vec!["two>five", "zero>two"]);
    assert_eq!(
        count(
            &svc,
            "MATCH (:Chunk)-[:FROM]->(:Source {id: 'book-1'}) RETURN count(*)"
        ),
        3
    );
}

#[test]
fn add_chunks_is_idempotent_on_id() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    let again = run_ok(
        &svc,
        &format!(
            "CALL drevo.memory.addChunks('book-1', [\
               {{order: 0, offset: 0, length: 48, text: '{CH0}'}}, \
               {{order: 1, offset: 49, length: 42, text: '{CH1}'}}, \
               {{order: 2, offset: 92, length: 38, text: '{CH2}'}}]) \
             YIELD node RETURN node"
        ),
    );
    assert_eq!(again.rows.len(), 3);
    assert_eq!(again.stats.nodes_created, 0);
    assert_eq!(again.stats.relationships_created, 0);
    assert_eq!(count(&svc, "MATCH (c:Chunk) RETURN count(c)"), 3);
    assert_eq!(count(&svc, "MATCH ()-[r:NEXT]->() RETURN count(r)"), 2);
    assert_eq!(count(&svc, "MATCH ()-[r:FROM]->() RETURN count(r)"), 3);
}

#[test]
fn add_chunks_updates_a_changed_text() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    let res = run_ok(
        &svc,
        "CALL drevo.memory.addChunks('book-1', [{order: 0, text: 'Dana woke early.'}]) \
         YIELD node RETURN node.text, node.body",
    );
    assert_eq!(
        res.rows[0],
        vec![
            Value::String("Dana woke early.".into()),
            Value::String("Dana woke early.".into()),
        ]
    );
    assert_eq!(count(&svc, "MATCH (c:Chunk) RETURN count(c)"), 3);
}

#[test]
fn a_chunk_filling_a_gap_relinks_next() {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CALL drevo.memory.addSource('b', null, null, null) YIELD node RETURN node",
    );
    run_ok(
        &svc,
        "CALL drevo.memory.addChunks('b', [{order: 0, text: 'a'}, {order: 2, text: 'c'}]) \
         YIELD node RETURN node",
    );
    run_ok(
        &svc,
        "CALL drevo.memory.addChunks('b', [{order: 1, text: 'b'}]) YIELD node RETURN node",
    );
    let chain = run_ok(
        &svc,
        "MATCH (a:Chunk)-[:NEXT]->(b:Chunk) RETURN a.text + '>' + b.text AS step ORDER BY step",
    );
    assert_eq!(strings(&chain, 0), vec!["a>b", "b>c"]);
}

#[test]
fn next_never_crosses_sources() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    run_ok(
        &svc,
        "CALL drevo.memory.addSource('book-2', null, 'Book Two', 'document') YIELD node RETURN node",
    );
    run_ok(
        &svc,
        "CALL drevo.memory.addChunks('book-2', [{order: 0, text: 'Another story.'}]) \
         YIELD node RETURN node",
    );
    assert_eq!(count(&svc, "MATCH ()-[r:NEXT]->() RETURN count(r)"), 2);
}

#[test]
fn add_chunks_on_an_unknown_source_errors() {
    let svc = NativeService::in_memory();
    let err = invalid_call(
        &svc,
        "CALL drevo.memory.addChunks('nope', [{order: 0, text: 'x'}]) YIELD node RETURN node",
    );
    assert!(err.contains("no source `nope`"), "{err}");
}

#[test]
fn a_chunk_id_of_another_source_is_refused() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    run_ok(
        &svc,
        "CALL drevo.memory.addSource('book-2', null, null, null) YIELD node RETURN node",
    );
    let err = invalid_call(
        &svc,
        "CALL drevo.memory.addChunks('book-2', [{id: 'book-1#0', order: 0, text: 'x'}]) \
         YIELD node RETURN node",
    );
    assert!(
        err.contains("chunk `book-1#0` belongs to source `book-1`, not `book-2`"),
        "{err}"
    );
}

#[test]
fn a_bad_chunk_list_is_refused() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    let err = invalid_call(
        &svc,
        "CALL drevo.memory.addChunks('book-1', [{text: 'x'}]) YIELD node RETURN node",
    );
    assert!(err.contains("chunk 0 needs an order"), "{err}");
}

// ----- assertFact with provenance ------------------------------------------------

#[test]
fn assert_fact_with_maps_upserts_source_and_chunk_and_cites_the_quote() {
    let svc = NativeService::in_memory();
    for (name, ty) in [("Dana", "PERSON"), ("Acme", "ORGANIZATION")] {
        run_ok(
            &svc,
            &format!(
                "CALL drevo.memory.rememberEntity(null, '{name}', '{ty}', null) YIELD node \
                 RETURN node"
            ),
        );
    }
    let q = format!(
        "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', true, {{\
           source: {{id: 'book-1', uri: 'file:///one.md', title: 'Book One', kind: 'document'}}, \
           chunk: {{id: 'book-1#1', order: 1, offset: 49, length: 42, text: '{CH1}'}}, \
           quote: 'Dana had joined Acme'}}) YIELD rel \
         RETURN rel.source_id, rel.chunk_id, rel.quote, rel.offset, rel.length, rel.type"
    );
    let res = run_ok(&svc, &q);
    assert_eq!(
        res.rows[0],
        vec![
            Value::String("book-1".into()),
            Value::String("book-1#1".into()),
            Value::String("Dana had joined Acme".into()),
            // Character offset of the quote in the chunk, and its length.
            Value::Integer(8),
            Value::Integer(20),
            Value::String("WORKS_AT".into()),
        ]
    );
    // substring(chunk.text, offset, length) is the quote.
    let check = run_ok(
        &svc,
        "MATCH (:Entity {name: 'Dana'})-[f:RELATED_TO]->(), (c:Chunk {id: f.chunk_id}) \
         RETURN substring(c.text, f.offset, f.length) = f.quote",
    );
    assert_eq!(check.rows[0][0], Value::Bool(true));
    let chunk = run_ok(
        &svc,
        "MATCH (c:Chunk)-[:FROM]->(s:Source) \
         RETURN c.id, c.source_id, c.order, c.offset, c.length, c.body = c.text, s.title",
    );
    assert_eq!(
        chunk.rows[0],
        vec![
            Value::String("book-1#1".into()),
            Value::String("book-1".into()),
            Value::Integer(1),
            Value::Integer(49),
            Value::Integer(42),
            Value::Bool(true),
            Value::String("Book One".into()),
        ]
    );

    // Again with the same ids: nothing is duplicated.
    run_ok(&svc, &q);
    assert_eq!(count(&svc, "MATCH (s:Source) RETURN count(s)"), 1);
    assert_eq!(count(&svc, "MATCH (c:Chunk) RETURN count(c)"), 1);
    assert_eq!(count(&svc, "MATCH ()-[r:FROM]->() RETURN count(r)"), 1);
    assert_eq!(
        count(&svc, "MATCH ()-[r:RELATED_TO]->() RETURN count(r)"),
        1
    );
}

#[test]
fn assert_fact_by_chunk_id_takes_the_source_from_the_chunk() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    let res = run_ok(
        &svc,
        "CALL drevo.memory.assertFact('Acme', 'PRINTED', 'Acme', false, \
           {chunk: 'book-1#2', quote: 'first edition'}) YIELD rel \
         RETURN rel.source_id, rel.chunk_id, rel.offset, rel.length",
    );
    assert_eq!(
        res.rows[0],
        vec![
            Value::String("book-1".into()),
            Value::String("book-1#2".into()),
            Value::Integer(17),
            Value::Integer(13),
        ]
    );
}

#[test]
fn explicit_offset_and_length_are_kept() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    let res = run_ok(
        &svc,
        "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false, \
           {source: 'book-1', chunk: 'book-1#1', offset: 8, length: 20}) YIELD rel \
         RETURN rel.source_id, rel.quote, rel.offset, rel.length",
    );
    assert_eq!(
        res.rows[0],
        vec![
            Value::String("book-1".into()),
            Value::Null,
            Value::Integer(8),
            Value::Integer(20),
        ]
    );
}

#[test]
fn a_quote_not_in_the_chunk_is_refused_naming_both() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    let err = invalid_call(
        &svc,
        "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false, \
           {chunk: 'book-1#1', quote: 'Dana left Acme for Globex'}) YIELD rel RETURN rel",
    );
    assert!(
        err.contains("quote 'Dana left Acme for Globex' is not in the text of chunk `book-1#1`"),
        "{err}"
    );
    assert!(
        err.contains("'By noon Dana had joined Acme as an edito…'"),
        "{err}"
    );
    assert!(err.contains("book-1#1"), "{err}");
    // Nothing was written.
    assert_eq!(
        count(&svc, "MATCH ()-[r:RELATED_TO]->() RETURN count(r)"),
        0
    );
}

#[test]
fn an_explicit_offset_must_point_at_the_quote() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    // "joined Acme" starts at character 17 of chunk 1, not 8.
    let err = invalid_call(
        &svc,
        "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false, \
           {chunk: 'book-1#1', quote: 'joined Acme', offset: 8}) YIELD rel RETURN rel",
    );
    assert!(
        err.contains(
            "quote 'joined Acme' is not at offset 8 of chunk `book-1#1` \
             ('By noon Dana had joined Acme as an edito…')"
        ),
        "{err}"
    );
    // A length that does not match the quote is refused the same way.
    let err = invalid_call(
        &svc,
        "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false, \
           {chunk: 'book-1#1', quote: 'joined Acme', offset: 17, length: 6}) YIELD rel RETURN rel",
    );
    assert!(
        err.contains("is not at offset 17 of chunk `book-1#1`"),
        "{err}"
    );
    assert_eq!(
        count(&svc, "MATCH ()-[r:RELATED_TO]->() RETURN count(r)"),
        0
    );

    let res = run_ok(
        &svc,
        "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false, \
           {chunk: 'book-1#1', quote: 'joined Acme', offset: 17}) YIELD rel \
         RETURN rel.offset, rel.length",
    );
    assert_eq!(res.rows[0], vec![Value::Integer(17), Value::Integer(11)]);
}

#[test]
fn an_explicit_mention_offset_must_point_at_the_surface() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    let err = invalid_call(
        &svc,
        "CALL drevo.memory.rememberEntity(null, 'Dana', 'PERSON', null, \
           {chunk: 'book-1#0', surface: 'Dana', offset: 5}) YIELD node RETURN node",
    );
    assert!(
        err.contains(
            "surface 'Dana' is not at offset 5 of chunk `book-1#0` \
             ('Dana opened the manuscript; Dana read ti…')"
        ),
        "{err}"
    );
    assert_eq!(
        count(&svc, "MATCH ()-[m:MENTIONED_IN]->() RETURN count(m)"),
        0
    );
}

#[test]
fn an_unknown_source_id_is_refused() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    let err = invalid_call(
        &svc,
        "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false, {source: 'book-9'}) \
         YIELD rel RETURN rel",
    );
    assert!(err.contains("no source `book-9`"), "{err}");
}

#[test]
fn an_unknown_chunk_id_is_refused() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    let err = invalid_call(
        &svc,
        "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false, {chunk: 'book-1#9'}) \
         YIELD rel RETURN rel",
    );
    assert!(err.contains("no chunk `book-1#9`"), "{err}");
}

#[test]
fn a_chunk_map_needs_a_source() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    let err = invalid_call(
        &svc,
        "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false, \
           {chunk: {id: 'loose', text: 'Dana works at Acme'}}) YIELD rel RETURN rel",
    );
    assert!(
        err.contains("chunk `loose` is given as a map, so provenance needs its source"),
        "{err}"
    );
}

#[test]
fn a_chunk_of_another_source_is_refused() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    run_ok(
        &svc,
        "CALL drevo.memory.addSource('book-2', null, null, null) YIELD node RETURN node",
    );
    let err = invalid_call(
        &svc,
        "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false, \
           {source: 'book-2', chunk: 'book-1#1'}) YIELD rel RETURN rel",
    );
    assert!(
        err.contains("chunk `book-1#1` belongs to source `book-1`, not `book-2`"),
        "{err}"
    );
}

#[test]
fn an_unknown_provenance_key_is_refused() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    let err = invalid_call(
        &svc,
        "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false, {page: 3}) \
         YIELD rel RETURN rel",
    );
    assert!(err.contains("unknown provenance key `page`"), "{err}");
}

#[test]
fn a_null_provenance_is_the_four_argument_form() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    let res = run_ok(
        &svc,
        "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false, null) YIELD rel \
         RETURN keys(rel) AS keys",
    );
    let Value::List(keys) = &res.rows[0][0] else {
        panic!("{:?}", res.rows[0]);
    };
    for key in ["source_id", "chunk_id", "quote", "offset", "length"] {
        assert!(!keys.contains(&Value::String(key.into())), "{keys:?}");
    }
}

#[test]
fn a_held_fact_without_a_citation_gains_one() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    run_ok(
        &svc,
        "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false) YIELD rel RETURN rel",
    );
    let res = run_ok(
        &svc,
        "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false, \
           {chunk: 'book-1#1', quote: 'joined Acme'}) YIELD rel RETURN rel.quote, rel.chunk_id",
    );
    assert_eq!(
        res.rows[0],
        vec![
            Value::String("joined Acme".into()),
            Value::String("book-1#1".into())
        ]
    );
    assert_eq!(
        count(&svc, "MATCH ()-[r:RELATED_TO]->() RETURN count(r)"),
        1
    );
}

#[test]
fn six_arguments_are_refused() {
    let svc = NativeService::in_memory();
    let err = invalid_call(
        &svc,
        "CALL drevo.memory.assertFact('a', 'R', 'b', false, null, null) YIELD rel RETURN rel",
    );
    assert!(err.contains("expected 4 or 5 arguments, got 6"), "{err}");
    let err = invalid_call(
        &svc,
        "CALL drevo.memory.rememberEntity(null, 'a', 'PERSON', null, null, null) YIELD node \
         RETURN node",
    );
    assert!(err.contains("expected 4 or 5 arguments, got 6"), "{err}");
}

// ----- rememberEntity with provenance --------------------------------------------

#[test]
fn remember_entity_links_mentioned_in_once_per_offset() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    let q = "CALL drevo.memory.rememberEntity(null, 'Dana', 'PERSON', null, \
               {chunk: 'book-1#0', surface: 'Dana'}) YIELD node RETURN node.name";
    let first = run_ok(&svc, q);
    assert_eq!(first.stats.relationships_created, 1);
    let again = run_ok(&svc, q);
    assert_eq!(again.stats.relationships_created, 0);
    let mention = run_ok(
        &svc,
        "MATCH (:Entity {name: 'Dana'})-[m:MENTIONED_IN]->(c:Chunk) \
         RETURN c.id, m.offset, m.length, m.surface",
    );
    assert_eq!(
        mention.rows,
        vec![vec![
            Value::String("book-1#0".into()),
            // The first occurrence of the surface, in characters.
            Value::Integer(0),
            Value::Integer(4),
            Value::String("Dana".into()),
        ]]
    );
    // Another place in the same chunk is another mention.
    run_ok(
        &svc,
        "CALL drevo.memory.rememberEntity(null, 'Dana', 'PERSON', null, \
           {chunk: 'book-1#0', surface: 'Dana', offset: 28}) YIELD node RETURN node",
    );
    assert_eq!(
        count(
            &svc,
            "MATCH (:Entity {name: 'Dana'})-[m:MENTIONED_IN]->() RETURN count(m)"
        ),
        2
    );
}

#[test]
fn remember_entity_with_maps_creates_the_entity_and_its_chunk() {
    let svc = NativeService::in_memory();
    let res = run_ok(
        &svc,
        &format!(
            "CALL drevo.memory.rememberEntity(null, 'Dana', 'PERSON', 'protagonist', {{\
               source: {{id: 'book-1', title: 'Book One'}}, \
               chunk: {{id: 'book-1#0', order: 0, text: '{CH0}'}}}}) YIELD node \
             RETURN node.name"
        ),
    );
    // The entity, the source and the chunk.
    assert_eq!(res.stats.nodes_created, 3);
    let mention = run_ok(
        &svc,
        "MATCH (:Entity {name: 'Dana'})-[m:MENTIONED_IN]->(:Chunk)-[:FROM]->(s:Source) \
         RETURN s.title, m.surface, m.offset",
    );
    // The surface defaults to the entity's name, found in the text.
    assert_eq!(
        mention.rows,
        vec![vec![
            Value::String("Book One".into()),
            Value::String("Dana".into()),
            Value::Integer(0),
        ]]
    );
}

#[test]
fn a_surface_not_in_the_chunk_is_refused() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    let err = invalid_call(
        &svc,
        "CALL drevo.memory.rememberEntity(null, 'Dana', 'PERSON', null, \
           {chunk: 'book-1#2', surface: 'Dana'}) YIELD node RETURN node",
    );
    assert!(
        err.contains("surface 'Dana' is not in the text of chunk `book-1#2`"),
        "{err}"
    );
}

#[test]
fn a_mention_needs_a_chunk() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    let err = invalid_call(
        &svc,
        "CALL drevo.memory.rememberEntity(null, 'Dana', 'PERSON', null, {source: 'book-1'}) \
         YIELD node RETURN node",
    );
    assert!(
        err.contains("a mention needs a chunk: the entity is MENTIONED_IN a chunk"),
        "{err}"
    );
}

// ----- evidence ----------------------------------------------------------------

#[test]
fn evidence_returns_mentions_and_cited_facts_newest_first_once_each() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    // Chunk 0 mentions Dana twice; chunk 1 mentions her and backs a fact;
    // chunk 2 backs a fact about Acme only.
    for offset in [0, 28] {
        run_ok(
            &svc,
            &format!(
                "CALL drevo.memory.rememberEntity(null, 'Dana', 'PERSON', null, \
                   {{chunk: 'book-1#0', surface: 'Dana', offset: {offset}}}) YIELD node \
                 RETURN node"
            ),
        );
    }
    run_ok(
        &svc,
        "CALL drevo.memory.rememberEntity(null, 'Dana', 'PERSON', null, \
           {chunk: 'book-1#1', surface: 'Dana'}) YIELD node RETURN node",
    );
    run_ok(
        &svc,
        "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false, \
           {chunk: 'book-1#1', quote: 'joined Acme'}) YIELD rel RETURN rel",
    );
    run_ok(
        &svc,
        "CALL drevo.memory.assertFact('Acme', 'PRINTED', 'Acme', false, \
           {chunk: 'book-1#2', quote: 'first edition'}) YIELD rel RETURN rel",
    );

    let res = run_ok(
        &svc,
        "CALL drevo.memory.evidence('Dana', 10) YIELD chunk, quote, offset, source, fact \
         RETURN chunk.id, quote, offset, source.id, fact.type",
    );
    assert_eq!(
        res.rows,
        vec![
            vec![
                Value::String("book-1#1".into()),
                Value::String("joined Acme".into()),
                Value::Integer(17),
                Value::String("book-1".into()),
                Value::String("WORKS_AT".into()),
            ],
            vec![
                Value::String("book-1#0".into()),
                Value::String("Dana".into()),
                Value::Integer(0),
                Value::String("book-1".into()),
                Value::Null,
            ],
        ]
    );

    let acme = run_ok(
        &svc,
        "CALL drevo.memory.evidence('Acme', 10) YIELD chunk, fact RETURN chunk.id, fact.type",
    );
    assert_eq!(
        strings(&acme, 0),
        vec!["book-1#2".to_string(), "book-1#1".to_string()]
    );

    let top = run_ok(
        &svc,
        "CALL drevo.memory.evidence('Dana', 1) YIELD chunk RETURN chunk.id",
    );
    assert_eq!(strings(&top, 0), vec!["book-1#1"]);
}

#[test]
fn evidence_of_an_uncited_entity_is_empty() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    let res = run_ok(
        &svc,
        "CALL drevo.memory.evidence('Acme', 5) YIELD chunk RETURN chunk",
    );
    assert!(res.rows.is_empty(), "{:?}", res.rows);
}

#[test]
fn evidence_of_an_unknown_entity_errors() {
    let svc = NativeService::in_memory();
    let err = invalid_call(
        &svc,
        "CALL drevo.memory.evidence('Nobody', 5) YIELD chunk RETURN chunk",
    );
    assert!(err.contains("no memory entity named `Nobody`"), "{err}");
}

// ----- factsAt -----------------------------------------------------------------

#[test]
fn facts_at_yields_source_and_quote_last() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    run_ok(
        &svc,
        "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false, \
           {chunk: 'book-1#1', quote: 'joined Acme'}) YIELD rel RETURN rel",
    );
    run_ok(
        &svc,
        "CALL drevo.memory.assertFact('Dana', 'KNOWS', 'Acme', false) YIELD rel RETURN rel",
    );
    let bare = run_ok(&svc, "CALL drevo.memory.factsAt('Dana', null)");
    assert_eq!(
        bare.columns,
        vec![
            "subject",
            "relation",
            "object",
            "valid_from",
            "valid_until",
            "source",
            "quote"
        ]
    );
    let res = run_ok(
        &svc,
        "CALL drevo.memory.factsAt('Dana', null) YIELD relation, source, quote \
         RETURN relation, source.title, quote ORDER BY relation",
    );
    assert_eq!(
        res.rows,
        vec![
            vec![Value::String("KNOWS".into()), Value::Null, Value::Null],
            vec![
                Value::String("WORKS_AT".into()),
                Value::String("Book One".into()),
                Value::String("joined Acme".into()),
            ],
        ]
    );
}

// ----- FTS -----------------------------------------------------------------------

#[test]
fn full_text_search_reaches_chunk_text() {
    let svc = NativeService::in_memory();
    manuscript(&svc);
    let res = run_ok(
        &svc,
        "CALL fts.search('edition', 5) YIELD node RETURN node.id",
    );
    assert_eq!(strings(&res, 0), vec!["book-1#2"]);
}

// ----- Bolt ----------------------------------------------------------------------

#[test]
fn bolt_facts_at_carries_a_source_node_and_a_null_for_uncited_facts() {
    let svc = Arc::new(NativeService::in_memory());
    manuscript(&svc);
    run_ok(
        &svc,
        "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', false, \
           {chunk: 'book-1#1', quote: 'joined Acme'}) YIELD rel RETURN rel",
    );
    run_ok(
        &svc,
        "CALL drevo.memory.assertFact('Acme', 'EMPLOYS', 'Dana', false) YIELD rel RETURN rel",
    );
    let mut s = Session::new_durable(Arc::clone(&svc));
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });
    let mut replies = s.handle(ClientMessage::Run {
        query: "CALL drevo.memory.factsAt('Dana', null) \
                YIELD relation, source, quote RETURN relation, source, quote ORDER BY relation"
            .to_string(),
        parameters: BTreeMap::new(),
        extra: BTreeMap::new(),
    });
    replies.extend(s.handle(ClientMessage::Pull {
        extra: BTreeMap::new(),
    }));
    let records: Vec<&Vec<Bolt>> = replies
        .iter()
        .filter_map(|m| match m {
            ServerMessage::Record { fields } => Some(fields),
            _ => None,
        })
        .collect();
    assert_eq!(records.len(), 2, "{replies:?}");
    assert_eq!(records[0][0], Bolt::String("EMPLOYS".into()));
    assert_eq!(records[0][1], Bolt::Null);
    assert_eq!(records[0][2], Bolt::Null);
    assert_eq!(records[1][0], Bolt::String("WORKS_AT".into()));
    // A node travels as a PackStream structure.
    assert!(
        matches!(records[1][1], Bolt::Structure { .. }),
        "{:?}",
        records[1]
    );
    assert_eq!(records[1][2], Bolt::String("joined Acme".into()));
}

// ----- across a restart ----------------------------------------------------------

static NEXT_DIR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn tmp_wal() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "drevo_prov_{}_{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("native.wal")
}

/// An editor's workflow over two manuscripts across a restart: chunks,
/// mentions and cited facts survive the reopen, re-ingesting is a no-op, and
/// evidence keeps the two books apart.
#[test]
fn citations_survive_a_restart() {
    let wal = tmp_wal();
    {
        let svc = NativeService::open(&wal).unwrap();
        manuscript(&svc);
        run_ok(
            &svc,
            "CALL drevo.memory.assertFact('Dana', 'WORKS_AT', 'Acme', true, \
               {chunk: 'book-1#1', quote: 'joined Acme'}) YIELD rel RETURN rel",
        );
    }
    let svc = NativeService::open(&wal).unwrap();
    manuscript(&svc);
    assert_eq!(count(&svc, "MATCH (c:Chunk) RETURN count(c)"), 3);
    run_ok(
        &svc,
        "CALL drevo.memory.addSource('book-2', null, 'Book Two', 'document') YIELD node \
         RETURN node",
    );
    run_ok(
        &svc,
        "CALL drevo.memory.addChunks('book-2', [{order: 0, text: 'Years later Dana ran Acme.'}]) \
         YIELD node RETURN node",
    );
    run_ok(
        &svc,
        "CALL drevo.memory.assertFact('Dana', 'RUNS', 'Acme', false, \
           {chunk: 'book-2#0', quote: 'Dana ran Acme'}) YIELD rel RETURN rel",
    );
    let res = run_ok(
        &svc,
        "CALL drevo.memory.evidence('Dana', 10) YIELD chunk, source \
         RETURN chunk.id, source.title",
    );
    assert_eq!(
        res.rows,
        vec![
            vec![
                Value::String("book-2#0".into()),
                Value::String("Book Two".into())
            ],
            vec![
                Value::String("book-1#1".into()),
                Value::String("Book One".into())
            ],
        ]
    );
    let _ = std::fs::remove_dir_all(wal.parent().unwrap());
}
