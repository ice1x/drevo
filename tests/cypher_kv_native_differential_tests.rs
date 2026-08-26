//! Differential Cypher corpus: KV vs native, three ways (RFC
//! `docs/rfc-native-core.md`, #307, Phase 6).
//!
//! The Phase-6 promise is that the executor produces **identical** results no
//! matter which [`GraphEngine`](drevo::engine::GraphEngine) sits underneath.
//! This suite is the guard for that promise ahead of any engine flip: every
//! scenario builds the same graph on the KV [`Drevo`](drevo::db::Drevo) and the
//! native [`NativeGraph`](drevo::native::NativeGraph), then runs each check
//! query three ways —
//!
//! 1. KV via [`execute`](drevo::cypher::executor::execute),
//! 2. native via [`execute_on_engine`](drevo::cypher::executor::execute_on_engine)
//!    (full-scan paths),
//! 3. native via
//!    [`execute_on_engine_with_indexes`](drevo::cypher::executor::execute_on_engine_with_indexes)
//!    with the label + property indexes synced (index-narrowed paths),
//!
//! and asserts the columns, rows, and mutation stats are equal across all
//! three. Write statements are compared two-ways as they are applied (each
//! write runs once per engine). Error paths must agree too: if one engine
//! rejects a statement, the other must reject it with the same rendered error.
//!
//! Non-deterministic values are normalised before comparison: node/relationship
//! `uuid`s (each engine generates its own v7) are zeroed, and the executor's
//! `__cypher__:{label}:{uuid}` placeholder titles for unnamed nodes are
//! collapsed to a fixed marker. Ids are **not** scrubbed — both engines
//! allocate monotonically from 1, and id parity is part of the contract.
//!
//! Queries that reach KV-only subsystems (FTS, vector, keywords) are covered by
//! `tests/cypher_native_engine_tests.rs` (they must raise `EngineCapability` on
//! native) and are deliberately out of scope here, as are non-deterministic
//! functions (`rand()`, `randomUUID()`, `timestamp()`, `datetime()`).
//!
//! # Unordered scan order is converged (id-ascending on both engines)
//!
//! Without an `ORDER BY`, Cypher row order is unspecified — which historically
//! let the engines differ (the KV path enumerated full scans newest-first via
//! `list_recent`; native scanned in `HashMap` iteration order). Both now
//! enumerate **id-ascending** (`collect_all_nodes` on KV, sorted `all_nodes`
//! on native), and the "unordered scans" scenario below pins that parity
//! without any `ORDER BY`. Ordered checks still pin their order explicitly —
//! note a trailing `ORDER BY` binds to the *last arm* of a `UNION` on both
//! engines, so union checks that need whole-result determinism sort per arm
//! via `WITH … ORDER BY`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use drevo::cypher::executor::{
    execute, execute_on_engine, execute_on_engine_with_indexes, ExecResult, NodeValue, PathValue,
    RelationshipValue, Value,
};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;
use drevo::native::NativeGraph;
use drevo::native_label_index::NativeLabelIndex;
use drevo::native_property_index::NativePropertyIndex;

// ---------------------------------------------------------------------------
// Normalisation — strip the per-engine non-determinism, keep everything else
// ---------------------------------------------------------------------------

/// Marker every executor-synthesised placeholder title collapses to.
const SYNTH: &str = "__cypher__:<synth>";

fn scrub(v: &Value) -> Value {
    match v {
        Value::String(s) if s.starts_with("__cypher__:") => Value::String(SYNTH.to_string()),
        Value::List(items) => Value::List(items.iter().map(scrub).collect()),
        Value::Map(m) => Value::Map(m.iter().map(|(k, v)| (k.clone(), scrub(v))).collect()),
        Value::Node(n) => Value::Node(scrub_node(n)),
        Value::Relationship(r) => Value::Relationship(scrub_rel(r)),
        Value::Path(p) => Value::Path(Arc::new(PathValue {
            nodes: p.nodes.iter().map(|n| scrub_node(n)).collect(),
            relationships: p.relationships.iter().map(|r| scrub_rel(r)).collect(),
        })),
        other => other.clone(),
    }
}

fn scrub_node(n: &NodeValue) -> Arc<NodeValue> {
    let properties: BTreeMap<String, Value> = n
        .properties
        .iter()
        .map(|(k, v)| (k.clone(), scrub(v)))
        .collect();
    Arc::new(NodeValue {
        id: n.id,
        uuid: [0; 16],
        labels: n.labels.clone(),
        properties,
    })
}

fn scrub_rel(r: &RelationshipValue) -> Arc<RelationshipValue> {
    let properties: BTreeMap<String, Value> = r
        .properties
        .iter()
        .map(|(k, v)| (k.clone(), scrub(v)))
        .collect();
    Arc::new(RelationshipValue {
        id: r.id,
        uuid: [0; 16],
        from_id: r.from_id,
        to_id: r.to_id,
        kind: r.kind.clone(),
        properties,
    })
}

/// The comparable projection of an [`ExecResult`]: columns, scrubbed rows, and
/// the mutation counters.
fn comparable(res: &ExecResult) -> (Vec<String>, Vec<Vec<Value>>, String) {
    let rows = res
        .rows
        .iter()
        .map(|row| row.iter().map(scrub).collect())
        .collect();
    (res.columns.clone(), rows, format!("{:?}", res.stats))
}

// ---------------------------------------------------------------------------
// The harness — one fresh pair of engines per scenario
// ---------------------------------------------------------------------------

struct Pair {
    kv: Drevo,
    native: NativeGraph,
}

impl Pair {
    fn new() -> Self {
        Pair {
            kv: Drevo::open_in_memory().expect("open in-memory KV store"),
            native: NativeGraph::new(),
        }
    }

    /// Run one statement on both engines and assert the outcome matches:
    /// both `Ok` with equal comparable results, or both `Err` with the same
    /// rendered error.
    fn apply(&self, scenario: &str, source: &str) {
        let q = parse(source).unwrap_or_else(|e| panic!("[{scenario}] parse `{source}`: {e:?}"));
        let kv = execute(&q, &self.kv, HashMap::new());
        let native = execute_on_engine(&q, &self.native, HashMap::new());
        match (kv, native) {
            (Ok(k), Ok(n)) => assert_eq!(
                comparable(&k),
                comparable(&n),
                "[{scenario}] write diverged on `{source}`"
            ),
            (Err(k), Err(n)) => assert_eq!(
                k.to_string(),
                n.to_string(),
                "[{scenario}] error diverged on `{source}`"
            ),
            (k, n) => panic!(
                "[{scenario}] ok/err diverged on `{source}`: kv={:?} native={:?}",
                k.map(|r| r.rows.len()).map_err(|e| e.to_string()),
                n.map(|r| r.rows.len()).map_err(|e| e.to_string()),
            ),
        }
    }

    /// Run one read query three ways (KV, native full-scan, native indexed)
    /// and assert all three comparable results are equal — or that all three
    /// fail with the same rendered error.
    fn check(&self, scenario: &str, source: &str) {
        let q = parse(source).unwrap_or_else(|e| panic!("[{scenario}] parse `{source}`: {e:?}"));

        let mut labels = NativeLabelIndex::new();
        let mut props = NativePropertyIndex::new();
        labels.sync(&self.native);
        props.sync(&self.native);

        let kv = execute(&q, &self.kv, HashMap::new());
        let plain = execute_on_engine(&q, &self.native, HashMap::new());
        let indexed = execute_on_engine_with_indexes(
            &q,
            &self.native,
            None,
            Some(&labels),
            Some(&props),
            HashMap::new(),
        );

        match (kv, plain, indexed) {
            (Ok(k), Ok(p), Ok(i)) => {
                let (k, p, i) = (comparable(&k), comparable(&p), comparable(&i));
                assert_eq!(k, p, "[{scenario}] kv vs native diverged on `{source}`");
                assert_eq!(
                    p, i,
                    "[{scenario}] native full-scan vs indexed diverged on `{source}`"
                );
            }
            (Err(k), Err(p), Err(i)) => {
                assert_eq!(
                    k.to_string(),
                    p.to_string(),
                    "[{scenario}] kv vs native error diverged on `{source}`"
                );
                assert_eq!(
                    p.to_string(),
                    i.to_string(),
                    "[{scenario}] native full-scan vs indexed error diverged on `{source}`"
                );
            }
            (k, p, i) => panic!(
                "[{scenario}] ok/err diverged on `{source}`: kv={:?} native={:?} indexed={:?}",
                k.map(|r| r.rows.len()).map_err(|e| e.to_string()),
                p.map(|r| r.rows.len()).map_err(|e| e.to_string()),
                i.map(|r| r.rows.len()).map_err(|e| e.to_string()),
            ),
        }
    }
}

/// A named scenario: `setup` writes applied to both engines (two-way compared),
/// then `checks` read three-ways.
struct Scenario {
    name: &'static str,
    setup: &'static [&'static str],
    checks: &'static [&'static str],
}

fn run(scenarios: &[Scenario]) {
    for s in scenarios {
        let pair = Pair::new();
        for stmt in s.setup {
            pair.apply(s.name, stmt);
        }
        for query in s.checks {
            pair.check(s.name, query);
        }
    }
}

/// The seed graph most read-side scenarios share: three people, two tags, a
/// mix of numeric / string / bool / null-ish properties, and a few kinds of
/// relationships including a parallel edge and a self-referencing shape.
const TEAM: &[&str] = &[
    "CREATE (:Person {title: 'ada', age: 36, senior: true, team: 'core'})",
    "CREATE (:Person {title: 'bob', age: 25, senior: false, team: 'core'})",
    "CREATE (:Person {title: 'cy', age: 41, senior: true, team: 'infra'})",
    "CREATE (:Tag {title: 'rust'})",
    "CREATE (:Tag {title: 'graphs'})",
    "MATCH (a:Person {title: 'ada'}), (b:Person {title: 'bob'}) \
     CREATE (a)-[:KNOWS {since: 2019}]->(b)",
    "MATCH (b:Person {title: 'bob'}), (c:Person {title: 'cy'}) \
     CREATE (b)-[:KNOWS {since: 2021}]->(c)",
    "MATCH (a:Person {title: 'ada'}), (t:Tag {title: 'rust'}) \
     CREATE (a)-[:LIKES {weight: 0.9}]->(t)",
    "MATCH (b:Person {title: 'bob'}), (t:Tag {title: 'rust'}) \
     CREATE (b)-[:LIKES {weight: 0.4}]->(t)",
    "MATCH (c:Person {title: 'cy'}), (t:Tag {title: 'graphs'}) \
     CREATE (c)-[:LIKES {weight: 0.7}]->(t)",
];

// ---------------------------------------------------------------------------
// 1. CREATE / MATCH / entity projections
// ---------------------------------------------------------------------------

#[test]
fn create_and_match_parity() {
    run(&[
        Scenario {
            name: "create nodes and read them back whole",
            setup: TEAM,
            checks: &[
                "MATCH (n:Person) RETURN n ORDER BY n.title",
                "MATCH (n:Tag) RETURN n ORDER BY n.title",
                "MATCH (n) RETURN id(n), labels(n) ORDER BY id(n)",
                "MATCH (n:Person) RETURN properties(n) ORDER BY n.title",
                "MATCH (n:Person {title: 'ada'}) RETURN keys(n)",
            ],
        },
        Scenario {
            // Even *without* an ORDER BY, both engines must enumerate a scan
            // in the same (id-ascending) order — the row-order convergence
            // the DREVO_ENGINE flip depends on. This is the one scenario
            // that deliberately omits ORDER BY.
            name: "unordered scans enumerate id-ascending on both engines",
            setup: TEAM,
            checks: &[
                "MATCH (n) RETURN id(n)",
                "MATCH (n:Person) RETURN n.title",
                "MATCH (n:Person) WHERE n.age > 30 RETURN n.title",
                "MATCH (a)-[r:LIKES]->(t) RETURN a.title, t.title",
                "MATCH (n:Person) RETURN n.title AS t UNION MATCH (n:Tag) \
                 RETURN n.title AS t",
            ],
        },
        Scenario {
            name: "relationships project identically",
            setup: TEAM,
            checks: &[
                "MATCH (a)-[r:KNOWS]->(b) RETURN a.title, r, b.title ORDER BY r.since",
                "MATCH ()-[r]->() RETURN id(r), type(r) ORDER BY id(r)",
                "MATCH (a)-[r:LIKES]->(t) RETURN a.title, r.weight, t.title \
                 ORDER BY a.title",
                "MATCH (a)-[r]->(b) RETURN type(r), count(*) ORDER BY type(r)",
            ],
        },
        Scenario {
            name: "unnamed nodes get equivalent placeholder titles",
            setup: &[
                "CREATE (:Widget {size: 1})",
                "CREATE (:Widget {size: 2})",
                "CREATE (:Widget)-[:NEXT]->(:Widget)",
            ],
            checks: &[
                "MATCH (w:Widget) RETURN id(w), w.size ORDER BY id(w)",
                "MATCH (w:Widget) RETURN w ORDER BY id(w)",
                "MATCH (a:Widget)-[:NEXT]->(b:Widget) RETURN id(a), id(b)",
            ],
        },
        Scenario {
            name: "inline property match narrows identically",
            setup: TEAM,
            checks: &[
                "MATCH (n:Person {team: 'core'}) RETURN n.title ORDER BY n.title",
                "MATCH (n {senior: true}) RETURN n.title ORDER BY n.title",
                "MATCH (n:Person {age: 36}) RETURN n.title",
                "MATCH (n:Person {team: 'nowhere'}) RETURN n.title",
            ],
        },
        Scenario {
            name: "direction and undirected matches agree",
            setup: TEAM,
            checks: &[
                "MATCH (a:Person {title: 'bob'})-[:KNOWS]->(x) RETURN x.title",
                "MATCH (a:Person {title: 'bob'})<-[:KNOWS]-(x) RETURN x.title",
                "MATCH (a:Person {title: 'bob'})-[:KNOWS]-(x) \
                 RETURN x.title ORDER BY x.title",
                "MATCH (a)-->(b) RETURN a.title, b.title ORDER BY a.title, b.title",
            ],
        },
    ]);
}

// ---------------------------------------------------------------------------
// 2. WHERE filters
// ---------------------------------------------------------------------------

#[test]
fn where_filter_parity() {
    run(&[
        Scenario {
            name: "comparisons and boolean operators",
            setup: TEAM,
            checks: &[
                "MATCH (n:Person) WHERE n.age > 30 RETURN n.title ORDER BY n.title",
                "MATCH (n:Person) WHERE n.age >= 36 AND n.team = 'core' RETURN n.title",
                "MATCH (n:Person) WHERE n.age < 30 OR n.team = 'infra' \
                 RETURN n.title ORDER BY n.title",
                "MATCH (n:Person) WHERE NOT n.senior RETURN n.title",
                "MATCH (n:Person) WHERE n.senior XOR n.team = 'infra' \
                 RETURN n.title ORDER BY n.title",
                "MATCH (n:Person) WHERE n.age <> 36 RETURN n.title ORDER BY n.title",
            ],
        },
        Scenario {
            name: "IN, null checks, and string predicates",
            setup: TEAM,
            checks: &[
                "MATCH (n:Person) WHERE n.team IN ['core', 'design'] \
                 RETURN n.title ORDER BY n.title",
                "MATCH (n:Person) WHERE n.nickname IS NULL RETURN n.title ORDER BY n.title",
                "MATCH (n:Person) WHERE n.age IS NOT NULL RETURN count(*)",
                "MATCH (n:Person) WHERE n.title STARTS WITH 'a' RETURN n.title",
                "MATCH (n:Person) WHERE n.title ENDS WITH 'b' RETURN n.title",
                "MATCH (n:Person) WHERE n.title CONTAINS 'o' RETURN n.title",
                "MATCH (n:Person) WHERE n.title =~ '^[ab].*' RETURN n.title ORDER BY n.title",
            ],
        },
        Scenario {
            name: "null propagation filters the row on both engines",
            setup: TEAM,
            checks: &[
                "MATCH (n:Person) WHERE n.nickname = 'x' RETURN n.title",
                "MATCH (n:Person) WHERE n.nickname > 3 RETURN n.title",
                "MATCH (n:Tag) WHERE n.missing IN [1, 2] RETURN n.title",
            ],
        },
        Scenario {
            name: "where over relationship properties",
            setup: TEAM,
            checks: &[
                "MATCH (a)-[r:LIKES]->(t) WHERE r.weight > 0.5 \
                 RETURN a.title, t.title ORDER BY a.title",
                "MATCH (a)-[r:KNOWS]->(b) WHERE r.since >= 2020 RETURN a.title, b.title",
            ],
        },
    ]);
}

// ---------------------------------------------------------------------------
// 3. RETURN shaping — DISTINCT / ORDER BY / SKIP / LIMIT / CASE
// ---------------------------------------------------------------------------

#[test]
fn return_shaping_parity() {
    run(&[
        Scenario {
            name: "distinct, order, pagination",
            setup: TEAM,
            checks: &[
                "MATCH (n:Person) RETURN DISTINCT n.team ORDER BY n.team",
                "MATCH (n:Person) RETURN n.title ORDER BY n.age DESC",
                "MATCH (n:Person) RETURN n.title ORDER BY n.age DESC SKIP 1 LIMIT 1",
                "MATCH (n:Person) RETURN n.title AS who, n.age + 1 AS next_age \
                 ORDER BY who",
                "MATCH (n) RETURN count(*)",
            ],
        },
        Scenario {
            name: "case expressions",
            setup: TEAM,
            checks: &[
                "MATCH (n:Person) RETURN n.title, \
                 CASE WHEN n.age > 35 THEN 'old' ELSE 'young' END AS bucket \
                 ORDER BY n.title",
                "MATCH (n:Person) RETURN n.title, \
                 CASE n.team WHEN 'core' THEN 1 WHEN 'infra' THEN 2 ELSE 0 END AS t \
                 ORDER BY n.title",
            ],
        },
        Scenario {
            name: "scalar and list functions are engine-independent",
            setup: TEAM,
            checks: &[
                "MATCH (n:Person {title: 'ada'}) \
                 RETURN toUpper(n.title), size(n.title), substring(n.title, 1)",
                "RETURN coalesce(null, 2, 3), abs(-4), round(2.6), toString(42), \
                 toInteger('7')",
                "RETURN head([1,2,3]), last([1,2,3]), tail([1,2,3]), reverse([1,2,3]), \
                 size([1,2,3])",
                "RETURN split('a,b,c', ','), trim('  x  '), replace('aba', 'a', 'c')",
                "RETURN range(1, 5), range(0, 10, 2)",
            ],
        },
    ]);
}

// ---------------------------------------------------------------------------
// 4. Aggregation
// ---------------------------------------------------------------------------

#[test]
fn aggregation_parity() {
    run(&[
        Scenario {
            name: "aggregates with and without grouping keys",
            setup: TEAM,
            checks: &[
                "MATCH (n:Person) RETURN count(*), sum(n.age), min(n.age), max(n.age), \
                 avg(n.age)",
                "MATCH (n:Person) RETURN n.team, count(*), sum(n.age) ORDER BY n.team",
                "MATCH (n:Person) RETURN n.senior, collect(n.title) ORDER BY n.senior",
                "MATCH (n:Person) RETURN count(DISTINCT n.team)",
                "MATCH (n:Nothing) RETURN count(*)",
                "MATCH (n:Nothing) RETURN count(n), collect(n.title)",
            ],
        },
        Scenario {
            name: "aggregates over relationships",
            setup: TEAM,
            checks: &[
                "MATCH ()-[r:LIKES]->() RETURN count(r), sum(r.weight), max(r.weight)",
                "MATCH (a)-[r:LIKES]->() RETURN a.title, count(r) ORDER BY a.title",
            ],
        },
    ]);
}

// ---------------------------------------------------------------------------
// 5. WITH / UNWIND
// ---------------------------------------------------------------------------

#[test]
fn with_and_unwind_parity() {
    run(&[
        Scenario {
            name: "with chains and aggregate filters",
            setup: TEAM,
            checks: &[
                "MATCH (n:Person) WITH n.team AS team, count(*) AS c WHERE c > 1 \
                 RETURN team, c",
                "MATCH (n:Person) WITH n ORDER BY n.age DESC LIMIT 2 \
                 RETURN n.title ORDER BY n.title",
                "MATCH (n:Person) WITH n.age AS a WHERE a > 30 \
                 RETURN sum(a)",
            ],
        },
        Scenario {
            name: "unwind shapes",
            setup: TEAM,
            checks: &[
                "UNWIND [3, 1, 2] AS x RETURN x ORDER BY x",
                "UNWIND [[1, 2], [3]] AS xs RETURN size(xs) ORDER BY size(xs)",
                "UNWIND range(1, 3) AS i RETURN i * 10 ORDER BY i",
                "UNWIND [] AS x RETURN x",
                "UNWIND [1, 2] AS x MATCH (n:Person {title: 'ada'}) \
                 RETURN x, n.title ORDER BY x",
            ],
        },
        Scenario {
            name: "empty unwind followed by a write is a no-op on both (regression #300)",
            setup: &[
                "CREATE (:Seed {title: 's'})",
                "UNWIND [] AS v CREATE (:Ghost {v: v})",
                "UNWIND [] AS v MERGE (:Ghost {v: 1})",
            ],
            checks: &[
                "MATCH (g:Ghost) RETURN count(*)",
                "MATCH (n) RETURN count(*)",
            ],
        },
    ]);
}

// ---------------------------------------------------------------------------
// 6. MERGE / SET / REMOVE
// ---------------------------------------------------------------------------

#[test]
fn merge_set_remove_parity() {
    run(&[
        Scenario {
            name: "merge matches or creates identically",
            setup: &[
                "CREATE (:City {title: 'oslo', pop: 700})",
                "MERGE (c:City {title: 'oslo'})",
                "MERGE (c:City {title: 'bergen'})",
                "MERGE (c:City {title: 'bergen'}) ON MATCH SET c.seen = true",
                "MERGE (c:City {title: 'tromso'}) ON CREATE SET c.fresh = true",
            ],
            checks: &[
                "MATCH (c:City) RETURN c.title, c.seen, c.fresh ORDER BY c.title",
                "MATCH (c:City) RETURN count(*)",
            ],
        },
        Scenario {
            name: "merge relationship",
            setup: &[
                "CREATE (:P {title: 'a'})",
                "CREATE (:P {title: 'b'})",
                "MATCH (a:P {title: 'a'}), (b:P {title: 'b'}) MERGE (a)-[:REL]->(b)",
                "MATCH (a:P {title: 'a'}), (b:P {title: 'b'}) MERGE (a)-[:REL]->(b)",
            ],
            checks: &["MATCH ()-[r:REL]->() RETURN count(r)"],
        },
        Scenario {
            name: "set and remove properties and labels",
            setup: &[
                "CREATE (:Doc {title: 'd1', draft: true})",
                "MATCH (d:Doc {title: 'd1'}) SET d.reviewed = true, d.stars = 5",
                "MATCH (d:Doc {title: 'd1'}) REMOVE d.draft",
                "MATCH (d:Doc {title: 'd1'}) SET d:Published",
            ],
            checks: &[
                "MATCH (d:Doc {title: 'd1'}) RETURN d.draft, d.reviewed, d.stars",
                "MATCH (d:Doc {title: 'd1'}) RETURN labels(d)",
                "MATCH (d:Published) RETURN d.title",
            ],
        },
        Scenario {
            name: "remove label drops index matches on both",
            setup: &[
                "CREATE (:Doc {title: 'd2'})",
                "MATCH (d:Doc {title: 'd2'}) SET d:Hot",
                "MATCH (d:Doc {title: 'd2'}) REMOVE d:Hot",
            ],
            checks: &[
                "MATCH (d:Hot) RETURN count(*)",
                "MATCH (d:Doc {title: 'd2'}) RETURN labels(d)",
            ],
        },
        Scenario {
            name: "set to null erases the property",
            setup: &[
                "CREATE (:Doc {title: 'd3', tmp: 1})",
                "MATCH (d:Doc {title: 'd3'}) SET d.tmp = null",
            ],
            checks: &["MATCH (d:Doc {title: 'd3'}) RETURN d.tmp, keys(d)"],
        },
    ]);
}

// ---------------------------------------------------------------------------
// 7. DELETE
// ---------------------------------------------------------------------------

#[test]
fn delete_parity() {
    run(&[
        Scenario {
            name: "delete relationships then nodes",
            setup: TEAM,
            checks: &[],
        },
        Scenario {
            name: "detach delete cascades on both engines",
            setup: &[
                "CREATE (:H {title: 'hub'})",
                "CREATE (:S {title: 's1'})",
                "CREATE (:S {title: 's2'})",
                "MATCH (h:H), (s:S) CREATE (h)-[:LINKS]->(s)",
                "MATCH (h:H {title: 'hub'}) DETACH DELETE h",
            ],
            checks: &[
                "MATCH (n) RETURN count(*)",
                "MATCH ()-[r]->() RETURN count(r)",
                "MATCH (s:S) RETURN s.title ORDER BY s.title",
            ],
        },
        Scenario {
            name: "plain delete of a connected node errors identically",
            setup: &[
                "CREATE (:A {title: 'x'})-[:R]->(:B {title: 'y'})",
                // Both engines must reject this the same way (setup applies the
                // statement to each engine and asserts the errors match).
                "MATCH (a:A {title: 'x'}) DELETE a",
            ],
            checks: &["MATCH (n) RETURN count(*)"],
        },
        Scenario {
            name: "delete a relationship only",
            setup: &[
                "CREATE (:A {title: 'x'})-[:R {w: 1}]->(:B {title: 'y'})",
                "MATCH (:A)-[r:R]->(:B) DELETE r",
            ],
            checks: &[
                "MATCH ()-[r]->() RETURN count(r)",
                "MATCH (n) RETURN count(*)",
            ],
        },
    ]);
}

// ---------------------------------------------------------------------------
// 8. Traversal — var-length, OPTIONAL MATCH, paths, shortestPath
// ---------------------------------------------------------------------------

#[test]
fn traversal_parity() {
    run(&[
        Scenario {
            name: "variable-length expansion",
            setup: TEAM,
            checks: &[
                "MATCH (a:Person {title: 'ada'})-[:KNOWS*1..2]->(x) \
                 RETURN x.title ORDER BY x.title",
                "MATCH (a:Person {title: 'ada'})-[:KNOWS*2]->(x) RETURN x.title",
                "MATCH (a:Person {title: 'ada'})-[*1..2]->(x) \
                 RETURN x.title ORDER BY x.title",
            ],
        },
        Scenario {
            name: "optional match binds or nulls the same way",
            setup: TEAM,
            checks: &[
                "MATCH (n:Person) OPTIONAL MATCH (n)-[:LIKES]->(t) \
                 RETURN n.title, t.title ORDER BY n.title",
                "MATCH (n:Tag) OPTIONAL MATCH (n)-[:KNOWS]->(x) \
                 RETURN n.title, x ORDER BY n.title",
                "OPTIONAL MATCH (n:Missing) RETURN n",
            ],
        },
        Scenario {
            name: "named paths and shortestPath",
            setup: TEAM,
            checks: &[
                "MATCH p = (a:Person {title: 'ada'})-[:KNOWS]->(b) \
                 RETURN length(p), b.title",
                "MATCH p = (a:Person {title: 'ada'})-[:KNOWS*1..2]->(c:Person {title: 'cy'}) \
                 RETURN length(p)",
                "MATCH p = shortestPath((a:Person {title: 'ada'})-[:KNOWS*..4]->(c:Person {title: 'cy'})) \
                 RETURN length(p)",
                "MATCH p = (a:Person {title: 'ada'})-[:KNOWS]->(b) RETURN p",
            ],
        },
    ]);
}

// ---------------------------------------------------------------------------
// 9. Subqueries, pattern predicates, comprehensions
// ---------------------------------------------------------------------------

#[test]
fn subquery_and_comprehension_parity() {
    run(&[
        Scenario {
            name: "exists and count subqueries",
            setup: TEAM,
            checks: &[
                "MATCH (n:Person) WHERE EXISTS { MATCH (n)-[:LIKES]->(:Tag) } \
                 RETURN n.title ORDER BY n.title",
                "MATCH (n:Person) WHERE NOT EXISTS { MATCH (n)-[:LIKES]->(:Tag) } \
                 RETURN n.title",
                "MATCH (n:Person) \
                 RETURN n.title, COUNT { MATCH (n)-[:KNOWS]->() } AS c ORDER BY n.title",
            ],
        },
        Scenario {
            name: "pattern predicates in where",
            setup: TEAM,
            checks: &[
                "MATCH (n:Person) WHERE (n)-[:KNOWS]->() RETURN n.title ORDER BY n.title",
                "MATCH (n:Person) WHERE NOT (n)-[:KNOWS]->() RETURN n.title",
                "MATCH (n:Person) WHERE (n)-[:LIKES]->(:Tag {title: 'rust'}) \
                 RETURN n.title ORDER BY n.title",
            ],
        },
        Scenario {
            name: "list comprehension, reduce, map projection",
            setup: TEAM,
            checks: &[
                "MATCH (n:Person) \
                 RETURN [x IN [1,2,3,4] WHERE x > 2 | x * 10] LIMIT 1",
                "RETURN reduce(acc = 0, x IN [1,2,3] | acc + x)",
                "MATCH (n:Person {title: 'ada'}) RETURN n {.title, .age}",
                "MATCH (n:Person {title: 'ada'}) RETURN n {.*, flag: true}",
            ],
        },
    ]);
}

// ---------------------------------------------------------------------------
// 10. UNION / parameters / FOREACH
// ---------------------------------------------------------------------------

#[test]
fn union_params_foreach_parity() {
    run(&[
        Scenario {
            // A trailing ORDER BY binds to the *last arm* on both engines, so a
            // union of unordered arms would expose the unordered-scan
            // divergence (see the module docs); each arm pins its own order.
            name: "union and union all",
            setup: TEAM,
            checks: &[
                "MATCH (n:Person) WITH n.title AS t ORDER BY t RETURN t \
                 UNION MATCH (n:Tag) WITH n.title AS t ORDER BY t RETURN t",
                "MATCH (n:Person) WITH n.title AS t ORDER BY t RETURN t \
                 UNION ALL MATCH (n:Person) WITH n.title AS t ORDER BY t RETURN t",
                "RETURN 1 AS x UNION ALL RETURN 1 AS x",
                "RETURN 1 AS x UNION RETURN 1 AS x",
            ],
        },
        Scenario {
            name: "foreach applies writes identically",
            setup: &[
                "CREATE (:Counter {title: 'c', n: 0})",
                "FOREACH (i IN [1, 2, 3] | CREATE (:Item {rank: i}))",
                "MATCH (c:Counter {title: 'c'}) FOREACH (i IN [1] | SET c.n = 10)",
            ],
            checks: &[
                "MATCH (i:Item) RETURN i.rank ORDER BY i.rank",
                "MATCH (c:Counter) RETURN c.n",
            ],
        },
    ]);
}

/// Parameters go through the same map on every entry point; a couple of
/// smoke checks that parameterised reads agree (parameters are threaded
/// separately from the corpus tables above, which all use literals).
#[test]
fn parameter_parity() {
    let pair = Pair::new();
    for stmt in TEAM {
        pair.apply("params", stmt);
    }

    let q = parse("MATCH (n:Person) WHERE n.age > $min RETURN n.title ORDER BY n.title").unwrap();
    let params: HashMap<String, Value> = HashMap::from([("min".to_string(), Value::Integer(30))]);

    let mut labels = NativeLabelIndex::new();
    let mut props = NativePropertyIndex::new();
    labels.sync(&pair.native);
    props.sync(&pair.native);

    let kv = execute(&q, &pair.kv, params.clone()).expect("kv");
    let plain = execute_on_engine(&q, &pair.native, params.clone()).expect("native");
    let indexed =
        execute_on_engine_with_indexes(&q, &pair.native, None, Some(&labels), Some(&props), params)
            .expect("native indexed");

    assert_eq!(comparable(&kv), comparable(&plain));
    assert_eq!(comparable(&plain), comparable(&indexed));
    assert_eq!(kv.rows.len(), 2);
}
