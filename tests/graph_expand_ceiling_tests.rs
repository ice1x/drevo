//! `drevo.graph.expand` is held to the server's ceilings
//! (`DREVO_EXPAND_MAX_HOPS`, `DREVO_EXPAND_MAX_NODES`), which the server sets
//! for the whole process at start. Alone in its own test binary, because it
//! changes them for the process.

use std::collections::HashMap;

use drevo::cypher::executor::ExecError;
use drevo::cypher::expand::{ceilings, set_ceilings, ExpandCeilings};
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;

#[test]
fn calls_are_held_to_the_ceilings_set_for_the_process() {
    assert_eq!(ceilings(), ExpandCeilings::default());
    let db = NativeService::in_memory();
    let call = |config: &str| {
        let q = parse(&format!(
            "CREATE (a:Step)-[:NEXT]->(:Step)-[:NEXT]->(:Step)-[:NEXT]->(:Step) \
             WITH a CALL drevo.graph.expand(a, {config}) YIELD node RETURN count(node)"
        ))
        .unwrap();
        db.execute(&q, HashMap::new())
    };
    assert!(call("{hops: 3}").is_ok());

    set_ceilings(ExpandCeilings {
        max_hops: 2,
        max_nodes: 10,
    });
    assert_eq!(
        ceilings(),
        ExpandCeilings {
            max_hops: 2,
            max_nodes: 10
        }
    );
    match call("{hops: 3}") {
        Err(ExecError::InvalidProcedureCall { message, .. }) => assert_eq!(
            message,
            "hops 3 is above the server's limit of 2 (DREVO_EXPAND_MAX_HOPS)"
        ),
        other => panic!("expected the hops ceiling, got {other:?}"),
    }
    match call("{maxNodes: 11}") {
        Err(ExecError::InvalidProcedureCall { message, .. }) => assert_eq!(
            message,
            "maxNodes 11 is above the server's limit of 10 (DREVO_EXPAND_MAX_NODES)"
        ),
        other => panic!("expected the node ceiling, got {other:?}"),
    }
    let rows = call("{hops: 2, maxNodes: 10}").unwrap().rows;
    assert_eq!(rows[0][0], drevo::cypher::executor::Value::Integer(3));
}
