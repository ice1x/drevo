//! Integration tests for the CDC → graph bridge (Phase 15 task `00097`).
//!
//! Where the inline unit tests pin individual decode / map methods, these
//! exercise the whole `00097` path end-to-end as a deployment would: a Postgres
//! logical-replication slot's [`wal2json`] output is decoded by a
//! [`SchemaMap`], the resulting [`IngestEvent`]s are pushed onto a
//! [`MemorySource`] and drained by the unchanged `00096`
//! [`IngestConsumer`] into a [`MemoryGraphSink`], and the materialized graph is
//! asserted against the upstream relational intent. Each test frames one of
//! drevo's five target scenarios (CBT journal, story / book editor, IT task
//! manager, ERP, bug tracker), plus the CDC-specific guarantees: foreign keys
//! become edges, deletes tear nodes down, out-of-order rows survive replay, and
//! a corrupt payload is a clean error rather than a panic.
//!
//! [`wal2json`]: https://github.com/eulerto/wal2json

use drevo::streaming::{
    ForeignKey, IngestConsumer, IngestEvent, MemoryGraphSink, MemorySource, PropertyColumns,
    SchemaMap, TableMapping,
};
use serde_json::json;

/// Drain a batch of mapped events through the real `00096` consumer into a
/// fresh sink, returning the sink for assertions.
fn ingest(events: &[IngestEvent]) -> MemoryGraphSink {
    let mut source = MemorySource::new();
    for ev in events {
        source.push_event(ev).unwrap();
    }
    let mut sink = MemoryGraphSink::new();
    let mut consumer = IngestConsumer::new();
    consumer.run_to_idle(&mut source, &mut sink).unwrap();
    sink
}

// ---------------------------------------------------------------------------
// CBT journal — a wellbeing app's Postgres tail streams journal entries and
// the cognitive distortions they were tagged with, plus the foreign key
// linking them. The CDC bridge materializes the journal graph live.
// ---------------------------------------------------------------------------
#[test]
fn cbt_journal_cdc_materializes_entries_distortions_and_links() {
    let schema = SchemaMap::new()
        .map_table(
            "public.distortions",
            TableMapping::new("distortion", "id").title_column("name"),
        )
        .map_table(
            "public.journal_entries",
            TableMapping::new("journal", "id")
                .title_column("title")
                .body_column("body")
                .properties(PropertyColumns::Only(vec!["mood".into()]))
                .foreign_key(ForeignKey::new(
                    "distortion_id",
                    "exhibits",
                    "public.distortions",
                )),
        );

    let wal = json!({
        "change": [
            {"kind":"insert","schema":"public","table":"distortions",
             "columnnames":["id","name"],"columnvalues":[1,"Mind reading"]},
            {"kind":"insert","schema":"public","table":"journal_entries",
             "columnnames":["id","title","body","mood","distortion_id"],
             "columnvalues":[100,"Rough standup","I froze when asked for status.","anxious",1]}
        ]
    })
    .to_string()
    .into_bytes();

    let events = schema.map_wal2json(&wal).unwrap();
    let sink = ingest(&events);

    assert_eq!(sink.node_count(), 2);
    assert_eq!(sink.edge_count(), 1);
    let entry = sink.node("public.journal_entries:100").unwrap();
    assert_eq!(entry.kind, "journal");
    assert_eq!(entry.title, "Rough standup");
    assert_eq!(entry.properties.get("mood"), Some(&json!("anxious")));
    let edge = sink
        .edge("public.journal_entries:100-exhibits->public.distortions:1")
        .unwrap();
    assert_eq!(edge.from, "public.journal_entries:100");
    assert_eq!(edge.to, "public.distortions:1");
    assert_eq!(edge.kind, "exhibits");
}

// ---------------------------------------------------------------------------
// Story / book editor — a scene row is revised (UPDATE → last-writer-wins
// upsert) and a scrapped scene is removed (DELETE → node teardown).
// ---------------------------------------------------------------------------
#[test]
fn story_editor_cdc_revises_and_scraps_scenes() {
    let schema = SchemaMap::new().map_table(
        "public.scenes",
        TableMapping::new("scene", "id")
            .title_column("title")
            .body_column("content"),
    );

    let wal = json!({
        "change": [
            {"kind":"insert","schema":"public","table":"scenes",
             "columnnames":["id","title","content"],
             "columnvalues":[7,"The Arrival","Draft text."]},
            {"kind":"update","schema":"public","table":"scenes",
             "columnnames":["id","title","content"],
             "columnvalues":[7,"The Arrival","Revised, sharper text."],
             "oldkeys":{"keynames":["id"],"keyvalues":[7]}},
            {"kind":"insert","schema":"public","table":"scenes",
             "columnnames":["id","title","content"],
             "columnvalues":[8,"Cut Scene","To be scrapped."]},
            {"kind":"delete","schema":"public","table":"scenes",
             "oldkeys":{"keynames":["id"],"keyvalues":[8]}}
        ]
    })
    .to_string()
    .into_bytes();

    let sink = ingest(&schema.map_wal2json(&wal).unwrap());

    // Scene 7 survives, carrying its revised body (last-writer-wins).
    assert_eq!(sink.node_count(), 1);
    let scene = sink.node("public.scenes:7").unwrap();
    assert_eq!(scene.body, "Revised, sharper text.");
    // Scene 8 was created then deleted.
    assert!(sink.node("public.scenes:8").is_none());
}

// ---------------------------------------------------------------------------
// IT task manager — a high-volume burst of task rows in a single wal2json
// transaction drains in one pass; foreign keys to users become assignment
// edges.
// ---------------------------------------------------------------------------
#[test]
fn task_manager_cdc_drains_high_volume_batch() {
    let schema = SchemaMap::new()
        .map_table(
            "public.users",
            TableMapping::new("user", "id").title_column("name"),
        )
        .map_table(
            "public.tasks",
            TableMapping::new("task", "id")
                .title_column("summary")
                .properties(PropertyColumns::Only(vec!["status".into()]))
                .foreign_key(ForeignKey::new(
                    "assignee_id",
                    "assigned_to",
                    "public.users",
                )),
        );

    let mut changes = vec![json!({
        "kind":"insert","schema":"public","table":"users",
        "columnnames":["id","name"],"columnvalues":[1,"Ada"]
    })];
    for i in 0..50 {
        changes.push(json!({
            "kind":"insert","schema":"public","table":"tasks",
            "columnnames":["id","summary","status","assignee_id"],
            "columnvalues":[i, format!("Task {i}"), "open", 1]
        }));
    }
    let wal = json!({ "change": changes }).to_string().into_bytes();

    let sink = ingest(&schema.map_wal2json(&wal).unwrap());

    assert_eq!(sink.node_count(), 51); // 1 user + 50 tasks
    assert_eq!(sink.edge_count(), 50); // 50 assignment edges
    let task = sink.node("public.tasks:42").unwrap();
    assert_eq!(task.properties.get("status"), Some(&json!("open")));
}

// ---------------------------------------------------------------------------
// ERP — an order row's CDC change arrives *before* its customer's (a common
// out-of-order tail across tables). With endpoint-strict referential
// integrity the dangling edge is dead-lettered, then replayed cleanly once the
// customer exists — at-least-once + idempotency in action.
// ---------------------------------------------------------------------------
#[test]
fn erp_cdc_out_of_order_edge_dead_lettered_then_replayed() {
    let schema = SchemaMap::new()
        .map_table("public.customers", TableMapping::new("customer", "id"))
        .map_table(
            "public.orders",
            TableMapping::new("order", "id")
                .title_column("ref")
                .foreign_key(ForeignKey::new(
                    "customer_id",
                    "placed_by",
                    "public.customers",
                )),
        );

    // The order arrives first: a node + an edge to a not-yet-known customer.
    let order_wal = json!({
        "change":[{"kind":"insert","schema":"public","table":"orders",
            "columnnames":["id","ref","customer_id"],
            "columnvalues":[9001,"PO-9001",55]}]
    })
    .to_string()
    .into_bytes();
    let order_events = schema.map_wal2json(&order_wal).unwrap();

    let mut source = MemorySource::new();
    for ev in &order_events {
        source.push_event(ev).unwrap();
    }
    // Endpoint-strict sink: the dangling edge is rejected → dead-lettered.
    let mut sink = MemoryGraphSink::new().require_edge_endpoints();
    let mut consumer = IngestConsumer::new();
    consumer.run_to_idle(&mut source, &mut sink).unwrap();

    assert_eq!(sink.node_count(), 1); // order node landed
    assert_eq!(sink.edge_count(), 0); // edge could not (dangling)
    assert_eq!(consumer.dead_letters().len(), 1);

    // The customer's change arrives, then the order window replays. Because
    // every event is idempotent and keyed, the order node re-applies harmlessly
    // and the edge now resolves.
    let customer_wal = json!({
        "change":[{"kind":"insert","schema":"public","table":"customers",
            "columnnames":["id","name"],"columnvalues":[55,"Globex"]}]
    })
    .to_string()
    .into_bytes();
    for ev in schema.map_wal2json(&customer_wal).unwrap() {
        source.push_event(&ev).unwrap();
    }
    for ev in &order_events {
        source.push_event(ev).unwrap();
    }
    consumer.run_to_idle(&mut source, &mut sink).unwrap();

    assert_eq!(sink.node_count(), 2); // order + customer
    assert_eq!(sink.edge_count(), 1); // placed_by edge now resolves
    assert!(sink
        .edge("public.orders:9001-placed_by->public.customers:55")
        .is_some());
}

// ---------------------------------------------------------------------------
// Bug tracker — a corrupt replication payload (truncated JSON) is a clean,
// inspectable error from the decoder, never a panic that takes the tail down.
// ---------------------------------------------------------------------------
#[test]
fn bug_tracker_cdc_corrupt_payload_is_a_clean_error() {
    let schema = SchemaMap::new().map_table(
        "public.bugs",
        TableMapping::new("bug", "id").title_column("title"),
    );

    // A clean change maps fine...
    let ok = json!({
        "change":[{"kind":"insert","schema":"public","table":"bugs",
            "columnnames":["id","title"],"columnvalues":[1,"Crash on save"]}]
    })
    .to_string()
    .into_bytes();
    assert_eq!(schema.map_wal2json(&ok).unwrap().len(), 1);

    // ...a corrupt one errors rather than panicking.
    let corrupt = br#"{"change":[{"kind":"insert","schema":"public","tab"#;
    assert!(schema.map_wal2json(corrupt).is_err());
}

// ---------------------------------------------------------------------------
// CDC delete under REPLICA IDENTITY FULL tears down both the node and its
// foreign-key edge, applied through the real consumer/sink.
// ---------------------------------------------------------------------------
#[test]
fn cdc_delete_with_replica_identity_full_tears_down_node_and_edge() {
    let schema = SchemaMap::new()
        .map_table("public.users", TableMapping::new("user", "id"))
        .map_table(
            "public.tasks",
            TableMapping::new("task", "id")
                .title_column("summary")
                .foreign_key(ForeignKey::new(
                    "assignee_id",
                    "assigned_to",
                    "public.users",
                )),
        );

    let setup = json!({
        "change":[
            {"kind":"insert","schema":"public","table":"users",
             "columnnames":["id"],"columnvalues":[1]},
            {"kind":"insert","schema":"public","table":"tasks",
             "columnnames":["id","summary","assignee_id"],
             "columnvalues":[5,"Ship it",1]}
        ]
    })
    .to_string()
    .into_bytes();

    let mut source = MemorySource::new();
    for ev in schema.map_wal2json(&setup).unwrap() {
        source.push_event(&ev).unwrap();
    }
    let mut sink = MemoryGraphSink::new();
    let mut consumer = IngestConsumer::new();
    consumer.run_to_idle(&mut source, &mut sink).unwrap();
    assert_eq!(sink.edge_count(), 1);

    // Delete the task with the full old row image present.
    let del = json!({
        "change":[{"kind":"delete","schema":"public","table":"tasks",
            "oldkeys":{"keynames":["id","summary","assignee_id"],
                       "keyvalues":[5,"Ship it",1]}}]
    })
    .to_string()
    .into_bytes();
    for ev in schema.map_wal2json(&del).unwrap() {
        source.push_event(&ev).unwrap();
    }
    consumer.run_to_idle(&mut source, &mut sink).unwrap();

    assert!(sink.node("public.tasks:5").is_none());
    assert_eq!(sink.edge_count(), 0); // edge torn down explicitly
    assert!(sink.node("public.users:1").is_some()); // the user remains
}
