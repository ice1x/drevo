//! Integration tests for the streaming-ingestion engine (Phase 15 task
//! `00096`).
//!
//! Where the inline unit tests pin individual methods, these exercise the
//! [`IngestConsumer`] + [`StreamSource`] + [`IngestSink`] triple end-to-end as
//! an operator would: a broker partition (modelled by [`MemorySource`]) carries
//! a realistic firehose of change events, the consumer drains it into a
//! [`MemoryGraphSink`], and the materialized graph is asserted against the
//! producer's intent. Each test frames the stream around one of drevo's five
//! target scenarios (CBT journal, story / book editor, IT task manager, ERP,
//! bug tracker), plus the broker-grade guarantees: at-least-once replay,
//! dead-lettering, and resume-after-crash.

use drevo::streaming::{
    ErrorPolicy, EventProperties, IngestConsumer, IngestEvent, MemoryGraphSink, MemorySource,
    Offset, StreamSource, Transport,
};
use serde_json::json;

/// Build an `UpsertNode` event with optional properties.
fn node(key: &str, kind: &str, title: &str, body: &str, props: EventProperties) -> IngestEvent {
    IngestEvent::UpsertNode {
        key: key.into(),
        kind: kind.into(),
        title: title.into(),
        body: body.into(),
        properties: props,
    }
}

/// Build an `UpsertEdge` event.
fn edge(key: &str, from: &str, to: &str, kind: &str) -> IngestEvent {
    IngestEvent::UpsertEdge {
        key: key.into(),
        from: from.into(),
        to: to.into(),
        kind: kind.into(),
        weight: 1.0,
        properties: EventProperties::new(),
    }
}

/// CBT journal — a wellbeing app streams journal entries and the cognitive
/// distortions / emotions the user tagged, plus the links between them. The
/// consumer materializes the journal graph in real time.
#[test]
fn cbt_journal_stream_materializes_entries_and_tags() {
    let mut source = MemorySource::new();
    source
        .push_event(&node(
            "entry-2026-06-13",
            "journal",
            "Rough standup",
            "I froze when asked for a status update.",
            EventProperties::from([("mood".into(), json!("anxious"))]),
        ))
        .unwrap();
    source
        .push_event(&node(
            "distortion-mindreading",
            "distortion",
            "Mind reading",
            "Assuming others judged me.",
            EventProperties::new(),
        ))
        .unwrap();
    source
        .push_event(&edge(
            "link-1",
            "entry-2026-06-13",
            "distortion-mindreading",
            "exhibits",
        ))
        .unwrap();

    let mut sink = MemoryGraphSink::new();
    let mut consumer = IngestConsumer::new();
    let report = consumer.run_to_idle(&mut source, &mut sink).unwrap();

    assert_eq!(report.applied, 3);
    assert_eq!(sink.node_count(), 2);
    assert_eq!(sink.edge_count(), 1);
    assert_eq!(
        sink.node("entry-2026-06-13").unwrap().properties["mood"],
        json!("anxious")
    );
    let link = sink.edge("link-1").unwrap();
    assert_eq!(link.from, "entry-2026-06-13");
    assert_eq!(link.kind, "exhibits");
    // Broker is fully committed.
    assert_eq!(source.committed(), Offset(3));
}

/// Story / book editor — chapters arrive, then a later edit revises a chapter
/// title (an upsert under the same key) and deletes a scrapped scene. Last
/// writer wins; the graph reflects the latest state.
#[test]
fn story_editor_stream_applies_revisions_and_deletions() {
    let mut source = MemorySource::new();
    source
        .push_event(&node(
            "ch-1",
            "chapter",
            "Chaptr One",
            "draft",
            EventProperties::new(),
        ))
        .unwrap();
    source
        .push_event(&node(
            "scene-1a",
            "scene",
            "Cut scene",
            "to be removed",
            EventProperties::new(),
        ))
        .unwrap();
    // A later editorial pass fixes the typo in the chapter title …
    source
        .push_event(&node(
            "ch-1",
            "chapter",
            "Chapter One",
            "revised",
            EventProperties::new(),
        ))
        .unwrap();
    // … and scraps the scene.
    source
        .push_event(&IngestEvent::DeleteNode {
            key: "scene-1a".into(),
        })
        .unwrap();

    let mut sink = MemoryGraphSink::new();
    let mut consumer = IngestConsumer::new();
    consumer.run_to_idle(&mut source, &mut sink).unwrap();

    assert_eq!(sink.node_count(), 1);
    let ch = sink.node("ch-1").unwrap();
    assert_eq!(ch.title, "Chapter One");
    assert_eq!(ch.body, "revised");
    assert!(sink.node("scene-1a").is_none());
}

/// IT task manager — a CI system streams task status changes faster than they
/// can be consumed; a small batch size means several polling rounds. Every
/// update lands and the offset advances monotonically.
#[test]
fn task_manager_high_volume_stream_drains_in_batches() {
    let mut source = MemorySource::new();
    for i in 0..50 {
        source
            .push_event(&node(
                &format!("task-{i}"),
                "task",
                &format!("Task {i}"),
                "open",
                EventProperties::from([("status".into(), json!("todo"))]),
            ))
            .unwrap();
    }

    let mut sink = MemoryGraphSink::new();
    let mut consumer = IngestConsumer::new().with_batch_size(8);
    let report = consumer.run_to_idle(&mut source, &mut sink).unwrap();

    assert_eq!(report.applied, 50);
    assert_eq!(sink.node_count(), 50);
    assert_eq!(source.committed(), Offset(50));
    assert_eq!(consumer.applied_total(), 50);
}

/// ERP — an upstream Postgres CDC tail streams purchase orders and the
/// supplier they belong to, but referential integrity matters: an edge whose
/// endpoints have not yet arrived must not silently create a dangling link.
/// The strict sink dead-letters the premature edge; a later replay (after the
/// nodes exist) would succeed.
#[test]
fn erp_cdc_stream_dead_letters_dangling_edges() {
    let mut source = MemorySource::new();
    // The edge arrives BEFORE its endpoints (out-of-order CDC).
    source
        .push_event(&edge(
            "po-belongs-1",
            "po-1001",
            "supplier-acme",
            "ordered_from",
        ))
        .unwrap();
    source
        .push_event(&node(
            "po-1001",
            "purchase_order",
            "PO-1001",
            "",
            EventProperties::new(),
        ))
        .unwrap();
    source
        .push_event(&node(
            "supplier-acme",
            "supplier",
            "ACME Corp",
            "",
            EventProperties::new(),
        ))
        .unwrap();

    let mut sink = MemoryGraphSink::new().require_edge_endpoints();
    let mut consumer = IngestConsumer::new(); // DeadLetter default
    let report = consumer.run_to_idle(&mut source, &mut sink).unwrap();

    assert_eq!(report.dead_lettered, 1);
    assert_eq!(report.applied, 2); // the two nodes
    assert_eq!(sink.node_count(), 2);
    assert_eq!(sink.edge_count(), 0);
    assert_eq!(consumer.dead_letters().len(), 1);
    assert_eq!(consumer.dead_letters()[0].offset, Offset(1));

    // Operator replays the dead-lettered edge now that the endpoints exist.
    let mut replay = MemorySource::new();
    let payload = consumer.dead_letters()[0].payload.clone();
    replay.push(payload);
    let mut replay_consumer = IngestConsumer::new();
    replay_consumer.run_to_idle(&mut replay, &mut sink).unwrap();
    assert_eq!(sink.edge_count(), 1);
    assert_eq!(sink.edge("po-belongs-1").unwrap().to, "supplier-acme");
}

/// Bug tracker — a flaky producer interleaves valid bug events with a corrupt
/// payload. Under the dead-letter policy the corrupt message is quarantined and
/// ingestion continues; the dead-letter queue preserves the raw bytes for
/// inspection.
#[test]
fn bug_tracker_stream_quarantines_corrupt_payloads() {
    let mut source = MemorySource::new();
    source
        .push_event(&node(
            "bug-1",
            "bug",
            "NPE on save",
            "stack trace…",
            EventProperties::new(),
        ))
        .unwrap();
    source.push(b"{ this is corrupt json".to_vec());
    source
        .push_event(&node(
            "bug-2",
            "bug",
            "Timeout on export",
            "…",
            EventProperties::new(),
        ))
        .unwrap();

    let mut sink = MemoryGraphSink::new();
    let mut consumer = IngestConsumer::new();
    let report = consumer.run_to_idle(&mut source, &mut sink).unwrap();

    assert_eq!(report.applied, 2);
    assert_eq!(report.dead_lettered, 1);
    assert_eq!(sink.node_count(), 2);
    let dl = &consumer.dead_letters()[0];
    assert_eq!(dl.offset, Offset(2));
    assert_eq!(dl.payload, b"{ this is corrupt json");
    assert!(dl.reason.contains("malformed") || !dl.reason.is_empty());
}

/// At-least-once delivery — a consumer crashes after applying a batch but
/// before committing all of it. On restart the broker re-delivers the
/// uncommitted suffix; idempotent upserts mean the graph is identical to a
/// crash-free run, with no duplicates.
#[test]
fn at_least_once_replay_is_idempotent_after_crash() {
    let mut source = MemorySource::new();
    for i in 0..6 {
        source
            .push_event(&node(
                &format!("n{i}"),
                "note",
                &format!("T{i}"),
                "",
                EventProperties::new(),
            ))
            .unwrap();
    }

    let mut sink = MemoryGraphSink::new();

    // First run: process the first 4 in two batches, but the second commit is
    // "lost" to a crash — only offset 2 made it to durable storage.
    let mut c1 = IngestConsumer::new().with_batch_size(2);
    c1.run_once(&mut source, &mut sink).unwrap(); // n0,n1 -> commit 2
    c1.run_once(&mut source, &mut sink).unwrap(); // n2,n3 applied to sink…
                                                  // …but simulate the commit never reaching the broker: roll committed back.
                                                  // (MemorySource committed is at 4 here; emulate the lost commit by rewinding
                                                  //  to a hand-set committed mark of 2 via a fresh source replay.)
    let committed_before_crash = Offset(2);

    // Restart: rebuild the broker view from the durable committed mark and
    // re-deliver everything after it.
    let mut restarted = MemorySource::new();
    for i in 0..6 {
        restarted
            .push_event(&node(
                &format!("n{i}"),
                "note",
                &format!("T{i}"),
                "",
                EventProperties::new(),
            ))
            .unwrap();
    }
    restarted.commit(committed_before_crash);
    restarted.rewind_to_committed();

    let mut c2 = IngestConsumer::new().resume_after(restarted.committed());
    c2.run_to_idle(&mut restarted, &mut sink).unwrap();

    // All six present exactly once despite n2,n3 being delivered twice.
    assert_eq!(sink.node_count(), 6);
    for i in 0..6 {
        assert!(sink.node(&format!("n{i}")).is_some());
    }
    assert_eq!(restarted.committed(), Offset(6));
}

/// The halt policy gives an attended operator a hard stop on the first bad
/// message, while still committing the clean prefix so a fix can resume past
/// the failure.
#[test]
fn halt_policy_stops_but_commits_clean_prefix() {
    let mut source = MemorySource::new();
    source
        .push_event(&node("ok-1", "note", "A", "", EventProperties::new()))
        .unwrap();
    source
        .push_event(&node("ok-2", "note", "B", "", EventProperties::new()))
        .unwrap();
    source.push(b"corrupt".to_vec());
    source
        .push_event(&node("ok-3", "note", "C", "", EventProperties::new()))
        .unwrap();

    let mut sink = MemoryGraphSink::new();
    let mut consumer = IngestConsumer::new().with_policy(ErrorPolicy::Halt);
    let err = consumer.run_once(&mut source, &mut sink).unwrap_err();

    assert_eq!(consumer.policy(), ErrorPolicy::Halt);
    assert!(err.to_string().contains("offset 3"));
    // The two clean events before the failure are applied and committed.
    assert_eq!(sink.node_count(), 2);
    assert_eq!(source.committed(), Offset(2));
}

/// The `Transport` label is descriptive metadata an operator can attach to a
/// source for logs / metrics; durable transports survive a restart.
#[test]
fn transport_labels_describe_broker_durability() {
    assert!(Transport::Kafka.is_durable());
    assert!(Transport::Nats.is_durable());
    assert!(Transport::Cdc.is_durable());
    assert!(!Transport::InMemory.is_durable());
    assert_eq!(Transport::Kafka.as_str(), "kafka");
}
