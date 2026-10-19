//! The change feed keeps only the recent history event subscribers resume
//! from — also right after a store is opened, when the engine seeds it with a
//! copy of the whole graph, and as soon as the history setting is lowered.

use drevo::model::{NewNode, Properties};
use drevo::native_service::NativeService;

fn junctions(n: usize) -> Vec<NewNode> {
    (0..n)
        .map(|i| {
            let mut properties = Properties::default();
            properties.insert("osm_id", serde_json::json!(i));
            NewNode {
                kind: "Junction".to_string(),
                title: String::new(),
                body: String::new(),
                body_html: String::new(),
                properties,
            }
        })
        .collect()
}

fn retained(svc: &NativeService) -> u64 {
    svc.graph().change_head() - svc.graph().change_floor()
}

#[test]
fn a_reopened_store_does_not_copy_the_graph_into_the_feed() {
    let dir = tempfile::tempdir().unwrap();
    let wal = dir.path().join("native.wal");
    let total = 1_500;
    {
        let svc = NativeService::open(&wal).unwrap();
        svc.create_nodes(junctions(total)).unwrap();
    }
    let svc = NativeService::open(&wal).unwrap();
    assert_eq!(svc.graph().node_count() as usize, total);
    assert_eq!(retained(&svc), 0);
}

#[test]
fn indexes_serve_the_recovered_graph_after_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let wal = dir.path().join("native.wal");
    {
        let svc = NativeService::open(&wal).unwrap();
        svc.execute(
            &drevo::cypher::parser::parse(
                "UNWIND range(1, 30) AS i CREATE (:Junction {title: 'stop ' + toString(i), osm_id: i})",
            )
            .unwrap(),
            std::collections::HashMap::new(),
        )
        .unwrap();
    }
    let svc = NativeService::open(&wal).unwrap();
    let run = |q: &str| {
        svc.execute(
            &drevo::cypher::parser::parse(q).unwrap(),
            std::collections::HashMap::new(),
        )
        .unwrap()
        .rows
    };
    // The property index, the label index and full-text all see it.
    assert_eq!(
        run("MATCH (j:Junction {osm_id: 17}) RETURN j.title"),
        vec![vec![drevo::cypher::executor::Value::String(
            "stop 17".into()
        )]]
    );
    assert_eq!(
        run("MATCH (j:Junction) WHERE j.osm_id > 25 RETURN j").len(),
        5
    );
    assert_eq!(
        run("CALL fts.search('stop 23', 1) YIELD node RETURN node.title"),
        vec![vec![drevo::cypher::executor::Value::String(
            "stop 23".into()
        )]]
    );
    // New writes after the reopen flow through the feed as usual.
    run("CREATE (:Junction {title: 'new stop', osm_id: 99})");
    assert_eq!(run("MATCH (j {osm_id: 99}) RETURN j").len(), 1);
}

#[test]
fn lowering_the_history_trims_the_feed_at_once() {
    let svc = NativeService::in_memory();
    svc.create_nodes(junctions(50)).unwrap();
    // A statement brings the indexes up to the head; history they have not
    // read is never dropped.
    svc.execute(
        &drevo::cypher::parser::parse("MATCH (n) RETURN count(n)").unwrap(),
        std::collections::HashMap::new(),
    )
    .unwrap();
    svc.set_event_history(5);
    assert_eq!(retained(&svc), 5);
    // Reads and new writes still work against the trimmed feed.
    svc.create_nodes(junctions(2)).unwrap();
    assert_eq!(svc.graph().node_count(), 52);
}
