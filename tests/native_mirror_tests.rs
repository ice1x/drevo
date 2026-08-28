//! Engine-flip slice A guards (RFC `docs/rfc-native-core.md` #307, Phase 6):
//! the mutation epoch, the read-only classifier, and the `NativeMirror`
//! router — writes always land on KV, reads are served natively only while
//! the mirror is provably fresh, and a stale mirror can only cost speed,
//! never answers.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use drevo::cypher::executor::{execute, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::cypher::read_only::mirror_can_serve;
use drevo::db::Drevo;
use drevo::native_mirror::NativeMirror;

fn run_kv(db: &Drevo, source: &str) -> ExecResult {
    let q = parse(source).expect("parse");
    execute(&q, db, HashMap::new()).expect("kv execute")
}

fn run_mirror(mirror: &Arc<NativeMirror>, db: &Arc<Drevo>, source: &str) -> ExecResult {
    let q = parse(source).expect("parse");
    mirror
        .execute(db, &q, HashMap::new())
        .expect("mirror execute")
}

fn titles(result: &ExecResult) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::String(s) => s.clone(),
            other => panic!("expected string title, got {other:?}"),
        })
        .collect()
}

fn seeded_db() -> Arc<Drevo> {
    let db = Drevo::open_in_memory().expect("open");
    run_kv(
        &db,
        "CREATE (:Person {title: 'ada', age: 36}), (:Person {title: 'bob', age: 41}), \
         (:City {title: 'paris'})",
    );
    run_kv(
        &db,
        "MATCH (a {title: 'ada'}), (p {title: 'paris'}) CREATE (a)-[:LIVES_IN]->(p)",
    );
    Arc::new(db)
}

// ── mutation epoch ─────────────────────────────────────────────────────

#[test]
fn writes_bump_the_mutation_epoch_and_reads_do_not() {
    let db = Drevo::open_in_memory().expect("open");
    let e0 = db.mutation_epoch();
    run_kv(&db, "CREATE (:Person {title: 'ada'})");
    let e1 = db.mutation_epoch();
    assert!(e1 > e0, "a create must bump the epoch ({e0} -> {e1})");

    run_kv(&db, "MATCH (n) RETURN n.title");
    assert_eq!(db.mutation_epoch(), e1, "reads must not bump the epoch");

    run_kv(&db, "MATCH (n {title: 'ada'}) SET n.age = 36");
    let e2 = db.mutation_epoch();
    assert!(e2 > e1, "a property update must bump the epoch");

    run_kv(&db, "MATCH (n {title: 'ada'}) DELETE n");
    assert!(db.mutation_epoch() > e2, "a delete must bump the epoch");
}

#[test]
fn edge_writes_bump_the_mutation_epoch() {
    let db = seeded_db();
    let before = db.mutation_epoch();
    run_kv(
        &db,
        "MATCH (a {title: 'bob'}), (p {title: 'paris'}) CREATE (a)-[:LIVES_IN]->(p)",
    );
    assert!(db.mutation_epoch() > before);
}

#[test]
fn export_dump_consistent_stamps_the_live_epoch() {
    let db = seeded_db();
    let (dump, epoch) = db.export_dump_consistent().expect("export");
    assert_eq!(dump.nodes.len(), 3);
    assert_eq!(dump.edges.len(), 1);
    assert_eq!(
        epoch,
        db.mutation_epoch(),
        "with no concurrent writers the stamp is the live epoch"
    );
}

// ── read-only classifier ───────────────────────────────────────────────

#[test]
fn classifier_accepts_reads_the_mirror_can_serve() {
    for source in [
        "MATCH (n) RETURN n",
        "MATCH (n:Person) WHERE n.age > 1 WITH n RETURN count(*)",
        "UNWIND [1, 2] AS x RETURN x",
        "MATCH (n) RETURN n.title UNION MATCH (m) RETURN m.title",
        "CALL db.labels()",
        "CALL db.relationshipTypes()",
        "CALL db.propertyKeys()",
        "CALL drevo.info()",
    ] {
        let q = parse(source).expect("parse");
        assert!(mirror_can_serve(&q), "expected mirrorable: {source}");
    }
}

#[test]
fn classifier_rejects_writes_and_non_mirror_procedures() {
    for source in [
        "CREATE (:Person {title: 'x'})",
        "MERGE (:Person {title: 'x'})",
        "MATCH (n) SET n.age = 1",
        "MATCH (n) REMOVE n.age",
        "MATCH (n) DETACH DELETE n",
        "FOREACH (x IN [1] | CREATE (:Marker))",
        "MATCH (n) RETURN n UNION CREATE (:X) RETURN 1",
        "CALL fts.search('ada')",
        "CALL drevo.semantic.status()",
        "CALL drevo.vector.query([0.1], 1)",
    ] {
        let q = parse(source).expect("parse");
        assert!(!mirror_can_serve(&q), "expected KV-routed: {source}");
    }
}

// ── mirror routing ─────────────────────────────────────────────────────

#[test]
fn fresh_mirror_serves_reads_natively_with_kv_identical_rows() {
    let db = seeded_db();
    let mirror = Arc::new(NativeMirror::new());
    mirror.rebuild_blocking(&db).expect("rebuild");
    assert!(mirror.is_fresh(&db));

    for source in [
        "MATCH (n) RETURN n.title ORDER BY n.title",
        "MATCH (n:Person) RETURN n.title ORDER BY n.title",
        "MATCH (n {age: 36}) RETURN n.title",
        "MATCH (a)-[:LIVES_IN]->(b) RETURN a.title, b.title",
    ] {
        let kv = run_kv(&db, source);
        let native = run_mirror(&mirror, &db, source);
        assert_eq!(kv.rows, native.rows, "engines disagree on `{source}`");
    }
    let stats = mirror.stats();
    assert_eq!(
        stats.native_hits, 4,
        "all four reads must be served natively"
    );
    assert_eq!(stats.kv_fallbacks, 0);
}

#[test]
fn writes_route_to_kv_and_stale_reads_fall_back_with_correct_data() {
    let db = seeded_db();
    let mirror = Arc::new(NativeMirror::new());
    mirror.rebuild_blocking(&db).expect("rebuild");

    // A write through the mirror lands on KV (durable) and staleness is
    // detected immediately after.
    run_mirror(&mirror, &db, "CREATE (:Person {title: 'eve'})");
    assert_eq!(mirror.stats().kv_routed, 1, "the write must route to KV");
    assert!(
        !mirror.is_fresh(&db),
        "a completed write must stale the mirror"
    );

    // The very next read must already see the write (read-your-writes) —
    // served from KV while the mirror is stale.
    let rows = titles(&run_mirror(
        &mirror,
        &db,
        "MATCH (n:Person) RETURN n.title ORDER BY n.title",
    ));
    assert_eq!(rows, ["ada", "bob", "eve"]);
    assert!(mirror.stats().kv_fallbacks >= 1);
}

#[test]
fn rebuild_blocking_restores_native_serving_after_a_write() {
    let db = seeded_db();
    let mirror = Arc::new(NativeMirror::new());
    mirror.rebuild_blocking(&db).expect("rebuild");
    run_mirror(&mirror, &db, "MATCH (n {title: 'ada'}) SET n.age = 37");

    mirror.rebuild_blocking(&db).expect("rebuild after write");
    assert!(mirror.is_fresh(&db));
    let before = mirror.stats().native_hits;
    let rows = run_mirror(&mirror, &db, "MATCH (n {age: 37}) RETURN n.title");
    assert_eq!(titles(&rows), ["ada"]);
    assert_eq!(mirror.stats().native_hits, before + 1);
}

#[test]
fn background_rebuild_converges_after_a_write() {
    let db = seeded_db();
    let mirror = Arc::new(NativeMirror::new());
    mirror.rebuild_blocking(&db).expect("rebuild");
    run_mirror(&mirror, &db, "CREATE (:City {title: 'oslo'})");

    // The first stale read falls back to KV (correct data) and triggers the
    // background rebuild; serving must return to native within the deadline.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let rows = titles(&run_mirror(
            &mirror,
            &db,
            "MATCH (n:City) RETURN n.title ORDER BY n.title",
        ));
        assert_eq!(rows, ["oslo", "paris"], "every answer must be correct");
        if mirror.is_fresh(&db) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "background rebuild did not land within 10s: {:?}",
            mirror.stats()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let before = mirror.stats().native_hits;
    run_mirror(&mirror, &db, "MATCH (n:City) RETURN n.title");
    assert_eq!(mirror.stats().native_hits, before + 1);
    assert_eq!(mirror.stats().rebuild_errors, 0);
}

#[test]
fn empty_mirror_serves_from_kv_until_built() {
    let db = seeded_db();
    let mirror = Arc::new(NativeMirror::new());
    let rows = titles(&run_mirror(
        &mirror,
        &db,
        "MATCH (n) RETURN n.title ORDER BY n.title",
    ));
    assert_eq!(rows, ["ada", "bob", "paris"]);
    let stats = mirror.stats();
    assert_eq!(stats.native_hits, 0);
    assert_eq!(stats.kv_fallbacks, 1);
}

#[test]
fn non_mirror_procedures_execute_on_kv_through_the_mirror() {
    let db = seeded_db();
    let mirror = Arc::new(NativeMirror::new());
    mirror.rebuild_blocking(&db).expect("rebuild");
    // `fts.search` needs the KV secondary store — the classifier must route
    // it there even though the mirror is fresh.
    let result = run_mirror(&mirror, &db, "CALL fts.search('ada', 5)");
    assert!(
        !result.rows.is_empty(),
        "fts.search must find the seeded node via the KV path"
    );
    assert_eq!(mirror.stats().native_hits, 0);
    assert_eq!(mirror.stats().kv_routed, 1);
}

// ── drevo.engine.status observability ──────────────────────────────────

#[test]
fn engine_status_reports_kv_with_null_stats_when_no_mirror_is_attached() {
    let db = seeded_db();
    let result = run_kv(&db, "CALL drevo.engine.status()");
    assert_eq!(
        result.columns,
        [
            "engine",
            "mirror_fresh",
            "native_hits",
            "kv_fallbacks",
            "kv_routed",
            "rebuild_errors"
        ]
    );
    assert_eq!(
        result.rows,
        vec![vec![
            Value::String("kv".to_string()),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn engine_status_reports_the_attached_mirrors_live_counters() {
    use drevo::native_mirror::MirrorRegistry;

    let db = seeded_db();
    let registry = MirrorRegistry::new();
    let mirror = registry.for_db(&db);
    mirror.rebuild_blocking(&db).expect("rebuild");
    run_mirror(&mirror, &db, "MATCH (n) RETURN count(*)");

    // The status call itself is not mirrorable, so routing it through the
    // mirror counts as one more kv_routed — the counters it reports are
    // live, including itself.
    let result = run_mirror(&mirror, &db, "CALL drevo.engine.status()");
    let stats = mirror.stats();
    assert_eq!(
        result.rows,
        vec![vec![
            Value::String("native".to_string()),
            Value::Bool(true),
            Value::Integer(stats.native_hits as i64),
            Value::Integer(stats.kv_fallbacks as i64),
            Value::Integer(stats.kv_routed as i64),
            Value::Integer(stats.rebuild_errors as i64),
        ]]
    );
    assert_eq!(stats.native_hits, 1, "the count read was served natively");
    assert_eq!(stats.kv_routed, 1, "the status call routed to KV");

    // YIELD projection works like any procedure.
    let engine_only = run_mirror(
        &mirror,
        &db,
        "CALL drevo.engine.status() YIELD engine RETURN engine",
    );
    assert_eq!(
        engine_only.rows,
        vec![vec![Value::String("native".to_string())]]
    );

    // A write stales the mirror; the status reflects it immediately.
    run_mirror(&mirror, &db, "CREATE (:Person {title: 'zed'})");
    let stale = run_mirror(
        &mirror,
        &db,
        "CALL drevo.engine.status() YIELD mirror_fresh RETURN mirror_fresh",
    );
    assert_eq!(stale.rows, vec![vec![Value::Bool(false)]]);
}

#[test]
fn engine_status_requires_the_kv_path() {
    use drevo::cypher::executor::execute_on_engine;
    use drevo::native::NativeGraph;

    let graph = NativeGraph::new();
    let q = parse("CALL drevo.engine.status()").expect("parse");
    let err = execute_on_engine(&q, &graph, HashMap::new()).expect_err("no KV secondary");
    assert!(
        err.to_string().contains("drevo.engine.status"),
        "unexpected error: {err}"
    );
}
