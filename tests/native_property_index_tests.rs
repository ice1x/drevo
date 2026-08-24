//! The native property-value index (RFC `docs/rfc-native-core.md`, #307,
//! Phase 6.7) — a change-feed consumer indexing `(property key, value)` pairs so
//! a `MATCH (n {key: value})` equality pattern skips the full node scan.
//!
//! These lock: it indexes indexable scalar values in ascending id order,
//! reflects value changes / deletes after `sync`, rebuilds when the feed was
//! trimmed past its cursor, and — the headline — makes a native
//! `MATCH (n {..})` return the **same rows as the full-scan path and as the KV
//! store**, including patterns that mix an indexable and a non-indexable filter.

use drevo::cypher::executor::{
    execute, execute_on_engine, execute_on_engine_with_indexes, ExecResult, Value,
};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;
use drevo::engine::GraphEngine;
use drevo::model::{NewNode, NodePatch, Properties};
use drevo::native::NativeGraph;
use drevo::native_label_index::NativeLabelIndex;
use drevo::native_property_index::NativePropertyIndex;
use std::collections::HashMap;

fn node(kind: &str, title: &str, props: &[(&str, serde_json::Value)]) -> NewNode {
    let map: HashMap<String, serde_json::Value> = props
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    NewNode {
        kind: kind.into(),
        title: title.into(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties(map),
    }
}

#[test]
fn indexes_scalar_values_in_ascending_id_order() {
    let g = NativeGraph::new();
    let a = g
        .create_node(node("task", "a", &[("status", serde_json::json!("open"))]))
        .unwrap();
    let b = g
        .create_node(node(
            "task",
            "b",
            &[
                ("status", serde_json::json!("open")),
                ("prio", serde_json::json!(1)),
            ],
        ))
        .unwrap();
    let _c = g
        .create_node(node(
            "task",
            "c",
            &[("status", serde_json::json!("closed"))],
        ))
        .unwrap();
    // A node whose only property is a non-indexable float is not tracked.
    let _d = g
        .create_node(node("task", "d", &[("score", serde_json::json!(3.5))]))
        .unwrap();

    let mut idx = NativePropertyIndex::new();
    idx.sync(&g);

    assert_eq!(
        idx.node_ids("status", &serde_json::json!("open")),
        vec![a.id, b.id]
    );
    assert_eq!(
        idx.node_ids("status", &serde_json::json!("closed")),
        vec![_c.id]
    );
    assert_eq!(idx.node_ids("prio", &serde_json::json!(1)), vec![b.id]);
    // Absent value, and a non-indexable query value, both yield empty.
    assert!(idx
        .node_ids("status", &serde_json::json!("archived"))
        .is_empty());
    assert!(idx.node_ids("score", &serde_json::json!(3.5)).is_empty());
    // a, b, c carry an indexable property; d (float only) does not.
    assert_eq!(idx.len(), 3);
    assert!(!idx.is_empty());
}

#[test]
fn reflects_value_changes_and_deletes_after_sync() {
    let g = NativeGraph::new();
    let a = g
        .create_node(node("task", "a", &[("status", serde_json::json!("open"))]))
        .unwrap();

    let mut idx = NativePropertyIndex::new();
    idx.sync(&g);
    assert_eq!(
        idx.node_ids("status", &serde_json::json!("open")),
        vec![a.id]
    );

    // Flip the value: open -> done (full property replace).
    g.update_node(
        a.id,
        NodePatch {
            properties: Some(Properties(HashMap::from([(
                "status".to_string(),
                serde_json::json!("done"),
            )]))),
            ..Default::default()
        },
    )
    .unwrap();
    idx.sync(&g);
    assert!(idx
        .node_ids("status", &serde_json::json!("open"))
        .is_empty());
    assert_eq!(
        idx.node_ids("status", &serde_json::json!("done")),
        vec![a.id]
    );

    g.delete_node(a.id).unwrap();
    idx.sync(&g);
    assert!(idx
        .node_ids("status", &serde_json::json!("done"))
        .is_empty());
    assert!(idx.is_empty());
}

#[test]
fn rebuilds_when_feed_trimmed_past_cursor() {
    let g = NativeGraph::new();
    let a = g
        .create_node(node("task", "a", &[("status", serde_json::json!("open"))]))
        .unwrap();

    let mut idx = NativePropertyIndex::new();
    idx.sync(&g);
    assert_eq!(
        idx.node_ids("status", &serde_json::json!("open")),
        vec![a.id]
    );

    let b = g
        .create_node(node("task", "b", &[("status", serde_json::json!("open"))]))
        .unwrap();
    g.trim_before(g.change_head());

    idx.sync(&g);
    assert_eq!(
        idx.node_ids("status", &serde_json::json!("open")),
        vec![a.id, b.id]
    );
}

fn matched_ids(res: ExecResult) -> Vec<u64> {
    let mut ids: Vec<u64> = res
        .rows
        .iter()
        .filter_map(|row| match row.first() {
            Some(Value::Node(n)) => Some(n.id),
            _ => None,
        })
        .collect();
    ids.sort_unstable();
    ids
}

#[test]
fn indexed_property_scan_equals_full_scan_and_kv() {
    // Identical graph on both engines so ids line up.
    let build = |eng: &dyn GraphEngine| {
        eng.create_node(node(
            "task",
            "a",
            &[
                ("status", serde_json::json!("open")),
                ("prio", serde_json::json!(1)),
            ],
        ))
        .unwrap();
        eng.create_node(node(
            "task",
            "b",
            &[
                ("status", serde_json::json!("open")),
                ("prio", serde_json::json!(2)),
            ],
        ))
        .unwrap();
        eng.create_node(node(
            "task",
            "c",
            &[("status", serde_json::json!("closed"))],
        ))
        .unwrap();
        // Mixes an indexable (`status`) and a non-indexable (`score` float) prop.
        eng.create_node(node(
            "task",
            "d",
            &[
                ("status", serde_json::json!("open")),
                ("score", serde_json::json!(4.2)),
            ],
        ))
        .unwrap();
    };
    let native = NativeGraph::new();
    let kv = Drevo::open_in_memory().unwrap();
    build(&native);
    build(&kv);

    let mut idx = NativePropertyIndex::new();
    idx.sync(&native);

    // Three shapes: single indexable filter; two indexable filters; an
    // indexable + a non-indexable filter (the index narrows on `status`, the
    // exact check applies `score`). Every one must agree across the three paths.
    for cypher in [
        "MATCH (n {status: 'open'}) RETURN n",
        "MATCH (n {status: 'open', prio: 1}) RETURN n",
        "MATCH (n {status: 'open', score: 4.2}) RETURN n",
        "MATCH (n {status: 'archived'}) RETURN n",
    ] {
        let query = parse(cypher).unwrap();
        let full_scan = matched_ids(execute_on_engine(&query, &native, HashMap::new()).unwrap());
        let indexed = matched_ids(
            execute_on_engine_with_indexes(&query, &native, None, None, Some(&idx), HashMap::new())
                .unwrap(),
        );
        let kv_rows = matched_ids(execute(&query, &kv, HashMap::new()).unwrap());
        assert_eq!(
            full_scan, indexed,
            "indexed diverged from full scan for `{cypher}`"
        );
        assert_eq!(
            indexed, kv_rows,
            "native indexed diverged from KV for `{cypher}`"
        );
    }
}

#[test]
fn indexed_label_and_property_intersection_equals_full_scan_and_kv() {
    // `MATCH (n:Label {key: value})` — the common form — must use the
    // intersection of the label and property candidate sets on native and still
    // agree with the full scan and the KV store. The graph mixes the label
    // `user` as a primary kind and (for one node) as a secondary label, with
    // varying `status`, so the intersection is non-trivial.
    let build = |eng: &dyn GraphEngine| {
        // (kind, title, status, secondary-labels)
        let mk = |kind: &str, title: &str, status: &str, labels: &[&str]| {
            let mut props: Vec<(&str, serde_json::Value)> =
                vec![("status", serde_json::json!(status))];
            if !labels.is_empty() {
                props.push(("_labels", serde_json::json!(labels)));
            }
            node(kind, title, &props)
        };
        eng.create_node(mk("user", "a", "active", &[])).unwrap(); // kind user, active
        eng.create_node(mk("user", "b", "banned", &[])).unwrap(); // kind user, banned
        eng.create_node(mk("admin", "c", "active", &["user"]))
            .unwrap(); // secondary user, active
        eng.create_node(mk("task", "d", "active", &[])).unwrap(); // not a user
    };
    let native = NativeGraph::new();
    let kv = Drevo::open_in_memory().unwrap();
    build(&native);
    build(&kv);

    let mut labels = NativeLabelIndex::new();
    labels.sync(&native);
    let mut props = NativePropertyIndex::new();
    props.sync(&native);

    for cypher in [
        "MATCH (n:user {status: 'active'}) RETURN n", // a + c (b banned, d not user)
        "MATCH (n:user {status: 'banned'}) RETURN n", // b
        "MATCH (n:user) RETURN n",                    // a, b, c
        "MATCH (n:admin {status: 'active'}) RETURN n", // c
    ] {
        let query = parse(cypher).unwrap();
        let full_scan = matched_ids(execute_on_engine(&query, &native, HashMap::new()).unwrap());
        let indexed = matched_ids(
            execute_on_engine_with_indexes(
                &query,
                &native,
                None,
                Some(&labels),
                Some(&props),
                HashMap::new(),
            )
            .unwrap(),
        );
        let kv_rows = matched_ids(execute(&query, &kv, HashMap::new()).unwrap());
        assert_eq!(
            full_scan, indexed,
            "indexed diverged from full scan for `{cypher}`"
        );
        assert_eq!(
            indexed, kv_rows,
            "native indexed diverged from KV for `{cypher}`"
        );
    }
}

#[test]
fn where_equality_pushdown_equals_full_scan_and_kv() {
    // A WHERE equality must narrow through the property index exactly like an
    // inline `{key: value}`, while OR / non-indexable terms must NOT wrongly
    // narrow. All shapes must agree across native full-scan, native indexed, and
    // KV. The graph mixes `task` and `note` kinds with varying status/prio.
    let build = |eng: &dyn GraphEngine| {
        eng.create_node(node(
            "task",
            "a",
            &[
                ("status", serde_json::json!("open")),
                ("prio", serde_json::json!(1)),
            ],
        ))
        .unwrap();
        eng.create_node(node(
            "task",
            "b",
            &[
                ("status", serde_json::json!("open")),
                ("prio", serde_json::json!(2)),
            ],
        ))
        .unwrap();
        eng.create_node(node(
            "task",
            "c",
            &[("status", serde_json::json!("closed"))],
        ))
        .unwrap();
        eng.create_node(node(
            "note",
            "d",
            &[
                ("status", serde_json::json!("open")),
                ("score", serde_json::json!(4.2)),
            ],
        ))
        .unwrap();
    };
    let native = NativeGraph::new();
    let kv = Drevo::open_in_memory().unwrap();
    build(&native);
    build(&kv);

    let mut labels = NativeLabelIndex::new();
    labels.sync(&native);
    let mut props = NativePropertyIndex::new();
    props.sync(&native);

    for cypher in [
        // label-less WHERE equality → pushdown narrows on the property index
        "MATCH (n) WHERE n.status = 'open' RETURN n",
        // label + WHERE equality → label ∩ property(pushdown)
        "MATCH (n:task) WHERE n.status = 'open' RETURN n",
        // two conjunctive equalities
        "MATCH (n) WHERE n.status = 'open' AND n.prio = 1 RETURN n",
        // OR must NOT be pushed down — pushing either side would drop matches
        "MATCH (n) WHERE n.status = 'open' OR n.status = 'closed' RETURN n",
        // inline property + WHERE equality + label, all combined
        "MATCH (n:task {prio: 1}) WHERE n.status = 'open' RETURN n",
        // non-indexable WHERE term (float) must be left to the exact filter
        "MATCH (n) WHERE n.score = 4.2 RETURN n",
        // param on the right-hand side of the equality
        "MATCH (n) WHERE n.status = $s RETURN n",
    ] {
        let query = parse(cypher).unwrap();
        let params: HashMap<String, Value> =
            HashMap::from([("s".to_string(), Value::String("open".to_string()))]);
        let full_scan = matched_ids(execute_on_engine(&query, &native, params.clone()).unwrap());
        let indexed = matched_ids(
            execute_on_engine_with_indexes(
                &query,
                &native,
                None,
                Some(&labels),
                Some(&props),
                params.clone(),
            )
            .unwrap(),
        );
        let kv_rows = matched_ids(execute(&query, &kv, params).unwrap());
        assert_eq!(
            full_scan, indexed,
            "indexed diverged from full scan for `{cypher}`"
        );
        assert_eq!(
            indexed, kv_rows,
            "native indexed diverged from KV for `{cypher}`"
        );
    }
}

#[test]
fn where_in_pushdown_equals_full_scan_and_kv() {
    // `WHERE n.prop IN [...]` narrows through the property index as the UNION of
    // the elements' postings — but only when every element is indexable. All
    // shapes must agree across native full-scan, native indexed, and KV.
    let build = |eng: &dyn GraphEngine| {
        eng.create_node(node(
            "task",
            "a",
            &[
                ("status", serde_json::json!("open")),
                ("prio", serde_json::json!(1)),
            ],
        ))
        .unwrap();
        eng.create_node(node(
            "task",
            "b",
            &[
                ("status", serde_json::json!("pending")),
                ("prio", serde_json::json!(2)),
            ],
        ))
        .unwrap();
        eng.create_node(node(
            "task",
            "c",
            &[("status", serde_json::json!("closed"))],
        ))
        .unwrap();
        eng.create_node(node("note", "d", &[("status", serde_json::json!("open"))]))
            .unwrap();
    };
    let native = NativeGraph::new();
    let kv = Drevo::open_in_memory().unwrap();
    build(&native);
    build(&kv);

    let mut labels = NativeLabelIndex::new();
    labels.sync(&native);
    let mut props = NativePropertyIndex::new();
    props.sync(&native);

    let vals = Value::List(vec![
        Value::String("open".into()),
        Value::String("pending".into()),
    ]);
    for (cypher, params) in [
        // union of two postings
        (
            "MATCH (n) WHERE n.status IN ['open', 'pending'] RETURN n",
            None,
        ),
        // IN combined with a label
        (
            "MATCH (n:task) WHERE n.status IN ['open', 'closed'] RETURN n",
            None,
        ),
        // IN combined with an equality
        (
            "MATCH (n) WHERE n.status IN ['open','pending'] AND n.prio = 1 RETURN n",
            None,
        ),
        // a non-indexable element (float) means the whole IN must NOT narrow
        ("MATCH (n) WHERE n.status IN ['open', 4.2] RETURN n", None),
        // empty list matches nothing
        ("MATCH (n) WHERE n.status IN [] RETURN n", None),
        // integer list on a numeric property
        ("MATCH (n) WHERE n.prio IN [1, 2] RETURN n", None),
        // $param list on the right-hand side
        (
            "MATCH (n) WHERE n.status IN $vals RETURN n",
            Some(HashMap::from([("vals".to_string(), vals.clone())])),
        ),
    ] {
        let query = parse(cypher).unwrap();
        let params = params.unwrap_or_default();
        let full_scan = matched_ids(execute_on_engine(&query, &native, params.clone()).unwrap());
        let indexed = matched_ids(
            execute_on_engine_with_indexes(
                &query,
                &native,
                None,
                Some(&labels),
                Some(&props),
                params.clone(),
            )
            .unwrap(),
        );
        let kv_rows = matched_ids(execute(&query, &kv, params).unwrap());
        assert_eq!(
            full_scan, indexed,
            "indexed diverged from full scan for `{cypher}`"
        );
        assert_eq!(
            indexed, kv_rows,
            "native indexed diverged from KV for `{cypher}`"
        );
    }
}
