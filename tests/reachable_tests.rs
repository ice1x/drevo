//! `CALL drevo.path.reachable(start, {…}) YIELD node, cost, path` and
//! [`NativeService::reachable`] on a small street grid: every node within a
//! cost budget of one start, for isochrones and for the walk to nearby stops.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use drevo::bolt::packstream::Value as Bolt;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::model::Direction;
use drevo::native_service::NativeService;
use drevo::weighted_path::{EdgeCost, ReachableOptions, WeightedPathOptions};

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

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

/// Six crossings on two streets:
///
/// ```text
///   A ──▶ B ──▶ C        A→B→C is one-way for cars (60 s a block)
///   │     │     │        B–E is slow (90 s), A–D residential (120 s)
///   D ─── E ─── F        a footpath runs A–E (walk only, 100 s)
/// ```
///
/// Two-way streets are a relationship each way. Every street takes 300 s on
/// foot. `C` and `F` are also `:Stop`s. `G` is not connected to anything.
fn sofia_grid() -> NativeService {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CREATE (a:RoadNode {name: 'A'}), (b:RoadNode {name: 'B'}), \
                (c:RoadNode:Stop {name: 'C'}), (d:RoadNode {name: 'D'}), \
                (e:RoadNode {name: 'E'}), (f:RoadNode:Stop {name: 'F'}), \
                (:RoadNode {name: 'G'}), \
         (a)-[:ROAD {car_s: 60, walk_s: 300}]->(b), \
         (b)-[:ROAD {car_s: 60, walk_s: 300}]->(c), \
         (c)-[:ROAD {car_s: 60, walk_s: 300}]->(f), (f)-[:ROAD {car_s: 60, walk_s: 300}]->(c), \
         (b)-[:ROAD {car_s: 90, walk_s: 300}]->(e), (e)-[:ROAD {car_s: 90, walk_s: 300}]->(b), \
         (a)-[:ROAD {car_s: 120, walk_s: 300}]->(d), (d)-[:ROAD {car_s: 120, walk_s: 300}]->(a), \
         (d)-[:ROAD {car_s: 60, walk_s: 300}]->(e), (e)-[:ROAD {car_s: 60, walk_s: 300}]->(d), \
         (e)-[:ROAD {car_s: 60, walk_s: 300}]->(f), (f)-[:ROAD {car_s: 60, walk_s: 300}]->(e), \
         (a)-[:PATH {walk_s: 100}]->(e)",
    );
    svc
}

/// `(name, cost)` of every node reachable from `from`, cheapest first.
fn reach(svc: &NativeService, from: &str, config: &str) -> Vec<(String, f64)> {
    let res = run_ok(
        svc,
        &format!(
            "MATCH (a:RoadNode {{name: '{from}'}}) \
             CALL drevo.path.reachable(a, {config}) YIELD node, cost \
             RETURN node.name AS name, cost"
        ),
    );
    assert_eq!(res.columns, ["name", "cost"]);
    res.rows
        .into_iter()
        .map(|row| match (&row[0], &row[1]) {
            (Value::String(n), Value::Float(c)) => (n.clone(), *c),
            other => panic!("row {other:?}"),
        })
        .collect()
}

fn expect(rows: &[(&str, f64)]) -> Vec<(String, f64)> {
    rows.iter().map(|(n, c)| (n.to_string(), *c)).collect()
}

const WALK: &str = "relTypes: ['ROAD', 'PATH'], direction: 'BOTH', cost: 'walk_s'";
const CAR: &str = "relTypes: ['ROAD'], direction: 'OUTGOING', cost: 'car_s'";

#[test]
fn a_walking_isochrone_lists_every_crossing_within_the_budget() {
    let svc = sofia_grid();
    assert_eq!(
        reach(&svc, "A", &format!("{{{WALK}, maxCost: 450}}")),
        expect(&[
            ("A", 0.0),
            ("E", 100.0),
            ("B", 300.0),
            ("D", 300.0),
            ("F", 400.0)
        ])
    );
}

#[test]
fn incoming_gives_the_cost_from_each_node_to_the_start() {
    // How long each crossing takes to drive to F, over the one-way streets.
    let svc = sofia_grid();
    let incoming = "{relTypes: ['ROAD'], direction: 'INCOMING', cost: 'car_s', maxCost: 1000}";
    let mut got = reach(&svc, "F", incoming);
    got.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    assert_eq!(
        got,
        expect(&[
            ("F", 0.0),
            ("C", 60.0),
            ("E", 60.0),
            ("B", 120.0),
            ("D", 120.0),
            ("A", 180.0),
        ])
    );
}

#[test]
fn targets_yield_only_labelled_nodes_but_the_search_goes_through_the_rest() {
    let svc = sofia_grid();
    assert_eq!(
        reach(
            &svc,
            "A",
            &format!("{{{WALK}, maxCost: 1000, targets: {{label: 'Stop'}}}}")
        ),
        expect(&[("F", 400.0), ("C", 600.0)])
    );
}

#[test]
fn limit_stops_after_that_many_nodes() {
    let svc = sofia_grid();
    assert_eq!(
        reach(&svc, "A", &format!("{{{CAR}, maxCost: 1000, limit: 2}}")),
        expect(&[("A", 0.0), ("B", 60.0)])
    );
    assert_eq!(
        reach(
            &svc,
            "A",
            &format!("{{{WALK}, maxCost: 1000, targets: {{label: 'Stop'}}, limit: 1}}")
        ),
        expect(&[("F", 400.0)])
    );
}

#[test]
fn a_start_with_nowhere_to_go_is_reached_alone() {
    let svc = sofia_grid();
    assert_eq!(
        reach(&svc, "G", &format!("{{{WALK}, maxCost: 900}}")),
        expect(&[("G", 0.0)])
    );
}

#[test]
fn relationships_without_the_cost_are_not_used() {
    // The footpath has no `car_s`, so E is reached by road.
    let svc = sofia_grid();
    let got = reach(
        &svc,
        "A",
        "{relTypes: ['ROAD', 'PATH'], cost: 'car_s', maxCost: 150}",
    );
    assert_eq!(
        got,
        expect(&[
            ("A", 0.0),
            ("B", 60.0),
            ("C", 120.0),
            ("D", 120.0),
            ("E", 150.0)
        ])
    );
}

#[test]
fn every_cost_matches_drevo_path_weighted() {
    let svc = sofia_grid();
    for (from, config) in [("A", WALK), ("A", CAR), ("C", CAR)] {
        let res = run_ok(
            &svc,
            &format!(
                "MATCH (a:RoadNode {{name: '{from}'}}) \
                 CALL drevo.path.reachable(a, {{{config}, maxCost: 1000}}) YIELD node, cost \
                 CALL drevo.path.weighted(a, node, {{{config}}}) YIELD cost AS weighted \
                 RETURN node.name, cost, weighted"
            ),
        );
        assert!(res.rows.len() >= 4, "{from} {config}: {:?}", res.rows);
        for row in &res.rows {
            assert_eq!(row[1], row[2], "{from} {config}: {row:?}");
        }
    }
}

#[test]
fn path_true_yields_the_path_to_each_node() {
    let svc = sofia_grid();
    let res = run_ok(
        &svc,
        &format!(
            "MATCH (a:RoadNode {{name: 'A'}}) \
             CALL drevo.path.reachable(a, {{{WALK}, maxCost: 450, path: true}}) YIELD node, cost, path \
             WHERE node.name = 'F' \
             RETURN [n IN nodes(path) | n.name] AS route, [r IN relationships(path) | type(r)] AS kinds, cost"
        ),
    );
    assert_eq!(
        res.rows,
        vec![vec![
            Value::List(vec![s("A"), s("E"), s("F")]),
            Value::List(vec![s("PATH"), s("ROAD")]),
            Value::Float(400.0),
        ]]
    );
    let without = run_ok(
        &svc,
        &format!(
            "MATCH (a:RoadNode {{name: 'A'}}) \
             CALL drevo.path.reachable(a, {{{WALK}, maxCost: 450}}) YIELD path \
             RETURN path"
        ),
    );
    assert_eq!(without.rows.len(), 5);
    assert!(without.rows.iter().all(|r| r[0] == Value::Null));
}

#[test]
fn one_search_per_input_row() {
    let svc = sofia_grid();
    let res = run_ok(
        &svc,
        &format!(
            "MATCH (a:RoadNode) WHERE a.name IN ['B', 'D'] \
             CALL drevo.path.reachable(a, {{{CAR}, maxCost: 60}}) YIELD node, cost \
             RETURN a.name AS start, node.name AS reached, cost ORDER BY start, cost, reached"
        ),
    );
    assert_eq!(
        res.rows,
        vec![
            vec![s("B"), s("B"), Value::Float(0.0)],
            vec![s("B"), s("C"), Value::Float(60.0)],
            vec![s("D"), s("D"), Value::Float(0.0)],
            vec![s("D"), s("E"), Value::Float(60.0)],
        ]
    );
}

#[test]
fn a_null_start_gives_no_rows() {
    let svc = sofia_grid();
    let res = run_ok(
        &svc,
        "OPTIONAL MATCH (a:RoadNode {name: 'Z'}) \
         CALL drevo.path.reachable(a, {maxCost: 100}) YIELD node RETURN node",
    );
    assert!(res.rows.is_empty());
}

#[test]
fn a_standalone_call_returns_node_cost_and_path() {
    let svc = sofia_grid();
    let res = run_ok(
        &svc,
        "MATCH (a:RoadNode {name: 'G'}) CALL drevo.path.reachable(a, {maxCost: 10})",
    );
    assert_eq!(res.columns, ["node", "cost", "path"]);
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0][1], Value::Float(0.0));
    assert_eq!(res.rows[0][2], Value::Null);
}

fn assert_refused(err: ExecError, needle: &str) {
    let text = err.to_string();
    assert!(
        matches!(err, ExecError::InvalidProcedureCall { .. }),
        "{err:?}"
    );
    assert!(text.contains(needle), "`{text}` should mention `{needle}`");
}

fn search_from_a(config: &str) -> String {
    format!(
        "MATCH (a:RoadNode {{name: 'A'}}) \
         CALL drevo.path.reachable(a, {config}) YIELD node RETURN node"
    )
}

#[test]
fn bad_options_are_refused() {
    let svc = sofia_grid();
    for (config, needle) in [
        ("{}", "maxCost"),
        ("{relTypes: ['ROAD']}", "maxCost"),
        ("{maxCost: -1}", "maxCost"),
        ("{maxCost: 'far'}", "maxCost"),
        ("{maxCost: 10, direction: 'SIDEWAYS'}", "direction"),
        ("{maxCost: 10, relTypes: 'ROAD'}", "relTypes"),
        ("{maxCost: 10, cost: 5}", "cost"),
        ("{maxCost: 10, heuristic: {maxSpeedMps: 1.4}}", "heuristic"),
        ("{maxCost: 10, targets: 'Stop'}", "targets"),
        ("{maxCost: 10, targets: {labl: 'Stop'}}", "labl"),
        ("{maxCost: 10, targets: {label: 5}}", "label"),
        ("{maxCost: 10, limit: -1}", "limit"),
        ("{maxCost: 10, limit: 1.5}", "limit"),
        ("{maxCost: 10, path: 'yes'}", "path"),
        ("{maxCost: 10, relTypez: ['ROAD']}", "relTypez"),
        ("900", "map"),
    ] {
        assert_refused(fails(&svc, &search_from_a(config)), needle);
    }
    assert_refused(
        fails(
            &svc,
            "CALL drevo.path.reachable('A', {maxCost: 10}) YIELD node RETURN node",
        ),
        "node",
    );
}

#[test]
fn a_negative_cost_is_refused() {
    let svc = sofia_grid();
    run_ok(
        &svc,
        "MATCH (b:RoadNode {name: 'B'}), (c:RoadNode {name: 'C'}) CREATE (b)-[:ROAD {car_s: -5}]->(c)",
    );
    assert_refused(
        fails(&svc, &search_from_a(&format!("{{{CAR}, maxCost: 1000}}"))),
        "non-negative",
    );
}

fn node_id(svc: &NativeService, name: &str) -> u64 {
    let res = run_ok(
        svc,
        &format!("MATCH (n:RoadNode {{name: '{name}'}}) RETURN id(n)"),
    );
    match res.rows[0][0] {
        Value::Integer(id) => id as u64,
        ref other => panic!("id {other:?}"),
    }
}

#[test]
fn the_native_api_returns_the_same_nodes() {
    let svc = sofia_grid();
    let a = node_id(&svc, "A");
    let walk = WeightedPathOptions {
        kinds: vec!["ROAD".into(), "PATH".into()],
        direction: Direction::Both,
        cost: EdgeCost::Property("walk_s".into()),
        max_cost: Some(450.0),
        ..WeightedPathOptions::default()
    };
    let found = svc
        .reachable(a, &walk, &ReachableOptions::default(), None)
        .unwrap();
    let got: Vec<(u64, f64)> = found.iter().map(|r| (r.node, r.cost)).collect();
    assert_eq!(
        got,
        vec![
            (a, 0.0),
            (node_id(&svc, "E"), 100.0),
            (node_id(&svc, "B"), 300.0),
            (node_id(&svc, "D"), 300.0),
            (node_id(&svc, "F"), 400.0),
        ]
    );

    let stops = WeightedPathOptions {
        max_cost: Some(1000.0),
        ..walk.clone()
    };
    let paths = ReachableOptions {
        paths: true,
        ..ReachableOptions::default()
    };
    let found = svc.reachable(a, &stops, &paths, Some("Stop")).unwrap();
    let got: Vec<(u64, f64)> = found.iter().map(|r| (r.node, r.cost)).collect();
    assert_eq!(
        got,
        vec![(node_id(&svc, "F"), 400.0), (node_id(&svc, "C"), 600.0)]
    );
    let to_f = found[0].path.as_ref().unwrap();
    assert_eq!(to_f.nodes, vec![a, node_id(&svc, "E"), node_id(&svc, "F")]);

    assert!(svc
        .reachable(999_999, &walk, &ReachableOptions::default(), None)
        .unwrap()
        .is_empty());
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
    let svc = Arc::new(sofia_grid());
    let mut session = Session::new_durable(Arc::clone(&svc));
    session.handle(ClientMessage::Hello {
        extra: dict(vec![]),
    });
    for config in [WALK, CAR] {
        let query = format!(
            "MATCH (a:RoadNode {{name: 'A'}}) \
             CALL drevo.path.reachable(a, {{{config}, maxCost: 1000}}) YIELD node, cost \
             RETURN node.name, cost"
        );
        let embedded: Vec<Vec<Bolt>> = run_ok(&svc, &query)
            .rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|v| match v {
                        Value::String(n) => Bolt::String(n),
                        Value::Float(c) => Bolt::Float(c),
                        other => panic!("{other:?}"),
                    })
                    .collect()
            })
            .collect();
        assert!(embedded.len() >= 5);
        assert_eq!(bolt_rows(&mut session, &query), embedded);
    }
}
