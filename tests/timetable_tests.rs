//! `CALL drevo.path.timetable(starts, ends, {…}) YIELD arriveAt, departAt,
//! transfers, legs` on small hand-made timetables: the earliest arrival over
//! trips, transfers and footpaths, one journey per number of transfers.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use drevo::bolt::packstream::Value as Bolt;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

fn run_ok(svc: &NativeService, q: &str) -> ExecResult {
    svc.execute(&parse(q).expect("parse"), HashMap::new())
        .unwrap_or_else(|e| panic!("execute `{q}`: {e:?}"))
}

fn fails(svc: &NativeService, q: &str) -> ExecError {
    match svc.execute(&parse(q).expect("parse"), HashMap::new()) {
        Ok(r) => panic!("`{q}` should fail, got {:?}", r.rows),
        Err(e) => e,
    }
}

/// Bus 1 runs A → B → C (trips 11 and 12), tram 2 runs C → D (trips 21, 22,
/// 23). Times are seconds after midnight; every trip runs on service 1.
///
/// ```text
///   trip 11  A 1000 → B 1100/1110 → C 1200        tram 21  C 1230 → D 1330
///   trip 12  A 2000 → B 2100/2110 → C 2200        tram 22  C 1300 → D 1400
///                                                 tram 23  C 2300 → D 2400
/// ```
///
/// `G` is served by nothing.
const CITY: &str = "CREATE (a:Stop {name: 'A'}), (b:Stop {name: 'B'}), (c:Stop {name: 'C'}), \
       (d:Stop {name: 'D'}), (e:Stop {name: 'E'}), (:Stop {name: 'G'}), \
       (a)-[:HOP {dep: [1000, 2000], arr: [1100, 2100], trip: [11, 12], svc: [1, 1]}]->(b), \
       (b)-[:HOP {dep: [1110, 2110], arr: [1200, 2200], trip: [11, 12], svc: [1, 1]}]->(c), \
       (c)-[:HOP {dep: [1230, 1300, 2300], arr: [1330, 1400, 2400], trip: [21, 22, 23], svc: [1, 1, 1]}]->(d)";

fn city() -> NativeService {
    let svc = NativeService::in_memory();
    run_ok(&svc, CITY);
    svc
}

/// The options map, starting from leaving at 900 on service 1 with a 60 s
/// transfer, with `overrides` replacing or adding entries.
fn opts(overrides: &[(&str, &str)]) -> String {
    let mut map: BTreeMap<&str, String> = BTreeMap::from([
        ("departAt", "900".to_string()),
        ("days", "[{offset: 0, services: [1]}]".to_string()),
        (
            "hops",
            "{relType: 'HOP', dep: 'dep', arr: 'arr', trip: 'trip', service: 'svc'}".to_string(),
        ),
        ("walk", "{relTypes: ['WALK'], cost: 's'}".to_string()),
        ("minTransfer", "60".to_string()),
        ("maxTransfers", "4".to_string()),
        ("maxDuration", "7200".to_string()),
    ]);
    for (k, v) in overrides {
        map.insert(k, v.to_string());
    }
    let body: Vec<String> = map.iter().map(|(k, v)| format!("{k}: {v}")).collect();
    format!("{{{}}}", body.join(", "))
}

fn endpoints(list: &[(&str, i64)]) -> String {
    let items: Vec<String> = list
        .iter()
        .map(|(name, cost)| {
            format!("{{node: [x IN stops WHERE x.name = '{name}'][0], cost: {cost}}}")
        })
        .collect();
    format!("[{}]", items.join(", "))
}

fn query(starts: &[(&str, i64)], ends: &[(&str, i64)], options: &str, ret: &str) -> String {
    format!(
        "MATCH (x:Stop) WITH collect(x) AS stops \
         CALL drevo.path.timetable({}, {}, {options}) \
         YIELD arriveAt, departAt, transfers, legs RETURN {ret}",
        endpoints(starts),
        endpoints(ends),
    )
}

/// A leg as `mode from>to #trip`, `-` for no stop.
const LEG: &str = "[l IN legs | l.mode + ' ' + coalesce(l.from.name, '-') + '>' + \
                   coalesce(l.to.name, '-') + coalesce(' #' + toString(l.trip), '')]";

type Row = (i64, i64, i64, Vec<String>);

fn int(v: &Value) -> i64 {
    match v {
        Value::Integer(i) => *i,
        other => panic!("expected an integer, got {other:?}"),
    }
}

fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::List(items) => items
            .iter()
            .map(|i| match i {
                Value::String(s) => s.clone(),
                other => panic!("expected a string, got {other:?}"),
            })
            .collect(),
        other => panic!("expected a list, got {other:?}"),
    }
}

/// `(arriveAt, departAt, transfers, legs)` per journey, in the order returned.
fn plan(
    svc: &NativeService,
    starts: &[(&str, i64)],
    ends: &[(&str, i64)],
    options: &str,
) -> Vec<Row> {
    let q = query(
        starts,
        ends,
        options,
        &format!("arriveAt, departAt, transfers, {LEG} AS legs"),
    );
    run_ok(svc, &q)
        .rows
        .iter()
        .map(|r| (int(&r[0]), int(&r[1]), int(&r[2]), strings(&r[3])))
        .collect()
}

fn legs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn a_forced_transfer_arrives_earliest() {
    let svc = city();
    assert_eq!(
        plan(&svc, &[("A", 0)], &[("D", 0)], &opts(&[])),
        vec![(
            1400,
            1000,
            1,
            legs(&["access ->A", "ride A>C #11", "ride C>D #22", "egress D>-"])
        )],
        "trip 11 reaches C at 1200; with a 60 s transfer tram 21 (1230) is missed, tram 22 (1300) is caught",
    );
}

#[test]
fn staying_on_the_same_trip_is_not_a_transfer() {
    let svc = city();
    assert_eq!(
        plan(&svc, &[("A", 0)], &[("C", 0)], &opts(&[])),
        vec![(
            1200,
            1000,
            0,
            legs(&["access ->A", "ride A>C #11", "egress C>-"])
        )],
    );
}

#[test]
fn the_minimum_transfer_time_makes_a_tight_connection_miss() {
    let svc = city();
    let tight = plan(
        &svc,
        &[("A", 0)],
        &[("D", 0)],
        &opts(&[("minTransfer", "0")]),
    );
    assert_eq!(tight[0].0, 1330, "{tight:?}");
    assert!(
        tight[0].3.contains(&"ride C>D #21".to_string()),
        "{tight:?}"
    );
    let slack = plan(
        &svc,
        &[("A", 0)],
        &[("D", 0)],
        &opts(&[("minTransfer", "60")]),
    );
    assert_eq!(slack[0].0, 1400, "{slack:?}");
}

#[test]
fn a_footpath_between_rides() {
    let svc = city();
    run_ok(
        &svc,
        "MATCH (c:Stop {name: 'C'}), (e:Stop {name: 'E'}), (d:Stop {name: 'D'}) \
         CREATE (c)-[:WALK {s: 120}]->(e), \
                (e)-[:HOP {dep: [1330], arr: [1380], trip: [31], svc: [1]}]->(d)",
    );
    assert_eq!(
        plan(&svc, &[("A", 0)], &[("D", 0)], &opts(&[])),
        vec![(
            1380,
            1000,
            1,
            legs(&[
                "access ->A",
                "ride A>C #11",
                "walk C>E",
                "ride E>D #31",
                "egress D>-"
            ])
        )],
        "walking needs no transfer time: C 1200 + 120 s = E 1320, trip 31 leaves at 1330",
    );
    let without_walking = plan(&svc, &[("A", 0)], &[("D", 0)], &opts(&[("walk", "null")]));
    assert_eq!(without_walking[0].0, 1400, "{without_walking:?}");
}

#[test]
fn a_trip_whose_service_does_not_run_is_ignored() {
    let svc = city();
    run_ok(
        &svc,
        "MATCH (:Stop {name: 'C'})-[h:HOP]->() SET h.svc = [1, 2, 1]",
    );
    assert_eq!(
        plan(&svc, &[("A", 0)], &[("D", 0)], &opts(&[])),
        vec![(
            2400,
            1000,
            1,
            legs(&["access ->A", "ride A>C #11", "ride C>D #23", "egress D>-"])
        )],
        "tram 22 runs on service 2 only",
    );
    let both = plan(
        &svc,
        &[("A", 0)],
        &[("D", 0)],
        &opts(&[("days", "[{offset: 0, services: [1, 2]}]")]),
    );
    assert_eq!(both[0].0, 1400, "{both:?}");
}

#[test]
fn a_trip_of_the_previous_service_day_after_midnight() {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CREATE (:Stop {name: 'N1'})-[:HOP {dep: [87000], arr: [87600], trip: [91], svc: [7]}]->(:Stop {name: 'N2'})",
    );
    // Yesterday's night bus leaves N1 at 24:10 (87000 s), 00:10 today.
    let with_yesterday = opts(&[
        ("departAt", "300"),
        (
            "days",
            "[{offset: 0, services: [1]}, {offset: -86400, services: [7]}]",
        ),
    ]);
    assert_eq!(
        plan(&svc, &[("N1", 0)], &[("N2", 0)], &with_yesterday),
        vec![(
            1200,
            600,
            0,
            legs(&["access ->N1", "ride N1>N2 #91", "egress N2>-"])
        )],
    );
    let today_only = opts(&[("departAt", "300")]);
    assert_eq!(plan(&svc, &[("N1", 0)], &[("N2", 0)], &today_only), vec![]);
}

#[test]
fn max_transfers_cuts_journeys_that_need_more() {
    let svc = city();
    assert_eq!(
        plan(
            &svc,
            &[("A", 0)],
            &[("D", 0)],
            &opts(&[("maxTransfers", "0")])
        ),
        vec![]
    );
    assert_eq!(
        plan(
            &svc,
            &[("A", 0)],
            &[("D", 0)],
            &opts(&[("maxTransfers", "1")])
        )
        .len(),
        1
    );
}

#[test]
fn nothing_arrives_gives_no_rows() {
    let svc = city();
    assert_eq!(plan(&svc, &[("A", 0)], &[("G", 0)], &opts(&[])), vec![]);
    assert_eq!(
        plan(
            &svc,
            &[("A", 0)],
            &[("D", 0)],
            &opts(&[("maxDuration", "400")])
        ),
        vec![],
        "the earliest arrival, 1400, is 500 s after leaving at 900",
    );
    assert_eq!(
        plan(&svc, &[("D", 0)], &[("A", 0)], &opts(&[])),
        vec![],
        "hops are followed in their direction only",
    );
}

#[test]
fn faster_journeys_with_more_transfers_come_back_too() {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CREATE (p1:Stop {name: 'P1'}), (p2:Stop {name: 'P2'}), (p3:Stop {name: 'P3'}), (p4:Stop {name: 'P4'}), \
         (p1)-[:HOP {dep: [1000], arr: [3000], trip: [51], svc: [1]}]->(p4), \
         (p1)-[:HOP {dep: [1000], arr: [1100], trip: [61], svc: [1]}]->(p2), \
         (p2)-[:HOP {dep: [1200], arr: [1300], trip: [62], svc: [1]}]->(p3), \
         (p3)-[:HOP {dep: [1400], arr: [1500], trip: [63], svc: [1]}]->(p4), \
         (p2)-[:HOP {dep: [1200], arr: [2500], trip: [64], svc: [1]}]->(p4)",
    );
    let rows = plan(&svc, &[("P1", 0)], &[("P4", 0)], &opts(&[]));
    let summary: Vec<(i64, i64)> = rows.iter().map(|r| (r.0, r.2)).collect();
    assert_eq!(summary, vec![(1500, 2), (2500, 1), (3000, 0)], "{rows:?}");
    assert_eq!(
        rows[0].3,
        legs(&[
            "access ->P1",
            "ride P1>P2 #61",
            "ride P2>P3 #62",
            "ride P3>P4 #63",
            "egress P4>-"
        ])
    );
    assert_eq!(
        rows[2].3,
        legs(&["access ->P1", "ride P1>P4 #51", "egress P4>-"])
    );
}

#[test]
fn the_walk_to_the_first_stop_sets_when_to_leave() {
    let svc = city();
    let q = query(
        &[("A", 120)],
        &[("C", 30)],
        &opts(&[("departAt", "600")]),
        "arriveAt, departAt, [l IN legs | [l.mode, l.depart, l.arrive]] AS times",
    );
    let rows = run_ok(&svc, &q).rows;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((int(&rows[0][0]), int(&rows[0][1])), (1230, 880));
    let Value::List(times) = &rows[0][2] else {
        panic!("{rows:?}")
    };
    let times: Vec<(String, i64, i64)> = times
        .iter()
        .map(|t| match t {
            Value::List(t) => (
                strings(&Value::List(vec![t[0].clone()]))[0].clone(),
                int(&t[1]),
                int(&t[2]),
            ),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        times,
        vec![
            ("access".to_string(), 880, 1000),
            ("ride".to_string(), 1000, 1200),
            ("egress".to_string(), 1200, 1230),
        ]
    );
}

#[test]
fn the_best_of_several_start_and_end_stops() {
    let svc = city();
    let rows = plan(
        &svc,
        &[("A", 0), ("B", 300)],
        &[("D", 60), ("C", 500)],
        &opts(&[]),
    );
    assert_eq!(
        rows,
        vec![
            (
                1460,
                1000,
                1,
                legs(&["access ->A", "ride A>C #11", "ride C>D #22", "egress D>-"])
            ),
            (
                1700,
                1000,
                0,
                legs(&["access ->A", "ride A>C #11", "egress C>-"])
            ),
        ],
    );
}

#[test]
fn a_walk_only_journey_needs_no_ride() {
    let svc = city();
    assert_eq!(
        plan(&svc, &[("A", 100)], &[("A", 50)], &opts(&[])),
        vec![(1050, 900, 0, legs(&["access ->A", "egress A>-"]))],
    );
}

#[test]
fn a_ride_lists_its_hops_as_relationships() {
    let svc = city();
    let q = query(
        &[("A", 0)],
        &[("C", 0)],
        &opts(&[]),
        "[l IN legs WHERE l.mode = 'ride' | [size(l.hops), type(l.hops[0]), startNode(l.hops[0]).name, endNode(l.hops[1]).name]] AS rides",
    );
    let rows = run_ok(&svc, &q).rows;
    assert_eq!(
        rows[0][0],
        Value::List(vec![Value::List(vec![
            Value::Integer(2),
            Value::String("HOP".into()),
            Value::String("A".into()),
            Value::String("C".into()),
        ])])
    );
}

#[test]
fn one_plan_per_input_row() {
    let svc = city();
    let rows = run_ok(
        &svc,
        "UNWIND ['C', 'D'] AS dest \
         MATCH (a:Stop {name: 'A'}), (b:Stop {name: dest}) \
         CALL drevo.path.timetable([{node: a, cost: 0}], [{node: b, cost: 0}], \
              {departAt: 900, days: [{offset: 0, services: [1]}], minTransfer: 60}) \
         YIELD arriveAt RETURN dest, arriveAt",
    )
    .rows;
    assert_eq!(
        rows,
        vec![
            vec![Value::String("C".into()), Value::Integer(1200)],
            vec![Value::String("D".into()), Value::Integer(1400)],
        ],
        "hops default to HOP with dep/arr/trip/svc",
    );
}

#[test]
fn a_standalone_call_returns_all_columns() {
    let svc = city();
    let r = run_ok(
        &svc,
        "CALL drevo.path.timetable([], [], {departAt: 0, days: []})",
    );
    assert_eq!(r.columns, vec!["arriveAt", "departAt", "transfers", "legs"]);
    assert!(r.rows.is_empty());
}

fn assert_refused(err: ExecError, needle: &str) {
    let text = err.to_string();
    assert!(
        text.contains("drevo.path.timetable") && text.contains(needle),
        "expected a refusal mentioning `{needle}`, got: {text}"
    );
}

#[test]
fn bad_options_are_refused() {
    let svc = city();
    let call = |starts: &str, options: &str| {
        format!(
            "MATCH (a:Stop {{name: 'A'}}), (d:Stop {{name: 'D'}}) \
             CALL drevo.path.timetable({starts}, [{{node: d, cost: 0}}], {options}) YIELD arriveAt RETURN arriveAt"
        )
    };
    let start = "[{node: a, cost: 0}]";
    assert_refused(fails(&svc, &call(start, "{days: []}")), "departAt");
    assert_refused(fails(&svc, &call(start, "{departAt: 900}")), "days");
    assert_refused(
        fails(&svc, &call(start, "{departAt: 900, days: [{offset: 0}]}")),
        "services",
    );
    assert_refused(
        fails(&svc, &call(start, "{departAt: 900, days: [], speed: 3}")),
        "speed",
    );
    assert_refused(
        fails(
            &svc,
            &call(start, "{departAt: 900, days: [], maxTransfers: -1}"),
        ),
        "maxTransfers",
    );
    assert_refused(
        fails(
            &svc,
            &call(start, "{departAt: 900, days: [], minTransfer: 'soon'}"),
        ),
        "minTransfer",
    );
    assert_refused(
        fails(
            &svc,
            &call(start, "{departAt: 900, days: [], hops: {colour: 'x'}}"),
        ),
        "colour",
    );
    assert_refused(
        fails(
            &svc,
            &call(
                start,
                "{departAt: 900, days: [], walk: {relTypes: ['WALK']}}",
            ),
        ),
        "cost",
    );
    assert_refused(
        fails(&svc, &call("[a]", "{departAt: 900, days: []}")),
        "node",
    );
    assert_refused(fails(&svc, &call("a", "{departAt: 900, days: []}")), "list");
    assert_refused(
        fails(
            &svc,
            &call("[{node: a, cost: -5}]", "{departAt: 900, days: []}"),
        ),
        "cost",
    );
}

#[test]
fn a_malformed_timetable_is_refused() {
    let svc = city();
    run_ok(
        &svc,
        "MATCH (:Stop {name: 'A'})-[h:HOP]->() SET h.arr = [1100]",
    );
    let q = query(&[("A", 0)], &[("D", 0)], &opts(&[]), "arriveAt");
    assert_refused(fails(&svc, &q), "dep");
}

fn dict(e: Vec<(&str, Bolt)>) -> BTreeMap<String, Bolt> {
    e.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

/// The rows of `query` run over a Bolt session.
fn bolt_rows(session: &mut Session, query: &str) -> Vec<Vec<Bolt>> {
    let run = session.handle(ClientMessage::Run {
        query: query.to_string(),
        parameters: dict(vec![]),
        extra: dict(vec![]),
    });
    assert!(
        matches!(run.as_slice(), [ServerMessage::Success { .. }]),
        "{run:?}"
    );
    session
        .handle(ClientMessage::Pull {
            extra: dict(vec![("n", Bolt::Integer(-1))]),
        })
        .into_iter()
        .filter_map(|m| match m {
            ServerMessage::Record { fields } => Some(fields),
            _ => None,
        })
        .collect()
}

#[test]
fn bolt_returns_what_embedded_returns() {
    let svc = Arc::new(city());
    let mut session = Session::new_durable(Arc::clone(&svc));
    session.handle(ClientMessage::Hello {
        extra: dict(vec![]),
    });
    let q = query(
        &[("A", 0)],
        &[("D", 0)],
        &opts(&[]),
        &format!(
            "arriveAt, departAt, transfers, {LEG} AS legs, [l IN legs | size(l.hops)] AS hops"
        ),
    );
    let embedded = run_ok(&svc, &q).rows;
    let bolt = bolt_rows(&mut session, &q);
    assert_eq!(bolt.len(), embedded.len());
    assert_eq!(bolt.len(), 1);
    let to_bolt = |v: &Value| -> Bolt {
        match v {
            Value::Integer(i) => Bolt::Integer(*i),
            Value::String(s) => Bolt::String(s.clone()),
            Value::List(items) => Bolt::List(
                items
                    .iter()
                    .map(|i| match i {
                        Value::Integer(i) => Bolt::Integer(*i),
                        Value::String(s) => Bolt::String(s.clone()),
                        other => panic!("{other:?}"),
                    })
                    .collect(),
            ),
            other => panic!("{other:?}"),
        }
    };
    let expected: Vec<Bolt> = embedded[0].iter().map(to_bolt).collect();
    assert_eq!(bolt[0], expected);
    assert_eq!(bolt[0][0], Bolt::Integer(1400));
}

fn node_id(svc: &NativeService, name: &str) -> u64 {
    let rows = run_ok(
        svc,
        &format!("MATCH (s:Stop {{name: '{name}'}}) RETURN id(s)"),
    )
    .rows;
    match rows[0][0] {
        Value::Integer(id) => id as u64,
        ref other => panic!("{other:?}"),
    }
}

#[test]
fn the_native_api_returns_the_same_journeys() {
    use drevo::timetable::{Endpoint, LegMode, ServiceDay, TimetableOptions};
    let svc = city();
    let (a, c, d) = (node_id(&svc, "A"), node_id(&svc, "C"), node_id(&svc, "D"));
    let options = TimetableOptions {
        depart_at: 900,
        days: vec![ServiceDay {
            offset: 0,
            services: vec![1],
        }],
        min_transfer: 60,
        ..TimetableOptions::default()
    };
    let journeys = svc
        .timetable(
            &[Endpoint { node: a, cost: 0 }],
            &[Endpoint { node: d, cost: 0 }],
            &options,
        )
        .expect("plan");
    assert_eq!(journeys.len(), 1, "{journeys:?}");
    let j = &journeys[0];
    assert_eq!((j.arrive_at, j.depart_at, j.transfers), (1400, 1000, 1));
    let modes: Vec<LegMode> = j.legs.iter().map(|l| l.mode).collect();
    assert_eq!(
        modes,
        vec![
            LegMode::Access,
            LegMode::Ride,
            LegMode::Ride,
            LegMode::Egress
        ]
    );
    assert_eq!((j.legs[1].trip, j.legs[2].trip), (Some(11), Some(22)));
    assert_eq!((j.legs[1].from, j.legs[1].to), (Some(a), Some(c)));
    assert_eq!(j.legs[1].hops.len(), 2);
}
