//! A writing auto-commit statement is one transaction: its writes reach the
//! log as one record, with one fsync, and a statement that fails leaves
//! none of them. Concurrent auto-commit writers take turns rather than
//! failing each other's commits.
//!
//! The scenario is a ticket importer: batches of `UNWIND … CREATE`, a
//! clean-up loop, and several workers filing tickets at once.

use std::collections::HashMap;
use std::sync::Arc;

use drevo::cypher::executor::Value;
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

fn run(
    svc: &NativeService,
    q: &str,
) -> Result<Vec<Vec<Value>>, drevo::cypher::executor::ExecError> {
    svc.execute(&parse(q).unwrap(), HashMap::new())
        .map(|r| r.rows)
}

fn count(svc: &NativeService, q: &str) -> i64 {
    match run(svc, q).unwrap()[0][0] {
        Value::Integer(n) => n,
        ref other => panic!("{other:?}"),
    }
}

fn log_records(dir: &std::path::Path) -> usize {
    let bytes = std::fs::read(dir.join("native.wal")).unwrap();
    bytes
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .count()
}

#[test]
fn a_writing_statement_is_one_log_record() {
    let dir = tempfile::tempdir().unwrap();
    let svc = NativeService::open(dir.path().join("native.wal")).unwrap();
    let before = log_records(dir.path());
    run(&svc, "UNWIND range(1, 500) AS i CREATE (:Ticket {n: i})").unwrap();
    assert_eq!(log_records(dir.path()), before + 1, "500 nodes, one record");
    run(&svc, "MATCH (t:Ticket) WHERE t.n <= 100 SET t.open = true").unwrap();
    run(&svc, "MATCH (t:Ticket) WHERE t.n > 400 DETACH DELETE t").unwrap();
    assert_eq!(log_records(dir.path()), before + 3);
    drop(svc);
    // And all of it is there after a reopen.
    let svc = NativeService::open(dir.path().join("native.wal")).unwrap();
    assert_eq!(count(&svc, "MATCH (t:Ticket) RETURN count(t)"), 400);
    assert_eq!(
        count(&svc, "MATCH (t:Ticket {open: true}) RETURN count(t)"),
        100
    );
}

#[test]
fn a_failing_statement_leaves_none_of_its_writes() {
    let svc = NativeService::in_memory();
    run(&svc, "CREATE (:Ticket {n: 0})").unwrap();
    // The third row divides by zero after two tickets were created.
    let err = run(&svc, "UNWIND [1, 2, 0] AS x CREATE (:Ticket {n: 10 / x})").unwrap_err();
    assert!(err.to_string().to_lowercase().contains("zero"), "{err}");
    assert_eq!(count(&svc, "MATCH (t:Ticket) RETURN count(t)"), 1);
    // A failed SET changes nothing either.
    assert!(run(&svc, "MATCH (t:Ticket) SET t.v = 1 WITH t SET t.w = 1 / 0").is_err());
    assert_eq!(
        count(
            &svc,
            "MATCH (t:Ticket) WHERE t.v IS NOT NULL RETURN count(t)"
        ),
        0
    );
}

#[test]
fn concurrent_writers_take_turns_without_conflicts() {
    let svc = Arc::new(NativeService::in_memory());
    let workers: Vec<_> = (0..8)
        .map(|w| {
            let svc = Arc::clone(&svc);
            std::thread::spawn(move || {
                for i in 0..25 {
                    run(&svc, &format!("CREATE (:Ticket {{worker: {w}, n: {i}}})"))
                        .expect("no commit conflicts");
                }
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }
    assert_eq!(count(&svc, "MATCH (t:Ticket) RETURN count(t)"), 200);
}
