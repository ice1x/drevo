//! `CALL drevo.graph.expand(seeds[, config]) YIELD node, distance, via,
//! truncated`: a breadth-first walk from seed nodes inside the database that
//! returns each node reached once, at its shortest distance, with the
//! relationship that reached it — the "find seeds, then widen" step of
//! graph-RAG in one call instead of one `neighbors` round-trip per node.
//!
//! The fixtures are a small IT task tracker (tasks blocking tasks, owned by
//! people, tagged with topics) and an agent's long-term memory.

#![cfg(feature = "http")]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use drevo::bolt::packstream::Value as Bolt;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;
use drevo::server::{Config, ConfigError};

fn try_run(db: &NativeService, source: &str) -> Result<ExecResult, ExecError> {
    let q = parse(source).unwrap_or_else(|e| panic!("parse `{source}`: {e}"));
    db.execute(&q, HashMap::new())
}

fn run(db: &NativeService, source: &str) -> ExecResult {
    try_run(db, source).unwrap_or_else(|e| panic!("`{source}`: {e}"))
}

/// The `InvalidProcedureCall` message `source` fails with.
fn invalid_call(db: &NativeService, source: &str) -> String {
    match try_run(db, source) {
        Err(ExecError::InvalidProcedureCall { name, message, .. }) => {
            assert_eq!(name, "drevo.graph.expand", "{source}");
            message
        }
        other => panic!("`{source}`: expected InvalidProcedureCall, got {other:?}"),
    }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Integer(i) => *i,
        other => panic!("expected an Integer, got {other:?}"),
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => panic!("expected a String, got {other:?}"),
    }
}

/// The id of the node titled `title`.
fn id_of(db: &NativeService, title: &str) -> i64 {
    int(&run(db, &format!("MATCH (n {{title: '{title}'}}) RETURN id(n)")).rows[0][0])
}

/// A task tracker:
///
/// ```text
/// (deploy:Task)-[:BLOCKS]->(migrate:Task)-[:BLOCKS]->(backup:Task)-[:BLOCKS]->(audit:Task)
/// (deploy)-[:OWNED_BY]->(ana:Person)
/// (migrate)-[:TAGGED]->(postgres:Topic)
/// (chat:Message)-[:MENTIONS]->(deploy)
/// ```
fn tracker() -> NativeService {
    let db = NativeService::in_memory();
    for q in [
        "CREATE (:Task {title: 'deploy'}), (:Task {title: 'migrate'}), \
                (:Task {title: 'backup'}), (:Task {title: 'audit'}), \
                (:Person {title: 'ana'}), (:Topic {title: 'postgres'}), \
                (:Message {title: 'chat'})",
        "MATCH (a {title: 'deploy'}), (b {title: 'migrate'}) CREATE (a)-[:BLOCKS]->(b)",
        "MATCH (a {title: 'migrate'}), (b {title: 'backup'}) CREATE (a)-[:BLOCKS]->(b)",
        "MATCH (a {title: 'backup'}), (b {title: 'audit'}) CREATE (a)-[:BLOCKS]->(b)",
        "MATCH (a {title: 'deploy'}), (b {title: 'ana'}) CREATE (a)-[:OWNED_BY]->(b)",
        "MATCH (a {title: 'migrate'}), (b {title: 'postgres'}) CREATE (a)-[:TAGGED]->(b)",
        "MATCH (a {title: 'chat'}), (b {title: 'deploy'}) CREATE (a)-[:MENTIONS]->(b)",
    ] {
        run(&db, q);
    }
    db
}

/// `(title, distance)` rows of an expand call that `RETURN node.title,
/// distance`.
fn reached(db: &NativeService, source: &str) -> Vec<(String, i64)> {
    run(db, source)
        .rows
        .iter()
        .map(|r| (text(&r[0]), int(&r[1])))
        .collect()
}

fn pairs(expected: &[(&str, i64)]) -> Vec<(String, i64)> {
    expected
        .iter()
        .map(|(t, d)| ((*t).to_string(), *d))
        .collect()
}

// ── Seeds ───────────────────────────────────────────────────────────────────

#[test]
fn a_single_node_seed_walks_k_hops() {
    let db = tracker();
    let rows = reached(
        &db,
        "MATCH (t {title: 'deploy'}) \
         CALL drevo.graph.expand(t, {hops: 2}) YIELD node, distance \
         RETURN node.title, distance",
    );
    // Distance 1 and 2 rows come by node id: creation order here.
    assert_eq!(
        rows,
        pairs(&[
            ("deploy", 0),
            ("migrate", 1),
            ("ana", 1),
            ("chat", 1),
            ("backup", 2),
            ("postgres", 2),
        ])
    );
}

#[test]
fn a_node_id_seed_defaults_to_one_hop() {
    let db = tracker();
    let id = id_of(&db, "backup");
    let rows = reached(
        &db,
        &format!("CALL drevo.graph.expand({id}) YIELD node, distance RETURN node.title, distance"),
    );
    assert_eq!(rows, pairs(&[("backup", 0), ("migrate", 1), ("audit", 1)]));
}

#[test]
fn a_list_mixing_nodes_and_ids_keeps_seed_order() {
    let db = tracker();
    let audit = id_of(&db, "audit");
    let rows = reached(
        &db,
        &format!(
            "MATCH (p {{title: 'postgres'}}) \
             CALL drevo.graph.expand([{audit}, p], {{hops: 1}}) YIELD node, distance \
             RETURN node.title, distance"
        ),
    );
    assert_eq!(
        rows,
        pairs(&[("audit", 0), ("postgres", 0), ("migrate", 1), ("backup", 1)])
    );
}

#[test]
fn seeds_collected_from_a_query_are_walked_once_per_row() {
    let db = tracker();
    let rows = run(
        &db,
        "UNWIND ['ana', 'postgres'] AS t MATCH (n {title: t}) \
         CALL drevo.graph.expand(n) YIELD node, distance \
         WHERE distance = 1 \
         RETURN t, node.title",
    )
    .rows;
    let rows: Vec<(String, String)> = rows.iter().map(|r| (text(&r[0]), text(&r[1]))).collect();
    assert_eq!(
        rows,
        [
            ("ana".to_string(), "deploy".to_string()),
            ("postgres".to_string(), "migrate".to_string()),
        ]
    );
}

#[test]
fn no_seeds_give_no_rows() {
    let db = tracker();
    for seeds in ["[]", "null"] {
        let res = run(
            &db,
            &format!("CALL drevo.graph.expand({seeds}) YIELD node RETURN node"),
        );
        assert!(res.rows.is_empty(), "{seeds}: {:?}", res.rows);
    }
}

#[test]
fn an_id_that_is_not_a_node_is_refused_by_id() {
    let db = tracker();
    let deploy = id_of(&db, "deploy");
    assert_eq!(
        invalid_call(
            &db,
            &format!("CALL drevo.graph.expand([{deploy}, 424242]) YIELD node RETURN node")
        ),
        "seeds: 424242 is not a node id"
    );
    assert_eq!(
        invalid_call(&db, "CALL drevo.graph.expand(-1) YIELD node RETURN node"),
        "seeds: -1 is not a node id"
    );
    assert_eq!(
        invalid_call(
            &db,
            "CALL drevo.graph.expand('deploy') YIELD node RETURN node"
        ),
        "seeds must be a node, a node id or a list of them, got String"
    );
}

#[test]
fn the_procedure_takes_one_or_two_arguments() {
    let db = tracker();
    assert_eq!(
        invalid_call(&db, "CALL drevo.graph.expand() YIELD node RETURN node"),
        "expected 1 or 2 arguments, got 0"
    );
    assert_eq!(
        invalid_call(
            &db,
            "CALL drevo.graph.expand(1, {}, 2) YIELD node RETURN node"
        ),
        "expected 1 or 2 arguments, got 3"
    );
}

// ── Row shape ───────────────────────────────────────────────────────────────

#[test]
fn via_names_the_first_relationship_that_reached_the_node() {
    let db = tracker();
    let res = run(
        &db,
        "MATCH (t {title: 'migrate'}) \
         CALL drevo.graph.expand(t) YIELD node, distance, via, truncated \
         RETURN node.title, via, truncated",
    );
    let migrate = id_of(&db, "migrate");
    let blocks_in = int(&run(
        &db,
        "MATCH ({title: 'deploy'})-[r:BLOCKS]->({title: 'migrate'}) RETURN id(r)",
    )
    .rows[0][0]);
    let tagged = int(&run(&db, "MATCH ()-[r:TAGGED]->() RETURN id(r)").rows[0][0]);
    let by_title: HashMap<String, (Value, Value)> = res
        .rows
        .iter()
        .map(|r| (text(&r[0]), (r[1].clone(), r[2].clone())))
        .collect();
    assert_eq!(by_title.len(), 4, "{:?}", res.rows);
    assert_eq!(by_title["migrate"].0, Value::Null, "a seed has no via");
    let via = |from: i64, rel: i64, ty: &str, dir: &str| {
        Value::Map(BTreeMap::from([
            ("fromId".to_string(), Value::Integer(from)),
            ("relId".to_string(), Value::Integer(rel)),
            ("relType".to_string(), Value::String(ty.to_string())),
            ("direction".to_string(), Value::String(dir.to_string())),
        ]))
    };
    assert_eq!(
        by_title["deploy"].0,
        via(migrate, blocks_in, "BLOCKS", "in")
    );
    assert_eq!(
        by_title["postgres"].0,
        via(migrate, tagged, "TAGGED", "out")
    );
    assert!(by_title
        .values()
        .all(|(_, truncated)| *truncated == Value::Bool(false)));
}

#[test]
fn max_nodes_truncates_and_flags_every_row() {
    let db = tracker();
    let res = run(
        &db,
        "MATCH (t {title: 'deploy'}) \
         CALL drevo.graph.expand(t, {hops: 3, maxNodes: 3}) YIELD node, truncated \
         RETURN node.title, truncated",
    );
    assert_eq!(res.rows.len(), 3, "{:?}", res.rows);
    assert_eq!(text(&res.rows[0][0]), "deploy");
    assert!(res.rows.iter().all(|r| r[1] == Value::Bool(true)));
}

#[test]
fn include_seeds_false_leaves_the_seeds_out() {
    let db = tracker();
    let rows = reached(
        &db,
        "MATCH (t {title: 'backup'}) \
         CALL drevo.graph.expand(t, {includeSeeds: false}) YIELD node, distance \
         RETURN node.title, distance",
    );
    assert_eq!(rows, pairs(&[("migrate", 1), ("audit", 1)]));
}

// ── Filters ─────────────────────────────────────────────────────────────────

#[test]
fn direction_rel_types_and_labels_narrow_the_walk() {
    let db = tracker();
    let expand = |config: &str| {
        reached(
            &db,
            &format!(
                "MATCH (t {{title: 'migrate'}}) \
                 CALL drevo.graph.expand(t, {config}) YIELD node, distance \
                 RETURN node.title, distance"
            ),
        )
    };
    assert_eq!(
        expand("{hops: 3, direction: 'outgoing', relTypes: ['BLOCKS']}"),
        pairs(&[("migrate", 0), ("backup", 1), ("audit", 2)])
    );
    assert_eq!(
        expand("{hops: 3, direction: 'incoming'}"),
        pairs(&[("migrate", 0), ("deploy", 1), ("chat", 2)])
    );
    assert_eq!(
        expand("{hops: 3, labels: ['Task']}"),
        pairs(&[("migrate", 0), ("deploy", 1), ("backup", 1), ("audit", 2)])
    );
    assert_eq!(
        expand("{hops: 3, excludeLabels: ['Message', 'Person']}"),
        pairs(&[
            ("migrate", 0),
            ("deploy", 1),
            ("backup", 1),
            ("postgres", 1),
            ("audit", 2)
        ])
    );
    assert_eq!(
        expand("{hops: 1, perNodeLimit: 1}"),
        pairs(&[("migrate", 0), ("deploy", 1)])
    );
}

#[test]
fn as_of_walks_only_facts_valid_then() {
    let db = NativeService::in_memory();
    for (name, ty) in [
        ("Alice", "PERSON"),
        ("Acme", "ORGANIZATION"),
        ("Globex", "ORGANIZATION"),
    ] {
        run(
            &db,
            &format!(
                "CALL drevo.memory.rememberEntity('s1', '{name}', '{ty}', null) YIELD node \
                 RETURN node"
            ),
        );
    }
    run(
        &db,
        "CALL drevo.memory.assertFact('Alice', 'WORKS_AT', 'Acme', false) YIELD rel RETURN rel",
    );
    // Moving to Globex closes the Acme fact (valid_until = now).
    run(
        &db,
        "CALL drevo.memory.assertFact('Alice', 'WORKS_AT', 'Globex', true) YIELD rel RETURN rel",
    );
    let employers = |as_of: &str| -> Vec<String> {
        run(
            &db,
            &format!(
                "MATCH (a {{name: 'Alice'}}) \
                 CALL drevo.graph.expand(a, {{relTypes: ['RELATED_TO'], asOf: {as_of}, \
                 includeSeeds: false}}) YIELD node \
                 RETURN node.name ORDER BY node.name"
            ),
        )
        .rows
        .iter()
        .map(|r| text(&r[0]))
        .collect()
    };
    assert_eq!(
        employers("null"),
        ["Acme", "Globex"],
        "no asOf walks every fact"
    );
    assert_eq!(employers("datetime()"), ["Globex"]);
    assert!(employers("'2000-01-01T00:00:00Z'").is_empty());
}

/// Alice `WORKS_AT` Acme from `valid_from`, with no end.
fn employment_from(valid_from: &str) -> NativeService {
    let db = NativeService::in_memory();
    for (name, ty) in [("Alice", "PERSON"), ("Acme", "ORGANIZATION")] {
        run(
            &db,
            &format!(
                "CALL drevo.memory.rememberEntity('s1', '{name}', '{ty}', null) YIELD node \
                 RETURN node"
            ),
        );
    }
    run(
        &db,
        "CALL drevo.memory.assertFact('Alice', 'WORKS_AT', 'Acme', false) YIELD rel RETURN rel",
    );
    run(
        &db,
        &format!("MATCH ()-[f:RELATED_TO]->() SET f.valid_from = '{valid_from}'"),
    );
    db
}

#[test]
fn as_of_must_be_an_rfc3339_timestamp() {
    let db = employment_from("2024-03-01T00:30:00Z");
    let q = "MATCH (a {name: 'Alice'}) \
             CALL drevo.graph.expand(a, {relTypes: ['RELATED_TO'], asOf: 'yesterday'}) \
             YIELD node RETURN node.name";
    match try_run(&db, q) {
        Err(ExecError::InvalidProcedureCall { name, message, .. }) => {
            assert_eq!(name, "drevo.graph.expand");
            assert!(message.contains("asOf"), "{message}");
        }
        other => panic!(
            "#808: asOf 'yesterday' is not a timestamp and must be refused naming asOf; \
             instead it is accepted and compared as a string: {other:?}"
        ),
    }
}

#[test]
fn as_of_with_a_timezone_offset_is_normalised() {
    // The fact holds from 00:30 UTC; 02:00+02:00 is 00:00 UTC, before it.
    let db = employment_from("2024-03-01T00:30:00Z");
    let rows = run(
        &db,
        "MATCH (a {name: 'Alice'}) \
         CALL drevo.graph.expand(a, {relTypes: ['RELATED_TO'], \
           asOf: '2024-03-01T02:00:00+02:00', includeSeeds: false}) \
         YIELD node RETURN node.name",
    )
    .rows;
    assert!(
        rows.is_empty(),
        "#808: asOf '2024-03-01T02:00:00+02:00' (= 00:00Z) is before valid_from \
         '2024-03-01T00:30:00Z', yet the string comparison walked the fact: {rows:?}"
    );
    // Sanity: one hour later (01:00Z) the fact holds.
    let later = run(
        &db,
        "MATCH (a {name: 'Alice'}) \
         CALL drevo.graph.expand(a, {relTypes: ['RELATED_TO'], \
           asOf: '2024-03-01T01:00:00Z', includeSeeds: false}) \
         YIELD node RETURN node.name",
    )
    .rows;
    assert_eq!(later.len(), 1, "{later:?}");
}

// ── Config errors ───────────────────────────────────────────────────────────

#[test]
fn every_config_error_names_the_key() {
    let db = tracker();
    let call = |config: &str| {
        invalid_call(
            &db,
            &format!(
                "MATCH (t {{title: 'deploy'}}) \
                 CALL drevo.graph.expand(t, {config}) YIELD node RETURN node"
            ),
        )
    };
    assert_eq!(
        call("{depth: 2}"),
        "unknown config key `depth`; expected hops, direction, relTypes, labels, \
         excludeLabels, perNodeLimit, maxNodes, asOf, includeSeeds"
    );
    assert_eq!(
        call("{hops: 7}"),
        "hops 7 is above the server's limit of 6 (DREVO_EXPAND_MAX_HOPS)"
    );
    assert_eq!(call("{hops: -1}"), "hops must be an integer ≥ 0, got -1");
    assert_eq!(
        call("{perNodeLimit: -2}"),
        "perNodeLimit must be an integer ≥ 1, got -2"
    );
    assert_eq!(
        call("{maxNodes: 0}"),
        "maxNodes must be an integer ≥ 1, got 0"
    );
    assert_eq!(
        call("{maxNodes: 5001}"),
        "maxNodes 5001 is above the server's limit of 5000 (DREVO_EXPAND_MAX_NODES)"
    );
    assert_eq!(
        call("{direction: 'up'}"),
        "direction must be 'outgoing', 'incoming' or 'both', got 'up'"
    );
    assert_eq!(call("'deep'"), "the config must be a map, got String");
}

#[test]
fn max_nodes_below_the_seed_count_is_refused() {
    let db = tracker();
    assert_eq!(
        invalid_call(
            &db,
            "MATCH (t:Task) WITH collect(t) AS tasks \
             CALL drevo.graph.expand(tasks, {maxNodes: 2}) YIELD node RETURN node"
        ),
        "maxNodes 2 is less than the 4 seeds; raise maxNodes or pass fewer seeds"
    );
}

// ── Composition and determinism ─────────────────────────────────────────────

#[test]
fn hybrid_search_hits_seed_the_expansion() {
    let db = tracker();
    let rows = reached(
        &db,
        "CALL drevo.search.hybrid('audit', null, 'embedding', 5, {mode: 'fts'}) YIELD node \
         WITH collect(node) AS seeds \
         CALL drevo.graph.expand(seeds, {hops: 2}) YIELD node, distance \
         RETURN node.title, distance",
    );
    assert_eq!(rows, pairs(&[("audit", 0), ("backup", 1), ("migrate", 2)]));
}

#[test]
fn the_same_call_gives_the_same_rows() {
    let db = tracker();
    let q = "MATCH (t:Task) WITH collect(t) AS tasks \
             CALL drevo.graph.expand(tasks, {hops: 2}) YIELD node, distance, via \
             RETURN id(node), distance, via";
    let first = run(&db, q).rows;
    for _ in 0..5 {
        assert_eq!(run(&db, q).rows, first);
    }
}

// ── Bolt ────────────────────────────────────────────────────────────────────

#[test]
fn bolt_round_trip_types() {
    let db = Arc::new(tracker());
    let mut s = Session::new_durable(Arc::clone(&db));
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });
    let mut replies = s.handle(ClientMessage::Run {
        query: "MATCH (t {title: 'backup'}) \
                CALL drevo.graph.expand(t) YIELD node, distance, via, truncated \
                RETURN node.title, distance, via, truncated"
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
    assert_eq!(records.len(), 3, "{replies:?}");
    // The seed: distance 0, no via.
    assert_eq!(records[0][1], Bolt::Integer(0));
    assert_eq!(records[0][2], Bolt::Null);
    assert_eq!(records[0][3], Bolt::Boolean(false));
    // A reached node: an Integer distance and a Map via.
    let row = records[1];
    assert_eq!(row[1], Bolt::Integer(1));
    let Bolt::Dictionary(via) = &row[2] else {
        panic!("via is a Map, got {:?}", row[2]);
    };
    assert!(
        matches!(via.get("fromId"), Some(Bolt::Integer(_))),
        "{via:?}"
    );
    assert!(
        matches!(via.get("relId"), Some(Bolt::Integer(_))),
        "{via:?}"
    );
    assert_eq!(
        via.get("relType"),
        Some(&Bolt::String("BLOCKS".to_string()))
    );
    assert!(
        matches!(via.get("direction"), Some(Bolt::String(d)) if d == "in" || d == "out"),
        "{via:?}"
    );
    assert_eq!(row[3], Bolt::Boolean(false));
}

// ── Server ceilings ─────────────────────────────────────────────────────────

fn config(hops: Option<&str>, nodes: Option<&str>) -> Result<Config, ConfigError> {
    let (hops, nodes) = (hops.map(str::to_string), nodes.map(str::to_string));
    Config::from_env(move |k| match k {
        "DREVO_EXPAND_MAX_HOPS" => hops.clone(),
        "DREVO_EXPAND_MAX_NODES" => nodes.clone(),
        _ => None,
    })
}

#[test]
fn the_ceilings_default_to_six_hops_and_5000_nodes() {
    let cfg = config(None, None).unwrap();
    assert_eq!((cfg.expand_max_hops, cfg.expand_max_nodes), (6, 5000));
    let cfg = config(Some("3"), Some("100")).unwrap();
    assert_eq!((cfg.expand_max_hops, cfg.expand_max_nodes), (3, 100));
}

#[test]
fn a_ceiling_that_is_not_a_positive_number_is_refused() {
    for raw in ["0", "-1", "many"] {
        let err = config(Some(raw), None).expect_err(raw);
        assert_eq!(
            err.to_string(),
            format!(
                "invalid DREVO_EXPAND_MAX_HOPS value `{raw}`: expected a whole number of hops, at least 1"
            )
        );
        let err = config(None, Some(raw)).expect_err(raw);
        assert_eq!(
            err.to_string(),
            format!(
                "invalid DREVO_EXPAND_MAX_NODES value `{raw}`: expected a whole number of nodes, at least 1"
            )
        );
    }
}

// ── Read filtering ──────────────────────────────────────────────────────────

/// A reader who may not see `:Secret` issues or `PAYS` relationships never
/// gets them from an expansion, and is never walked through them.
#[cfg(feature = "auth")]
mod read_filter {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use drevo::native_api::{build_native_router, NativeApiState};
    use drevo::native_service::NativeService;
    use drevo::security::Security;
    use drevo_auth::catalog::MemoryStore;
    use drevo_auth::users::{AuthManager, NewUser};
    use tower::ServiceExt;

    struct Server {
        app: axum::Router,
        users: Arc<AuthManager>,
    }

    impl Server {
        /// A bug tracker where `crash on save` blocks a `:Secret` exploit
        /// that blocks `slow search` (the only way from the first to the
        /// last), and a company that `PAYS` and `EMPLOYS` Ann. `rita` may not
        /// traverse `:Secret` nodes, `hank` may not traverse `PAYS`.
        async fn new() -> Self {
            let users = Arc::new(AuthManager::open(Arc::new(MemoryStore::new())).unwrap());
            for (name, role) in [("admin", "admin"), ("rita", "reader"), ("hank", "reader")] {
                users
                    .create_user(NewUser::new(name, "long-password").change_required(false))
                    .unwrap();
                users.grant_role(role, name).unwrap();
            }
            let state = NativeApiState::new(Arc::new(NativeService::in_memory()))
                .with_security(Security::new(Arc::clone(&users)));
            let s = Self {
                app: build_native_router(state),
                users,
            };
            for q in [
                "CREATE (:Bug {title: 'crash on save'}), \
                        (:Bug:Secret {title: 'crash exploit in auth'}), \
                        (:Bug {title: 'slow search'})",
                "MATCH (a {title: 'crash on save'}), (b {title: 'crash exploit in auth'}) \
                 CREATE (a)-[:BLOCKS]->(b)",
                "MATCH (b {title: 'crash exploit in auth'}), (c {title: 'slow search'}) \
                 CREATE (b)-[:BLOCKS]->(c)",
                "CREATE (:Company {title: 'Acme'}), (:Employee {title: 'Ann'}), \
                        (:Employee {title: 'Bob'})",
                "MATCH (co:Company), (e {title: 'Ann'}) CREATE (co)-[:EMPLOYS]->(e)",
                "MATCH (co:Company), (e {title: 'Bob'}) CREATE (co)-[:PAYS]->(e)",
                "CREATE ROLE no_secrets",
                "DENY TRAVERSE ON GRAPH * NODES Secret TO no_secrets",
                "GRANT ROLE no_secrets TO rita",
                "CREATE ROLE no_pay",
                "DENY TRAVERSE ON GRAPH * RELATIONSHIPS PAYS TO no_pay",
                "GRANT ROLE no_pay TO hank",
            ] {
                s.rows("admin", q).await;
            }
            s
        }

        async fn run(&self, user: &str, query: &str) -> (StatusCode, serde_json::Value) {
            let token = self.users.issue_session(user).unwrap();
            let body = serde_json::json!({ "query": query }).to_string();
            let req = Request::builder()
                .method("POST")
                .uri("/cypher")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap();
            let resp = self.app.clone().oneshot(req).await.unwrap();
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            (
                status,
                serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
            )
        }

        async fn rows(&self, user: &str, query: &str) -> serde_json::Value {
            let (status, json) = self.run(user, query).await;
            assert_eq!(status, StatusCode::OK, "{user}: {query}: {json}");
            json["rows"].clone()
        }

        /// The titles an expansion from `seed` returns to `user`.
        async fn expand(&self, user: &str, seed: &str, config: &str) -> Vec<String> {
            let rows = self
                .rows(
                    user,
                    &format!(
                        "MATCH (s {{title: '{seed}'}}) \
                         CALL drevo.graph.expand(s, {config}) YIELD node \
                         RETURN node.title"
                    ),
                )
                .await;
            rows.as_array()
                .unwrap()
                .iter()
                .map(|r| r[0].as_str().unwrap().to_string())
                .collect()
        }
    }

    #[tokio::test]
    async fn a_hidden_node_is_neither_returned_nor_walked_through() {
        let s = Server::new().await;
        assert_eq!(
            s.expand("admin", "crash on save", "{hops: 2}").await,
            ["crash on save", "crash exploit in auth", "slow search"]
        );
        assert_eq!(
            s.expand("rita", "crash on save", "{hops: 2}").await,
            ["crash on save"]
        );
    }

    #[tokio::test]
    async fn a_hidden_seed_is_refused_like_a_missing_one() {
        let s = Server::new().await;
        let id = s.rows("admin", "MATCH (b:Secret) RETURN id(b)").await[0][0]
            .as_i64()
            .unwrap();
        let (status, json) = s
            .run(
                "rita",
                &format!("CALL drevo.graph.expand({id}) YIELD node RETURN node.title"),
            )
            .await;
        assert_ne!(status, StatusCode::OK, "{json}");
        assert!(
            json.to_string()
                .contains(&format!("seeds: {id} is not a node id")),
            "{json}"
        );
    }

    #[tokio::test]
    async fn a_hidden_relationship_type_is_not_walked() {
        let s = Server::new().await;
        assert_eq!(
            s.expand("admin", "Acme", "{}").await,
            ["Acme", "Ann", "Bob"]
        );
        assert_eq!(s.expand("hank", "Acme", "{}").await, ["Acme", "Ann"]);
    }
}
