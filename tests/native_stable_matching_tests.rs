//! `CALL drevo.stableMatching(proposerLabel, acceptorLabel, relType,
//! rankProperty) YIELD proposer, acceptor, proposerRank, acceptorRank`
//! (issue #541) — Gale–Shapley stable matching over preferences stored as
//! edges, through [`NativeService::execute`].
//!
//! The scenario is mentoring: mentees and mentors each rank the other side with
//! `(:Mentee)-[:PREFERS {rank}]->(:Mentor)` and the reverse, lower rank = more
//! preferred. The expected matching is worked out by hand in the comments.

use std::collections::HashMap;

use drevo::cypher::executor::{ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

fn run_ok(svc: &NativeService, q: &str) -> ExecResult {
    svc.execute(&parse(q).expect("parse"), HashMap::new())
        .unwrap_or_else(|e| panic!("execute `{q}`: {e:?}"))
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

/// Mentees ann/bob/cyd and mentors xia/yan/zed with complete rankings:
///
/// ```text
/// ann: xia 1, yan 2, zed 3      xia: bob 1, ann 2, cyd 3
/// bob: xia 1, zed 2, yan 3      yan: ann 1, cyd 2, bob 3
/// cyd: yan 1, xia 2, zed 3      zed: ann 1, bob 2, cyd 3
/// ```
fn mentoring() -> NativeService {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CREATE (ann:Mentee {name: 'ann'}), (bob:Mentee {name: 'bob'}), (cyd:Mentee {name: 'cyd'}), \
         (xia:Mentor {name: 'xia'}), (yan:Mentor {name: 'yan'}), (zed:Mentor {name: 'zed'}), \
         (ann)-[:PREFERS {rank: 1}]->(xia), (ann)-[:PREFERS {rank: 2}]->(yan), (ann)-[:PREFERS {rank: 3}]->(zed), \
         (bob)-[:PREFERS {rank: 1}]->(xia), (bob)-[:PREFERS {rank: 2}]->(zed), (bob)-[:PREFERS {rank: 3}]->(yan), \
         (cyd)-[:PREFERS {rank: 1}]->(yan), (cyd)-[:PREFERS {rank: 2}]->(xia), (cyd)-[:PREFERS {rank: 3}]->(zed), \
         (xia)-[:PREFERS {rank: 1}]->(bob), (xia)-[:PREFERS {rank: 2}]->(ann), (xia)-[:PREFERS {rank: 3}]->(cyd), \
         (yan)-[:PREFERS {rank: 1}]->(ann), (yan)-[:PREFERS {rank: 2}]->(cyd), (yan)-[:PREFERS {rank: 3}]->(bob), \
         (zed)-[:PREFERS {rank: 1}]->(ann), (zed)-[:PREFERS {rank: 2}]->(bob), (zed)-[:PREFERS {rank: 3}]->(cyd)",
    );
    svc
}

const MATCH_MENTEES: &str = "CALL drevo.stableMatching('Mentee', 'Mentor', 'PREFERS', 'rank') \
     YIELD proposer, acceptor, proposerRank, acceptorRank \
     RETURN proposer.name AS p, acceptor.name AS a, proposerRank, acceptorRank ORDER BY p";

#[test]
fn mentees_propose_and_get_a_stable_matching() {
    // ann->xia (held); bob->xia: xia prefers bob, ann is released; cyd->yan
    // (held); ann->yan: yan prefers ann, cyd is released; cyd->xia: rejected
    // (xia holds bob); cyd->zed (held).
    let svc = mentoring();
    let res = run_ok(&svc, MATCH_MENTEES);
    assert_eq!(
        res.rows,
        vec![
            vec![s("ann"), s("yan"), Value::Integer(2), Value::Integer(1)],
            vec![s("bob"), s("xia"), Value::Integer(1), Value::Integer(1)],
            vec![s("cyd"), s("zed"), Value::Integer(3), Value::Integer(3)],
        ]
    );
}

#[test]
fn the_sides_can_be_swapped() {
    let svc = mentoring();
    let res = run_ok(
        &svc,
        "CALL drevo.stableMatching('Mentor', 'Mentee', 'PREFERS', 'rank') \
         YIELD proposer, acceptor RETURN proposer.name AS p, acceptor.name AS a ORDER BY p",
    );
    assert_eq!(
        res.rows,
        vec![
            vec![s("xia"), s("bob")],
            vec![s("yan"), s("ann")],
            vec![s("zed"), s("cyd")],
        ]
    );
}

#[test]
fn only_mutual_preferences_of_the_given_type_count() {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CREATE (a:Mentee {name: 'a'}), (b:Mentee {name: 'b'}), (x:Mentor {name: 'x'}), \
         (a)-[:PREFERS {rank: 1}]->(x), \
         (b)-[:PREFERS {rank: 1}]->(x), (x)-[:PREFERS {rank: 1}]->(b), \
         (x)-[:LIKES {rank: 0}]->(a)",
    );
    // x never ranks a with :PREFERS (the :LIKES edge is another type), so the
    // pair is not acceptable to x even though a wants x.
    let res = run_ok(
        &svc,
        "CALL drevo.stableMatching('Mentee', 'Mentor', 'PREFERS', 'rank') \
         YIELD proposer, acceptor RETURN proposer.name AS p, acceptor.name AS a",
    );
    assert_eq!(res.rows, vec![vec![s("b"), s("x")]]);
}

#[test]
fn an_unranked_preference_comes_after_ranked_ones() {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CREATE (a:Mentee {name: 'a'}), (x:Mentor {name: 'x'}), (y:Mentor {name: 'y'}), \
         (a)-[:PREFERS]->(x), (a)-[:PREFERS {rank: 5}]->(y), \
         (x)-[:PREFERS {rank: 1}]->(a), (y)-[:PREFERS {rank: 1}]->(a)",
    );
    let res = run_ok(
        &svc,
        "CALL drevo.stableMatching('Mentee', 'Mentor', 'PREFERS', 'rank') \
         YIELD acceptor, proposerRank RETURN acceptor.name AS a, proposerRank",
    );
    assert_eq!(res.rows, vec![vec![s("y"), Value::Integer(5)]]);
}

#[test]
fn no_candidates_means_no_rows_and_a_bare_call_has_four_columns() {
    let svc = NativeService::in_memory();
    let res = run_ok(
        &svc,
        "CALL drevo.stableMatching('Mentee', 'Mentor', 'PREFERS', 'rank')",
    );
    assert_eq!(
        res.columns,
        vec!["proposer", "acceptor", "proposerRank", "acceptorRank"]
    );
    assert!(res.rows.is_empty());
}

#[test]
fn a_large_instance_is_stable() {
    // 30 mentees x 30 mentors with rotated rankings; check stability of the
    // result against the stored preferences with Cypher alone.
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "UNWIND range(0, 29) AS i CREATE (:Mentee {name: 'e' + toString(i), i: i}), \
         (:Mentor {name: 'o' + toString(i), i: i})",
    );
    run_ok(
        &svc,
        "MATCH (e:Mentee), (o:Mentor) \
         CREATE (e)-[:PREFERS {rank: (o.i + 30 - e.i) % 30}]->(o), \
                (o)-[:PREFERS {rank: (e.i * 7 + o.i) % 30}]->(e)",
    );
    let res = run_ok(
        &svc,
        "CALL drevo.stableMatching('Mentee', 'Mentor', 'PREFERS', 'rank') \
         YIELD proposer, acceptor RETURN proposer.i AS p, acceptor.i AS a",
    );
    assert_eq!(
        res.rows.len(),
        30,
        "complete lists on equal sides match everyone"
    );
    let pairs: HashMap<i64, i64> = res
        .rows
        .iter()
        .map(|r| match (&r[0], &r[1]) {
            (Value::Integer(p), Value::Integer(a)) => (*p, *a),
            other => panic!("{other:?}"),
        })
        .collect();
    let partner_of_a: HashMap<i64, i64> = pairs.iter().map(|(p, a)| (*a, *p)).collect();
    let e_rank = |e: i64, o: i64| (o + 30 - e) % 30;
    let o_rank = |o: i64, e: i64| (e * 7 + o) % 30;
    for e in 0..30 {
        for o in 0..30 {
            let blocks =
                e_rank(e, o) < e_rank(e, pairs[&e]) && o_rank(o, e) < o_rank(o, partner_of_a[&o]);
            assert!(!blocks, "blocking pair (e{e}, o{o})");
        }
    }
}
