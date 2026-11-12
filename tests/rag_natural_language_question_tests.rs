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
