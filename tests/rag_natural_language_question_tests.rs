//! A natural-language question as the query of the graph-RAG procedures
//! (#820).
//!
//! The full-text leg (`fts.search`) requires every term of the query, so a
//! question such as "who watched the storm from the lighthouse" matches only
//! a node that contains all of its words. Without an embedder that is the
//! only seed leg: `drevo.rag.retrieve` and `drevo.rag.context` return nothing
//! for the question although one chapter answers it.

use std::collections::HashMap;

use drevo::cypher::executor::{ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

fn ok(svc: &NativeService, q: &str) -> ExecResult {
    svc.execute(&parse(q).expect("parse"), HashMap::new())
        .unwrap_or_else(|e| panic!("#820: `{q}` failed: {e:?}"))
}

fn strings(result: &ExecResult) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::String(s) => s.clone(),
            other => panic!("expected a string, got {other:?}"),
        })
        .collect()
}

const QUESTION: &str = "who watched the storm from the lighthouse";

fn harbour() -> NativeService {
    let svc = NativeService::in_memory();
    ok(
        &svc,
        "CREATE (:Chapter {title: 'The storm', body: 'The old lighthouse keeper watched \
         the storm roll in over the harbour.'})",
    );
    ok(
        &svc,
        "CREATE (:Chapter {title: 'The market', body: 'Fishmongers shouted prices in the \
         harbour market at dawn.'})",
    );
    ok(
        &svc,
        "CREATE (:Chapter {title: 'The ledger', body: 'Nobody had opened the ledger in years.'})",
    );
    svc
}

#[test]
fn the_words_of_the_question_match_on_their_own() {
    // Control: the chapter is reachable by its words.
    let svc = harbour();
    assert_eq!(
        strings(&ok(
            &svc,
            "CALL fts.search('lighthouse storm', 5) YIELD node RETURN node.title"
        )),
        ["The storm"]
    );
}

#[test]
fn retrieve_finds_the_chapter_that_answers_a_question() {
    let svc = harbour();
    let titles = strings(&ok(
        &svc,
        &format!("CALL drevo.rag.retrieve('{QUESTION}') YIELD node RETURN node.title"),
    ));
    assert_eq!(
        titles.first().map(String::as_str),
        Some("The storm"),
        "#820: the chapter sharing most of the question's words comes first: {titles:?}"
    );
}

#[test]
fn context_for_a_question_is_not_empty() {
    let svc = harbour();
    let text = strings(&ok(
        &svc,
        &format!("CALL drevo.rag.context('{QUESTION}') YIELD text RETURN text"),
    ))
    .remove(0);
    assert!(text.contains("The storm"), "#820: {text}");
}

#[test]
fn a_question_with_no_word_in_the_graph_finds_nothing() {
    let svc = harbour();
    let res = ok(
        &svc,
        "CALL drevo.rag.retrieve('quasar nebula telescope') YIELD node RETURN node.title",
    );
    assert!(res.rows.is_empty(), "{:?}", res.rows);
}

// ----- the option itself (`ftsMode`) ------------------------------------------

fn run(svc: &NativeService, q: &str) -> Result<ExecResult, drevo::cypher::executor::ExecError> {
    svc.execute(&parse(q).expect("parse"), HashMap::new())
}

/// The message of the `InvalidProcedureCall` `q` fails with.
fn refusal(svc: &NativeService, q: &str) -> String {
    match run(svc, q) {
        Err(drevo::cypher::executor::ExecError::InvalidProcedureCall { message, .. }) => message,
        other => panic!("`{q}`: expected InvalidProcedureCall, got {other:?}"),
    }
}

#[test]
fn fts_search_requires_every_word_unless_asked_for_any() {
    let svc = harbour();
    let search = |config: &str| {
        strings(&ok(
            &svc,
            &format!("CALL fts.search('{QUESTION}', 5{config}) YIELD node RETURN node.title"),
        ))
    };
    assert!(search("").is_empty(), "the documented behaviour stays");
    assert!(search(", null").is_empty());
    assert!(search(", {}").is_empty());
    assert!(search(", {ftsMode: 'all'}").is_empty());
    let any = search(", {ftsMode: 'any'}");
    assert_eq!(
        any.first().map(String::as_str),
        Some("The storm"),
        "{any:?}"
    );
}

#[test]
fn fts_search_refuses_a_config_it_does_not_understand() {
    let svc = harbour();
    assert_eq!(
        refusal(
            &svc,
            "CALL fts.search('storm', 5, {ftsMode: 'some'}) YIELD node RETURN node"
        ),
        "ftsMode must be 'all' or 'any', got 'some'"
    );
    assert_eq!(
        refusal(
            &svc,
            "CALL fts.search('storm', 5, {fuzzy: true}) YIELD node RETURN node"
        ),
        "unknown config key `fuzzy`; expected ftsMode"
    );
    assert_eq!(
        refusal(
            &svc,
            "CALL fts.search('storm', 5, 'any') YIELD node RETURN node"
        ),
        "the config must be a map, got String"
    );
}

#[test]
fn relationship_search_takes_the_option_too() {
    let svc = NativeService::in_memory();
    ok(
        &svc,
        "CREATE (:Person {name: 'Ada'})-[:SAW {note: 'the keeper watched the storm'}]->\
         (:Place {name: 'Pier'})",
    );
    let search = |config: &str| {
        ok(
            &svc,
            &format!(
                "CALL fts.searchRelationships('{QUESTION}', 5{config}) YIELD rel RETURN type(rel)"
            ),
        )
        .rows
        .len()
    };
    assert_eq!(search(""), 0);
    assert_eq!(search(", {ftsMode: 'any'}"), 1);
}

#[test]
fn hybrid_search_takes_the_option_and_defaults_to_all() {
    let svc = harbour();
    let search = |config: &str| {
        strings(&ok(
            &svc,
            &format!(
                "CALL drevo.search.hybrid('{QUESTION}', null, 'embedding', 5{config}) \
                 YIELD node RETURN node.title"
            ),
        ))
    };
    assert!(search("").is_empty());
    assert!(search(", {mode: 'fts'}").is_empty());
    let any = search(", {ftsMode: 'any'}");
    assert_eq!(
        any.first().map(String::as_str),
        Some("The storm"),
        "{any:?}"
    );
    assert_eq!(
        refusal(
            &svc,
            "CALL drevo.search.hybrid('storm', null, 'embedding', 5, {ftsMode: 1}) \
             YIELD node RETURN node"
        ),
        "ftsMode must be 'all' or 'any', got 1"
    );
}

#[test]
fn retrieval_can_still_require_every_word() {
    let svc = harbour();
    let res = ok(
        &svc,
        &format!(
            "CALL drevo.rag.retrieve('{QUESTION}', {{ftsMode: 'all'}}) YIELD node \
             RETURN node.title"
        ),
    );
    assert!(res.rows.is_empty(), "{:?}", res.rows);
    // The words on their own match in either mode.
    for config in ["{ftsMode: 'all'}", "{ftsMode: 'any'}", "{ftsMode: null}"] {
        let titles = strings(&ok(
            &svc,
            &format!(
                "CALL drevo.rag.retrieve('lighthouse storm', {config}) YIELD node \
                 RETURN node.title"
            ),
        ));
        assert_eq!(
            titles.first().map(String::as_str),
            Some("The storm"),
            "{config}"
        );
    }
}

#[test]
fn the_trace_records_the_mode_and_an_older_trace_replays_as_all() {
    let svc = harbour();
    let traced = |config: &str| {
        let res = ok(
            &svc,
            &format!(
                "CALL drevo.rag.context('{QUESTION}', {config}) YIELD traceId \
                 MATCH (t:ContextTrace {{id: traceId}}) RETURN traceId, t.config"
            ),
        );
        let config: serde_json::Value =
            serde_json::from_str(&strings_at(&res, 1)).expect("the stored config is JSON");
        (strings_at(&res, 0), config)
    };
    let (_, config) = traced("{trace: true}");
    assert_eq!(config["ftsMode"], "any", "the default of the pipeline");

    // A trace recorded before the option existed has no `ftsMode` and was
    // retrieved with every word required: its replay does the same.
    let (id, mut config) = traced("{trace: true, ftsMode: 'all'}");
    assert_eq!(config["ftsMode"], "all");
    config.as_object_mut().unwrap().remove("ftsMode");
    let params = HashMap::from([
        ("id".to_string(), Value::String(id.clone())),
        ("config".to_string(), Value::String(config.to_string())),
    ]);
    svc.execute(
        &parse("MATCH (t:ContextTrace {id: $id}) SET t.config = $config").unwrap(),
        params,
    )
    .expect("rewrite the stored config");
    let replay = ok(
        &svc,
        &format!("CALL drevo.rag.replay('{id}') YIELD changed RETURN changed"),
    );
    assert_eq!(replay.rows[0][0], Value::Bool(false));
}

fn strings_at(result: &ExecResult, column: usize) -> String {
    match &result.rows[0][column] {
        Value::String(s) => s.clone(),
        other => panic!("expected a string, got {other:?}"),
    }
}

#[test]
fn conversation_recall_still_requires_the_whole_query() {
    let svc = NativeService::in_memory();
    ok(
        &svc,
        "CALL drevo.memory.addMessage('s1', 'user', 'the keeper watched the storm') \
         YIELD node RETURN node",
    );
    let recall = |query: &str| {
        ok(
            &svc,
            &format!("CALL drevo.memory.recall('s1', '{query}', 5) YIELD node RETURN node.text"),
        )
        .rows
        .len()
    };
    assert_eq!(recall("keeper storm"), 1);
    assert_eq!(recall(QUESTION), 0);
}
