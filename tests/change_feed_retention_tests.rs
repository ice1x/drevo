//! The change feed keeps only the recent history event subscribers resume
//! from — also right after a store is opened, when the engine seeds it with a
//! copy of the whole graph, and as soon as the history setting is lowered.

use drevo::model::{NewNode, Properties};
use drevo::native_service::NativeService;

fn junctions(n: usize) -> Vec<NewNode> {
    (0..n)
        .map(|i| {
            let mut properties = Properties::default();
            properties.insert("osm_id".to_string(), serde_json::json!(i));
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
fn a_reopened_store_keeps_only_the_event_history() {
    let dir = tempfile::tempdir().unwrap();
    let wal = dir.path().join("native.wal");
    let total = NativeService::DEFAULT_EVENT_HISTORY as usize + 500;
    {
        let svc = NativeService::open(&wal).unwrap();
        svc.create_nodes(junctions(total)).unwrap();
    }
    let svc = NativeService::open(&wal).unwrap();
    assert_eq!(svc.graph().node_count() as usize, total);
    assert_eq!(retained(&svc), NativeService::DEFAULT_EVENT_HISTORY);
}

#[test]
fn a_small_store_keeps_its_whole_history() {
    let dir = tempfile::tempdir().unwrap();
    let wal = dir.path().join("native.wal");
    NativeService::open(&wal)
        .unwrap()
        .create_nodes(junctions(30))
        .unwrap();
    let svc = NativeService::open(&wal).unwrap();
    assert_eq!(retained(&svc), 30);
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
