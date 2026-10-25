//! Slice 1 of per-entity conflict detection (issue #749): the engine still
//! rejects an explicit transaction when anything committed since it began,
//! but it now knows *why* — and counts how many rejections a per-entity rule
//! would have let through.

use drevo_core::conflict::{ConflictStats, DEFAULT_HISTORY};
use drevo_core::engine::GraphEngine;
use drevo_core::model::{NewEdge, NewNode, Properties};
use drevo_core::native::{CommitError, Constraint, NativeGraph, PrepareError};

fn note(title: &str) -> NewNode {
    NewNode {
        title: title.to_string(),
        kind: "note".to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn link(from: u64, to: u64) -> NewEdge {
    NewEdge {
        from_id: from,
        to_id: to,
        kind: "links".to_string(),
        weight: 1.0,
        properties: Properties::default(),
    }
}

fn stats(g: &NativeGraph) -> ConflictStats {
    g.conflict_stats()
}

#[test]
fn every_commit_advances_the_commit_sequence() {
    let g = NativeGraph::new();
    assert_eq!(g.commit_seq(), 0);
    let a = g.create_node(note("a")).unwrap();
    assert_eq!(g.commit_seq(), 1);
    let tx = g.tx_begin();
    g.tx_engine(tx).unwrap().create_node(note("b")).unwrap();
    g.tx_commit(tx).unwrap();
    assert_eq!(g.commit_seq(), 2, "a transaction commit is one commit");
    g.delete_node(a.id).unwrap();
    assert_eq!(g.commit_seq(), 3);
    assert_eq!(g.conflict_history_len(), 3);
}

#[test]
fn a_transaction_that_wrote_nothing_commits_without_touching_the_counters() {
    let g = NativeGraph::new();
    let tx = g.tx_begin();
    g.create_node(note("elsewhere")).unwrap();
    g.tx_commit(tx).unwrap();
    assert_eq!(stats(&g), ConflictStats::default());
}

#[test]
fn disjoint_writers_still_conflict_but_are_counted_as_would_pass() {
    let g = NativeGraph::new();
    let n1 = g.create_node(note("1")).unwrap();
    let n3 = g.create_node(note("3")).unwrap();

    let a = g.tx_begin();
    let b = g.tx_begin();
    g.tx_engine(a)
        .unwrap()
        .update_node(
            n1.id,
            drevo_core::model::NodePatch {
                body_html: Some("a".into()),
                ..Default::default()
            },
        )
        .unwrap();
    g.tx_engine(b)
        .unwrap()
        .update_node(
            n3.id,
            drevo_core::model::NodePatch {
                body_html: Some("b".into()),
                ..Default::default()
            },
        )
        .unwrap();
    g.tx_commit(b).unwrap();
    assert!(matches!(g.tx_commit(a), Err(CommitError::Conflict)));
    assert_eq!(
        stats(&g),
        ConflictStats {
            overlap: 0,
            would_pass: 1,
            history: 0
        }
    );
}

#[test]
fn writers_of_the_same_node_are_counted_as_overlap() {
    let g = NativeGraph::new();
    let n1 = g.create_node(note("1")).unwrap();
    let a = g.tx_begin();
    // An auto-commit write to the same node, after A began.
    g.update_node(
        n1.id,
        drevo_core::model::NodePatch {
            body_html: Some("direct".into()),
            ..Default::default()
        },
    )
    .unwrap();
    g.tx_engine(a)
        .unwrap()
        .update_node(
            n1.id,
            drevo_core::model::NodePatch {
                body_html: Some("a".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(matches!(g.tx_commit(a), Err(CommitError::Conflict)));
    assert_eq!(stats(&g).overlap, 1);
    assert_eq!(stats(&g).would_pass, 0);
}

#[test]
fn attaching_a_relationship_to_a_node_someone_deleted_is_an_overlap() {
    let g = NativeGraph::new();
    let n1 = g.create_node(note("1")).unwrap();
    let n2 = g.create_node(note("2")).unwrap();
    let a = g.tx_begin();
    g.tx_engine(a)
        .unwrap()
        .create_edge(link(n1.id, n2.id))
        .unwrap();
    g.delete_node(n2.id).unwrap();
    assert!(matches!(g.tx_commit(a), Err(CommitError::Conflict)));
    assert_eq!(stats(&g).overlap, 1);

    // The mirror image: A deletes the node, someone else attached to it.
    let n4 = g.create_node(note("4")).unwrap();
    let b = g.tx_begin();
    g.tx_engine(b).unwrap().delete_node(n4.id).unwrap();
    g.create_edge(link(n1.id, n4.id)).unwrap();
    assert!(matches!(g.tx_commit(b), Err(CommitError::Conflict)));
    assert_eq!(stats(&g).overlap, 2);
    assert_eq!(stats(&g).would_pass, 0);
}

#[test]
fn a_schema_change_overlaps_with_every_transaction() {
    let g = NativeGraph::new();
    let n1 = g.create_node(note("1")).unwrap();
    let a = g.tx_begin();
    g.tx_engine(a)
        .unwrap()
        .update_node(
            n1.id,
            drevo_core::model::NodePatch {
                body_html: Some("a".into()),
                ..Default::default()
            },
        )
        .unwrap();
    g.add_constraint(Constraint::PropertyExists {
        kind: "note".into(),
        property: "title".into(),
    })
    .unwrap();
    assert!(matches!(g.tx_commit(a), Err(CommitError::Conflict)));
    assert_eq!(stats(&g).overlap, 1);
}

#[test]
fn a_transaction_older_than_the_history_is_counted_as_history() {
    let g = NativeGraph::new();
    g.set_conflict_history(2);
    let n1 = g.create_node(note("1")).unwrap();
    let a = g.tx_begin();
    for i in 0..3 {
        g.create_node(note(&format!("later {i}"))).unwrap();
    }
    g.tx_engine(a)
        .unwrap()
        .update_node(
            n1.id,
            drevo_core::model::NodePatch {
                body_html: Some("a".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(matches!(g.tx_commit(a), Err(CommitError::Conflict)));
    assert_eq!(
        stats(&g),
        ConflictStats {
            overlap: 0,
            would_pass: 0,
            history: 1
        }
    );
    assert_eq!(g.conflict_history_len(), 2);
    assert_eq!(
        NativeGraph::new().conflict_history_capacity(),
        DEFAULT_HISTORY
    );
}

#[test]
fn a_rejected_prepare_is_classified_too() {
    let g = NativeGraph::new();
    let n1 = g.create_node(note("1")).unwrap();
    let n3 = g.create_node(note("3")).unwrap();
    let a = g.tx_begin();
    g.tx_engine(a)
        .unwrap()
        .update_node(
            n1.id,
            drevo_core::model::NodePatch {
                body_html: Some("a".into()),
                ..Default::default()
            },
        )
        .unwrap();
    g.update_node(
        n3.id,
        drevo_core::model::NodePatch {
            body_html: Some("direct".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(matches!(
        g.tx_prepare(a, "gid-1"),
        Err(PrepareError::Conflict)
    ));
    assert_eq!(stats(&g).would_pass, 1);
}

#[test]
fn the_embedded_transaction_handle_is_classified_too() {
    let g = NativeGraph::new();
    let n1 = g.create_node(note("1")).unwrap();
    let n3 = g.create_node(note("3")).unwrap();
    let mut tx = g.begin();
    tx.update_node(
        n1.id,
        drevo_core::model::NodePatch {
            body_html: Some("tx".into()),
            ..Default::default()
        },
    )
    .unwrap();
    g.update_node(
        n3.id,
        drevo_core::model::NodePatch {
            body_html: Some("direct".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(matches!(tx.commit(), Err(CommitError::Conflict)));
    assert_eq!(stats(&g).would_pass, 1);
}
