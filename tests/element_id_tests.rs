//! `elementId(n)` / `elementId(r)`: the element id Bolt 5 drivers see on
//! nodes and relationships (`node.element_id`), as a string. A client hands
//! it back as a parameter — `MATCH (n) WHERE elementId(n) = $e` — and the
//! lookup is by id, as for `id(n) = $id`.
//!
//! The scenario is a bug tracker: a `Bug` reported by a `User`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use drevo::cypher::executor::{ExecError, Value};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

fn rows(svc: &NativeService, q: &str, params: &[(&str, Value)]) -> Vec<Vec<Value>> {
    let params = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect::<HashMap<_, _>>();
    svc.execute(&parse(q).unwrap(), params)
        .unwrap_or_else(|e| panic!("`{q}`: {e}"))
        .rows
}

fn string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => panic!("expected a string, got {other:?}"),
    }
}

fn tracker() -> NativeService {
    let svc = NativeService::in_memory();
    rows(
        &svc,
        "CREATE (:User {title: 'ann'})-[:REPORTED]->(:Bug {title: 'crash on save'}), \
                (:Bug {title: 'slow search'})",
        &[],
    );
    svc
}

#[test]
fn element_id_is_the_id_as_a_string() {
    let svc = tracker();
    let out = rows(
        &svc,
        "MATCH (u:User)-[r:REPORTED]->(b:Bug) \
         RETURN elementId(u), id(u), elementId(r), id(r), elementId(null)",
        &[],
    );
    let row = &out[0];
    let Value::Integer(uid) = row[1] else {
        panic!()
    };
    let Value::Integer(rid) = row[3] else {
        panic!()
    };
    assert_eq!(string(&row[0]), uid.to_string());
    assert_eq!(string(&row[2]), rid.to_string());
    assert_eq!(row[4], Value::Null);
}

#[test]
fn match_by_element_id_finds_the_node() {
    let svc = tracker();
    let eid = string(
        &rows(
            &svc,
            "MATCH (b:Bug {title: 'slow search'}) RETURN elementId(b)",
            &[],
        )[0][0],
    );
    let found = rows(
        &svc,
        "MATCH (n) WHERE elementId(n) = $e RETURN n.title",
        &[("e", Value::String(eid.clone()))],
    );
    assert_eq!(found, vec![vec![Value::String("slow search".into())]]);

    // In a list, and in reverse order.
    let ann = string(&rows(&svc, "MATCH (u:User) RETURN elementId(u)", &[])[0][0]);
    let found = rows(
        &svc,
        "MATCH (n) WHERE elementId(n) IN $ids RETURN n.title ORDER BY n.title",
        &[(
            "ids",
            Value::List(vec![Value::String(ann), Value::String(eid.clone())]),
        )],
    );
    assert_eq!(
        found,
        vec![
            vec![Value::String("ann".into())],
            vec![Value::String("slow search".into())]
        ]
    );
    let found = rows(
        &svc,
        &format!("MATCH (n) WHERE '{eid}' = elementId(n) RETURN count(n)"),
        &[],
    );
    assert_eq!(found, vec![vec![Value::Integer(1)]]);
}

#[test]
fn an_element_id_is_a_string_not_an_integer() {
    let svc = tracker();
    let Value::Integer(id) = rows(&svc, "MATCH (u:User) RETURN id(u)", &[])[0][0] else {
        panic!()
    };
    let count = |q: &str, v: Value| rows(&svc, q, &[("v", v)])[0][0].clone();
    // Types do not mix, on either function.
    assert_eq!(
        count(
            "MATCH (n) WHERE elementId(n) = $v RETURN count(n)",
            Value::Integer(id)
        ),
        Value::Integer(0)
    );
    assert_eq!(
        count(
            "MATCH (n) WHERE id(n) = $v RETURN count(n)",
            Value::String(id.to_string())
        ),
        Value::Integer(0)
    );
    // An element id from another server names nothing here.
    assert_eq!(
        count(
            "MATCH (n) WHERE elementId(n) = $v RETURN count(n)",
            Value::String("4:f1c2:7".into())
        ),
        Value::Integer(0)
    );
    let err = svc
        .execute(&parse("RETURN elementId(1)").unwrap(), HashMap::new())
        .unwrap_err();
    assert!(
        matches!(err, ExecError::InvalidFunctionCall { .. }),
        "{err:?}"
    );
}

#[test]
fn match_by_element_id_is_a_lookup_not_a_scan() {
    let svc = NativeService::in_memory();
    rows(
        &svc,
        "UNWIND range(1, 20000) AS i CREATE (:Bug {n: i})",
        &[],
    );
    let eid = string(&rows(&svc, "MATCH (b:Bug {n: 12345}) RETURN elementId(b)", &[])[0][0]);
    let ids = Value::List((0..500).map(|_| Value::String(eid.clone())).collect());
    let started = Instant::now();
    let out = rows(
        &svc,
        "UNWIND $ids AS e MATCH (b) WHERE elementId(b) = e RETURN count(b)",
        &[("ids", ids)],
    );
    assert_eq!(out, vec![vec![Value::Integer(500)]]);
    // 500 scans of 20,000 nodes would take seconds.
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

#[cfg(all(not(target_arch = "wasm32"), feature = "http"))]
#[test]
fn the_bolt_5_element_id_round_trips() {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use drevo::bolt::handshake::BoltVersion;
    use drevo::bolt::packstream::Value as BoltValue;
    use drevo::bolt::session::{ClientMessage, ServerMessage, Session};

    let svc = Arc::new(tracker());
    let mut s =
        Session::new_durable(Arc::clone(&svc)).with_version(BoltVersion { major: 5, minor: 0 });
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });
    let mut run = |q: &str, params: BTreeMap<String, BoltValue>| {
        s.handle(ClientMessage::Run {
            query: q.to_string(),
            parameters: params,
            extra: BTreeMap::new(),
        });
        s.handle(ClientMessage::Pull {
            extra: BTreeMap::from([("n".to_string(), BoltValue::Integer(-1))]),
        })
        .into_iter()
        .filter_map(|m| match m {
            ServerMessage::Record { fields } => Some(fields),
            _ => None,
        })
        .collect::<Vec<_>>()
    };
    let records = run(
        "MATCH (b:Bug {title: 'crash on save'}) RETURN b",
        BTreeMap::new(),
    );
    let BoltValue::Structure { fields, .. } = &records[0][0] else {
        panic!()
    };
    let element_id = fields[3].clone();
    let found = run(
        "MATCH (n) WHERE elementId(n) = $e RETURN n.title",
        BTreeMap::from([("e".to_string(), element_id)]),
    );
    assert_eq!(found, vec![vec![BoltValue::String("crash on save".into())]]);
}
