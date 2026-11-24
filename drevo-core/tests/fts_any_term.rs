//! `FtsMatch::Any`: a full-text search that takes a document holding any
//! word of the query, where [`FtsMatch::All`] (what `search` does) takes
//! only a document holding all of it.
//!
//! A natural-language question — "who watched the storm from the
//! lighthouse" — is the case: no document holds every word of it, and the
//! one that answers it holds most of them.

use drevo_core::engine::GraphEngine;
use drevo_core::model::{NewEdge, NewNode, Properties};
use drevo_core::native::NativeGraph;
use drevo_core::native_fts::{FtsMatch, NativeFtsIndex, NativeFtsRelIndex};

fn doc(title: &str) -> NewNode {
    NewNode {
        kind: "Doc".to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn index(g: &NativeGraph) -> NativeFtsIndex {
    let mut fts = NativeFtsIndex::new();
    fts.sync(g);
    fts
}

fn ids(hits: &[(u64, f32)]) -> Vec<u64> {
    hits.iter().map(|(id, _)| *id).collect()
}

const QUESTION: &str = "who watched the storm from the lighthouse";

/// Three chapters; only the first answers [`QUESTION`].
fn harbour() -> (NativeGraph, [u64; 3]) {
    let g = NativeGraph::new();
    let storm = g
        .create_node(doc(
            "The old lighthouse keeper watched the storm roll in over the harbour",
        ))
        .unwrap();
    let market = g
        .create_node(doc(
            "Fishmongers shouted prices in the harbour market at dawn",
        ))
        .unwrap();
    let ledger = g
        .create_node(doc("Nobody had opened the ledger in years"))
        .unwrap();
    (g, [storm.id, market.id, ledger.id])
}

#[test]
fn all_finds_nothing_for_a_question_and_any_finds_its_answer_first() {
    let (g, [storm, ..]) = harbour();
    let fts = index(&g);
    assert_eq!(fts.search(QUESTION, 10), Vec::new());
    assert_eq!(fts.search_matching(QUESTION, 10, FtsMatch::All), Vec::new());

    let hits = fts.search_matching(QUESTION, 10, FtsMatch::Any);
    assert_eq!(hits.first().map(|h| h.0), Some(storm), "{hits:?}");
}

#[test]
fn search_is_the_all_mode() {
    let (g, _) = harbour();
    let fts = index(&g);
    for query in ["harbour", "lighthouse storm", "the", QUESTION, "absent"] {
        assert_eq!(
            fts.search(query, 10),
            fts.search_matching(query, 10, FtsMatch::All),
            "{query}"
        );
    }
    assert_eq!(FtsMatch::default(), FtsMatch::All);
}

#[test]
fn any_takes_a_document_holding_one_of_the_words() {
    let (g, [storm, market, ledger]) = harbour();
    let fts = index(&g);
    let mut found = ids(&fts.search_matching("ledger market", 10, FtsMatch::Any));
    found.sort_unstable();
    assert_eq!(found, vec![market, ledger]);
    assert_eq!(fts.search("ledger market", 10), Vec::new());
    assert_eq!(
        ids(&fts.search_matching("lighthouse zeppelin", 10, FtsMatch::Any)),
        vec![storm]
    );
}

#[test]
fn a_query_with_no_word_in_the_corpus_finds_nothing() {
    let (g, _) = harbour();
    let fts = index(&g);
    assert_eq!(
        fts.search_matching("quasar nebula telescope", 10, FtsMatch::Any),
        Vec::new()
    );
}

#[test]
fn a_word_matches_whole_not_by_a_shared_fragment() {
    let g = NativeGraph::new();
    g.create_node(doc("a story told at the store")).unwrap();
    let fts = index(&g);
    // "storm" shares "sto" with both words and is in neither.
    assert_eq!(fts.search_matching("storm", 10, FtsMatch::Any), Vec::new());
    assert_eq!(
        fts.search_matching("storm story", 10, FtsMatch::Any).len(),
        1
    );
}

#[test]
fn more_words_in_common_rank_higher() {
    let g = NativeGraph::new();
    let one = g.create_node(doc("the keeper of the garden")).unwrap();
    let two = g.create_node(doc("the keeper of the lighthouse")).unwrap();
    let three = g
        .create_node(doc("the keeper of the lighthouse in the storm"))
        .unwrap();
    g.create_node(doc("a ledger nobody opened")).unwrap();
    let hits = index(&g).search_matching("lighthouse keeper storm", 10, FtsMatch::Any);
    assert_eq!(ids(&hits), vec![three.id, two.id, one.id]);
    assert!(hits[0].1 > hits[1].1 && hits[1].1 > hits[2].1, "{hits:?}");
}

#[test]
fn a_rare_word_outweighs_a_common_one() {
    let g = NativeGraph::new();
    for n in 0..8 {
        g.create_node(doc(&format!("harbour note number {n}")))
            .unwrap();
    }
    let rare = g.create_node(doc("the xylograph archive")).unwrap();
    let hits = index(&g).search_matching("harbour xylograph", 20, FtsMatch::Any);
    assert_eq!(hits.len(), 9);
    assert_eq!(hits[0].0, rare.id, "{hits:?}");
}

#[test]
fn one_word_gives_the_same_answer_in_both_modes() {
    let (g, _) = harbour();
    let fts = index(&g);
    for word in ["harbour", "lighthouse", "the", "absent"] {
        assert_eq!(
            fts.search_matching(word, 10, FtsMatch::Any),
            fts.search_matching(word, 10, FtsMatch::All),
            "{word}"
        );
    }
}

#[test]
fn a_repeated_word_counts_once() {
    let (g, _) = harbour();
    let fts = index(&g);
    assert_eq!(
        fts.search_matching("harbour harbour HARBOUR", 10, FtsMatch::Any),
        fts.search_matching("harbour", 10, FtsMatch::Any)
    );
}

#[test]
fn words_too_short_to_index_are_ignored() {
    let (g, [storm, ..]) = harbour();
    let fts = index(&g);
    assert_eq!(
        ids(&fts.search_matching("is it a lighthouse", 10, FtsMatch::Any)),
        vec![storm]
    );
    assert_eq!(fts.search_matching("is it", 10, FtsMatch::Any), Vec::new());
    assert_eq!(fts.search_matching("", 10, FtsMatch::Any), Vec::new());
    assert_eq!(
        fts.search_matching("lighthouse", 0, FtsMatch::Any),
        Vec::new()
    );
}

#[test]
fn the_limit_keeps_the_best() {
    let (g, [storm, ..]) = harbour();
    let fts = index(&g);
    let all = fts.search_matching(QUESTION, 10, FtsMatch::Any);
    assert_eq!(all.len(), 3, "every chapter holds `the`");
    assert_eq!(fts.search_matching(QUESTION, 1, FtsMatch::Any), all[..1]);
    assert_eq!(all[0].0, storm);
}

#[test]
fn a_transaction_reads_its_own_writes_in_any_mode() {
    let (g, [storm, market, _]) = harbour();
    let fts = index(&g);
    let tx = g.tx_begin();
    let engine = g.tx_engine(tx).unwrap();
    let wreck = engine
        .create_node(doc("The storm left a wreck below the lighthouse"))
        .unwrap();
    engine.delete_node(market).unwrap();
    let changes = g.tx_changes(tx).expect("open transaction");
    let inside = fts.search_matching_with_changes(QUESTION, 10, FtsMatch::Any, &changes.nodes);
    let found = ids(&inside);
    assert!(
        found.contains(&wreck.id) && found.contains(&storm),
        "{found:?}"
    );
    assert!(!found.contains(&market), "{found:?}");

    // The answer inside the transaction is the answer after its commit.
    g.tx_commit(tx).unwrap();
    assert_eq!(
        inside,
        index(&g).search_matching(QUESTION, 10, FtsMatch::Any)
    );
}

#[test]
fn no_changes_answer_exactly_as_search_matching() {
    let (g, _) = harbour();
    let fts = index(&g);
    for mode in [FtsMatch::All, FtsMatch::Any] {
        assert_eq!(
            fts.search_matching_with_changes(QUESTION, 10, mode, &[]),
            fts.search_matching(QUESTION, 10, mode),
            "{mode:?}"
        );
    }
}

#[test]
fn relationships_have_the_any_mode_too() {
    let g = NativeGraph::new();
    let a = g.create_node(doc("a")).unwrap();
    let b = g.create_node(doc("b")).unwrap();
    let rel = |text: &str| {
        let mut properties = Properties::default();
        properties.insert("note", serde_json::json!(text));
        NewEdge {
            from_id: a.id,
            to_id: b.id,
            kind: "MENTIONS".to_string(),
            weight: 1.0,
            properties,
        }
    };
    let storm = g.create_edge(rel("the keeper watched the storm")).unwrap();
    g.create_edge(rel("prices at the market")).unwrap();
    let mut fts = NativeFtsRelIndex::new();
    fts.sync(&g);
    assert_eq!(fts.search(QUESTION, 10), Vec::new());
    let hits = fts.search_matching(QUESTION, 10, FtsMatch::Any);
    assert_eq!(hits.first().map(|h| h.0), Some(storm.id), "{hits:?}");
    assert_eq!(
        fts.search_matching_with_changes(QUESTION, 10, FtsMatch::Any, &[]),
        hits
    );
}
