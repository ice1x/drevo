//! `drevo.community.summarize / query / of / stale` and the `scope` of
//! `drevo.rag.context` / `drevo.rag.retrieve`: the graph cut into
//! communities, each written back as a `:Community` with a summary, and
//! corpus-level questions answered from those summaries.
//!
//! The fixture is an IT task manager with two teams: a backend team (Alice,
//! Bob, Carol and their tickets) and a mobile team (Dave, Erin, Frank and
//! theirs), densely linked inside, with one bridge — a mobile crash that
//! depends on a backend login fix. A stand-up `:Message` hangs off a backend
//! ticket and must stay out of every community. The embedder is an
//! in-process stub: text about logins points at `[1, 0, 0]`, about mobile
//! releases at `[0, 1, 0]`, anything else at `[0, 0, 1]`.

#![cfg(feature = "http")]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use drevo::bolt::packstream::Value as Bolt;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::embeddings::{EmbeddingsError, TextEmbedder};
use drevo::native_service::NativeService;

struct StubEmbedder;

impl TextEmbedder for StubEmbedder {
    fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbeddingsError> {
        let text = text.to_lowercase();
        Ok(if text.contains("login") {
            vec![1.0, 0.0, 0.0]
        } else if text.contains("mobile") || text.contains("release") {
            vec![0.0, 1.0, 0.0]
        } else {
            vec![0.0, 0.0, 1.0]
        })
    }
}

fn try_run(db: &NativeService, source: &str) -> Result<ExecResult, ExecError> {
    let q = parse(source).unwrap_or_else(|e| panic!("parse `{source}`: {e}"));
    db.execute(&q, HashMap::new())
}

fn run(db: &NativeService, source: &str) -> ExecResult {
    try_run(db, source).unwrap_or_else(|e| panic!("`{source}`: {e}"))
}

/// The `InvalidProcedureCall` message `source` fails with, from `proc`.
fn invalid_call(db: &NativeService, source: &str, proc: &str) -> String {
    match try_run(db, source) {
        Err(ExecError::InvalidProcedureCall { name, message, .. }) => {
            assert_eq!(name, proc, "{message}");
            message
        }
        other => panic!("`{source}`: expected InvalidProcedureCall, got {other:?}"),
    }
}

fn tracker(db: &NativeService) {
    run(
        db,
        "CREATE (alice:Person {name: 'Alice', description: 'Backend lead'}), \
                (bob:Person {name: 'Bob', description: 'Backend engineer'}), \
                (carol:Person {name: 'Carol', description: 'Database engineer'}), \
                (be101:Ticket {title: 'BE-101 API rate limiter', body: 'Throttle the public API gateway'}), \
                (be142:Ticket {title: 'BE-142 Login token expires early', body: 'Session tokens on the auth server expire after five minutes', embedding: [1.0, 0.0, 0.0]}), \
                (be150:Ticket {title: 'BE-150 Database migration', body: 'Move the orders table to the new server'}), \
                (dave:Person {name: 'Dave', description: 'Mobile lead'}), \
                (erin:Person {name: 'Erin', description: 'iOS developer'}), \
                (frank:Person {name: 'Frank', description: 'Android developer'}), \
                (mo7:Ticket {title: 'MO-7 iOS release build', body: 'Ship the mobile release to the app store', embedding: [0.0, 1.0, 0.0]}), \
                (mo9:Ticket {title: 'MO-9 Android crash on startup', body: 'The mobile app crashes before the release screen', embedding: [0.0, 1.0, 0.0]}), \
                (mo12:Ticket {title: 'MO-12 Mobile release notes', body: 'Write the mobile release notes for the app store', embedding: [0.0, 1.0, 0.0]}), \
                (msg:Message {text: 'stand-up: the rate limiter is late', role: 'user'}), \
                (alice)-[:WORKS_ON]->(be101), (alice)-[:WORKS_ON]->(be142), (alice)-[:WORKS_ON]->(be150), \
                (bob)-[:WORKS_ON]->(be101), (bob)-[:WORKS_ON]->(be142), \
                (carol)-[:WORKS_ON]->(be150), (carol)-[:WORKS_ON]->(be142), \
                (alice)-[:MENTORS]->(bob), (alice)-[:MENTORS]->(carol), (be101)-[:BLOCKS]->(be150), \
                (dave)-[:WORKS_ON]->(mo7), (dave)-[:WORKS_ON]->(mo9), (dave)-[:WORKS_ON]->(mo12), \
                (erin)-[:WORKS_ON]->(mo7), (erin)-[:WORKS_ON]->(mo9), \
                (frank)-[:WORKS_ON]->(mo12), (frank)-[:WORKS_ON]->(mo9), \
                (dave)-[:MENTORS]->(erin), (dave)-[:MENTORS]->(frank), (mo9)-[:BLOCKS]->(mo7), \
                (mo9)-[:DEPENDS_ON]->(be142), \
                (msg)-[:ABOUT]->(be101)",
    );
}

fn plain() -> NativeService {
    let db = NativeService::in_memory();
    tracker(&db);
    db
}

fn with_embedder() -> NativeService {
    let db = NativeService::in_memory();
    assert!(db.set_embedder(Arc::new(StubEmbedder)));
    tracker(&db);
    db
}

fn string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => panic!("expected a string, got {other:?}"),
    }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Integer(i) => *i,
        other => panic!("expected an integer, got {other:?}"),
    }
}

fn float(v: &Value) -> f64 {
    match v {
        Value::Float(f) => *f,
        other => panic!("expected a float, got {other:?}"),
    }
}

fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::List(items) => items.iter().map(string).collect(),
        other => panic!("expected a list, got {other:?}"),
    }
}

/// One `drevo.community.summarize` row.
#[derive(Debug)]
struct Summarized {
    id: String,
    size: i64,
    summary: Option<String>,
    keywords: Vec<String>,
    mode: Option<String>,
}

fn summarize(db: &NativeService, config: &str) -> Vec<Summarized> {
    try_summarize(db, config).unwrap_or_else(|e| panic!("summarize({config}): {e}"))
}

/// [`summarize`], or the error it fails with.
fn try_summarize(db: &NativeService, config: &str) -> Result<Vec<Summarized>, ExecError> {
    Ok(try_run(
        db,
        &format!(
            "CALL drevo.community.summarize({config}) \
             YIELD community, size, summary, keywords, mode \
             RETURN community.id, size, summary, keywords, mode"
        ),
    )?
    .rows
    .iter()
    .map(|r| Summarized {
        id: string(&r[0]),
        size: int(&r[1]),
        summary: match &r[2] {
            Value::Null => None,
            v => Some(string(v)),
        },
        keywords: strings(&r[3]),
        mode: match &r[4] {
            Value::Null => None,
            v => Some(string(v)),
        },
    })
    .collect())
}

/// The members of each community of `layer`: id → sorted names.
fn members(db: &NativeService, layer: &str) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for r in run(
        db,
        &format!(
            "MATCH (n)-[m:MEMBER_OF]->(c:Community {{layer: '{layer}'}}) \
             WHERE m.layer = '{layer}' \
             RETURN c.id, coalesce(n.name, n.title)"
        ),
    )
    .rows
    {
        out.entry(string(&r[0])).or_default().push(string(&r[1]));
    }
    for names in out.values_mut() {
        names.sort();
    }
    out
}

fn count(db: &NativeService, query: &str) -> i64 {
    int(&run(db, query).rows[0][0])
}

const BACKEND: [&str; 6] = [
    "Alice",
    "BE-101 API rate limiter",
    "BE-142 Login token expires early",
    "BE-150 Database migration",
    "Bob",
    "Carol",
];

const MOBILE: [&str; 6] = [
    "Dave",
    "Erin",
    "Frank",
    "MO-12 Mobile release notes",
    "MO-7 iOS release build",
    "MO-9 Android crash on startup",
];

// ── summarize ────────────────────────────────────────────────────────

#[test]
fn summarize_finds_the_two_teams() {
    let db = plain();
    let rows = summarize(&db, "{mode: 'extractive'}");
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(
        rows.iter()
            .map(|r| (r.id.as_str(), r.size))
            .collect::<Vec<_>>(),
        [("default-0", 6), ("default-1", 6)]
    );
    for row in &rows {
        assert_eq!(row.mode.as_deref(), Some("extractive"));
        assert!(row.summary.is_some(), "{row:?}");
        assert!(
            !row.keywords.is_empty() && row.keywords.len() <= 10,
            "{row:?}"
        );
    }
    let found = members(&db, "default");
    assert_eq!(found["default-0"], BACKEND);
    assert_eq!(found["default-1"], MOBILE);
    // Bookkeeping stays out.
    assert_eq!(
        count(&db, "MATCH (m:Message)-[:MEMBER_OF]->() RETURN count(m)"),
        0
    );
    // What the community node holds.
    let node = run(
        &db,
        "MATCH (c:Community {id: 'default-0'}) \
         RETURN c.layer, c.size, c.modularity, c.summary, c.keywords, c.mode, \
                c.computed_at, c.member_ids_hash",
    )
    .rows[0]
        .clone();
    assert_eq!(string(&node[0]), "default");
    assert_eq!(int(&node[1]), 6);
    assert!(float(&node[2]) > 0.3, "{:?}", node[2]);
    assert_eq!(Some(string(&node[3])), rows[0].summary);
    assert_eq!(strings(&node[4]), rows[0].keywords);
    assert_eq!(string(&node[5]), "extractive");
    assert!(string(&node[6]).starts_with("20"), "{:?}", node[6]);
    assert_eq!(string(&node[7]).len(), 16);
}

#[test]
fn the_extractive_summary_names_members_keywords_and_relations() {
    let db = plain();
    let rows = summarize(&db, "{mode: 'extractive'}");
    let backend = rows[0].summary.clone().unwrap();
    assert!(
        backend.starts_with("6 members. Person: Alice, "),
        "{backend}"
    );
    assert!(backend.contains(" Ticket: BE-"), "{backend}");
    assert!(
        backend.contains("BE-142 Login token expires early"),
        "{backend}"
    );
    assert!(backend.contains(" Keywords: "), "{backend}");
    assert!(
        backend.contains(" Relations: WORKS_ON (7), MENTORS (2), BLOCKS (1)."),
        "{backend}"
    );
    assert!(drevo::rag::tokens::approx(&backend) <= 300);
    let mobile = rows[1].summary.clone().unwrap();
    assert!(mobile.starts_with("6 members. Person: Dave, "), "{mobile}");
    assert!(
        rows[1].keywords.contains(&"release".to_string()),
        "{:?}",
        rows[1].keywords
    );
    assert!(
        rows[1].keywords.contains(&"mobile".to_string()),
        "{:?}",
        rows[1].keywords
    );
    // Deterministic: recomputing gives the same text.
    let again = summarize(&db, "{mode: 'extractive'}");
    assert_eq!(again[0].summary, rows[0].summary);
    assert_eq!(again[1].summary, rows[1].summary);
    assert_eq!(again[1].keywords, rows[1].keywords);
    // A small budget is kept.
    let small = summarize(&db, "{mode: 'extractive', tokenBudget: 25}");
    for row in small {
        let text = row.summary.unwrap();
        assert!(drevo::rag::tokens::approx(&text) <= 25, "{text}");
    }
}

#[test]
fn the_hub_ranks_highest_in_its_community() {
    let db = plain();
    summarize(&db, "{mode: 'extractive'}");
    for (community, hub) in [("default-0", "Alice"), ("default-1", "Dave")] {
        let rows = run(
            &db,
            &format!(
                "MATCH (n)-[m:MEMBER_OF]->(:Community {{id: '{community}'}}) \
                 RETURN coalesce(n.name, n.title), m.rank ORDER BY m.rank DESC"
            ),
        )
        .rows;
        assert_eq!(rows.len(), 6);
        assert_eq!(string(&rows[0][0]), hub, "{rows:?}");
        assert!(float(&rows[0][1]) > float(&rows[1][1]), "{rows:?}");
        let total: f64 = rows.iter().map(|r| float(&r[1])).sum();
        assert!((total - 1.0).abs() < 1e-6, "{total}");
    }
}

#[test]
fn related_community_carries_the_bridge_weight() {
    let db = plain();
    summarize(&db, "{mode: 'extractive'}");
    let rows = run(
        &db,
        "MATCH (a:Community)-[r:RELATED_COMMUNITY]->(b:Community) RETURN a.id, b.id, r.weight",
    )
    .rows;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(string(&rows[0][0]), "default-0");
    assert_eq!(string(&rows[0][1]), "default-1");
    assert_eq!(int(&rows[0][2]), 1);
}

#[test]
fn replace_keeps_one_layer_and_layers_coexist() {
    let db = plain();
    summarize(&db, "{mode: 'extractive'}");
    summarize(&db, "{mode: 'extractive'}");
    assert_eq!(
        count(
            &db,
            "MATCH (c:Community {layer: 'default'}) RETURN count(c)"
        ),
        2
    );
    assert_eq!(count(&db, "MATCH ()-[m:MEMBER_OF]->() RETURN count(m)"), 12);
    let fine = summarize(&db, "{mode: 'extractive', layer: 'fine', resolution: 2.0}");
    assert!(fine.iter().all(|r| r.id.starts_with("fine-")), "{fine:?}");
    assert!(fine.len() >= 2, "{fine:?}");
    assert_eq!(
        count(
            &db,
            "MATCH (c:Community {layer: 'default'}) RETURN count(c)"
        ),
        2
    );
    let fine_count = count(&db, "MATCH (c:Community {layer: 'fine'}) RETURN count(c)");
    assert_eq!(fine_count, fine.len() as i64);
    // Every partitioned node is a member once per layer.
    assert_eq!(
        count(
            &db,
            "MATCH ()-[m:MEMBER_OF {layer: 'fine'}]->() RETURN count(m)"
        ),
        12
    );
    assert_eq!(members(&db, "default")["default-0"], BACKEND);
}

#[test]
fn replace_false_refuses_an_existing_layer() {
    let db = plain();
    summarize(&db, "{mode: 'extractive'}");
    let message = invalid_call(
        &db,
        "CALL drevo.community.summarize({mode: 'extractive', replace: false})",
        "drevo.community.summarize",
    );
    assert_eq!(
        message,
        "layer 'default' already has 2 communities; pass replace: true to recompute it, \
         or name another layer"
    );
    // A new layer is fine without replace.
    let other = summarize(&db, "{mode: 'extractive', replace: false, layer: 'other'}");
    assert_eq!(other.len(), 2);
}

#[test]
fn small_communities_get_members_but_no_summary() {
    let db = plain();
    let rows = summarize(&db, "{mode: 'extractive', minSize: 7}");
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row.summary, None, "{row:?}");
        assert_eq!(row.mode, None, "{row:?}");
        assert!(row.keywords.is_empty(), "{row:?}");
    }
    assert_eq!(count(&db, "MATCH ()-[m:MEMBER_OF]->() RETURN count(m)"), 12);
    assert_eq!(
        count(
            &db,
            "MATCH (c:Community) WHERE c.summary IS NULL RETURN count(c)"
        ),
        2
    );
}

#[test]
fn labels_and_rel_types_choose_the_subgraph() {
    let db = plain();
    let persons = summarize(&db, "{mode: 'extractive', labels: ['Person']}");
    assert_eq!(
        persons.iter().map(|r| r.size).collect::<Vec<_>>(),
        [3, 3],
        "{persons:?}"
    );
    let found = members(&db, "default");
    assert_eq!(found["default-0"], ["Alice", "Bob", "Carol"]);
    assert_eq!(found["default-1"], ["Dave", "Erin", "Frank"]);

    let mentoring = summarize(&db, "{mode: 'extractive', relTypes: ['MENTORS']}");
    // Two mentoring trios; every ticket alone.
    assert_eq!(mentoring.len(), 8, "{mentoring:?}");
    assert_eq!(
        mentoring.iter().filter(|r| r.summary.is_some()).count(),
        2,
        "{mentoring:?}"
    );
    assert_eq!(
        count(
            &db,
            "MATCH (c:Community {layer: 'default'}) RETURN count(c)"
        ),
        8
    );
}

#[test]
fn wcc_keeps_the_bridged_teams_together() {
    let db = plain();
    let rows = summarize(&db, "{mode: 'extractive', algorithm: 'wcc'}");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].size, 12);
    assert!(rows[0]
        .summary
        .as_deref()
        .unwrap()
        .starts_with("12 members."));
}

#[test]
fn llm_mode_without_a_chat_model_is_a_capability_error() {
    let db = plain();
    match try_run(&db, "CALL drevo.community.summarize({mode: 'llm'})") {
        Err(ExecError::EngineCapability { feature }) => {
            assert_eq!(
                feature,
                "LLM community summaries (no chat upstream is configured: set \
                 DREVO_EXTRACT_UPSTREAM or DREVO_TEXT2CYPHER_UPSTREAM; see CALL \
                 drevo.extract.status())"
            );
        }
        other => panic!("expected EngineCapability, got {other:?}"),
    }
    assert_eq!(count(&db, "MATCH (c:Community) RETURN count(c)"), 0);
}

#[test]
fn auto_mode_degrades_to_extractive() {
    let db = plain();
    let rows = summarize(&db, "");
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row.mode.as_deref(), Some("extractive"), "{row:?}");
    }
    let rows = summarize(&db, "{mode: 'auto'}");
    assert_eq!(rows[0].mode.as_deref(), Some("extractive"));
    assert_eq!(
        count(
            &db,
            "MATCH (c:Community {mode: 'extractive'}) RETURN count(c)"
        ),
        2
    );
}

#[test]
fn async_stores_a_community_job_and_writes_nothing_yet() {
    let db = plain();
    let res = run(
        &db,
        "CALL drevo.community.summarize({async: true, mode: 'extractive', layer: 'teams'}) \
         YIELD community, size, summary, keywords, mode, jobId \
         RETURN community, size, summary, keywords, mode, jobId",
    );
    assert_eq!(res.rows.len(), 1);
    let row = &res.rows[0];
    for v in &row[..5] {
        assert_eq!(*v, Value::Null, "{row:?}");
    }
    let job = string(&row[5]);
    // Without a scheduler the job waits, queued, with its one item.
    let res = run(
        &db,
        &format!(
            "MATCH (j:IngestJob {{id: '{job}'}})-[:HAS_ITEM]->(i:IngestItem) \
             RETURN j.pipeline, j.state, j.total, j.config, i.state, i.seq"
        ),
    );
    assert_eq!(res.rows.len(), 1, "{:?}", res.rows);
    let r = &res.rows[0];
    assert_eq!(string(&r[0]), "community");
    assert_eq!(string(&r[1]), "queued");
    assert_eq!(int(&r[2]), 1);
    let config: serde_json::Value = serde_json::from_str(&string(&r[3])).unwrap();
    assert_eq!(
        config,
        serde_json::json!({"mode": "extractive", "layer": "teams"}),
        "the job's config is the call's, less async"
    );
    assert_eq!(string(&r[4]), "queued");
    assert_eq!(int(&r[5]), 0);
    assert_eq!(count(&db, "MATCH (c:Community) RETURN count(c)"), 0);
}

#[test]
fn async_is_checked_like_a_synchronous_call() {
    let db = plain();
    let message = invalid_call(
        &db,
        "CALL drevo.community.summarize({async: true, levels: 2})",
        "drevo.community.summarize",
    );
    assert!(
        message.starts_with("unknown config key `levels`"),
        "{message}"
    );
    summarize(&db, "{mode: 'extractive'}");
    let message = invalid_call(
        &db,
        "CALL drevo.community.summarize({async: true, replace: false})",
        "drevo.community.summarize",
    );
    assert_eq!(
        message,
        "layer 'default' already has 2 communities; pass replace: true to recompute it, \
         or name another layer"
    );
    assert!(matches!(
        try_run(
            &db,
            "CALL drevo.community.summarize({async: true, mode: 'llm'})"
        ),
        Err(ExecError::EngineCapability { .. })
    ));
    assert_eq!(count(&db, "MATCH (j:IngestJob) RETURN count(j)"), 0);
}

#[test]
fn the_sync_llm_limit_is_read_from_the_environment() {
    use drevo::server::Config;
    let config = |raw: Option<&str>| {
        let raw = raw.map(str::to_string);
        Config::from_env(move |k| match k {
            "DREVO_COMMUNITY_MAX_LLM_SYNC" => raw.clone(),
            _ => None,
        })
    };
    assert_eq!(config(None).unwrap().community_max_llm_sync, 20);
    assert_eq!(config(Some("5")).unwrap().community_max_llm_sync, 5);
    for raw in ["0", "-1", "many"] {
        assert_eq!(
            config(Some(raw)).unwrap_err().to_string(),
            format!(
                "invalid DREVO_COMMUNITY_MAX_LLM_SYNC value `{raw}`: expected a whole number \
                 of communities, at least 1"
            )
        );
    }
}

#[test]
fn a_synchronous_row_has_a_null_job_id() {
    let db = plain();
    let res = run(
        &db,
        "CALL drevo.community.summarize({mode: 'extractive'}) YIELD jobId RETURN jobId",
    );
    assert_eq!(res.rows.len(), 2);
    assert!(
        res.rows.iter().all(|r| r[0] == Value::Null),
        "{:?}",
        res.rows
    );
}

#[test]
fn summarize_config_errors_name_the_key() {
    let db = plain();
    for (config, expected) in [
        (
            "{levels: 2}",
            "unknown config key `levels`; expected algorithm, resolution, labels, relTypes, \
             minSize, maxMembers, mode, model, tokenBudget, replace, layer, async",
        ),
        (
            "{algorithm: 'leiden'}",
            "algorithm must be 'louvain' or 'wcc', got 'leiden'",
        ),
        ("{minSize: 0}", "minSize must be an integer ≥ 1, got 0"),
        ("'fast'", "the config must be a map, got String"),
    ] {
        let message = invalid_call(
            &db,
            &format!("CALL drevo.community.summarize({config})"),
            "drevo.community.summarize",
        );
        assert_eq!(message, expected, "{config}");
    }
}

#[test]
fn an_empty_graph_has_no_communities() {
    let db = NativeService::in_memory();
    assert!(summarize(&db, "{mode: 'extractive'}").is_empty());
    let none = run(
        &db,
        "CALL drevo.community.query('anything', 3) YIELD community RETURN community",
    );
    assert!(none.rows.is_empty());
}

// ── query ────────────────────────────────────────────────────────────

/// `drevo.community.query` rows as (id, score, summary).
fn query(db: &NativeService, q: &str, k: i64, config: &str) -> Vec<(String, f64, String)> {
    run(
        db,
        &format!(
            "CALL drevo.community.query('{q}', {k}, {config}) YIELD community, score, summary \
             RETURN community.id, score, summary"
        ),
    )
    .rows
    .iter()
    .map(|r| (string(&r[0]), float(&r[1]), string(&r[2])))
    .collect()
}

#[test]
fn query_ranks_the_mobile_community_first() {
    let db = plain();
    summarize(&db, "{mode: 'extractive'}");
    let found = query(&db, "mobile release", 2, "null");
    assert_eq!(found[0].0, "default-1", "{found:?}");
    assert!(
        found[0].2.starts_with("6 members. Person: Dave"),
        "{found:?}"
    );
    // BM25 only: the best RRF score is one leg's first place.
    assert!((found[0].1 - 1.0 / 61.0).abs() < 1e-12, "{found:?}");
    let backend = query(&db, "login token", 1, "null");
    assert_eq!(backend.len(), 1);
    assert_eq!(backend[0].0, "default-0");
}

#[test]
fn query_stays_in_its_layer() {
    let db = plain();
    summarize(&db, "{mode: 'extractive'}");
    summarize(&db, "{mode: 'extractive', layer: 'fine', resolution: 2.0}");
    let fine = query(&db, "mobile release", 10, "{layer: 'fine'}");
    assert!(!fine.is_empty());
    assert!(
        fine.iter().all(|(id, _, _)| id.starts_with("fine-")),
        "{fine:?}"
    );
    let default = query(&db, "mobile release", 10, "null");
    assert!(
        default.iter().all(|(id, _, _)| id.starts_with("default-")),
        "{default:?}"
    );
}

#[test]
fn the_vector_leg_joins_when_communities_are_embedded() {
    let db = with_embedder();
    run(
        &db,
        "CALL drevo.semantic.register('Community', 'summary', 'embedding', 'auto')",
    );
    summarize(&db, "{mode: 'extractive'}");
    assert_eq!(
        count(
            &db,
            "MATCH (c:Community) WHERE c.embedding IS NOT NULL RETURN count(c)"
        ),
        2
    );
    let found = query(&db, "mobile release", 2, "null");
    assert_eq!(found[0].0, "default-1", "{found:?}");
    // First in both legs.
    assert!((found[0].1 - 2.0 / 61.0).abs() < 1e-12, "{found:?}");
}

#[test]
fn query_errors_name_the_argument() {
    let db = plain();
    for (call, expected) in [
        (
            "CALL drevo.community.query(5, 2)",
            "query must be a string, got Integer",
        ),
        (
            "CALL drevo.community.query('x', 0)",
            "k must be a positive integer, got 0",
        ),
        (
            "CALL drevo.community.query('x', 2, {k: 3})",
            "unknown config key `k`; expected layer, embeddingProperty, candidates, rrfK, mode",
        ),
    ] {
        assert_eq!(
            invalid_call(&db, call, "drevo.community.query"),
            expected,
            "{call}"
        );
    }
}

// ── of / stale ───────────────────────────────────────────────────────

#[test]
fn of_names_a_node_s_community_per_layer() {
    let db = plain();
    summarize(&db, "{mode: 'extractive'}");
    summarize(&db, "{mode: 'extractive', layer: 'fine', resolution: 2.0}");
    let rows = run(
        &db,
        "MATCH (a:Person {name: 'Alice'}) CALL drevo.community.of(a) YIELD community, level \
         RETURN community.id, level",
    )
    .rows;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(string(&rows[0][0]), "default-0");
    assert_eq!(string(&rows[0][1]), "default");
    assert_eq!(string(&rows[1][1]), "fine");
    // By id, too.
    let by_id = run(
        &db,
        "MATCH (d:Person {name: 'Dave'}) CALL drevo.community.of(id(d)) YIELD community, level \
         WHERE level = 'default' RETURN community.id",
    )
    .rows;
    assert_eq!(string(&by_id[0][0]), "default-1");
    // A message is in no community; null gives no rows.
    assert!(run(
        &db,
        "MATCH (m:Message) CALL drevo.community.of(m) YIELD community RETURN community"
    )
    .rows
    .is_empty());
    assert!(run(
        &db,
        "CALL drevo.community.of(null) YIELD community RETURN community"
    )
    .rows
    .is_empty());
    assert_eq!(
        invalid_call(
            &db,
            "CALL drevo.community.of('Alice')",
            "drevo.community.of"
        ),
        "node must be a node or a node id, got 'Alice'"
    );
    assert_eq!(
        invalid_call(&db, "CALL drevo.community.of(999999)", "drevo.community.of"),
        "no node with id 999999"
    );
}

/// `drevo.community.stale()` as id → (size, changed).
fn stale(db: &NativeService) -> BTreeMap<String, (i64, bool)> {
    run(
        db,
        "CALL drevo.community.stale() YIELD community, size, changed \
         RETURN community.id, size, changed",
    )
    .rows
    .iter()
    .map(|r| {
        let changed = match r[2] {
            Value::Bool(b) => b,
            ref other => panic!("changed: {other:?}"),
        };
        (string(&r[0]), (int(&r[1]), changed))
    })
    .collect()
}

#[test]
fn stale_reports_a_community_whose_members_changed() {
    let db = plain();
    summarize(&db, "{mode: 'extractive'}");
    assert_eq!(
        stale(&db),
        BTreeMap::from([
            ("default-0".to_string(), (6, false)),
            ("default-1".to_string(), (6, false)),
        ])
    );
    // A new backend ticket joins the backend team's community.
    run(
        &db,
        "MATCH (a:Person {name: 'Alice'}) \
         CREATE (a)-[:WORKS_ON]->(:Ticket {title: 'BE-160 Cache warm-up'})",
    );
    // A new stand-up message is bookkeeping, not a member.
    run(
        &db,
        "MATCH (d:Person {name: 'Dave'}) CREATE (:Message {text: 'hi'})-[:ABOUT]->(d)",
    );
    assert_eq!(
        stale(&db),
        BTreeMap::from([
            ("default-0".to_string(), (7, true)),
            ("default-1".to_string(), (6, false)),
        ])
    );
    // A member leaves the mobile team.
    run(&db, "MATCH (f:Person {name: 'Frank'}) DETACH DELETE f");
    assert_eq!(stale(&db)["default-1"], (5, true));
    // Recomputing makes everything fresh again.
    summarize(&db, "{mode: 'extractive'}");
    assert!(stale(&db).values().all(|(_, changed)| !changed));
}

// ── rag scope ────────────────────────────────────────────────────────

/// `drevo.rag.context` as (text, scope).
fn context(db: &NativeService, q: &str, config: &str) -> (String, String) {
    let row = run(
        db,
        &format!("CALL drevo.rag.context('{q}', {config}) YIELD text, scope RETURN text, scope"),
    )
    .rows[0]
        .clone();
    (string(&row[0]), string(&row[1]))
}

#[test]
fn global_context_starts_with_community_summaries() {
    let db = plain();
    let rows = summarize(&db, "{mode: 'extractive'}");
    let mobile = rows[1].summary.clone().unwrap();
    let (text, scope) = context(
        &db,
        "mobile release",
        "{scope: 'global', format: 'triples', k: 2}",
    );
    assert_eq!(scope, "global");
    assert!(
        text.starts_with(&format!("community default-1 | Community | {mobile}\n")),
        "{text}"
    );
    assert!(
        text.contains("community default-0 | Community | 6 members. Person: Alice"),
        "{text}"
    );
    assert!(text.contains("Dave | Person | Mobile lead\n"), "{text}");
    assert!(
        text.contains("Dave | MEMBER_OF | community default-1"),
        "{text}"
    );
    // The members follow both summaries.
    let first_member = text.find("Dave | Person").unwrap();
    let second_summary = text.find("community default-0 | Community").unwrap();
    assert!(second_summary < first_member, "{text}");
}

#[test]
fn global_retrieve_returns_communities_then_their_members() {
    let db = plain();
    summarize(&db, "{mode: 'extractive'}");
    let rows = run(
        &db,
        "CALL drevo.rag.retrieve('mobile release', {scope: 'global', k: 1}) \
         YIELD node, seedScore, distance, pprScore, rank, scope \
         RETURN coalesce(node.id, node.name, node.title), seedScore, distance, pprScore, rank, scope",
    )
    .rows;
    assert_eq!(rows.len(), 7, "{rows:?}");
    assert_eq!(string(&rows[0][0]), "default-1");
    assert!(float(&rows[0][1]) > 0.0);
    assert_eq!(int(&rows[0][2]), 0);
    assert_eq!(string(&rows[1][0]), "Dave");
    assert_eq!(rows[1][1], Value::Null);
    assert_eq!(int(&rows[1][2]), 1);
    assert!(float(&rows[1][3]) >= float(&rows[2][3]), "{rows:?}");
    assert_eq!(int(&rows[6][4]), 7);
    assert!(rows.iter().all(|r| string(&r[5]) == "global"));
}

#[test]
fn a_question_matching_no_community_gets_the_largest_ones() {
    let db = plain();
    summarize(&db, "{mode: 'extractive', labels: ['Person']}");
    let (text, scope) = context(
        &db,
        "zzzz qqqq",
        "{scope: 'global', format: 'triples', k: 2}",
    );
    assert_eq!(scope, "global");
    assert!(
        text.starts_with("community default-0 | Community | 3 members."),
        "{text}"
    );
    assert!(
        text.contains("community default-1 | Community | 3 members."),
        "{text}"
    );
}

#[test]
fn global_without_communities_is_empty_and_auto_falls_back_to_local() {
    let db = plain();
    let (text, scope) = context(&db, "mobile release", "{scope: 'global'}");
    assert_eq!((text.as_str(), scope.as_str()), ("", "global"));
    // The hint matches, but there is no community to answer from.
    let (text, scope) = context(
        &db,
        "Mobile release notes",
        "{scope: 'auto', globalHint: '^mobile'}",
    );
    assert_eq!(scope, "local");
    assert!(text.contains("MO-12 Mobile release notes"), "{text}");
}

#[test]
fn auto_picks_global_for_an_overview_and_local_for_a_ticket() {
    let db = with_embedder();
    summarize(&db, "{mode: 'extractive'}");
    let (text, scope) = context(
        &db,
        "What is this project about",
        "{scope: 'auto', label: 'Ticket', format: 'triples'}",
    );
    assert_eq!(scope, "global");
    assert!(text.starts_with("community default-"), "{text}");
    // A ticket id found by both legs scores 2/61 ≈ 0.033 ≥ 0.02: local.
    let (text, scope) = context(
        &db,
        "BE-142 login token",
        "{scope: 'auto', label: 'Ticket', format: 'triples'}",
    );
    assert_eq!(scope, "local");
    assert!(
        text.starts_with("BE-142 Login token expires early | Ticket"),
        "{text}"
    );
    // A weak match (one leg, 1/61 ≈ 0.016 < 0.02) goes global …
    let (_, scope) = context(&db, "zebra quokka", "{scope: 'auto', label: 'Ticket'}");
    assert_eq!(scope, "global");
    // … unless the threshold says otherwise; and the hint can be changed.
    let (_, scope) = context(
        &db,
        "zebra quokka",
        "{scope: 'auto', label: 'Ticket', globalThreshold: 0.01}",
    );
    assert_eq!(scope, "local");
    let (_, scope) = context(
        &db,
        "What is this project about",
        "{scope: 'auto', label: 'Ticket', globalHint: '^overview', globalThreshold: 0.0}",
    );
    assert_eq!(scope, "local");
}

#[test]
fn with_bm25_alone_auto_ignores_the_threshold() {
    // One leg's first place scores 1/61 < 0.02, but a single BM25 leg says
    // nothing about confidence: a query with hits stays local …
    let db = plain();
    summarize(&db, "{mode: 'extractive'}");
    let (text, scope) = context(
        &db,
        "BE-142 login token",
        "{scope: 'auto', format: 'triples'}",
    );
    assert_eq!(scope, "local");
    assert!(
        text.starts_with("BE-142 Login token expires early | Ticket"),
        "{text}"
    );
    // … whatever the threshold says …
    let (_, scope) = context(
        &db,
        "BE-142 login token",
        "{scope: 'auto', globalThreshold: 0.9}",
    );
    assert_eq!(scope, "local");
    // … and a query with no seed at all goes global.
    let (text, scope) = context(&db, "zebra quokka", "{scope: 'auto', format: 'triples'}");
    assert_eq!(scope, "global");
    assert!(text.starts_with("community default-"), "{text}");
}

#[test]
fn community_layer_picks_the_layer_the_global_scope_reads() {
    let db = plain();
    summarize(&db, "{mode: 'extractive'}");
    summarize(
        &db,
        "{mode: 'extractive', layer: 'people', labels: ['Person']}",
    );
    let (text, scope) = context(
        &db,
        "mobile release",
        "{scope: 'global', format: 'triples', communityLayer: 'people'}",
    );
    assert_eq!(scope, "global");
    assert!(text.starts_with("community people-"), "{text}");
    assert!(!text.contains("community default-"), "{text}");
    assert!(!text.contains("| Ticket"), "{text}");
    let (text, _) = context(
        &db,
        "mobile release",
        "{scope: 'global', format: 'triples'}",
    );
    assert!(
        text.starts_with("community default-1 | Community"),
        "{text}"
    );
    let rows = run(
        &db,
        "CALL drevo.rag.retrieve('mobile release', {scope: 'global', communityLayer: 'people', k: 1}) \
         YIELD node RETURN node.id",
    )
    .rows;
    assert!(string(&rows[0][0]).starts_with("people-"), "{rows:?}");
    // A layer with no community gives an empty global context.
    let (text, _) = context(
        &db,
        "mobile release",
        "{scope: 'global', communityLayer: 'none'}",
    );
    assert_eq!(text, "");
}

#[test]
fn stale_reuses_the_labels_a_layer_was_computed_with() {
    let db = NativeService::in_memory();
    run(
        &db,
        "CREATE (a:Task {title: 'T-1 Plan sprint'}), (b:Task {title: 'T-2 Review backlog'}), \
                (c:Task {title: 'T-3 Groom tickets'}), \
                (a)-[:BLOCKS]->(b), (b)-[:BLOCKS]->(c), (a)-[:BLOCKS]->(c)",
    );
    summarize(
        &db,
        "{mode: 'extractive', labels: ['Task'], layer: 'tasks'}",
    );
    let config =
        string(&run(&db, "MATCH (c:Community {layer: 'tasks'}) RETURN c.config").rows[0][0]);
    assert_eq!(config, r#"{"labels":["Task"],"relTypes":null}"#);
    // A person owning a task is outside the layer's labels: not a member.
    run(
        &db,
        "MATCH (t:Task {title: 'T-1 Plan sprint'}) CREATE (:Person {name: 'Gail'})-[:OWNS]->(t)",
    );
    assert_eq!(
        stale(&db),
        BTreeMap::from([("tasks-0".to_string(), (3, false))])
    );
    // A new task blocking one is.
    run(
        &db,
        "MATCH (t:Task {title: 'T-3 Groom tickets'}) \
         CREATE (:Task {title: 'T-4 Retro'})-[:BLOCKS]->(t)",
    );
    assert_eq!(
        stale(&db),
        BTreeMap::from([("tasks-0".to_string(), (4, true))])
    );
}

#[test]
fn local_is_the_default_scope_and_skips_communities() {
    let db = plain();
    summarize(&db, "{mode: 'extractive'}");
    let (text, scope) = context(&db, "mobile release", "null");
    assert_eq!(scope, "local");
    assert!(!text.contains("Community"), "{text}");
    let rows = run(
        &db,
        "CALL drevo.rag.retrieve('mobile release') YIELD node, scope \
         RETURN labels(node), scope",
    )
    .rows;
    assert!(!rows.is_empty());
    for r in rows {
        assert_eq!(string(&r[1]), "local");
        assert!(!strings(&r[0]).contains(&"Community".to_string()), "{r:?}");
    }
}

#[test]
fn a_context_trace_records_the_scope() {
    let db = plain();
    summarize(&db, "{mode: 'extractive'}");
    run(
        &db,
        "CALL drevo.rag.context('mobile release', {scope: 'global', trace: true}) YIELD traceId \
         RETURN traceId",
    );
    run(
        &db,
        "CALL drevo.rag.context('mobile release', {trace: true}) YIELD traceId RETURN traceId",
    );
    let scopes: Vec<String> = run(
        &db,
        "MATCH (t:ContextTrace) RETURN t.scope ORDER BY t.scope",
    )
    .rows
    .iter()
    .map(|r| string(&r[0]))
    .collect();
    assert_eq!(scopes, ["global", "local"]);
    // A replay of the global trace is global again.
    let id = string(&run(&db, "MATCH (t:ContextTrace {scope: 'global'}) RETURN t.id").rows[0][0]);
    let replayed = run(
        &db,
        &format!("CALL drevo.rag.replay('{id}') YIELD changed RETURN changed"),
    )
    .rows;
    assert_eq!(replayed[0][0], Value::Bool(false));
}

#[test]
fn scope_config_errors_name_the_key() {
    let db = plain();
    let message = match try_run(&db, "CALL drevo.rag.context('x', {scope: 'galaxy'})") {
        Err(ExecError::InvalidProcedureCall { message, .. }) => message,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        message,
        "scope must be 'local', 'global' or 'auto', got 'galaxy'"
    );
}

// ── Bolt ─────────────────────────────────────────────────────────────

fn bolt_records(db: &Arc<NativeService>, query: &str) -> Vec<Vec<Bolt>> {
    let mut s = Session::new_durable(Arc::clone(db));
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });
    let mut replies = s.handle(ClientMessage::Run {
        query: query.to_string(),
        parameters: BTreeMap::new(),
        extra: BTreeMap::new(),
    });
    replies.extend(s.handle(ClientMessage::Pull {
        extra: BTreeMap::new(),
    }));
    replies
        .into_iter()
        .filter_map(|m| match m {
            ServerMessage::Record { fields } => Some(fields),
            _ => None,
        })
        .collect()
}

#[test]
fn bolt_round_trip_types() {
    let db = Arc::new(plain());
    let rows = bolt_records(
        &db,
        "CALL drevo.community.summarize({mode: 'extractive'}) \
         YIELD community, size, summary, keywords, mode \
         RETURN community, size, summary, keywords, mode",
    );
    assert_eq!(rows.len(), 2, "{rows:?}");
    let row = &rows[0];
    assert!(matches!(row[0], Bolt::Structure { .. }), "{:?}", row[0]);
    assert_eq!(row[1], Bolt::Integer(6));
    assert!(
        matches!(&row[2], Bolt::String(s) if s.starts_with("6 members.")),
        "{:?}",
        row[2]
    );
    assert!(
        matches!(&row[3], Bolt::List(k) if !k.is_empty()),
        "{:?}",
        row[3]
    );
    assert_eq!(row[4], Bolt::String("extractive".to_string()));

    let rows = bolt_records(
        &db,
        "CALL drevo.community.query('mobile release', 1) YIELD community, score, summary \
         RETURN community, score, summary",
    );
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][1], Bolt::Float(_)), "{:?}", rows[0][1]);

    let rows = bolt_records(
        &db,
        "CALL drevo.community.stale() YIELD community, size, changed RETURN size, changed",
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], Bolt::Boolean(false));

    let rows = bolt_records(
        &db,
        "CALL drevo.rag.context('mobile release', {scope: 'global'}) YIELD scope RETURN scope",
    );
    assert_eq!(rows[0][0], Bolt::String("global".to_string()));
}

// ── #804: the LLM summariser, async and the sync cap ─────────────────

#[cfg(feature = "embeddings-proxy")]
mod llm {
    use super::*;

    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use axum::extract::State;
    use axum::routing::post;
    use axum::{Json, Router};
    use drevo::extract::settings::{install_config, ExtractConfig};
    use serde_json::json;

    /// What the fake model answers to every request.
    const LLM_SUMMARY: &str = "A team that ships its tickets together (summary by the fake model).";

    /// What the fake upstream shares with its handler.
    #[derive(Default)]
    struct Shared {
        calls: AtomicUsize,
        /// While set, every request waits (Hang mode).
        hang: AtomicBool,
    }

    async fn chat(
        State(shared): State<Arc<Shared>>,
        Json(_body): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        shared.calls.fetch_add(1, Ordering::SeqCst);
        while shared.hang.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Json(json!({
            "choices": [{"message": {"role": "assistant", "content": LLM_SUMMARY}}]
        }))
    }

    /// A fake OpenAI-compatible chat upstream, installed as the extractor's
    /// chat model (as `DREVO_EXTRACT_UPSTREAM` / `POST /config/extract`
    /// would) until dropped.
    struct Upstream {
        shared: Arc<Shared>,
        _runtime: tokio::runtime::Runtime,
    }

    impl Upstream {
        fn start() -> Self {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let shared = Arc::new(Shared::default());
            let state = Arc::clone(&shared);
            let addr: SocketAddr = runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let app = Router::new()
                    .route("/v1/chat/completions", post(chat))
                    .with_state(state);
                tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
                addr
            });
            install_config(&ExtractConfig {
                upstream: format!("http://{addr}/v1/chat/completions"),
                api_key: Some("sk-test".into()),
                model: "fake-model".into(),
                timeout_ms: 10_000,
            })
            .unwrap();
            Self {
                shared,
                _runtime: runtime,
            }
        }

        fn calls(&self) -> usize {
            self.shared.calls.load(Ordering::SeqCst)
        }

        /// Hang mode: every request waits until [`Self::release`].
        fn hang(&self) {
            self.shared.hang.store(true, Ordering::SeqCst);
        }

        fn release(&self) {
            self.shared.hang.store(false, Ordering::SeqCst);
        }

        /// Wait (at most 10 s) until the model has been asked.
        fn wait_for_a_call(&self) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while self.calls() == 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(self.calls() > 0, "the model was never asked");
        }
    }

    impl Drop for Upstream {
        fn drop(&mut self) {
            drevo::extract::uninstall();
        }
    }

    /// Serialises the tests of this module: the chat model is process-wide.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn llm_mode_uses_the_configured_chat_upstream() {
        let _s = serial();
        let llm = Upstream::start();
        let db = plain();
        let rows = try_summarize(&db, "{mode: 'llm'}").unwrap_or_else(|e| {
            panic!(
                "mode: 'llm' failed although a chat upstream is configured \
                 (community_llm_summarizer always returns None, #804): {e}"
            )
        });
        assert_eq!(rows.len(), 2, "{rows:?}");
        for row in &rows {
            assert_eq!(row.mode.as_deref(), Some("llm"), "{row:?}");
            assert_eq!(row.summary.as_deref(), Some(LLM_SUMMARY), "{row:?}");
            assert!(!row.keywords.is_empty(), "extractive keywords: {row:?}");
        }
        assert!(llm.calls() >= 2, "{} model calls", llm.calls());
        assert_eq!(
            count(&db, "MATCH (c:Community {mode: 'llm'}) RETURN count(c)"),
            2
        );
    }

    #[test]
    fn auto_mode_prefers_the_llm_when_an_upstream_is_configured() {
        let _s = serial();
        let llm = Upstream::start();
        let db = plain();
        let rows = summarize(&db, "{mode: 'auto'}");
        assert_eq!(rows.len(), 2, "{rows:?}");
        let modes: Vec<Option<String>> = rows.iter().map(|r| r.mode.clone()).collect();
        assert!(
            modes.iter().all(|m| m.as_deref() == Some("llm")),
            "mode: 'auto' wrote {modes:?} summaries although a chat upstream is configured \
             ({} model calls): it should prefer the LLM (#804)",
            llm.calls()
        );
    }

    #[test]
    fn async_summarize_returns_a_job_id_that_finishes() {
        let _s = serial();
        drevo::extract::uninstall();
        let db = Arc::new(plain());
        db.start_ingest();
        struct Stop<'a>(&'a NativeService);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.stop_ingest();
            }
        }
        let _stop = Stop(&db);
        let res = try_run(
            &db,
            "CALL drevo.community.summarize({async: true, mode: 'extractive'})",
        )
        .unwrap_or_else(|e| {
            panic!(
                "async: true was refused although ingest jobs (#788) exist: it should submit \
                 a job and return its jobId (#804): {e}"
            )
        });
        let column = res
            .columns
            .iter()
            .position(|c| c == "jobId")
            .unwrap_or_else(|| panic!("no jobId column in {:?} (#804)", res.columns));
        let job = string(&res.rows[0][column]);
        let state = || -> String {
            let res = db
                .execute(
                    &parse("CALL drevo.ingest.status($job) YIELD job RETURN job.state").unwrap(),
                    HashMap::from([("job".to_string(), Value::String(job.clone()))]),
                )
                .unwrap_or_else(|e| panic!("status of {job}: {e}"));
            string(&res.rows[0][0])
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut last = state();
        while last != "done" && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            last = state();
        }
        assert_eq!(last, "done", "the async summarize job {job} did not finish");
        assert_eq!(
            count(
                &db,
                "MATCH (c:Community {mode: 'extractive'}) RETURN count(c)"
            ),
            2
        );
    }

    /// `teams` separate three-person teams: one community each.
    fn many_teams(db: &NativeService, teams: i64) {
        run(
            db,
            &format!(
                "UNWIND range(1, {teams}) AS i \
                 CREATE (a:Person {{name: 'Ann ' + toString(i), description: 'team lead'}}), \
                        (b:Person {{name: 'Ben ' + toString(i), description: 'engineer'}}), \
                        (c:Person {{name: 'Cid ' + toString(i), description: 'tester'}}), \
                        (a)-[:MENTORS]->(b), (b)-[:WORKS_WITH]->(c), (c)-[:WORKS_WITH]->(a)"
            ),
        );
    }

    #[test]
    fn the_sync_llm_cap_is_enforced() {
        let _s = serial();
        // 21 communities: one more than the default cap of 20.
        let probe = NativeService::in_memory();
        many_teams(&probe, 21);
        assert_eq!(summarize(&probe, "{mode: 'extractive'}").len(), 21);

        let llm = Upstream::start();
        let db = NativeService::in_memory();
        many_teams(&db, 21);
        let res = try_run(&db, "CALL drevo.community.summarize({mode: 'llm'})");
        let got = match &res {
            Ok(r) => format!("{} rows", r.rows.len()),
            Err(e) => format!("{e:?}"),
        };
        assert!(
            res.is_err() && got.contains("async: true"),
            "a synchronous mode: 'llm' over 21 communities (more than the default \
             DREVO_COMMUNITY_MAX_LLM_SYNC = 20) was not refused with a pointer to \
             `async: true` (#804); got: {got}"
        );
        assert_eq!(
            llm.calls(),
            0,
            "the model was called before the cap refused"
        );
        assert_eq!(count(&db, "MATCH (c:Community) RETURN count(c)"), 0);
    }

    /// Whether `work`, run on a thread of its own, ends within 2 s.
    fn ends_within_two_seconds(work: impl FnOnce() + Send + 'static) -> bool {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            work();
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(2)).is_ok()
    }

    #[test]
    fn community_llm_summary_does_not_hold_the_writer_lock_or_the_scheduler_gate() {
        let _s = serial();
        let llm = Upstream::start();

        // A synchronous call: while the model is asked, others write.
        let db = Arc::new(plain());
        llm.hang();
        let summarizing = {
            let db = Arc::clone(&db);
            std::thread::spawn(move || {
                try_run(&db, "CALL drevo.community.summarize({mode: 'llm'})")
                    .map(|r| r.rows.len())
                    .map_err(|e| e.to_string())
            })
        };
        llm.wait_for_a_call();
        let writer = Arc::clone(&db);
        let wrote = ends_within_two_seconds(move || {
            run(&writer, "CREATE (:X)");
        });
        llm.release();
        let summarized = summarizing.join().unwrap();
        assert!(
            wrote,
            "a CREATE waited while drevo.community.summarize({{mode: 'llm'}}) asked the \
             model: the model ran under the writer lock (#804)"
        );
        // The new node may land in the layer as a community of its own,
        // too small for a summary.
        assert!(matches!(summarized, Ok(n) if n >= 2), "{summarized:?}");
        assert_eq!(
            llm.calls(),
            2,
            "one model call per community, none in the write"
        );
        assert_eq!(
            count(&db, "MATCH (c:Community {mode: 'llm'}) RETURN count(c)"),
            2
        );
        assert_eq!(count(&db, "MATCH (x:X) RETURN count(x)"), 1);

        // A community job: while its model call hangs, the scheduler stops.
        let db = Arc::new(plain());
        db.start_ingest();
        llm.hang();
        let before = llm.calls();
        let res = run(
            &db,
            "CALL drevo.community.summarize({async: true, mode: 'llm'}) YIELD jobId \
             RETURN jobId",
        );
        let job = string(&res.rows[0][0]);
        let deadline = Instant::now() + Duration::from_secs(10);
        while llm.calls() == before && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(llm.calls() > before, "the job {job} never asked the model");
        let stopping = Arc::clone(&db);
        let stopped = ends_within_two_seconds(move || stopping.stop_ingest());
        llm.release();
        assert!(
            stopped,
            "stop_ingest() waited for a community job's model call: the item held the \
             scheduler gate while the model answered (#804)"
        );
        db.stop_ingest();
    }
}
