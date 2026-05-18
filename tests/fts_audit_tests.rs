//! Audit tests for `src/fts/*` and `Drevo::search_fts` — Phase 8.5 task
//! `00108`.
//!
//! These tests pin behaviours that the audit verified:
//!
//! * **Tokenizer is deterministic on Unicode** — Cyrillic, Hebrew (RTL),
//!   combining diacritics, mixed scripts and emoji follow the documented
//!   normalise → trigram pipeline (`drevo-database` §"FTS index").
//! * **Posting-list intersection has clean AND semantics** — idempotent,
//!   commutative w.r.t. trigram order, empty-on-mismatch
//!   (`drevo-database` §"intersect posting lists").
//! * **FTS reindex invariant** — under random create / update / delete
//!   mutation streams, every node's *current* title+body trigrams are
//!   exactly the trigrams whose posting lists currently contain the
//!   node's id, and no stale entries linger
//!   (`drevo-database` invariants #2 + #3; `drevo-rust` common pitfalls
//!   #1 + #2).
//! * **`updated_idx` parity under mutation** — every create / update /
//!   delete leaves exactly one (or zero, for deleted nodes) entry per
//!   node in the inverted-timestamp index
//!   (`drevo-database` §"updated_idx"; cross-link with `00106` invariant
//!   #4).
//! * **`search_fts` ordering is deterministic** — the same query against
//!   the same corpus produces the same result vector across runs
//!   (`drevo-tdd` §"Property-based tests for invariants").
//!
//! Test style follows the precedent set by `00106` (DB invariant fuzzer)
//! and `00107` (traversal audit): a manual `xorshift32` PRNG so the
//! tests are deterministic across runs without pulling in `proptest`
//! (which is scheduled for Phase 9 task `00057`).

use drevo::db::Drevo;
use drevo::fts::{extract_trigrams, normalize, trigrams};
use drevo::model::{NewNode, NodePatch, Properties};
use std::collections::{BTreeSet, HashSet};

// ===================================================================
// Helpers
// ===================================================================

fn db() -> Drevo {
    Drevo::open_in_memory().unwrap()
}

fn node(kind: &str, title: &str, body: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn xorshift32(state: &mut u32) -> u32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    x
}

fn random_in_range(state: &mut u32, lo: usize, hi: usize) -> usize {
    let span = (hi - lo) as u32;
    if span == 0 {
        lo
    } else {
        lo + (xorshift32(state) % span) as usize
    }
}

// ===================================================================
// Tokenizer — Unicode class coverage
// ===================================================================

#[test]
fn tokenizer_cyrillic_lowercase_and_trigrams() {
    // Cyrillic is `alphanumeric` so it is kept; the trigram pipeline
    // should treat it like any other Latin/Greek script.
    let norm = normalize("Привет Мир");
    assert_eq!(norm, "привет мир");

    let tg = trigrams("Привет");
    assert!(tg.contains(&"при".to_string()));
    assert!(tg.contains(&"рив".to_string()));
    assert!(tg.contains(&"иве".to_string()));
    assert!(tg.contains(&"вет".to_string()));
}

#[test]
fn tokenizer_hebrew_rtl_is_kept() {
    // Hebrew runs RTL in display but the underlying byte order is
    // logical; the tokenizer operates on logical order so it should
    // not need any special handling.
    let norm = normalize("שלום עולם");
    // Hebrew has no lowercase variant; chars are kept verbatim.
    assert_eq!(norm, "שלום עולם");

    let tg = trigrams("שלום");
    // 4 characters → 2 unique trigrams in logical order.
    assert!(!tg.is_empty(), "Hebrew trigrams must not be empty");
    assert!(tg.iter().all(|t| t.chars().count() == 3));
}

#[test]
fn tokenizer_combining_diacritics_decomposed_form() {
    // "résumé" can be either precomposed (U+00E9) or decomposed
    // (e + U+0301). The tokenizer does NOT NFC-normalise; this test
    // pins that behaviour so a future tokenizer refactor either
    // (a) keeps the byte-exact behaviour or
    // (b) consciously adopts Unicode normalization with a test update.
    let precomposed = trigrams("résumé"); // U+00E9
    let decomposed = trigrams("re\u{0301}sume\u{0301}"); // e + U+0301
                                                         // The two forms produce *different* trigram sets — this is the
                                                         // observed-and-pinned behaviour. The audit flags it for follow-up
                                                         // but does not change semantics in this PR.
    assert_ne!(
        precomposed, decomposed,
        "tokenizer pins NFC-naive behaviour; see audit/AUDIT-fts.md F2"
    );
}

#[test]
fn tokenizer_emoji_are_stripped() {
    // Emoji are not `alphanumeric` and are not in the CJK ranges, so
    // they become spaces and produce no own trigrams.
    let norm = normalize("hi 🎉 there");
    assert_eq!(norm, "hi there");

    // Emoji-only input must produce zero trigrams (not panic).
    assert!(trigrams("🎉🎊🎈").is_empty());
}

#[test]
fn tokenizer_zero_width_chars_become_word_breaks() {
    // ZWJ / ZWNJ are non-alphanumeric so they break the word.
    let norm = normalize("test\u{200D}word\u{200C}here");
    assert_eq!(norm, "test word here");
}

#[test]
fn tokenizer_cjk_bigrams_only_between_consecutive_cjk() {
    // "a世界b" → "a", " ", "世", "界", " ", "b" after normalise (no
    // strip — all alphanumeric/CJK), but bigrams only fire on
    // *consecutive* CJK characters.
    let tg = trigrams("a世界b");
    assert!(
        tg.contains(&"世界".to_string()),
        "consecutive CJK pair must produce a bigram"
    );
    // Latin-then-CJK and CJK-then-Latin must NOT produce a bigram —
    // only standard 3-char trigrams across that boundary.
    assert!(!tg.contains(&"a世".to_string()));
    assert!(!tg.contains(&"界b".to_string()));
}

#[test]
fn tokenizer_extract_trigrams_is_deterministic_on_random_unicode() {
    // Build a random text from a small alphabet that covers each
    // skill-cited Unicode class (Latin, Cyrillic, CJK, emoji,
    // combining mark, RTL) and verify the tokenizer is a *pure
    // function*: same input → same output across runs.
    let alphabet: Vec<&str> = vec![
        "a", "b", "c", "1", "2", " ", ".", "!", // Latin / digits / punct
        "п", "р", "и", "в", // Cyrillic
        "世", "界", "你", "好", // CJK
        "🎉", "🎊",       // emoji
        "\u{0301}", // combining acute
        "ש", "ל", "ו", "ם", // Hebrew
        "א", "ב", // Hebrew
    ];
    let seeds = [0x1u32, 42u32, 0xc0ffeeu32, 0xdeadbeefu32];
    for &seed in &seeds {
        let mut state = seed;
        let mut text = String::new();
        for _ in 0..200 {
            let idx = random_in_range(&mut state, 0, alphabet.len());
            text.push_str(alphabet[idx]);
        }
        let a = extract_trigrams(&text, "");
        let b = extract_trigrams(&text, "");
        assert_eq!(
            a, b,
            "extract_trigrams must be a pure function (seed={:#x})",
            seed
        );
        // No empty-string trigrams should leak from the pipeline.
        assert!(a.iter().all(|t| !t.is_empty()));
        // All trigrams must be exactly 2 or 3 characters
        // (CJK bigrams are length 2, the rest are length 3).
        assert!(
            a.iter()
                .all(|t| { t.chars().count() == 2 || t.chars().count() == 3 }),
            "every trigram must be length 2 (CJK bigram) or 3"
        );
    }
}

// ===================================================================
// Posting-list intersection: AND semantics
// ===================================================================

#[test]
fn intersect_trigrams_is_idempotent() {
    let db = db();
    let a = db.create_node(node("note", "Hello World", "")).unwrap();
    let b = db.create_node(node("note", "Hello There", "")).unwrap();
    let _ = db.create_node(node("note", "Other Body", "")).unwrap();

    let q1 = vec!["hel".to_string()];
    let r1 = db.fts_intersect_trigrams(&q1).unwrap();
    let r2 = db.fts_intersect_trigrams(&q1).unwrap();
    assert_eq!(r1, r2, "intersect_trigrams must be deterministic");
    let set: HashSet<u64> = r1.into_iter().collect();
    assert!(set.contains(&a.id));
    assert!(set.contains(&b.id));
}

#[test]
fn intersect_trigrams_is_commutative_in_input_order() {
    // The set-AND of posting lists must not depend on the order in
    // which trigrams are presented.
    let db = db();
    let a = db
        .create_node(node("note", "Rust programming", ""))
        .unwrap();
    let _ = db.create_node(node("note", "Rust language", "")).unwrap();
    let _ = db
        .create_node(node("note", "Python programming", ""))
        .unwrap();

    let r1 = db
        .fts_intersect_trigrams(&["rus".to_string(), "pro".to_string()])
        .unwrap();
    let r2 = db
        .fts_intersect_trigrams(&["pro".to_string(), "rus".to_string()])
        .unwrap();
    assert_eq!(
        r1, r2,
        "intersect_trigrams must be commutative in input order"
    );
    assert_eq!(r1, vec![a.id]);
}

#[test]
fn intersect_trigrams_empty_when_any_trigram_misses() {
    let db = db();
    let _ = db.create_node(node("note", "Hello", "")).unwrap();

    // One trigram present, one absent → AND of the two is empty.
    let r = db
        .fts_intersect_trigrams(&["hel".to_string(), "zzz".to_string()])
        .unwrap();
    assert!(
        r.is_empty(),
        "intersection with a non-existent trigram must be empty"
    );
}

// ===================================================================
// FTS reindex invariant — random mutation stream
// ===================================================================

/// Recompute every node's current title+body trigram set and compare
/// it against the FTS posting lists by:
///   1. For each (node, expected_trigram), assert that the posting
///      list of `expected_trigram` contains the node id.
///   2. For each known trigram in any node, the union of all node
///      ids in its posting list must be a subset of the live nodes
///      (i.e. no stale entries from deleted/old text remain).
fn assert_fts_index_matches_live_nodes(db: &Drevo, live_node_ids: &HashSet<u64>) {
    // For every live node, every trigram in its current title+body
    // MUST appear in the index pointing at that node.
    for &id in live_node_ids {
        let n = db
            .get_node(id)
            .unwrap()
            .expect("live node must be retrievable");
        let expected: Vec<String> = extract_trigrams(&n.title, &n.body);
        for tg in &expected {
            let posting = db.fts_node_ids_for_trigram(tg).unwrap();
            assert!(
                posting.contains(&id),
                "FTS index missing trigram '{}' for live node id={} \
                 (title='{}', body='{}'). posting={:?}",
                tg,
                id,
                n.title,
                n.body,
                posting,
            );
        }
    }

    // The union of every trigram known to any *live* node forms the
    // complete set of trigrams that SHOULD be present in the index.
    // Build that set, then walk the index and assert no node id sits
    // in a posting list it does not belong to.
    let mut all_trigrams: BTreeSet<String> = BTreeSet::new();
    let mut expected_membership: std::collections::HashMap<String, HashSet<u64>> =
        std::collections::HashMap::new();
    for &id in live_node_ids {
        let n = db.get_node(id).unwrap().unwrap();
        for tg in extract_trigrams(&n.title, &n.body) {
            all_trigrams.insert(tg.clone());
            expected_membership.entry(tg).or_default().insert(id);
        }
    }
    for tg in &all_trigrams {
        let observed: HashSet<u64> = db
            .fts_node_ids_for_trigram(tg)
            .unwrap()
            .into_iter()
            .collect();
        let expected = expected_membership.get(tg).cloned().unwrap_or_default();
        assert_eq!(
            observed, expected,
            "posting list for trigram '{}' has stale or missing entries: \
             observed={:?}, expected={:?}",
            tg, observed, expected,
        );
    }
}

/// Random title/body text from a small Latin + CJK + Cyrillic alphabet
/// — keeps trigram space small enough that posting-list assertions
/// stay fast.
fn random_text(state: &mut u32) -> String {
    let alphabet: Vec<&str> = vec![
        "a", "b", "c", "d", "e", " ", "1", "2", "中", "国", "你", "好", "п", "р", "и", "в",
    ];
    let len = random_in_range(state, 3, 12);
    let mut out = String::new();
    for _ in 0..len {
        let idx = random_in_range(state, 0, alphabet.len());
        out.push_str(alphabet[idx]);
    }
    out
}

#[test]
fn fts_index_matches_node_text_under_random_mutations() {
    // Seed × 30 ops fuzzer, asserting the FTS invariant after every
    // mutation. The mutation kinds (create / update / delete) mirror
    // the three FTS-touching paths in `db.rs`.
    let seeds = [0x1u32, 42u32, 0xc0ffeeu32];
    for &seed in &seeds {
        let db = db();
        let mut state = seed;
        let mut live: HashSet<u64> = HashSet::new();

        for _ in 0..30 {
            let op = xorshift32(&mut state) % 3;
            match op {
                // create
                0 => {
                    let title = format!("t{}_{}", xorshift32(&mut state), random_text(&mut state));
                    let body = random_text(&mut state);
                    let n = db.create_node(node("audit", &title, &body)).unwrap();
                    live.insert(n.id);
                }
                // update (only if there is a live node)
                1 if !live.is_empty() => {
                    let ids: Vec<u64> = live.iter().copied().collect();
                    let pick = ids[random_in_range(&mut state, 0, ids.len())];
                    let new_title =
                        format!("u{}_{}", xorshift32(&mut state), random_text(&mut state));
                    let new_body = random_text(&mut state);
                    db.update_node(
                        pick,
                        NodePatch {
                            title: Some(new_title),
                            body: Some(new_body),
                            ..Default::default()
                        },
                    )
                    .unwrap();
                }
                // delete (only if there is a live node)
                2 if !live.is_empty() => {
                    let ids: Vec<u64> = live.iter().copied().collect();
                    let pick = ids[random_in_range(&mut state, 0, ids.len())];
                    db.delete_node(pick).unwrap();
                    live.remove(&pick);
                }
                _ => {}
            }
            assert_fts_index_matches_live_nodes(&db, &live);
        }
    }
}

// ===================================================================
// updated_idx parity invariant — random mutation stream
// ===================================================================

#[test]
fn updated_idx_parity_under_random_mutations() {
    // Cross-link with `00106` invariant #4: every create / update /
    // delete leaves exactly one entry per live node in the
    // `updated:` index, and zero for deleted nodes.
    // We delegate the heavy lifting to `Drevo::verify_invariants` —
    // this test exercises the FTS-adjacent path (title/body updates)
    // specifically, so a regression in `updated_idx` maintenance
    // *during an FTS-touching mutation* is caught here.
    let seeds = [0x2u32, 100u32, 0xdeadbeefu32];
    for &seed in &seeds {
        let db = db();
        let mut state = seed;
        let mut live: Vec<u64> = Vec::new();

        for _ in 0..25 {
            let op = xorshift32(&mut state) % 4;
            match op {
                0 => {
                    let title = format!("p{}_{}", xorshift32(&mut state), random_text(&mut state));
                    let n = db.create_node(node("audit", &title, "body")).unwrap();
                    live.push(n.id);
                }
                1 if !live.is_empty() => {
                    let pick = live[random_in_range(&mut state, 0, live.len())];
                    let new_title =
                        format!("q{}_{}", xorshift32(&mut state), random_text(&mut state));
                    db.update_node(
                        pick,
                        NodePatch {
                            title: Some(new_title),
                            ..Default::default()
                        },
                    )
                    .unwrap();
                }
                2 if !live.is_empty() => {
                    let pick = live[random_in_range(&mut state, 0, live.len())];
                    let new_body = random_text(&mut state);
                    db.update_node(
                        pick,
                        NodePatch {
                            body: Some(new_body),
                            ..Default::default()
                        },
                    )
                    .unwrap();
                }
                3 if !live.is_empty() => {
                    let pos = random_in_range(&mut state, 0, live.len());
                    let pick = live.remove(pos);
                    db.delete_node(pick).unwrap();
                }
                _ => {}
            }

            let violations = db.verify_invariants().unwrap();
            assert!(
                violations.is_empty(),
                "invariants violated after seed={:#x} op={:?}: {:?}",
                seed,
                op,
                violations,
            );
        }
    }
}

// ===================================================================
// search_fts — score stability + ordering determinism
// ===================================================================

#[test]
fn search_fts_results_are_deterministic_across_runs() {
    // Same corpus + same query should produce byte-identical
    // ScoredNode vectors across repeated calls. Pin this so that
    // future ranking changes (BM25, smoothing tweaks) are an
    // observable diff.
    let db = db();
    let _ = db
        .create_node(node("note", "Rust programming", ""))
        .unwrap();
    let _ = db.create_node(node("note", "Rust language", "")).unwrap();
    let _ = db
        .create_node(node("note", "Python programming", ""))
        .unwrap();
    let _ = db
        .create_node(node("note", "Programming Rust idioms", ""))
        .unwrap();

    let r1 = db.search_fts("rust programming", 10).unwrap();
    let r2 = db.search_fts("rust programming", 10).unwrap();
    assert_eq!(r1.len(), r2.len());
    for (a, b) in r1.iter().zip(r2.iter()) {
        assert_eq!(a.node.id, b.node.id);
        // f32 scores must be bit-identical because both runs hit
        // the same code path with the same inputs.
        assert_eq!(a.score.to_bits(), b.score.to_bits());
    }
}

#[test]
fn search_fts_ordering_stable_on_tied_scores() {
    // When two nodes have identical TF-IDF scores (same title length,
    // same matching trigrams) the result must be ordered by ascending
    // node id — the documented secondary sort key.
    let db = db();
    let a = db.create_node(node("note", "Alpha beta", "")).unwrap();
    let b = db.create_node(node("note", "Gamma beta", "")).unwrap();
    let results = db.search_fts("beta", 10).unwrap();
    // Both nodes contain "bet" so both are candidates.
    let ids: Vec<u64> = results.iter().map(|s| s.node.id).collect();
    assert_eq!(ids.len(), 2);
    // For tied scores: ascending node id (a was created first).
    assert!(ids[0] < ids[1]);
    assert_eq!(ids, vec![a.id, b.id]);
}

#[test]
fn search_fts_smoothed_idf_is_positive_when_df_equals_n() {
    // The smoothed IDF `ln(1 + N/df)` is strictly positive when
    // `df == N` (every node contains the trigram). The naive
    // `ln(N/df)` would collapse to 0 and produce a zero-score
    // (excluded) result — pin the smoothed behaviour so a future
    // change to plain TF-IDF is an observable test diff.
    let db = db();
    // All three nodes contain "rus" — df == N == 3.
    for i in 0..3 {
        db.create_node(node("note", &format!("rust note {}", i), ""))
            .unwrap();
    }
    let results = db.search_fts("rust", 10).unwrap();
    assert_eq!(
        results.len(),
        3,
        "smoothed IDF must yield positive scores when df == N"
    );
    for r in &results {
        assert!(
            r.score > 0.0,
            "score must be strictly positive under smoothed IDF, got {}",
            r.score
        );
    }
}
