//! Triggers (`apoc.trigger.*`) in an IT task manager: Cypher statements that
//! run when a transaction commits and see what it changed.
//!
//! - `before` triggers run inside the transaction, so their writes commit
//!   with it, and a failing one rolls the whole transaction back;
//! - `after` triggers run right after the commit in a transaction of their
//!   own; `afterAsync` ones off the committing thread;
//! - a trigger's own writes do not fire triggers;
//! - definitions are kept per database and survive a restart.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native::CommitError;
use drevo::native_service::NativeService;

fn run(svc: &NativeService, q: &str) -> Result<ExecResult, ExecError> {
    svc.execute(&parse(q).expect("parse"), HashMap::new())
}

fn ok(svc: &NativeService, q: &str) -> ExecResult {
    run(svc, q).unwrap_or_else(|e| panic!("`{q}`: {e}"))
}

fn int(svc: &NativeService, q: &str) -> i64 {
    match ok(svc, q).rows.first().and_then(|r| r.first()) {
        Some(Value::Integer(n)) => *n,
        other => panic!("`{q}` gave {other:?}"),
    }
}

fn strings(result: &ExecResult, column: usize) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|r| match &r[column] {
            Value::String(s) => s.clone(),
            other => panic!("expected a string, got {other:?}"),
        })
        .collect()
}

fn install(svc: &NativeService, name: &str, statement: &str, phase: &str) {
    let q = format!(
        "CALL apoc.trigger.install('drevo', '{name}', \"{statement}\", {{phase: '{phase}'}})"
    );
    ok(svc, &q);
}

/// Stamps `updated_at` on every task whose properties a transaction set.
const STAMP_UPDATED_AT: &str = "UNWIND keys($assignedNodeProperties) AS k \
     UNWIND $assignedNodeProperties[k] AS change \
     WITH k, change WHERE k <> 'updated_at' \
     WITH DISTINCT change.node AS n WHERE n:Task \
     SET n.updated_at = 1000";

/// Writes an `Audit` node per created task.
const AUDIT_CREATED: &str = "UNWIND $createdNodes AS n WITH n WHERE n:Task \
     CREATE (:Audit {task: n.title, event: 'created'})";

#[test]
fn installed_triggers_are_listed_with_their_phase() {
    let svc = NativeService::in_memory();
    let rows = ok(
        &svc,
        "CALL apoc.trigger.install('drevo', 'stamp', 'RETURN 1', {phase: 'before'}) \
         YIELD name, query, selector, installed, paused \
         RETURN name, query, selector.phase, installed, paused",
    );
    assert_eq!(
        rows.rows,
        vec![vec![
            Value::String("stamp".into()),
            Value::String("RETURN 1".into()),
            Value::String("before".into()),
            Value::Bool(true),
            Value::Bool(false),
        ]]
    );
    install(&svc, "audit", AUDIT_CREATED, "after");
    let listed = ok(&svc, "CALL apoc.trigger.list() YIELD name RETURN name");
    assert_eq!(strings(&listed, 0), vec!["stamp", "audit"]);
}

#[test]
fn a_before_trigger_writes_inside_the_transaction() {
    let svc = NativeService::in_memory();
    ok(&svc, "CREATE (:Task {title: 'ship', status: 'open'})");
    install(&svc, "stamp_updated_at", STAMP_UPDATED_AT, "before");

    ok(&svc, "MATCH (t:Task {title: 'ship'}) SET t.status = 'done'");
    assert_eq!(
        int(&svc, "MATCH (t:Task {title: 'ship'}) RETURN t.updated_at"),
        1000
    );
    // A new task gets the stamp too: its properties are assigned.
    ok(&svc, "CREATE (:Task {title: 'review'})");
    assert_eq!(
        int(&svc, "MATCH (t:Task {title: 'review'}) RETURN t.updated_at"),
        1000
    );
    // Only tasks.
    ok(&svc, "CREATE (:Note {title: 'idea'})");
    assert_eq!(int(&svc, "MATCH (n:Note) RETURN count(n.updated_at)"), 0);
}

#[test]
fn a_failing_before_trigger_rolls_the_write_back() {
    let svc = NativeService::in_memory();
    // Refuse tasks without an owner: the division fails only for them.
    install(
        &svc,
        "require_owner",
        "UNWIND $createdNodes AS n WITH n WHERE n:Task AND n.owner IS NULL RETURN 1 / 0",
        "before",
    );
    let err = run(&svc, "CREATE (:Task {title: 'orphan'})").unwrap_err();
    assert!(
        matches!(&err, ExecError::TriggerFailed { name, .. } if name == "require_owner"),
        "{err:?}"
    );
    assert!(err.to_string().contains("require_owner"), "{err}");
    assert_eq!(int(&svc, "MATCH (t:Task) RETURN count(t)"), 0);

    ok(&svc, "CREATE (:Task {title: 'owned', owner: 'ann'})");
    assert_eq!(int(&svc, "MATCH (t:Task) RETURN count(t)"), 1);
}

#[test]
fn an_after_trigger_audits_without_firing_itself() {
    let svc = NativeService::in_memory();
    install(&svc, "audit_created", AUDIT_CREATED, "after");
    // Would audit the audit nodes if a trigger's writes fired triggers.
    install(
        &svc,
        "audit_everything",
        "UNWIND $createdNodes AS n CREATE (:Audit {of: labels(n)[0]})",
        "after",
    );
    ok(
        &svc,
        "CREATE (:Task {title: 'ship'}), (:Task {title: 'test'})",
    );

    let audits = ok(
        &svc,
        "MATCH (a:Audit) WHERE a.task IS NOT NULL RETURN a.task ORDER BY a.task",
    );
    assert_eq!(strings(&audits, 0), vec!["ship", "test"]);
    // `audit_everything` saw the two tasks only, not the audits written by
    // `audit_created` or by itself.
    assert_eq!(
        int(
            &svc,
            "MATCH (a:Audit) WHERE a.of IS NOT NULL RETURN count(a)"
        ),
        2
    );
    assert_eq!(int(&svc, "MATCH (a:Audit) RETURN count(a)"), 4);
}

#[test]
fn a_failing_after_trigger_leaves_the_commit_standing() {
    let svc = NativeService::in_memory();
    install(
        &svc,
        "broken",
        "UNWIND $createdNodes AS n RETURN 1 / 0",
        "after",
    );
    ok(&svc, "CREATE (:Task {title: 'kept'})");
    assert_eq!(int(&svc, "MATCH (t:Task) RETURN count(t)"), 1);
}

#[test]
fn the_parameters_describe_the_whole_transaction() {
    let svc = NativeService::in_memory();
    ok(
        &svc,
        "CREATE (a:Task {title: 'a', estimate: 3})-[:BLOCKS {since: 1}]->(:Task {title: 'b'})",
    );
    install(
        &svc,
        "record",
        "CREATE (:Seen { \
           created: size($createdNodes), deleted: size($deletedNodes), \
           createdRels: size($createdRelationships), \
           deletedRels: size($deletedRelationships), \
           assigned: keys($assignedNodeProperties), \
           removed: keys($removedNodeProperties), \
           labels: keys($assignedLabels), \
           relAssigned: keys($assignedRelationshipProperties)})",
        "after",
    );
    ok(
        &svc,
        "MATCH (a:Task {title: 'a'}) SET a.status = 'done', a:Urgent REMOVE a.estimate \
         WITH a MATCH (b:Task {title: 'b'}) DETACH DELETE b \
         CREATE (a)-[:ASSIGNED_TO {role: 'lead'}]->(:User {title: 'ann'})",
    );
    let seen = ok(
        &svc,
        "MATCH (s:Seen) RETURN s.created, s.deleted, s.createdRels, s.deletedRels, \
         s.assigned, s.removed, s.labels, s.relAssigned",
    );
    let strs =
        |items: &[&str]| Value::List(items.iter().map(|s| Value::String((*s).into())).collect());
    assert_eq!(
        seen.rows,
        vec![vec![
            Value::Integer(1),
            Value::Integer(1),
            Value::Integer(1),
            Value::Integer(1),
            strs(&["status", "title"]),
            strs(&["estimate", "title"]),
            strs(&["Urgent", "User"]),
            strs(&["role"]),
        ]]
    );
}

#[test]
fn trigger_params_from_the_config_are_visible() {
    let svc = NativeService::in_memory();
    ok(
        &svc,
        "CALL apoc.trigger.install('drevo', 'tag', \
           'UNWIND $createdNodes AS n SET n.team = $team', \
           {phase: 'before'}, {params: {team: 'core'}})",
    );
    ok(&svc, "CREATE (:Task {title: 'x'})");
    let team = ok(&svc, "MATCH (t:Task) RETURN t.team");
    assert_eq!(team.rows, vec![vec![Value::String("core".into())]]);
}

#[test]
fn a_stopped_trigger_does_not_run_until_started() {
    let svc = NativeService::in_memory();
    install(&svc, "audit", AUDIT_CREATED, "after");
    let stopped = ok(
        &svc,
        "CALL apoc.trigger.stop('drevo', 'audit') YIELD name, paused RETURN name, paused",
    );
    assert_eq!(
        stopped.rows,
        vec![vec![Value::String("audit".into()), Value::Bool(true)]]
    );
    ok(&svc, "CREATE (:Task {title: 'quiet'})");
    assert_eq!(int(&svc, "MATCH (a:Audit) RETURN count(a)"), 0);

    ok(&svc, "CALL apoc.trigger.start('drevo', 'audit')");
    ok(&svc, "CREATE (:Task {title: 'loud'})");
    assert_eq!(int(&svc, "MATCH (a:Audit) RETURN count(a)"), 1);
}

#[test]
fn dropped_triggers_are_gone() {
    let svc = NativeService::in_memory();
    install(&svc, "a", AUDIT_CREATED, "after");
    install(&svc, "b", "RETURN 1", "after");
    install(&svc, "c", "RETURN 1", "after");
    let dropped = ok(
        &svc,
        "CALL apoc.trigger.drop('drevo', 'a') YIELD name, installed RETURN name, installed",
    );
    assert_eq!(
        dropped.rows,
        vec![vec![Value::String("a".into()), Value::Bool(false)]]
    );
    ok(&svc, "CREATE (:Task {title: 'x'})");
    assert_eq!(int(&svc, "MATCH (a:Audit) RETURN count(a)"), 0);
    assert!(ok(&svc, "CALL apoc.trigger.drop('drevo', 'missing')")
        .rows
        .is_empty());
    let all = ok(
        &svc,
        "CALL apoc.trigger.dropAll('drevo') YIELD name RETURN name",
    );
    assert_eq!(strings(&all, 0), vec!["b", "c"]);
    assert!(ok(&svc, "CALL apoc.trigger.list()").rows.is_empty());
}

#[test]
fn the_apoc_4_procedures_act_on_the_current_database() {
    let svc = NativeService::in_memory();
    ok(
        &svc,
        &format!("CALL apoc.trigger.add('audit', \"{AUDIT_CREATED}\", {{phase: 'after'}})"),
    );
    ok(&svc, "CALL apoc.trigger.pause('audit')");
    ok(&svc, "CREATE (:Task {title: 'x'})");
    assert_eq!(int(&svc, "MATCH (a:Audit) RETURN count(a)"), 0);
    ok(&svc, "CALL apoc.trigger.resume('audit')");
    ok(&svc, "CREATE (:Task {title: 'y'})");
    assert_eq!(int(&svc, "MATCH (a:Audit) RETURN count(a)"), 1);
    ok(&svc, "CALL apoc.trigger.remove('audit')");
    ok(&svc, "CALL apoc.trigger.add('t', 'RETURN 1', {})");
    assert_eq!(
        strings(
            &ok(&svc, "CALL apoc.trigger.removeAll() YIELD name RETURN name"),
            0
        ),
        vec!["t"]
    );
}

#[test]
fn an_empty_selector_means_before() {
    let svc = NativeService::in_memory();
    ok(&svc, "CALL apoc.trigger.add('t', 'RETURN 1', {})");
    let phase = ok(
        &svc,
        "CALL apoc.trigger.list() YIELD selector RETURN selector.phase",
    );
    assert_eq!(phase.rows, vec![vec![Value::String("before".into())]]);
}

#[test]
fn bad_definitions_are_refused() {
    let svc = NativeService::in_memory();
    for q in [
        "CALL apoc.trigger.install('drevo', 't', 'RETURN (', {phase: 'before'})",
        "CALL apoc.trigger.install('drevo', 't', 'RETURN 1', {phase: 'rollback'})",
        "CALL apoc.trigger.install('drevo', 't', 'RETURN 1', {table: 'x'})",
        "CALL apoc.trigger.install('drevo', 't', 'RETURN 1', {}, {limit: 1})",
        "CALL apoc.trigger.install('drevo', '', 'RETURN 1', {})",
        "CALL apoc.trigger.install('elsewhere', 't', 'RETURN 1', {})",
        "CALL apoc.trigger.install('drevo', 't', 'RETURN 1')",
        "CALL apoc.trigger.drop('elsewhere', 't')",
    ] {
        let err = run(&svc, q).expect_err(q);
        assert!(
            matches!(err, ExecError::InvalidProcedureCall { .. }),
            "{q}: {err:?}"
        );
    }
    assert!(ok(&svc, "CALL apoc.trigger.list()").rows.is_empty());
}

#[test]
fn statements_that_change_nothing_fire_nothing() {
    let svc = NativeService::in_memory();
    install(&svc, "audit", "CREATE (:Audit)", "after");
    ok(&svc, "MATCH (n) RETURN count(n)");
    ok(&svc, "MATCH (t:Task {title: 'none'}) SET t.x = 1");
    assert_eq!(int(&svc, "MATCH (a:Audit) RETURN count(a)"), 0);
}

#[test]
fn explicit_transactions_fire_at_commit() {
    let svc = NativeService::in_memory();
    install(&svc, "stamp_updated_at", STAMP_UPDATED_AT, "before");
    install(&svc, "audit_created", AUDIT_CREATED, "after");

    let tx = svc.begin_tx();
    svc.execute_in_tx(
        tx,
        &parse("CREATE (:Task {title: 'one'})").unwrap(),
        HashMap::new(),
    )
    .unwrap();
    svc.execute_in_tx(
        tx,
        &parse("CREATE (:Task {title: 'two'})").unwrap(),
        HashMap::new(),
    )
    .unwrap();
    assert_eq!(int(&svc, "MATCH (a:Audit) RETURN count(a)"), 0);
    svc.commit_tx(tx).unwrap();
    assert_eq!(
        int(
            &svc,
            "MATCH (t:Task) WHERE t.updated_at = 1000 RETURN count(t)"
        ),
        2
    );
    assert_eq!(int(&svc, "MATCH (a:Audit) RETURN count(a)"), 2);
}

#[test]
fn a_failing_before_trigger_rejects_an_explicit_commit() {
    let svc = NativeService::in_memory();
    install(
        &svc,
        "no_tasks",
        "UNWIND $createdNodes AS n WITH n WHERE n:Task RETURN 1 / 0",
        "before",
    );
    let tx = svc.begin_tx();
    svc.execute_in_tx(tx, &parse("CREATE (:Task)").unwrap(), HashMap::new())
        .unwrap();
    let err = svc.commit_tx(tx).unwrap_err();
    assert!(
        matches!(&err, CommitError::Rejected(reason) if reason.contains("no_tasks")),
        "{err:?}"
    );
    assert_eq!(int(&svc, "MATCH (t:Task) RETURN count(t)"), 0);
    assert!(svc
        .execute_in_tx(tx, &parse("RETURN 1").unwrap(), HashMap::new())
        .is_err());
}

#[test]
fn prepared_transactions_fire_after_triggers_once_committed() {
    let svc = NativeService::in_memory();
    install(&svc, "audit_created", AUDIT_CREATED, "after");
    let tx = svc.begin_tx();
    svc.execute_in_tx(
        tx,
        &parse("CREATE (:Task {title: 'two-phase'})").unwrap(),
        HashMap::new(),
    )
    .unwrap();
    svc.prepare_tx(tx, "gid-1").unwrap();
    assert_eq!(int(&svc, "MATCH (a:Audit) RETURN count(a)"), 0);
    svc.commit_prepared("gid-1").unwrap();
    assert_eq!(int(&svc, "MATCH (a:Audit) RETURN count(a)"), 1);
}

#[test]
fn trigger_management_is_refused_in_an_explicit_transaction() {
    let svc = NativeService::in_memory();
    let tx = svc.begin_tx();
    let err = svc
        .execute_in_tx(
            tx,
            &parse("CALL apoc.trigger.install('drevo', 't', 'RETURN 1', {})").unwrap(),
            HashMap::new(),
        )
        .unwrap_err();
    assert!(
        matches!(err, ExecError::ForbiddenInTransaction { .. }),
        "{err:?}"
    );
    // Listing is a read.
    svc.execute_in_tx(
        tx,
        &parse("CALL apoc.trigger.list()").unwrap(),
        HashMap::new(),
    )
    .unwrap();
    svc.rollback_tx(tx);
}

#[test]
fn after_async_triggers_run_on_the_worker() {
    let svc = Arc::new(NativeService::in_memory());
    svc.start_trigger_worker();
    install(&svc, "audit_async", AUDIT_CREATED, "afterAsync");
    ok(&svc, "CREATE (:Task {title: 'later'})");
    let deadline = Instant::now() + Duration::from_secs(10);
    while int(&svc, "MATCH (a:Audit) RETURN count(a)") == 0 {
        assert!(
            Instant::now() < deadline,
            "the afterAsync trigger never ran"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(int(&svc, "MATCH (a:Audit) RETURN count(a)"), 1);
}

#[test]
fn after_async_triggers_run_inline_without_a_worker() {
    let svc = NativeService::in_memory();
    install(&svc, "audit_async", AUDIT_CREATED, "afterAsync");
    ok(&svc, "CREATE (:Task {title: 'now'})");
    assert_eq!(int(&svc, "MATCH (a:Audit) RETURN count(a)"), 1);
}

#[test]
fn triggers_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let wal = dir.path().join("native.wal");
    {
        let svc = NativeService::open(&wal).unwrap();
        install(&svc, "audit_created", AUDIT_CREATED, "after");
        install(&svc, "paused_one", "RETURN 1", "before");
        ok(&svc, "CALL apoc.trigger.stop('drevo', 'paused_one')");
    }
    let svc = NativeService::open(&wal).unwrap();
    let listed = ok(
        &svc,
        "CALL apoc.trigger.list() YIELD name, selector, paused \
         RETURN name, selector.phase, paused",
    );
    assert_eq!(
        listed.rows,
        vec![
            vec![
                Value::String("audit_created".into()),
                Value::String("after".into()),
                Value::Bool(false),
            ],
            vec![
                Value::String("paused_one".into()),
                Value::String("before".into()),
                Value::Bool(true),
            ],
        ]
    );
    ok(&svc, "CREATE (:Task {title: 'after restart'})");
    assert_eq!(int(&svc, "MATCH (a:Audit) RETURN count(a)"), 1);
}

mod bolt {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Arc;

    use drevo::bolt::packstream::Value as Bolt;
    use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
    use drevo::cypher::parser::parse;
    use drevo::native_service::NativeService;

    const HOOK_FAILED: &str = "Neo.ClientError.Transaction.TransactionHookFailed";

    fn hello(svc: &Arc<NativeService>) -> Session<'static> {
        let mut s = Session::new_durable(Arc::clone(svc));
        s.handle(ClientMessage::Hello {
            extra: BTreeMap::new(),
        });
        s
    }

    fn send(s: &mut Session<'_>, msg: ClientMessage) -> Vec<ServerMessage> {
        s.handle(msg)
    }

    fn run(s: &mut Session<'_>, q: &str) -> Vec<ServerMessage> {
        let mut out = send(
            s,
            ClientMessage::Run {
                query: q.to_string(),
                parameters: BTreeMap::new(),
                extra: BTreeMap::new(),
            },
        );
        if matches!(out.first(), Some(ServerMessage::Success { .. })) {
            let mut n = BTreeMap::new();
            n.insert("n".to_string(), Bolt::Integer(-1));
            out.extend(send(s, ClientMessage::Pull { extra: n }));
        }
        out
    }

    fn failure_code(replies: &[ServerMessage]) -> Option<String> {
        replies.iter().find_map(|m| match m {
            ServerMessage::Failure { metadata } => match metadata.get("code") {
                Some(Bolt::String(c)) => Some(c.clone()),
                _ => Some(String::new()),
            },
            _ => None,
        })
    }

    fn refusing_tasks() -> Arc<NativeService> {
        let svc = Arc::new(NativeService::in_memory());
        svc.execute(
            &parse(
                "CALL apoc.trigger.add('no_tasks', \
                 'UNWIND $createdNodes AS n WITH n WHERE n:Task RETURN 1 / 0', \
                 {phase: 'before'})",
            )
            .unwrap(),
            HashMap::new(),
        )
        .unwrap();
        svc
    }

    #[test]
    fn a_failing_before_trigger_fails_an_autocommit_run_with_the_hook_code() {
        let svc = refusing_tasks();
        let mut s = hello(&svc);
        let replies = run(&mut s, "CREATE (:Task)");
        assert_eq!(
            failure_code(&replies).as_deref(),
            Some(HOOK_FAILED),
            "{replies:?}"
        );
    }

    #[test]
    fn a_failing_before_trigger_fails_commit_with_the_hook_code() {
        let svc = refusing_tasks();
        let mut s = hello(&svc);
        send(
            &mut s,
            ClientMessage::Begin {
                extra: BTreeMap::new(),
            },
        );
        let created = run(&mut s, "CREATE (:Task)");
        assert_eq!(failure_code(&created), None, "{created:?}");
        let commit = send(&mut s, ClientMessage::Commit);
        assert_eq!(
            failure_code(&commit).as_deref(),
            Some(HOOK_FAILED),
            "{commit:?}"
        );
    }
}

#[test]
fn a_read_only_database_refuses_trigger_changes() {
    let svc = NativeService::in_memory();
    ok(&svc, "CALL apoc.trigger.add('t', 'RETURN 1', {})");
    svc.set_read_only(true).unwrap();
    let err = run(&svc, "CALL apoc.trigger.add('u', 'RETURN 1', {})").unwrap_err();
    assert!(err.to_string().contains("read-only"), "{err}");
    assert!(run(&svc, "CALL apoc.trigger.remove('t')").is_err());
    // Reading the list still works.
    assert_eq!(ok(&svc, "CALL apoc.trigger.list()").rows.len(), 1);
}
