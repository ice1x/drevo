//! The value cache rebuilt from a snapshot while commits land: when the change
//! feed was trimmed past its cursor it rebuilds and resumes from a cursor. A
//! commit landing during the rebuild must not be skipped, or that node is
//! never cached again until it changes (a miss costs speed, not answers).

use std::sync::atomic::{AtomicBool, Ordering};

use drevo::engine::GraphEngine;
use drevo::model::{NewNode, Properties};
use drevo::native::NativeGraph;
use drevo::native_value_cache::NativeValueCache;
use serde_json::Value as Json;

const ROUNDS: usize = 30;
const NODES: u64 = 300;

fn note(i: u64) -> NewNode {
    let mut properties = Properties::new();
    properties.insert("k", Json::from(i));
    NewNode {
        kind: "Note".into(),
        title: format!("note {i}"),
        properties,
        body: String::new(),
        body_html: String::new(),
    }
}

#[test]
fn a_value_cache_rebuilt_during_commits_caches_every_node() {
    for round in 0..ROUNDS {
        let graph = NativeGraph::new();
        let mut cache = NativeValueCache::new();
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for i in 0..NODES {
                    graph.create_nodes(vec![note(i)]).expect("create");
                }
                done.store(true, Ordering::SeqCst);
            });
            while !done.load(Ordering::SeqCst) {
                graph.trim_before(graph.change_head());
                cache.sync(&graph);
            }
        });
        cache.sync(&graph);
        let uncached = graph
            .all_nodes()
            .expect("nodes")
            .iter()
            .filter(|n| {
                cache
                    .value_for(&graph.get_node_arc(n.id).expect("node"))
                    .is_none()
            })
            .count();
        assert_eq!(uncached, 0, "round {round}");
    }
}
