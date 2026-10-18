//! `CALL drevo.path.weighted(start, end, {…}) YIELD path, cost` and
//! [`NativeService::weighted_path`] on a small street grid: the lowest-cost
//! route for a chosen cost property, relationship kinds and direction, with
//! an optional A* bound and a cost budget.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use drevo::bolt::packstream::Value as Bolt;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::model::Direction;
use drevo::native_service::NativeService;
use drevo::weighted_path::{EdgeCost, GeoHeuristic, WeightedPathOptions};

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

fn names(v: &[&str]) -> Value {
    Value::List(v.iter().map(|n| s(n)).collect())
}

/// Six crossings on two streets, ~111 m apart north–south and ~82 m east–west:
///
/// ```text
///   A ──▶ B ──▶ C        A→B→C is one-way for cars (60 s a block)
///   │     │     │        B–E is slow (90 s), A–D residential (120 s)
///   D ─── E ─── F        a footpath runs A–E (walk only, 100 s)
/// ```
///
/// Two-way streets are a relationship each way. Every street takes 300 s on
/// foot. `G` is not connected to anything.
fn sofia_grid() -> NativeService {
    let svc = NativeService::in_memory();
    run_ok(
        &svc,
        "CREATE (a:RoadNode {name: 'A', lat: 42.698, lon: 23.320}), \
                (b:RoadNode {name: 'B', lat: 42.698, lon: 23.321}), \
                (c:RoadNode {name: 'C', lat: 42.698, lon: 23.322}), \
                (d:RoadNode {name: 'D', lat: 42.697, lon: 23.320}), \
                (e:RoadNode {name: 'E', lat: 42.697, lon: 23.321}), \
                (f:RoadNode {name: 'F', lat: 42.697, lon: 23.322}), \
                (:RoadNode {name: 'G', lat: 42.690, lon: 23.300}), \
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

fn route(from: &str, to: &str, config: &str) -> String {
    format!(
        "MATCH (a:RoadNode {{name: '{from}'}}), (b:RoadNode {{name: '{to}'}}) \
         CALL drevo.path.weighted(a, b, {config}) YIELD path, cost \
         RETURN [n IN nodes(path) | n.name] AS route, cost, \
                [r IN relationships(path) | type(r)] AS kinds"
    )
}

const CAR: &str = "{relTypes: ['ROAD'], direction: 'OUTGOING', cost: 'car_s'}";
const WALK: &str = "{relTypes: ['ROAD', 'PATH'], direction: 'BOTH', cost: 'walk_s'}";

#[test]
fn car_and_walk_take_different_routes() {
    let svc = sofia_grid();
    let car = run_ok(&svc, &route("A", "F", CAR));
    assert_eq!(car.columns, ["route", "cost", "kinds"]);
    assert_eq!(
        car.rows,
        vec![vec![
            names(&["A", "B", "C", "F"]),
            Value::Float(180.0),
            names(&["ROAD", "ROAD", "ROAD"]),
        ]]
    );
    let walk = run_ok(&svc, &route("A", "F", WALK));
    assert_eq!(
        walk.rows,
        vec![vec![
            names(&["A", "E", "F"]),
            Value::Float(400.0),
            names(&["PATH", "ROAD"]),
        ]]
    );
}

#[test]
fn cars_keep_to_one_way_streets() {
    // C → B and B → A do not exist, so the car goes round by F, E and D.
    let svc = sofia_grid();
    let back = run_ok(&svc, &route("C", "A", CAR));
    assert_eq!(back.rows[0][0], names(&["C", "F", "E", "D", "A"]));
    assert_eq!(back.rows[0][1], Value::Float(300.0));
}

#[test]
fn a_star_finds_the_same_routes() {
    let svc = sofia_grid();
    for (from, to, config, a_star) in [
        (
            "A",
            "F",
            CAR,
            "{relTypes: ['ROAD'], cost: 'car_s', heuristic: {lat: 'lat', lon: 'lon', maxSpeedMps: 30}}",
        ),
        (
            "A",
            "F",
            WALK,
            "{relTypes: ['ROAD', 'PATH'], direction: 'BOTH', cost: 'walk_s', heuristic: {maxSpeedMps: 1.4}}",
        ),
        (
            "C",
            "A",
            CAR,
            "{relTypes: ['ROAD'], cost: 'car_s', heuristic: {maxSpeedMps: 30}}",
        ),
    ] {
        assert_eq!(
            run_ok(&svc, &route(from, to, a_star)).rows,
            run_ok(&svc, &route(from, to, config)).rows,
            "{from} -> {to} with {a_star}"
        );
    }
}

#[test]
fn an_unreachable_target_gives_no_rows() {
    let svc = sofia_grid();
    assert!(run_ok(&svc, &route("A", "G", CAR)).rows.is_empty());
    assert!(run_ok(&svc, &route("A", "G", WALK)).rows.is_empty());
}

#[test]
fn max_cost_cuts_the_search_off() {
    let svc = sofia_grid();
    let within = "{relTypes: ['ROAD', 'PATH'], direction: 'BOTH', cost: 'walk_s', maxCost: 400}";
    assert_eq!(run_ok(&svc, &route("A", "F", within)).rows.len(), 1);
    let over = "{relTypes: ['ROAD', 'PATH'], direction: 'BOTH', cost: 'walk_s', maxCost: 399}";
    assert!(run_ok(&svc, &route("A", "F", over)).rows.is_empty());
}

#[test]
fn relationships_without_the_cost_are_not_used() {
    // The footpath has no `car_s`, so even with PATH allowed the car can't
    // take it.
    let svc = sofia_grid();
    let car_any = "{relTypes: ['ROAD', 'PATH'], cost: 'car_s'}";
    assert_eq!(
        run_ok(&svc, &route("A", "F", car_any)).rows[0][0],
        names(&["A", "B", "C", "F"])
    );
}

#[test]
fn one_route_per_input_row() {
    let svc = sofia_grid();
    let res = run_ok(
        &svc,
        "MATCH (a:RoadNode {name: 'A'}), (b:RoadNode) WHERE b.name IN ['C', 'E', 'F', 'G'] \
         CALL drevo.path.weighted(a, b, {relTypes: ['ROAD'], cost: 'car_s'}) YIELD cost \
         RETURN b.name AS to, cost ORDER BY to",
    );
    assert_eq!(
        res.rows,
        vec![
            vec![s("C"), Value::Float(120.0)],
            vec![s("E"), Value::Float(150.0)],
            vec![s("F"), Value::Float(180.0)],
        ]
    );
}

#[test]
fn the_path_is_a_real_path_value() {
    let svc = sofia_grid();
    let res = run_ok(
        &svc,
        "MATCH (a:RoadNode {name: 'A'}), (b:RoadNode {name: 'F'}) \
         CALL drevo.path.weighted(a, b, {relTypes: ['ROAD', 'PATH'], direction: 'BOTH', cost: 'walk_s'}) \
         YIELD path WHERE length(path) = 2 \
         RETURN startNode(relationships(path)[0]).name AS s, \
                reduce(t = 0, r IN relationships(path) | t + r.walk_s) AS total",
    );
    assert_eq!(res.rows, vec![vec![s("A"), Value::Integer(400)]]);
}

#[test]
fn without_a_cost_property_each_relationship_costs_its_weight() {
    // Relationships created through Cypher have weight 1, so the cheapest
    // path is the one with the fewest hops.
    let svc = sofia_grid();
    let res = run_ok(&svc, &route("A", "F", "{relTypes: ['ROAD', 'PATH']}"));
    assert_eq!(res.rows[0][0], names(&["A", "E", "F"]));
    assert_eq!(res.rows[0][1], Value::Float(2.0));
}

#[test]
fn a_standalone_call_returns_path_and_cost() {
    let svc = sofia_grid();
    let res = run_ok(
        &svc,
        "MATCH (a:RoadNode {name: 'A'}), (b:RoadNode {name: 'B'}) \
         CALL drevo.path.weighted(a, b, {cost: 'car_s'}) YIELD path, cost RETURN length(path), cost",
    );
    assert_eq!(res.rows, vec![vec![Value::Integer(1), Value::Float(60.0)]]);
}

fn assert_refused(err: ExecError, needle: &str) {
    let text = err.to_string();
    assert!(
        matches!(err, ExecError::InvalidProcedureCall { .. }),
        "{err:?}"
    );
    assert!(text.contains(needle), "`{text}` should mention `{needle}`");
}

#[test]
fn a_negative_cost_is_refused() {
    let svc = sofia_grid();
    run_ok(
        &svc,
        "MATCH (b:RoadNode {name: 'B'}), (c:RoadNode {name: 'C'}) CREATE (b)-[:ROAD {car_s: -5}]->(c)",
    );
    assert_refused(fails(&svc, &route("A", "F", CAR)), "non-negative");
}

#[test]
fn bad_options_are_refused() {
    let svc = sofia_grid();
    for (config, needle) in [
        ("{direction: 'SIDEWAYS'}", "direction"),
        ("{relTypes: 'ROAD'}", "relTypes"),
        ("{cost: 5}", "cost"),
        ("{maxCost: -1}", "maxCost"),
        ("{heuristic: {lat: 'lat'}}", "maxSpeedMps"),
        ("{heuristic: {maxSpeedMps: 0}}", "maxSpeedMps"),
        ("{relTypez: ['ROAD']}", "relTypez"),
        ("'car_s'", "map"),
    ] {
        assert_refused(fails(&svc, &route("A", "F", config)), needle);
    }
    assert_refused(
        fails(
            &svc,
            "CALL drevo.path.weighted('A', 'F', {}) YIELD cost RETURN cost",
        ),
        "node",
    );
}

#[test]
fn null_endpoints_give_no_rows() {
    let svc = sofia_grid();
    let res = run_ok(
        &svc,
        "MATCH (a:RoadNode {name: 'A'}) OPTIONAL MATCH (b:RoadNode {name: 'Z'}) \
         CALL drevo.path.weighted(a, b, {}) YIELD cost RETURN cost",
    );
    assert!(res.rows.is_empty());
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
fn the_native_api_returns_nodes_relationships_and_cost() {
    let svc = sofia_grid();
    let (a, f) = (node_id(&svc, "A"), node_id(&svc, "F"));
    let walk = WeightedPathOptions {
        kinds: vec!["ROAD".into(), "PATH".into()],
        direction: Direction::Both,
        cost: EdgeCost::Property("walk_s".into()),
        ..WeightedPathOptions::default()
    };
    let path = svc.weighted_path(a, f, &walk).unwrap().unwrap();
    assert_eq!(path.cost, 400.0);
    assert_eq!(path.nodes, vec![a, node_id(&svc, "E"), f]);
    assert_eq!(path.edges.len(), 2);

    let a_star = WeightedPathOptions {
        heuristic: Some(GeoHeuristic {
            lat: "lat".into(),
            lon: "lon".into(),
            max_speed_mps: 1.4,
        }),
        ..walk.clone()
    };
    // E–F is two equal one-way relationships, so either may be chosen.
    let guided = svc.weighted_path(a, f, &a_star).unwrap().unwrap();
    assert_eq!((guided.nodes, guided.cost), (path.nodes, path.cost));

    let g = node_id(&svc, "G");
    assert_eq!(svc.weighted_path(a, g, &walk).unwrap(), None);
    assert_eq!(svc.weighted_path(a, 999_999, &walk).unwrap(), None);
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
    for config in [CAR, WALK] {
        let embedded = run_ok(&svc, &route("A", "F", config));
        let bolt = bolt_rows(&mut session, &route("A", "F", config));
        assert_eq!(bolt.len(), 1);
        let Value::Float(cost) = embedded.rows[0][1] else {
            panic!("cost {:?}", embedded.rows[0][1]);
        };
        assert_eq!(bolt[0][1], Bolt::Float(cost));
        let Value::List(route) = &embedded.rows[0][0] else {
            panic!("route");
        };
        let expected: Vec<Bolt> = route
            .iter()
            .map(|n| match n {
                Value::String(n) => Bolt::String(n.clone()),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(bolt[0][0], Bolt::List(expected));
    }
}
