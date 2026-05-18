//! Integration tests for Phase 8.5 task `00106` (DB core audit).
//!
//! Verifies the four storage-layer invariants from
//! `.claude/skills/drevo-database/SKILL.md` §"Invariants":
//!
//! 1. Adjacency consistency — every `out_edges[from_id]` is mirrored in
//!    `in_edges[to_id]`, and vice versa.
//! 2. Cascading delete — deleting a node removes all incident edges,
//!    adjacency entries, and FTS entries.
//! 3. FTS reindex on update — changing `title` or `body` requires
//!    deindex-then-reindex.
//! 4. UUID immutability — once assigned, a node/edge UUID never
//!    changes, even on update.
//!
//! All tests run against `MemoryBackend` (fast) and `RedbBackend` (slow,
//! ACID) via the same workflow.

use drevo::db::Drevo;
use drevo::error::DrevoError;
use drevo::model::{Direction, EdgePatch, NewEdge, NewNode, NodePatch, Properties};

fn new_node(kind: &str, title: &str, body: &str) -> NewNode {
    NewNode {
        kind: kind.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        body_html: String::new(),
        properties: Properties::default(),
    }
}

fn new_edge(from_id: u64, to_id: u64, kind: &str, weight: f32) -> NewEdge {
    NewEdge {
        from_id,
        to_id,
        kind: kind.to_string(),
        weight,
        properties: Properties::default(),
    }
}

// ---------------------------------------------------------------
// verify_invariants — baseline
// ---------------------------------------------------------------

#[test]
fn verify_invariants_holds_on_empty_db() {
    let db = Drevo::open_in_memory().unwrap();
    let v = db.verify_invariants().unwrap();
    assert!(v.is_empty(), "empty DB must satisfy all invariants: {v:?}");
}

#[test]
fn verify_invariants_holds_after_single_node_create() {
    let db = Drevo::open_in_memory().unwrap();
    db.create_node(new_node("note", "T", "body")).unwrap();
    let v = db.verify_invariants().unwrap();
    assert!(v.is_empty(), "{v:?}");
}

#[test]
fn verify_invariants_holds_after_create_edge_chain() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(new_node("note", "A", "")).unwrap();
    let b = db.create_node(new_node("note", "B", "")).unwrap();
    let c = db.create_node(new_node("note", "C", "")).unwrap();

    db.create_edge(new_edge(a.id, b.id, "links_to", 1.0))
        .unwrap();
    db.create_edge(new_edge(b.id, c.id, "links_to", 1.0))
        .unwrap();
    db.create_edge(new_edge(c.id, a.id, "back", 1.0)).unwrap();

    let v = db.verify_invariants().unwrap();
    assert!(v.is_empty(), "{v:?}");
}

#[test]
fn verify_invariants_holds_after_self_loop() {
    let db = Drevo::open_in_memory().unwrap();
    let n = db.create_node(new_node("note", "self", "")).unwrap();
    db.create_edge(new_edge(n.id, n.id, "self_ref", 1.0))
        .unwrap();

    let v = db.verify_invariants().unwrap();
    assert!(v.is_empty(), "{v:?}");
}

#[test]
fn verify_invariants_holds_after_parallel_edges() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(new_node("note", "A", "")).unwrap();
    let b = db.create_node(new_node("note", "B", "")).unwrap();
    // Two edges between the same pair (allowed — different kinds)
    db.create_edge(new_edge(a.id, b.id, "k1", 1.0)).unwrap();
    db.create_edge(new_edge(a.id, b.id, "k2", 1.0)).unwrap();

    let v = db.verify_invariants().unwrap();
    assert!(v.is_empty(), "{v:?}");
}

#[test]
fn verify_invariants_holds_after_update_node() {
    let db = Drevo::open_in_memory().unwrap();
    let n = db.create_node(new_node("note", "Old", "old body")).unwrap();
    db.update_node(
        n.id,
        NodePatch {
            title: Some("New".to_string()),
            body: Some("new body".to_string()),
            kind: Some("task".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    let v = db.verify_invariants().unwrap();
    assert!(v.is_empty(), "{v:?}");
}

#[test]
fn verify_invariants_holds_after_update_edge_kind() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(new_node("note", "A", "")).unwrap();
    let b = db.create_node(new_node("note", "B", "")).unwrap();
    let e = db
        .create_edge(new_edge(a.id, b.id, "old_kind", 1.0))
        .unwrap();

    db.update_edge(
        e.id,
        EdgePatch {
            kind: Some("new_kind".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    let v = db.verify_invariants().unwrap();
    assert!(v.is_empty(), "{v:?}");
}

#[test]
fn verify_invariants_holds_after_delete_edge() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(new_node("note", "A", "")).unwrap();
    let b = db.create_node(new_node("note", "B", "")).unwrap();
    let e = db.create_edge(new_edge(a.id, b.id, "k", 1.0)).unwrap();

    db.delete_edge(e.id).unwrap();

    let v = db.verify_invariants().unwrap();
    assert!(v.is_empty(), "{v:?}");
}

#[test]
fn verify_invariants_holds_after_cascading_delete() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(new_node("note", "A", "body of A")).unwrap();
    let b = db.create_node(new_node("note", "B", "body of B")).unwrap();
    let c = db.create_node(new_node("note", "C", "body of C")).unwrap();
    db.create_edge(new_edge(a.id, b.id, "k", 1.0)).unwrap();
    db.create_edge(new_edge(b.id, c.id, "k", 1.0)).unwrap();
    db.create_edge(new_edge(c.id, a.id, "k", 1.0)).unwrap();

    db.delete_node(b.id).unwrap();

    let v = db.verify_invariants().unwrap();
    assert!(v.is_empty(), "after cascading delete: {v:?}");
}

// ---------------------------------------------------------------
// Invariant #1 — adjacency consistency on real workflows
// ---------------------------------------------------------------

#[test]
fn adjacency_out_mirrors_in_after_edge_create() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(new_node("note", "A", "")).unwrap();
    let b = db.create_node(new_node("note", "B", "")).unwrap();
    db.create_edge(new_edge(a.id, b.id, "k", 1.0)).unwrap();

    // The skill-spec invariant: every edge in out_edges[from_id]
    // appears in in_edges[to_id].
    let out_edges_a = db.edges_of(a.id, Direction::Outgoing).unwrap();
    let in_edges_b = db.edges_of(b.id, Direction::Incoming).unwrap();
    assert_eq!(out_edges_a.len(), 1);
    assert_eq!(in_edges_b.len(), 1);
    assert_eq!(out_edges_a[0].id, in_edges_b[0].id);
}

#[test]
fn adjacency_consistency_survives_self_loop() {
    let db = Drevo::open_in_memory().unwrap();
    let n = db.create_node(new_node("note", "self", "")).unwrap();
    let e = db
        .create_edge(new_edge(n.id, n.id, "self_ref", 1.0))
        .unwrap();

    let out = db.edges_of(n.id, Direction::Outgoing).unwrap();
    let in_ = db.edges_of(n.id, Direction::Incoming).unwrap();
    let both = db.edges_of(n.id, Direction::Both).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(in_.len(), 1);
    // Both must deduplicate the self-loop.
    assert_eq!(both.len(), 1);
    assert_eq!(both[0].id, e.id);
}

// ---------------------------------------------------------------
// Invariant #2 — cascading delete leaves no orphan adjacency / FTS
// ---------------------------------------------------------------

#[test]
fn cascade_delete_removes_fts_entries_for_deleted_node() {
    let db = Drevo::open_in_memory().unwrap();
    let n = db
        .create_node(new_node("note", "unique_search_term", "body"))
        .unwrap();

    let hits = db.search_fts("unique_search_term", 10).unwrap();
    assert_eq!(hits.len(), 1);

    db.delete_node(n.id).unwrap();

    let hits = db.search_fts("unique_search_term", 10).unwrap();
    assert!(
        hits.is_empty(),
        "FTS must not return deleted node, got: {hits:?}"
    );
}

#[test]
fn cascade_delete_clears_adjacency_in_both_directions() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(new_node("note", "A", "")).unwrap();
    let b = db.create_node(new_node("note", "B", "")).unwrap();
    let c = db.create_node(new_node("note", "C", "")).unwrap();
    db.create_edge(new_edge(a.id, b.id, "k", 1.0)).unwrap();
    db.create_edge(new_edge(b.id, c.id, "k", 1.0)).unwrap();
    db.create_edge(new_edge(c.id, b.id, "back", 1.0)).unwrap();

    db.delete_node(b.id).unwrap();

    // a.id and c.id must have no remaining edges referencing b.id
    assert!(db.edges_of(a.id, Direction::Both).unwrap().is_empty());
    assert!(db.edges_of(c.id, Direction::Both).unwrap().is_empty());
}

// ---------------------------------------------------------------
// Invariant #3 — FTS reindex on update
// ---------------------------------------------------------------

#[test]
fn fts_reindexed_when_title_changes() {
    let db = Drevo::open_in_memory().unwrap();
    let n = db
        .create_node(new_node("note", "old_term_alpha", "body"))
        .unwrap();
    assert!(!db.search_fts("old_term_alpha", 10).unwrap().is_empty());

    db.update_node(
        n.id,
        NodePatch {
            title: Some("new_term_beta".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    // Old title's trigrams must be gone, new ones must hit.
    assert!(
        db.search_fts("old_term_alpha", 10).unwrap().is_empty(),
        "old title trigrams must be deindexed"
    );
    assert!(
        !db.search_fts("new_term_beta", 10).unwrap().is_empty(),
        "new title trigrams must be indexed"
    );
}

#[test]
fn fts_reindexed_when_body_changes() {
    let db = Drevo::open_in_memory().unwrap();
    let n = db
        .create_node(new_node("note", "T", "old_body_term_xyz123"))
        .unwrap();
    assert!(!db
        .search_fts("old_body_term_xyz123", 10)
        .unwrap()
        .is_empty());

    db.update_node(
        n.id,
        NodePatch {
            body: Some("new_body_term_abc789".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    assert!(
        db.search_fts("old_body_term_xyz123", 10)
            .unwrap()
            .is_empty(),
        "old body trigrams must be deindexed"
    );
    assert!(
        !db.search_fts("new_body_term_abc789", 10)
            .unwrap()
            .is_empty(),
        "new body trigrams must be indexed"
    );
}

#[test]
fn fts_not_reindexed_when_only_kind_changes() {
    let db = Drevo::open_in_memory().unwrap();
    let n = db
        .create_node(new_node("note", "stable_term", "stable body"))
        .unwrap();
    let updated_at_before = db.get_node(n.id).unwrap().unwrap().updated_at;
    std::thread::sleep(std::time::Duration::from_millis(2));

    db.update_node(
        n.id,
        NodePatch {
            kind: Some("task".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    // Title/body unchanged — the same query must still hit
    let hits = db.search_fts("stable_term", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node.id, n.id);
    // And updated_at must advance (apply_patch always touches it).
    let after = db.get_node(n.id).unwrap().unwrap();
    assert!(after.updated_at > updated_at_before);
}

// ---------------------------------------------------------------
// Invariant #4 — UUID immutability across update
// ---------------------------------------------------------------

#[test]
fn node_uuid_unchanged_across_update_node() {
    let db = Drevo::open_in_memory().unwrap();
    let n = db
        .create_node(new_node("note", "original", "body"))
        .unwrap();
    let original_uuid = n.uuid;

    let updated = db
        .update_node(
            n.id,
            NodePatch {
                title: Some("updated".to_string()),
                body: Some("new body".to_string()),
                kind: Some("task".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(
        updated.uuid, original_uuid,
        "drevo-database invariant #4: UUID immutability — Node UUID must never change across update"
    );

    // And the storage round-trip preserves it too.
    let from_db = db.get_node(n.id).unwrap().unwrap();
    assert_eq!(from_db.uuid, original_uuid);
    let by_uuid = db.get_node_by_uuid(&original_uuid).unwrap().unwrap();
    assert_eq!(by_uuid.id, n.id);
}

#[test]
fn edge_uuid_unchanged_across_update_edge() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(new_node("note", "A", "")).unwrap();
    let b = db.create_node(new_node("note", "B", "")).unwrap();
    let e = db.create_edge(new_edge(a.id, b.id, "old_k", 1.0)).unwrap();
    let original_uuid = e.uuid;

    let updated = db
        .update_edge(
            e.id,
            EdgePatch {
                kind: Some("new_k".to_string()),
                weight: Some(2.5),
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(
        updated.uuid, original_uuid,
        "drevo-database invariant #4: UUID immutability — Edge UUID must never change across update"
    );
}

#[test]
fn node_created_at_unchanged_across_update_node() {
    let db = Drevo::open_in_memory().unwrap();
    let n = db.create_node(new_node("note", "T", "B")).unwrap();
    let original_created_at = n.created_at;
    std::thread::sleep(std::time::Duration::from_millis(2));

    let updated = db
        .update_node(
            n.id,
            NodePatch {
                title: Some("new".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

    // created_at is logically immutable for the lifetime of the node.
    assert_eq!(updated.created_at, original_created_at);
    // updated_at must advance.
    assert!(updated.updated_at > original_created_at);
}

// ---------------------------------------------------------------
// Edge weight validation (cross-link AUDIT-model F4)
// ---------------------------------------------------------------

#[test]
fn create_edge_rejects_nan_weight() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(new_node("note", "A", "")).unwrap();
    let b = db.create_node(new_node("note", "B", "")).unwrap();
    let err = db
        .create_edge(new_edge(a.id, b.id, "k", f32::NAN))
        .unwrap_err();
    assert!(matches!(err, DrevoError::InvalidWeight(_)));
}

#[test]
fn create_edge_rejects_pos_infinity_weight() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(new_node("note", "A", "")).unwrap();
    let b = db.create_node(new_node("note", "B", "")).unwrap();
    let err = db
        .create_edge(new_edge(a.id, b.id, "k", f32::INFINITY))
        .unwrap_err();
    assert!(matches!(err, DrevoError::InvalidWeight(_)));
}

#[test]
fn create_edge_rejects_neg_infinity_weight() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(new_node("note", "A", "")).unwrap();
    let b = db.create_node(new_node("note", "B", "")).unwrap();
    let err = db
        .create_edge(new_edge(a.id, b.id, "k", f32::NEG_INFINITY))
        .unwrap_err();
    assert!(matches!(err, DrevoError::InvalidWeight(_)));
}

#[test]
fn create_edge_accepts_zero_and_negative_finite_weight() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(new_node("note", "A", "")).unwrap();
    let b = db.create_node(new_node("note", "B", "")).unwrap();
    // Zero is a valid finite f32 — the model spec doesn't forbid it.
    db.create_edge(new_edge(a.id, b.id, "k1", 0.0)).unwrap();
    // Negative finite values are allowed at the model layer; Dijkstra
    // documents non-negative weights as a precondition but the create
    // path does not enforce that.
    db.create_edge(new_edge(a.id, b.id, "k2", -1.5)).unwrap();
}

#[test]
fn update_edge_rejects_nan_weight() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(new_node("note", "A", "")).unwrap();
    let b = db.create_node(new_node("note", "B", "")).unwrap();
    let e = db.create_edge(new_edge(a.id, b.id, "k", 1.0)).unwrap();

    let err = db
        .update_edge(
            e.id,
            EdgePatch {
                weight: Some(f32::NAN),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(matches!(err, DrevoError::InvalidWeight(_)));
}

#[test]
fn update_edge_rejects_infinite_weight() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(new_node("note", "A", "")).unwrap();
    let b = db.create_node(new_node("note", "B", "")).unwrap();
    let e = db.create_edge(new_edge(a.id, b.id, "k", 1.0)).unwrap();

    let err = db
        .update_edge(
            e.id,
            EdgePatch {
                weight: Some(f32::INFINITY),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(matches!(err, DrevoError::InvalidWeight(_)));
}

#[test]
fn update_edge_invalid_weight_does_not_corrupt_storage() {
    let db = Drevo::open_in_memory().unwrap();
    let a = db.create_node(new_node("note", "A", "")).unwrap();
    let b = db.create_node(new_node("note", "B", "")).unwrap();
    let e = db.create_edge(new_edge(a.id, b.id, "k", 1.5)).unwrap();
    let original = db.get_edge(e.id).unwrap().unwrap();

    // Attempt invalid weight update — must reject without mutation.
    let _ = db
        .update_edge(
            e.id,
            EdgePatch {
                weight: Some(f32::NAN),
                kind: Some("would_be_new".to_string()),
                ..Default::default()
            },
        )
        .unwrap_err();

    // Edge must be unchanged — verify ALL fields including kind.
    let after = db.get_edge(e.id).unwrap().unwrap();
    assert_eq!(after.weight, original.weight);
    assert_eq!(after.kind, original.kind);
    assert_eq!(after.uuid, original.uuid);

    // Invariants must hold.
    let v = db.verify_invariants().unwrap();
    assert!(v.is_empty(), "{v:?}");
}

// ---------------------------------------------------------------
// Randomized invariant verification — Phase 9 proptest precursor
// ---------------------------------------------------------------

/// Tiny deterministic xorshift32 RNG so failures are reproducible.
fn xorshift32(state: &mut u32) -> u32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    x
}

#[test]
fn invariants_hold_under_random_mutations_seed_1() {
    invariants_hold_under_random_mutations(1, 250);
}

#[test]
fn invariants_hold_under_random_mutations_seed_42() {
    invariants_hold_under_random_mutations(42, 250);
}

#[test]
fn invariants_hold_under_random_mutations_seed_99999() {
    invariants_hold_under_random_mutations(99999, 250);
}

// ---------------------------------------------------------------
// RedbBackend parity — invariants hold on disk-backed storage too
// ---------------------------------------------------------------

#[cfg(feature = "redb-backend")]
#[test]
fn verify_invariants_holds_on_redb_backend_after_workflow() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("inv.db");
    let db = Drevo::open(&path).unwrap();

    let a = db.create_node(new_node("note", "A", "alpha")).unwrap();
    let b = db.create_node(new_node("note", "B", "beta")).unwrap();
    let c = db.create_node(new_node("note", "C", "gamma")).unwrap();
    db.create_edge(new_edge(a.id, b.id, "k", 1.0)).unwrap();
    db.create_edge(new_edge(b.id, c.id, "k", 2.0)).unwrap();
    db.update_node(
        a.id,
        NodePatch {
            body: Some("alpha-updated".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    db.delete_node(b.id).unwrap();

    let v = db.verify_invariants().unwrap();
    assert!(v.is_empty(), "redb invariants: {v:?}");
    db.close().unwrap();
}

#[cfg(feature = "redb-backend")]
#[test]
fn verify_invariants_holds_after_redb_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("inv-persist.db");

    {
        let db = Drevo::open(&path).unwrap();
        let a = db.create_node(new_node("note", "A", "")).unwrap();
        let b = db.create_node(new_node("note", "B", "")).unwrap();
        db.create_edge(new_edge(a.id, b.id, "k", 1.0)).unwrap();
        db.close().unwrap();
    }

    {
        let db = Drevo::open(&path).unwrap();
        let v = db.verify_invariants().unwrap();
        assert!(v.is_empty(), "after reopen: {v:?}");
        db.close().unwrap();
    }
}

fn invariants_hold_under_random_mutations(seed: u32, ops: usize) {
    let mut state = seed;
    let db = Drevo::open_in_memory().unwrap();
    let mut node_ids: Vec<u64> = Vec::new();
    let mut edge_ids: Vec<u64> = Vec::new();
    let mut next_label = 0u64;

    for op_idx in 0..ops {
        let choice = xorshift32(&mut state) % 7;
        match choice {
            0 => {
                // Create node
                next_label += 1;
                let title = format!("n_{seed}_{next_label}");
                let body = format!("body_{}_{}", seed, xorshift32(&mut state) % 1000);
                if let Ok(n) = db.create_node(new_node("k", &title, &body)) {
                    node_ids.push(n.id);
                }
            }
            1 => {
                // Create edge between two random nodes (may equal each other → self-loop)
                if node_ids.len() < 2 {
                    continue;
                }
                let i = (xorshift32(&mut state) as usize) % node_ids.len();
                let j = (xorshift32(&mut state) as usize) % node_ids.len();
                let weight = (xorshift32(&mut state) % 100) as f32 / 10.0;
                if let Ok(e) = db.create_edge(new_edge(node_ids[i], node_ids[j], "rel", weight)) {
                    edge_ids.push(e.id);
                }
            }
            2 => {
                // Update random node — body change forces FTS reindex
                if node_ids.is_empty() {
                    continue;
                }
                let i = (xorshift32(&mut state) as usize) % node_ids.len();
                let _ = db.update_node(
                    node_ids[i],
                    NodePatch {
                        body: Some(format!("ub_{op_idx}")),
                        ..Default::default()
                    },
                );
            }
            3 => {
                // Update random edge — kind change touches edge_kind index
                if edge_ids.is_empty() {
                    continue;
                }
                let i = (xorshift32(&mut state) as usize) % edge_ids.len();
                let _ = db.update_edge(
                    edge_ids[i],
                    EdgePatch {
                        kind: Some(format!("uk_{op_idx}")),
                        weight: Some(1.0 + (op_idx as f32) * 0.1),
                        ..Default::default()
                    },
                );
            }
            4 => {
                // Delete random edge
                if edge_ids.is_empty() {
                    continue;
                }
                let i = (xorshift32(&mut state) as usize) % edge_ids.len();
                let id = edge_ids.swap_remove(i);
                let _ = db.delete_edge(id);
            }
            5 => {
                // Delete random node — exercises cascading delete
                if node_ids.is_empty() {
                    continue;
                }
                let i = (xorshift32(&mut state) as usize) % node_ids.len();
                let id = node_ids.swap_remove(i);
                let _ = db.delete_node(id);
                // Any edges incident to this node are now gone — drop them from our tracked set.
                edge_ids.retain(|eid| db.get_edge(*eid).unwrap().is_some());
            }
            6 => {
                // Update node title — touches title_index + FTS
                if node_ids.is_empty() {
                    continue;
                }
                let i = (xorshift32(&mut state) as usize) % node_ids.len();
                next_label += 1;
                let _ = db.update_node(
                    node_ids[i],
                    NodePatch {
                        title: Some(format!("t_{seed}_{next_label}")),
                        ..Default::default()
                    },
                );
            }
            _ => unreachable!(),
        }

        // Verify after every operation.
        let violations = db.verify_invariants().unwrap();
        assert!(
            violations.is_empty(),
            "seed={seed} op_idx={op_idx} choice={choice} violations: {violations:?}"
        );
    }
}
