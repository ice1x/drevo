//! Phase 9 task `00057` — property-based tests for graph invariants.
//!
//! This file upgrades the per-seed xorshift32 fuzzer in
//! `tests/db_invariants_tests.rs` (the "Phase 9 proptest precursor" section)
//! to a real `proptest` strategy. The four invariants from
//! `.claude/skills/drevo-database/SKILL.md` §"Invariants" must hold after
//! **every** step of an arbitrary CRUD sequence:
//!
//! 1. Adjacency consistency — every `out_edges[from_id]` entry is mirrored
//!    in `in_edges[to_id]`, and vice versa.
//! 2. Cascading delete — deleting a node removes all incident edges,
//!    adjacency entries, and FTS entries.
//! 3. FTS reindex on update — changing `title` or `body` deindexes the old
//!    trigrams and indexes the new ones.
//! 4. UUID immutability — once assigned, a node/edge UUID never changes,
//!    even on update.
//!
//! Strategy:
//! * `Op` is an enum representing a single mutation against `Drevo`.
//! * `op_strategy()` generates `Op` values with a meaningful distribution
//!   (more creates than deletes, so the graph actually grows before it's
//!   torn down).
//! * `prop::collection::vec(op_strategy(), 1..MAX_OPS)` produces a sequence.
//! * For each sequence we open a fresh in-memory `Drevo`, apply ops, and
//!   call `verify_invariants()` after every successful or failed op.
//!
//! `proptest` shrinks any failing case to a minimal reproducer
//! automatically — the `.proptest-regressions/` cache then locks the
//! reproducer in so a future contributor cannot accidentally regress it.

use drevo::db::Drevo;
use drevo::model::{Direction, EdgePatch, NewEdge, NewNode, NodePatch, Properties};

use proptest::collection::vec;
use proptest::prelude::*;

// ---------------------------------------------------------------
// Op generator
// ---------------------------------------------------------------

/// A single mutation against a `Drevo` instance.
///
/// IDs reference positions in the local tracking vectors maintained by
/// `apply_ops`; the harness re-indexes them at apply time so the proptest
/// shrinker can blindly mutate any field without producing
/// "edge_id 9999 not in graph" type errors.
#[derive(Debug, Clone)]
enum Op {
    CreateNode {
        kind: String,
        title: String,
        body: String,
    },
    CreateEdge {
        /// Index into `node_ids` (modulo length).
        from_idx: usize,
        /// Index into `node_ids` (modulo length).
        to_idx: usize,
        kind: String,
        /// Finite f32 — NaN/Inf are filtered at strategy level.
        weight: f32,
    },
    UpdateNodeTitle {
        idx: usize,
        new_title: String,
    },
    UpdateNodeBody {
        idx: usize,
        new_body: String,
    },
    UpdateNodeKind {
        idx: usize,
        new_kind: String,
    },
    UpdateEdgeKind {
        idx: usize,
        new_kind: String,
    },
    UpdateEdgeWeight {
        idx: usize,
        new_weight: f32,
    },
    DeleteEdge {
        idx: usize,
    },
    DeleteNode {
        idx: usize,
    },
}

/// A reasonably small character set that exercises ASCII, digits, spaces,
/// punctuation, and one Cyrillic / one CJK character — the same range the
/// Unicode-edge-case tests in `tests/db_invariants_tests.rs` cover.
///
/// Confining the alphabet keeps the search space small enough that the
/// default `proptest` budget (256 cases × 256 ops each) finds bugs in
/// minutes, not hours.
///
/// Returned as a `BoxedStrategy<String>` so it is `Clone` and can be
/// reused across multiple `prop_oneof!` arms below.
fn small_string_strategy(max_len: usize) -> BoxedStrategy<String> {
    proptest::collection::vec(
        prop_oneof![
            // ASCII letters / digits / space / common punctuation
            "[a-zA-Z0-9 _\\-,.!?]".prop_map(|s| s.chars().next().unwrap_or('a')),
            // Cyrillic
            Just('п'),
            Just('р'),
            Just('и'),
            // CJK
            Just('世'),
            Just('界'),
            // Emoji (multi-codepoint)
            Just('🚀'),
        ],
        0..max_len,
    )
    .prop_map(|chars| chars.into_iter().collect())
    .boxed()
}

fn op_strategy() -> impl Strategy<Value = Op> {
    let kind: BoxedStrategy<String> = "[a-z]{1,8}".prop_map(|s| s).boxed();
    let edge_kind: BoxedStrategy<String> = "[a-z_]{1,8}".prop_map(|s| s).boxed();
    let title = small_string_strategy(20);
    let body = small_string_strategy(40);

    // Finite f32 in a sane range — NaN/Inf are rejected at the validation
    // layer and we don't want every test run to immediately bottom out on
    // `DrevoError::InvalidWeight`.
    let weight: BoxedStrategy<f32> = (-100.0f32..100.0f32)
        .prop_filter("finite", |w| w.is_finite())
        .boxed();

    prop_oneof![
        // Heavily weight creation operations so the graph actually grows
        // before mutations / deletions kick in.
        4 => (kind.clone(), title.clone(), body.clone()).prop_map(|(k, t, b)| Op::CreateNode {
            kind: k,
            title: t,
            body: b,
        }),
        3 => (any::<usize>(), any::<usize>(), edge_kind.clone(), weight.clone()).prop_map(
            |(f, t, k, w)| Op::CreateEdge {
                from_idx: f,
                to_idx: t,
                kind: k,
                weight: w,
            }
        ),
        2 => (any::<usize>(), title).prop_map(|(i, t)| Op::UpdateNodeTitle {
            idx: i,
            new_title: t,
        }),
        2 => (any::<usize>(), body).prop_map(|(i, b)| Op::UpdateNodeBody {
            idx: i,
            new_body: b,
        }),
        1 => (any::<usize>(), kind).prop_map(|(i, k)| Op::UpdateNodeKind {
            idx: i,
            new_kind: k,
        }),
        2 => (any::<usize>(), edge_kind).prop_map(|(i, k)| Op::UpdateEdgeKind {
            idx: i,
            new_kind: k,
        }),
        1 => (any::<usize>(), weight).prop_map(|(i, w)| Op::UpdateEdgeWeight {
            idx: i,
            new_weight: w,
        }),
        1 => any::<usize>().prop_map(|i| Op::DeleteEdge { idx: i }),
        1 => any::<usize>().prop_map(|i| Op::DeleteNode { idx: i }),
    ]
}

// ---------------------------------------------------------------
// Op applier
// ---------------------------------------------------------------

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

/// Apply an op-sequence to a fresh in-memory `Drevo` and assert that
/// `verify_invariants()` returns empty after every step.
///
/// Per-op errors (e.g. `DuplicateTitle`, `NodeNotFound`) are tolerated —
/// the harness's job is *not* to construct a perfectly-coherent sequence,
/// it's to ensure the database stays internally consistent **regardless**
/// of which ops succeeded.
fn invariants_hold_under_ops(ops: &[Op]) -> Result<(), TestCaseError> {
    let db = Drevo::open_in_memory().map_err(|e| TestCaseError::reject(format!("open: {e}")))?;

    let mut node_ids: Vec<u64> = Vec::new();
    let mut edge_ids: Vec<u64> = Vec::new();
    // Track (uuid_before, uuid_after_update) per node/edge so we can
    // verify invariant #4 (UUID immutability) end-to-end.
    let mut node_uuids: std::collections::HashMap<u64, [u8; 16]> = std::collections::HashMap::new();
    let mut edge_uuids: std::collections::HashMap<u64, [u8; 16]> = std::collections::HashMap::new();

    for (op_idx, op) in ops.iter().enumerate() {
        match op {
            Op::CreateNode { kind, title, body } => {
                if let Ok(n) = db.create_node(new_node(kind, title, body)) {
                    node_ids.push(n.id);
                    node_uuids.insert(n.id, n.uuid);
                }
            }
            Op::CreateEdge {
                from_idx,
                to_idx,
                kind,
                weight,
            } => {
                if node_ids.is_empty() {
                    continue;
                }
                let from = node_ids[from_idx % node_ids.len()];
                let to = node_ids[to_idx % node_ids.len()];
                if let Ok(e) = db.create_edge(new_edge(from, to, kind, *weight)) {
                    edge_ids.push(e.id);
                    edge_uuids.insert(e.id, e.uuid);
                }
            }
            Op::UpdateNodeTitle { idx, new_title } => {
                if node_ids.is_empty() {
                    continue;
                }
                let id = node_ids[idx % node_ids.len()];
                let before = node_uuids.get(&id).copied();
                if let Ok(updated) = db.update_node(
                    id,
                    NodePatch {
                        title: Some(new_title.clone()),
                        ..Default::default()
                    },
                ) {
                    // Invariant #4 — UUID never changes on update.
                    if let Some(before_uuid) = before {
                        prop_assert_eq!(
                            updated.uuid,
                            before_uuid,
                            "invariant #4 violated: node {} UUID changed across update",
                            id
                        );
                    }
                }
            }
            Op::UpdateNodeBody { idx, new_body } => {
                if node_ids.is_empty() {
                    continue;
                }
                let id = node_ids[idx % node_ids.len()];
                let before = node_uuids.get(&id).copied();
                if let Ok(updated) = db.update_node(
                    id,
                    NodePatch {
                        body: Some(new_body.clone()),
                        ..Default::default()
                    },
                ) {
                    if let Some(before_uuid) = before {
                        prop_assert_eq!(updated.uuid, before_uuid);
                    }
                }
            }
            Op::UpdateNodeKind { idx, new_kind } => {
                if node_ids.is_empty() {
                    continue;
                }
                let id = node_ids[idx % node_ids.len()];
                let before = node_uuids.get(&id).copied();
                if let Ok(updated) = db.update_node(
                    id,
                    NodePatch {
                        kind: Some(new_kind.clone()),
                        ..Default::default()
                    },
                ) {
                    if let Some(before_uuid) = before {
                        prop_assert_eq!(updated.uuid, before_uuid);
                    }
                }
            }
            Op::UpdateEdgeKind { idx, new_kind } => {
                if edge_ids.is_empty() {
                    continue;
                }
                let id = edge_ids[idx % edge_ids.len()];
                let before = edge_uuids.get(&id).copied();
                if let Ok(updated) = db.update_edge(
                    id,
                    EdgePatch {
                        kind: Some(new_kind.clone()),
                        ..Default::default()
                    },
                ) {
                    if let Some(before_uuid) = before {
                        prop_assert_eq!(updated.uuid, before_uuid);
                    }
                }
            }
            Op::UpdateEdgeWeight { idx, new_weight } => {
                if edge_ids.is_empty() {
                    continue;
                }
                let id = edge_ids[idx % edge_ids.len()];
                let before = edge_uuids.get(&id).copied();
                if let Ok(updated) = db.update_edge(
                    id,
                    EdgePatch {
                        weight: Some(*new_weight),
                        ..Default::default()
                    },
                ) {
                    if let Some(before_uuid) = before {
                        prop_assert_eq!(updated.uuid, before_uuid);
                    }
                }
            }
            Op::DeleteEdge { idx } => {
                if edge_ids.is_empty() {
                    continue;
                }
                let target = idx % edge_ids.len();
                let id = edge_ids.swap_remove(target);
                let _ = db.delete_edge(id);
                edge_uuids.remove(&id);
            }
            Op::DeleteNode { idx } => {
                if node_ids.is_empty() {
                    continue;
                }
                let target = idx % node_ids.len();
                let id = node_ids.swap_remove(target);
                let _ = db.delete_node(id);
                node_uuids.remove(&id);
                // Cascading delete may have killed edges referencing this
                // node — drop them from our tracking so subsequent
                // EdgeUpdate ops don't hit `EdgeNotFound` every time.
                edge_ids.retain(|eid| {
                    db.get_edge(*eid)
                        .ok()
                        .flatten()
                        .map(|_| true)
                        .unwrap_or(false)
                });
            }
        }

        // The contract: after EVERY op (succeeded or not), the four
        // invariants from drevo-database must hold.
        let violations = db
            .verify_invariants()
            .map_err(|e| TestCaseError::fail(format!("verify_invariants: {e}")))?;
        prop_assert!(
            violations.is_empty(),
            "op_idx={} op={:?} violations={:?}",
            op_idx,
            op,
            violations
        );
    }

    // Cross-check: every UUID we tracked must still resolve to a node /
    // edge that exists, and the UUID must match (invariant #4 on the
    // storage round-trip as well as the in-memory return value).
    for (id, expected_uuid) in &node_uuids {
        if let Some(n) = db
            .get_node(*id)
            .map_err(|e| TestCaseError::fail(format!("get_node: {e}")))?
        {
            prop_assert_eq!(
                n.uuid,
                *expected_uuid,
                "node {} UUID on disk != tracked UUID",
                id
            );
        }
    }
    for (id, expected_uuid) in &edge_uuids {
        if let Some(e) = db
            .get_edge(*id)
            .map_err(|err| TestCaseError::fail(format!("get_edge: {err}")))?
        {
            prop_assert_eq!(e.uuid, *expected_uuid);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------
// The properties
// ---------------------------------------------------------------

proptest! {
    // 64 cases × up to 96 ops is enough to exercise the full state
    // machine without making `cargo test` slow.
    #![proptest_config(ProptestConfig {
        cases: 64,
        max_shrink_iters: 4096,
        .. ProptestConfig::default()
    })]

    /// drevo-database invariants 1-4 hold after every op in any sequence.
    #[test]
    fn graph_invariants_hold_under_arbitrary_op_sequences(
        ops in vec(op_strategy(), 1..96)
    ) {
        invariants_hold_under_ops(&ops)?;
    }

    /// FTS searches never return a deleted node — corollary of invariant #2
    /// (cascading delete) on the FTS sub-index.
    #[test]
    fn fts_never_returns_deleted_node(
        ops in vec(op_strategy(), 1..64)
    ) {
        let db = Drevo::open_in_memory().unwrap();
        let mut alive_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();

        for op in &ops {
            if let Op::CreateNode { kind, title, body } = op {
                if let Ok(n) = db.create_node(new_node(kind, title, body)) {
                    alive_ids.insert(n.id);
                }
            } else if let Op::DeleteNode { idx } = op {
                if alive_ids.is_empty() {
                    continue;
                }
                // Pick a stable id: convert set to a sorted vec so the
                // shrinker can shrink the index meaningfully.
                let mut ids: Vec<u64> = alive_ids.iter().copied().collect();
                ids.sort_unstable();
                let id = ids[*idx % ids.len()];
                if db.delete_node(id).is_ok() {
                    alive_ids.remove(&id);
                }
            }
        }

        // Search by every alive node's title — must hit only alive nodes.
        for id in &alive_ids {
            let n = db.get_node(*id).unwrap();
            if let Some(node) = n {
                if node.title.is_empty() {
                    continue;
                }
                let hits = db.search_fts(&node.title, 100).unwrap();
                for hit in hits {
                    prop_assert!(
                        alive_ids.contains(&hit.node.id),
                        "FTS hit on deleted node id={}",
                        hit.node.id
                    );
                }
            }
        }
    }

    /// edges_of returns symmetric results: every Outgoing edge from A to B
    /// must appear as an Incoming edge into B. This is the user-visible
    /// face of invariant #1 (adjacency consistency).
    #[test]
    fn edges_of_is_symmetric(
        ops in vec(op_strategy(), 1..64)
    ) {
        let db = Drevo::open_in_memory().unwrap();
        let mut node_ids: Vec<u64> = Vec::new();

        for op in &ops {
            if let Op::CreateNode { kind, title, body } = op {
                if let Ok(n) = db.create_node(new_node(kind, title, body)) {
                    node_ids.push(n.id);
                }
            } else if let Op::CreateEdge {
                from_idx,
                to_idx,
                kind,
                weight,
            } = op
            {
                if node_ids.is_empty() {
                    continue;
                }
                let from = node_ids[from_idx % node_ids.len()];
                let to = node_ids[to_idx % node_ids.len()];
                let _ = db.create_edge(new_edge(from, to, kind, *weight));
            }
        }

        // For every node, the outgoing edges into B match the incoming
        // edges on B's side.
        for id in &node_ids {
            let out = db.edges_of(*id, Direction::Outgoing).unwrap();
            for e in out {
                let in_to = db.edges_of(e.to_id, Direction::Incoming).unwrap();
                prop_assert!(
                    in_to.iter().any(|x| x.id == e.id),
                    "edge {} outgoing from {} not found in incoming of {}",
                    e.id, e.from_id, e.to_id
                );
            }
        }
    }
}
