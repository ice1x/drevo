//! `CALL drevo.info()` — version / build introspection over Cypher & Bolt
//! (issue #303).
//!
//! A read-only, no-arg, no-auth procedure that lets a Bolt client learn which
//! drevo it is talking to and assert a minimum-compatible version. graphiti's
//! `DrevoDriver` calls it on connect to fail fast against an older server, and
//! falls back gracefully when the procedure is absent. The YIELD contract
//! (`version, git_sha, build_date, protocol`) is stable across versions.

use std::collections::HashMap;

use drevo::cypher::executor::{execute, Value};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;

fn db() -> Drevo {
    Drevo::open_in_memory().expect("open in-memory drevo")
}

fn run(src: &str, d: &Drevo) -> (Vec<String>, Vec<Vec<Value>>) {
    let q = parse(src).expect("parse");
    let r = execute(&q, d, HashMap::new()).expect("execute");
    (r.columns, r.rows)
}

#[test]
fn drevo_info_returns_the_running_version() {
    let d = db();
    let (cols, rows) = run("CALL drevo.info() YIELD version RETURN version", &d);
    assert_eq!(cols, vec!["version".to_string()]);
    assert_eq!(rows.len(), 1, "exactly one row");
    match &rows[0][0] {
        Value::String(v) => assert_eq!(v, drevo::VERSION, "must match the running build"),
        other => panic!("expected version string, got {other:?}"),
    }
}

#[test]
fn drevo_info_yields_the_full_stable_contract() {
    let d = db();
    let (cols, rows) = run(
        "CALL drevo.info() YIELD version, git_sha, build_date, protocol \
         RETURN version, git_sha, build_date, protocol",
        &d,
    );
    assert_eq!(
        cols,
        vec![
            "version".to_string(),
            "git_sha".to_string(),
            "build_date".to_string(),
            "protocol".to_string(),
        ]
    );
    assert_eq!(rows.len(), 1);
    // version: non-empty string.
    assert!(
        matches!(&rows[0][0], Value::String(s) if !s.is_empty()),
        "version must be a non-empty string, got {:?}",
        rows[0][0]
    );
    // git_sha / build_date: string or null (nullable when unavailable).
    assert!(matches!(&rows[0][1], Value::String(_) | Value::Null));
    assert!(matches!(&rows[0][2], Value::String(_) | Value::Null));
    // protocol: a coarse capability integer clients can gate on without semver.
    assert!(
        matches!(&rows[0][3], Value::Integer(p) if *p >= 1),
        "protocol must be a positive integer, got {:?}",
        rows[0][3]
    );
}

#[test]
fn drevo_info_takes_no_arguments() {
    let d = db();
    let q = parse("CALL drevo.info(1) YIELD version RETURN version").expect("parse");
    assert!(
        execute(&q, &d, HashMap::new()).is_err(),
        "drevo.info has arity 0 — a positional argument must be rejected"
    );
}

// The procedure must be reachable over the actual Bolt RUN/PULL path — the way
// graphiti's neo4j-python client calls it — not only via the executor directly.
#[cfg(all(not(target_arch = "wasm32"), feature = "redb-backend"))]
mod over_bolt {
    use std::collections::BTreeMap;

    use drevo::bolt::packstream::Value as PackValue;
    use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
    use drevo::db::Drevo;

    #[test]
    fn drevo_info_returns_version_over_bolt() {
        let d = Drevo::open_in_memory().unwrap();
        let mut s = Session::new(&d);
        s.handle(ClientMessage::Hello {
            extra: BTreeMap::new(),
        });
        let run = s.handle(ClientMessage::Run {
            query: "CALL drevo.info() YIELD version RETURN version".to_string(),
            parameters: BTreeMap::new(),
            extra: BTreeMap::new(),
        });
        assert!(
            !run.iter()
                .any(|m| matches!(m, ServerMessage::Failure { .. })),
            "RUN over Bolt failed: {run:?}"
        );
        let mut n = BTreeMap::new();
        n.insert("n".to_string(), PackValue::Integer(-1));
        let pulled = s.handle(ClientMessage::Pull { extra: n });
        let rec = pulled
            .iter()
            .find_map(|m| match m {
                ServerMessage::Record { fields } => Some(fields.clone()),
                _ => None,
            })
            .expect("one record over Bolt");
        assert_eq!(rec.len(), 1);
        assert_eq!(rec[0], PackValue::String(drevo::VERSION.to_string()));
    }
}
