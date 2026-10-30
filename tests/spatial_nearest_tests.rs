//! `CALL drevo.spatial.nearest(label, {lat, lon}, {…}) YIELD node, meters`,
//! `CREATE POINT INDEX` and [`NativeService::nearest`]: the nodes nearest to a
//! latitude/longitude, from a point index that follows writes or, without one,
//! from a scan.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use drevo::bolt::packstream::Value as Bolt;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native_point_index::haversine_m;
use drevo::native_service::{NativeService, NearestOptions};

static NEXT: AtomicU64 = AtomicU64::new(0);

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

/// Sofia city centre.
const CENTRE: (f64, f64) = (42.6977, 23.3219);
const OPERA: (f64, f64) = (42.6935, 23.3290);
const LIONS: (f64, f64) = (42.7075, 23.3216);
const AIRPORT: (f64, f64) = (42.6952, 23.4062);

/// Four junctions in Sofia: a footway crossing at the centre (`r: 0`) and
/// three drivable ones (`r: 1`) joined by roads, plus a junction with no
/// position and a shop at the centre (not a junction).
fn sofia(indexed: bool) -> NativeService {
    let svc = NativeService::in_memory();
    if indexed {
        run_ok(
            &svc,
            "CREATE POINT INDEX junctions FOR (j:Junction) ON (j.lat, j.lon)",
        );
    }
    run_ok(
        &svc,
        &format!(
            "CREATE (:Junction {{name: 'centre', r: 0, lat: {}, lon: {}}}), \
                    (o:Junction {{name: 'opera', r: 1, lat: {}, lon: {}}}), \
                    (l:Junction {{name: 'lions', r: 1, lat: {}, lon: {}}}), \
                    (a:Junction {{name: 'airport', r: 1, lat: {}, lon: {}}}), \
                    (:Junction {{name: 'nowhere', r: 1}}), \
                    (:Shop {{name: 'shop', lat: {}, lon: {}}}), \
             (o)-[:ROAD {{car_s: 120}}]->(l), (l)-[:ROAD {{car_s: 120}}]->(o), \
             (l)-[:ROAD {{car_s: 600}}]->(a), (a)-[:ROAD {{car_s: 600}}]->(l)",
            CENTRE.0,
            CENTRE.1,
            OPERA.0,
            OPERA.1,
            LIONS.0,
            LIONS.1,
            AIRPORT.0,
            AIRPORT.1,
            CENTRE.0,
            CENTRE.1,
        ),
    );
    svc
}

/// `(name, metres)` of the junctions `drevo.spatial.nearest` returns.
fn near(svc: &NativeService, from: (f64, f64), options: &str) -> Vec<(String, f64)> {
    let options = if options.is_empty() {
        String::new()
    } else {
        format!(", {options}")
    };
    let res = run_ok(
        svc,
        &format!(
            "CALL drevo.spatial.nearest('Junction', {{lat: {}, lon: {}}}{options}) \
             YIELD node, meters RETURN node.name AS name, meters",
            from.0, from.1
        ),
    );
    assert_eq!(res.columns, ["name", "meters"]);
    res.rows
        .into_iter()
        .map(|row| match (&row[0], &row[1]) {
            (Value::String(n), Value::Float(m)) => (n.clone(), *m),
            other => panic!("row {other:?}"),
        })
        .collect()
}

fn names(rows: &[(String, f64)]) -> Vec<&str> {
    rows.iter().map(|(n, _)| n.as_str()).collect()
}

#[test]
fn the_nearest_junction_by_default() {
    for indexed in [true, false] {
        let svc = sofia(indexed);
        let rows = near(&svc, CENTRE, "");
        assert_eq!(names(&rows), ["centre"], "indexed {indexed}");
        assert!(rows[0].1.abs() < 1e-6, "{rows:?}");
    }
}

#[test]
fn k_nearest_first_with_distances_in_metres() {
    for indexed in [true, false] {
        let svc = sofia(indexed);
        let rows = near(&svc, CENTRE, "{k: 3}");
        assert_eq!(
            names(&rows),
            ["centre", "opera", "lions"],
            "indexed {indexed}"
        );
        assert!((rows[1].1 - haversine_m(CENTRE, OPERA)).abs() < 1e-6);
        assert!((rows[2].1 - haversine_m(CENTRE, LIONS)).abs() < 1e-6);
        // Nodes without a position and other labels are never returned.
        assert_eq!(near(&svc, CENTRE, "{k: 10}").len(), 4);
    }
}

#[test]
fn max_meters_bounds_the_search() {
    for indexed in [true, false] {
        let svc = sofia(indexed);
        assert_eq!(
            names(&near(&svc, CENTRE, "{k: 10, maxMeters: 2000}")),
            ["centre", "opera", "lions"]
        );
        // Out in the Vitosha foothills, nothing within 300 m.
        assert!(near(&svc, (42.60, 23.28), "{maxMeters: 300}").is_empty());
    }
}

#[test]
fn where_keeps_only_matching_nodes() {
    for indexed in [true, false] {
        let svc = sofia(indexed);
        assert_eq!(names(&near(&svc, CENTRE, "{where: {r: 1}}")), ["opera"]);
        assert_eq!(
            names(&near(
                &svc,
                CENTRE,
                "{k: 5, where: {r: 1.0, name: 'lions'}}"
            )),
            ["lions"]
        );
        assert!(near(&svc, CENTRE, "{where: {r: 2}}").is_empty());
    }
}

#[test]
fn other_position_properties_are_read_by_name() {
    let svc = NativeService::in_memory();
    run_ok(&svc, "CREATE POINT INDEX FOR (s:Stop) ON (s.y, s.x)");
    run_ok(
        &svc,
        "CREATE (:Stop {name: 'Opera', y: 42.6935, x: 23.3290}), \
                (:Stop {name: 'Airport', y: 42.6952, x: 23.4062}), \
                (:Stop {name: 'Lat-lon only', lat: 42.6977, lon: 23.3219})",
    );
    let q = "CALL drevo.spatial.nearest('Stop', {lat: 42.6977, lon: 23.3219}, \
             {lat: 'y', lon: 'x', k: 5}) YIELD node RETURN node.name";
    assert_eq!(
        run_ok(&svc, q).rows,
        [
            [Value::String("Opera".into())],
            [Value::String("Airport".into())]
        ]
    );
    // The default properties find only the node that has them.
    let q = "CALL drevo.spatial.nearest('Stop', {lat: 42.6977, lon: 23.3219}, {k: 5}) \
             YIELD node RETURN node.name";
    assert_eq!(
        run_ok(&svc, q).rows,
        [[Value::String("Lat-lon only".into())]]
    );
}

#[test]
fn the_index_follows_create_set_and_delete() {
    let svc = sofia(true);
    run_ok(
        &svc,
        "CREATE (:Junction {name: 'new', r: 1, lat: 42.6978, lon: 23.3218})",
    );
    assert_eq!(names(&near(&svc, CENTRE, "{where: {r: 1}}")), ["new"]);
    run_ok(
        &svc,
        "MATCH (j:Junction {name: 'new'}) SET j.lat = 42.80, j.lon = 23.50",
    );
    assert_eq!(names(&near(&svc, CENTRE, "{where: {r: 1}}")), ["opera"]);
    run_ok(&svc, "MATCH (j:Junction {name: 'opera'}) REMOVE j.lat");
    assert_eq!(names(&near(&svc, CENTRE, "{where: {r: 1}}")), ["lions"]);
    run_ok(&svc, "MATCH (j:Junction {name: 'lions'}) DETACH DELETE j");
    assert_eq!(
        names(&near(&svc, CENTRE, "{k: 10, where: {r: 1}}")),
        ["airport", "new"]
    );
}

#[test]
fn a_statement_sees_its_own_writes() {
    let svc = sofia(true);
    let res = run_ok(
        &svc,
        "CREATE (:Junction {name: 'new', r: 1, lat: 42.6978, lon: 23.3218}) \
         WITH 1 AS one \
         CALL drevo.spatial.nearest('Junction', {lat: 42.6977, lon: 23.3219}, {where: {r: 1}}) \
         YIELD node RETURN node.name",
    );
    assert_eq!(res.rows, [[Value::String("new".into())]]);
}

#[test]
fn a_lean_database_keeps_its_point_indexes() {
    let svc = sofia(true);
    run_ok(&svc, "CALL drevo.indexes.setSecondary(false)");
    assert_eq!(names(&near(&svc, CENTRE, "{k: 2}")), ["centre", "opera"]);
    run_ok(
        &svc,
        "CREATE (:Junction {name: 'new', r: 1, lat: 42.6977, lon: 23.3219})",
    );
    assert_eq!(names(&near(&svc, CENTRE, "{k: 2}")), ["centre", "new"]);
}

#[test]
fn point_index_ddl() {
    let svc = sofia(true);
    let show = |svc: &NativeService| {
        run_ok(
            svc,
            "SHOW POINT INDEXES YIELD name, type, labelsOrTypes, properties \
             RETURN name, type, labelsOrTypes, properties",
        )
        .rows
    };
    assert_eq!(
        show(&svc),
        [[
            Value::String("junctions".into()),
            Value::String("POINT".into()),
            Value::List(vec![Value::String("Junction".into())]),
            Value::List(vec![
                Value::String("lat".into()),
                Value::String("lon".into())
            ]),
        ]]
    );
    // The name is taken; IF NOT EXISTS makes that a no-op.
    fails(
        &svc,
        "CREATE POINT INDEX junctions FOR (j:Junction) ON (j.lat, j.lon)",
    );
    run_ok(
        &svc,
        "CREATE POINT INDEX junctions IF NOT EXISTS FOR (j:Junction) ON (j.lat, j.lon)",
    );
    // A single point-valued property is Neo4j's form: accepted, not indexed.
    run_ok(
        &svc,
        "CREATE POINT INDEX place FOR (p:Place) ON (p.location)",
    );
    run_ok(&svc, "CREATE POINT INDEX FOR (p:Place) ON (p.geo.location)");
    assert_eq!(show(&svc).len(), 1);
    // The latitude and longitude are top-level properties.
    let e =
        parse("CREATE POINT INDEX FOR (p:Place) ON (p.geo.lat, p.geo.lon)").expect_err("nested");
    assert!(e.to_string().contains("top-level"), "{e}");
    // An unnamed index gets a generated name.
    run_ok(&svc, "CREATE POINT INDEX FOR (s:Shop) ON (s.lat, s.lon)");
    assert_eq!(show(&svc).len(), 2);
    run_ok(&svc, "DROP INDEX junctions");
    assert_eq!(show(&svc).len(), 1);
    // The lookup still answers, from a scan.
    assert_eq!(names(&near(&svc, CENTRE, "")), ["centre"]);
    // Positions belong to a label.
    let e = parse("CREATE POINT INDEX FOR (n) ON (n.lat, n.lon)").expect_err("no label");
    assert!(e.to_string().contains("label"), "{e}");
}

#[test]
fn point_indexes_survive_a_restart() {
    let dir = std::env::temp_dir().join(format!(
        "drevo_point_idx_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    let wal = dir.join("native.wal");
    {
        let svc = NativeService::open(&wal).expect("open");
        run_ok(
            &svc,
            "CREATE POINT INDEX stops FOR (s:Stop) ON (s.lat, s.lon)",
        );
        run_ok(
            &svc,
            "CREATE (:Stop {name: 'Opera', lat: 42.6935, lon: 23.3290}), \
                    (:Stop {name: 'Airport', lat: 42.6952, lon: 23.4062})",
        );
    }
    let svc = NativeService::open(&wal).expect("reopen");
    assert_eq!(
        run_ok(&svc, "SHOW POINT INDEXES YIELD name RETURN name").rows,
        [[Value::String("stops".into())]]
    );
    let q = "CALL drevo.spatial.nearest('Stop', {lat: 42.6977, lon: 23.3219}, {k: 2}) \
             YIELD node RETURN node.name";
    assert_eq!(
        run_ok(&svc, q).rows,
        [
            [Value::String("Opera".into())],
            [Value::String("Airport".into())]
        ]
    );
    drop(svc);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bad_arguments_are_explained() {
    let svc = sofia(true);
    let call = |args: &str| {
        let e = fails(
            &svc,
            &format!("CALL drevo.spatial.nearest({args}) YIELD node RETURN node"),
        );
        e.to_string()
    };
    for (args, message) in [
        ("1, {lat: 42.0, lon: 23.0}", "label must be a string"),
        ("'Junction', [42.0, 23.0]", "{lat: …, lon: …}"),
        ("'Junction', {lat: 42.0}", "{lat: …, lon: …}"),
        ("'Junction', {lat: 'north', lon: 23.0}", "{lat: …, lon: …}"),
        ("'Junction', {lat: 91.0, lon: 23.0}", "latitude"),
        ("'Junction', {lat: 42.0, lon: 181.0}", "longitude"),
        (
            "'Junction', {lat: 42.0, lon: 23.0}, {radius: 3}",
            "unknown option `radius`",
        ),
        ("'Junction', {lat: 42.0, lon: 23.0}, {k: -1}", "k must be"),
        ("'Junction', {lat: 42.0, lon: 23.0}, {k: 1.5}", "k must be"),
        (
            "'Junction', {lat: 42.0, lon: 23.0}, {maxMeters: -5}",
            "maxMeters",
        ),
        (
            "'Junction', {lat: 42.0, lon: 23.0}, {where: 1}",
            "where must be a map",
        ),
        (
            "'Junction', {lat: 42.0, lon: 23.0}, {lat: 1}",
            "lat must be",
        ),
        (
            "'Junction', {lat: 42.0, lon: 23.0}, 'k'",
            "options must be a map",
        ),
    ] {
        let got = call(args);
        assert!(got.contains(message), "`{args}`: {got}");
    }
}

#[test]
fn standalone_call_yields_node_and_meters() {
    let svc = sofia(true);
    let res = run_ok(
        &svc,
        "CALL drevo.spatial.nearest('Junction', {lat: 42.6977, lon: 23.3219})",
    );
    assert_eq!(res.columns, ["node", "meters"]);
    assert_eq!(res.rows.len(), 1);
    assert!(matches!(res.rows[0][0], Value::Node(_)));
}

#[test]
fn the_native_call_returns_ids_and_metres() {
    for indexed in [true, false] {
        let svc = sofia(indexed);
        let id_of = |name: &str| match &run_ok(
            &svc,
            &format!("MATCH (j:Junction {{name: '{name}'}}) RETURN id(j)"),
        )
        .rows[0][0]
        {
            Value::Integer(id) => *id as u64,
            other => panic!("{other:?}"),
        };
        let options = NearestOptions {
            k: 2,
            filter: vec![("r".to_string(), serde_json::json!(1))],
            ..NearestOptions::default()
        };
        let found = svc.nearest("Junction", CENTRE, &options).expect("nearest");
        assert_eq!(
            found.iter().map(|n| n.node).collect::<Vec<_>>(),
            [id_of("opera"), id_of("lions")]
        );
        assert!((found[0].meters - haversine_m(CENTRE, OPERA)).abs() < 1e-6);
        assert!(svc
            .nearest("Junction", (95.0, 0.0), &NearestOptions::default())
            .is_err());
    }
}

/// The scenario that asked for this: snap a start and a destination to the
/// nearest drivable junctions and route between them, in one statement.
#[test]
fn snap_both_ends_and_route_in_one_statement() {
    let svc = sofia(true);
    let res = run_ok(
        &svc,
        "CALL drevo.spatial.nearest('Junction', {lat: 42.6940, lon: 23.3280}, {where: {r: 1}}) \
         YIELD node AS a \
         CALL drevo.spatial.nearest('Junction', {lat: 42.6950, lon: 23.4050}, {where: {r: 1}}) \
         YIELD node AS b \
         CALL drevo.path.weighted(a, b, {relTypes: ['ROAD'], cost: 'car_s'}) YIELD cost, path \
         RETURN a.name, b.name, cost, [n IN nodes(path) | n.name] AS route",
    );
    assert_eq!(
        res.rows,
        [[
            Value::String("opera".into()),
            Value::String("airport".into()),
            Value::Float(720.0),
            Value::List(vec![
                Value::String("opera".into()),
                Value::String("lions".into()),
                Value::String("airport".into()),
            ]),
        ]]
    );
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
    let svc = Arc::new(sofia(true));
    let mut session = Session::new_durable(Arc::clone(&svc));
    session.handle(ClientMessage::Hello {
        extra: dict(vec![]),
    });
    let query = "CALL drevo.spatial.nearest('Junction', {lat: 42.6977, lon: 23.3219}, {k: 3}) \
                 YIELD node, meters RETURN node.name, meters";
    let embedded: Vec<Vec<Bolt>> = run_ok(&svc, query)
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|v| match v {
                    Value::String(n) => Bolt::String(n),
                    Value::Float(m) => Bolt::Float(m),
                    other => panic!("{other:?}"),
                })
                .collect()
        })
        .collect();
    assert_eq!(embedded.len(), 3);
    assert_eq!(bolt_rows(&mut session, query), embedded);
}

/// The example in `docs/cypher-reference.md` returns what the text says.
#[test]
fn the_documented_example_starts_at_opera() {
    let svc = NativeService::in_memory();
    let res = run_ok(
        &svc,
        "CREATE (o:Junction {name: 'Opera', r: 1, lat: 42.6935, lon: 23.3290}), \
                (l:Junction {name: 'Lions Bridge', r: 1, lat: 42.7075, lon: 23.3216}), \
                (:Junction {name: 'Footway', r: 0, lat: 42.6977, lon: 23.3219}), \
                (o)-[:ROAD {car_s: 120}]->(l) \
         WITH 1 AS one \
         CALL drevo.spatial.nearest('Junction', {lat: 42.6977, lon: 23.3219}, {where: {r: 1}}) \
         YIELD node AS start, meters \
         MATCH (goal:Junction {name: 'Lions Bridge'}) \
         CALL drevo.path.weighted(start, goal, {relTypes: ['ROAD'], cost: 'car_s'}) \
         YIELD cost \
         RETURN start.name AS start, round(meters) AS meters, cost",
    );
    assert_eq!(
        res.rows,
        [[
            Value::String("Opera".into()),
            Value::Float(745.0),
            Value::Float(120.0)
        ]]
    );
}
