//! Two-phase commit in the native engine — #556, slice 1 (core).
//!
//! Design: `docs/rfc-two-phase-commit.md`. `tx_prepare(tx, gid)` validates a
//! registered transaction and durably records its write set as *prepared*.
//! While anything is prepared, a **fence** refuses every other write
//! (autocommit and other transactions; reads keep working), so
//! `commit_prepared(gid)` can never conflict. Prepared transactions survive a
//! crash and a WAL compaction, are listable, and are resolved by
//! `commit_prepared` / `rollback_prepared`, possibly after a restart.
//!
//! Each test maps to an acceptance item of the RFC (§5).

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use drevo_core::engine::GraphEngine;
use drevo_core::error::CoreError;
use drevo_core::model::{NewNode, Properties};
use drevo_core::native::{
    CommitError, Constraint, NativeGraph, NativeTxId, PrepareError, ResolveError,
};
use drevo_core::replica::NativeReplica;

static NEXT: AtomicU64 = AtomicU64::new(0);
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "drevo_2pc_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
    fn wal(&self) -> PathBuf {
        self.0.join("native.wal")
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn node(title: &str) -> NewNode {
    NewNode {
        kind: "person".to_string(),
        title: title.to_string(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties(Default::default()),
    }
}

fn titles(g: &NativeGraph) -> Vec<String> {
    let mut t: Vec<String> = g
        .all_nodes()
        .unwrap()
        .iter()
        .map(|n| n.title.clone())
        .collect();
    t.sort();
    t
}

/// Begin a transaction on `g` that creates one node per title.
fn tx_creating(g: &NativeGraph, names: &[&str]) -> NativeTxId {
    let tx = g.tx_begin();
    let engine = g.tx_engine(tx).expect("open tx");
    for name in names {
        engine.create_node(node(name)).expect("create in tx");
    }
    tx
}

fn gids(g: &NativeGraph) -> Vec<String> {
    g.list_prepared().into_iter().map(|p| p.gid).collect()
}

// ── 1 / 2: resolve, visibility, durability ──────────────────────────────

#[test]
fn prepare_then_commit_is_visible_and_durable() {
    let dir = TmpDir::new();
    {
        let g = NativeGraph::open_durable(dir.wal()).unwrap();
        let tx = tx_creating(&g, &["ada", "bo"]);
        g.tx_prepare(tx, "gid-1").unwrap();
        assert!(titles(&g).is_empty(), "prepared writes are invisible");
        assert_eq!(gids(&g), vec!["gid-1"]);
        g.commit_prepared("gid-1").unwrap();
        assert_eq!(titles(&g), vec!["ada", "bo"]);
        assert!(gids(&g).is_empty());
    }
    let g = NativeGraph::open_durable(dir.wal()).unwrap();
    assert_eq!(titles(&g), vec!["ada", "bo"]);
    assert!(gids(&g).is_empty());
}

#[test]
fn prepare_then_rollback_leaves_no_trace() {
    let dir = TmpDir::new();
    {
        let g = NativeGraph::open_durable(dir.wal()).unwrap();
        let tx = tx_creating(&g, &["ada"]);
        g.tx_prepare(tx, "gid-1").unwrap();
        g.rollback_prepared("gid-1").unwrap();
        assert!(titles(&g).is_empty());
        assert!(gids(&g).is_empty());
        g.create_node(node("after")).unwrap(); // fence lifted
    }
    let g = NativeGraph::open_durable(dir.wal()).unwrap();
    assert_eq!(titles(&g), vec!["after"]);
    assert!(gids(&g).is_empty());
}

#[test]
fn works_on_an_in_memory_graph_too() {
    let g = NativeGraph::new();
    let tx = tx_creating(&g, &["ada"]);
    g.tx_prepare(tx, "g").unwrap();
    g.commit_prepared("g").unwrap();
    assert_eq!(titles(&g), vec!["ada"]);
}

// ── 3 / 4: crash between prepare and resolution ─────────────────────────

#[test]
fn a_crash_after_prepare_restores_it_as_prepared_then_commit() {
    let dir = TmpDir::new();
    {
        let g = NativeGraph::open_durable(dir.wal()).unwrap();
        g.create_node(node("before")).unwrap();
        let tx = tx_creating(&g, &["ada"]);
        g.tx_prepare(tx, "in-doubt").unwrap();
        // Crash: the graph is dropped with the transaction unresolved.
    }
    let g = NativeGraph::open_durable(dir.wal()).unwrap();
    assert_eq!(gids(&g), vec!["in-doubt"], "recovered as prepared");
    assert_eq!(titles(&g), vec!["before"], "still invisible");
    assert!(
        matches!(
            g.create_node(node("blocked")),
            Err(CoreError::PreparedTransactionPending(ref ids)) if ids == &["in-doubt"]
        ),
        "the fence is back up after recovery"
    );
    g.commit_prepared("in-doubt").unwrap();
    assert_eq!(titles(&g), vec!["ada", "before"]);
    drop(g);
    let g = NativeGraph::open_durable(dir.wal()).unwrap();
    assert_eq!(titles(&g), vec!["ada", "before"]);
    assert!(gids(&g).is_empty());
}

#[test]
fn a_crash_after_prepare_then_rollback_after_restart() {
    let dir = TmpDir::new();
    {
        let g = NativeGraph::open_durable(dir.wal()).unwrap();
        let tx = tx_creating(&g, &["ada"]);
        g.tx_prepare(tx, "in-doubt").unwrap();
    }
    {
        let g = NativeGraph::open_durable(dir.wal()).unwrap();
        g.rollback_prepared("in-doubt").unwrap();
        g.create_node(node("after")).unwrap();
    }
    let g = NativeGraph::open_durable(dir.wal()).unwrap();
    assert_eq!(titles(&g), vec!["after"]);
    assert!(gids(&g).is_empty());
}

#[test]
fn a_torn_line_after_prepare_keeps_the_prepare() {
    let dir = TmpDir::new();
    {
        let g = NativeGraph::open_durable(dir.wal()).unwrap();
        let tx = tx_creating(&g, &["ada"]);
        g.tx_prepare(tx, "in-doubt").unwrap();
    }
    // A crash mid-way through the next record leaves a torn tail.
    let mut bytes = fs::read(dir.wal()).unwrap();
    bytes.extend_from_slice(b"{\"CommitPrep");
    fs::write(dir.wal(), &bytes).unwrap();
    let g = NativeGraph::open_durable(dir.wal()).unwrap();
    assert_eq!(gids(&g), vec!["in-doubt"]);
}

// ── 6: the fence ────────────────────────────────────────────────────────

#[test]
fn while_prepared_other_writers_fail_fast_and_reads_work() {
    let g = NativeGraph::new();
    let existing = g.create_node(node("existing")).unwrap();
    let other = tx_creating(&g, &["other"]);
    let tx = tx_creating(&g, &["ada"]);
    // `other` began before the prepare; even so, the fence refuses it.
    g.tx_prepare(tx, "g1").unwrap();

    let pending = |r: Result<_, CoreError>| matches!(r, Err(CoreError::PreparedTransactionPending(ref ids)) if ids == &["g1"]);
    assert!(pending(g.create_node(node("x")).map(|_| ())));
    assert!(pending(g.delete_node(existing.id)));
    assert!(pending(g.create_nodes(vec![node("y")]).map(|_| ())));
    assert!(pending(g.set_embedding(existing.id, vec![1.0]).map(|_| ())));
    assert!(matches!(
        g.tx_commit(other),
        Err(CommitError::PreparedPending(ref ids)) if ids == &["g1"]
    ));

    // Reads keep working.
    assert_eq!(titles(&g), vec!["existing"]);
    assert!(g.get_node(existing.id).unwrap().is_some());

    g.commit_prepared("g1").unwrap();
    g.create_node(node("after")).unwrap();
    assert_eq!(titles(&g), vec!["ada", "after", "existing"]);
}

#[test]
fn a_new_transaction_can_begin_and_read_while_prepared_but_not_commit() {
    let g = NativeGraph::new();
    let tx = tx_creating(&g, &["ada"]);
    g.tx_prepare(tx, "g1").unwrap();
    let reader = g.tx_begin();
    assert!(g.tx_engine(reader).unwrap().all_nodes().unwrap().is_empty());
    let writer = tx_creating(&g, &["x"]);
    assert!(matches!(
        g.tx_commit(writer),
        Err(CommitError::PreparedPending(_))
    ));
    assert!(g.tx_rollback(reader));
    g.rollback_prepared("g1").unwrap();
}

// ── 7: commit_prepared never conflicts ─────────────────────────────────

#[test]
fn commit_prepared_never_conflicts_whatever_was_attempted_meanwhile() {
    for round in 0..20u64 {
        let g = NativeGraph::new();
        let seed = g.create_node(node("seed")).unwrap();
        let tx = tx_creating(&g, &["ada"]);
        g.tx_prepare(tx, "g").unwrap();
        // A pseudo-random burst of writes that must all be refused.
        let mut x = round.wrapping_mul(6364136223846793005).wrapping_add(1);
        for i in 0..25 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let refused = match x % 5 {
                0 => g.create_node(node(&format!("n{round}-{i}"))).is_err(),
                1 => g.delete_node(seed.id).is_err(),
                2 => g.set_embedding(seed.id, vec![0.5]).is_err(),
                3 => {
                    let t = tx_creating(&g, &[&format!("t{round}-{i}")]);
                    g.tx_commit(t).is_err()
                }
                _ => {
                    let t = tx_creating(&g, &[&format!("p{round}-{i}")]);
                    g.tx_prepare(t, &format!("other-{i}")).is_err()
                }
            };
            assert!(refused, "round {round} op {i} slipped past the fence");
        }
        g.commit_prepared("g")
            .expect("commit_prepared must not fail");
        assert_eq!(titles(&g), vec!["ada", "seed"]);
    }
}

// ── 8: gid / state errors ───────────────────────────────────────────────

#[test]
fn gid_and_state_errors() {
    let g = NativeGraph::new();
    let tx = tx_creating(&g, &["ada"]);
    g.tx_prepare(tx, "dup").unwrap();

    // Prepare is single-shot: the transaction is no longer open.
    assert!(matches!(
        g.tx_prepare(tx, "again"),
        Err(PrepareError::UnknownTransaction)
    ));
    // Another prepare is refused by the fence.
    let second = tx_creating(&g, &["bo"]);
    assert!(matches!(
        g.tx_prepare(second, "dup"),
        Err(PrepareError::PreparedPending(_)) | Err(PrepareError::DuplicateGid(_))
    ));

    assert!(matches!(
        g.commit_prepared("missing"),
        Err(ResolveError::UnknownGid(ref gid)) if gid == "missing"
    ));
    g.commit_prepared("dup").unwrap();
    assert!(matches!(
        g.commit_prepared("dup"),
        Err(ResolveError::UnknownGid(_))
    ));
    assert!(matches!(
        g.rollback_prepared("dup"),
        Err(ResolveError::UnknownGid(_))
    ));
}

#[test]
fn prepare_reports_a_conflict_and_closes_the_transaction() {
    let g = NativeGraph::new();
    let tx = tx_creating(&g, &["ada"]);
    g.create_node(node("concurrent")).unwrap(); // the graph moved since begin
    assert!(matches!(g.tx_prepare(tx, "g"), Err(PrepareError::Conflict)));
    assert!(gids(&g).is_empty());
    assert!(
        !g.tx_rollback(tx),
        "a failed prepare closes the transaction"
    );
    g.create_node(node("free")).unwrap(); // no fence was raised
}

#[test]
fn prepare_validates_constraints() {
    let g = NativeGraph::new();
    g.add_constraint(Constraint::UniqueNodeProperty {
        kind: "user".into(),
        property: "email".into(),
    })
    .unwrap();
    let tx = g.tx_begin();
    let engine = g.tx_engine(tx).unwrap();
    for title in ["a", "b"] {
        let mut nn = node(title);
        nn.kind = "user".into();
        nn.properties = Properties(std::collections::HashMap::from([(
            "email".to_string(),
            serde_json::Value::String("same@example.com".into()),
        )]));
        engine.create_node(nn).unwrap();
    }
    assert!(matches!(
        g.tx_prepare(tx, "g"),
        Err(PrepareError::Constraint(_))
    ));
    assert!(gids(&g).is_empty());
}

#[test]
fn an_empty_transaction_can_be_prepared() {
    let g = NativeGraph::new();
    let tx = g.tx_begin();
    g.tx_prepare(tx, "read-only").unwrap();
    let info = g.list_prepared();
    assert_eq!(info.len(), 1);
    assert_eq!(info[0].op_count, 0);
    g.commit_prepared("read-only").unwrap();
}

#[test]
fn list_prepared_reports_age_and_size() {
    let g = NativeGraph::new();
    let tx = tx_creating(&g, &["a", "b", "c"]);
    g.tx_prepare(tx, "g").unwrap();
    let info = &g.list_prepared()[0];
    assert_eq!(info.gid, "g");
    assert_eq!(info.op_count, 3);
    assert!(info.prepared_at_ms > 0);
    g.rollback_prepared("g").unwrap();
}

// ── 9: compaction carries unresolved prepares ───────────────────────────

#[test]
fn compaction_keeps_an_unresolved_prepare() {
    let dir = TmpDir::new();
    {
        let g = NativeGraph::open_durable(dir.wal()).unwrap();
        g.create_node(node("before")).unwrap();
        let tx = tx_creating(&g, &["ada"]);
        g.tx_prepare(tx, "kept").unwrap();
        g.compact_wal().unwrap();
        assert_eq!(gids(&g), vec!["kept"]);
    }
    let g = NativeGraph::open_durable(dir.wal()).unwrap();
    assert_eq!(gids(&g), vec!["kept"]);
    assert_eq!(titles(&g), vec!["before"]);
    g.commit_prepared("kept").unwrap();
    assert_eq!(titles(&g), vec!["ada", "before"]);
}

// ── 11: replicas only ever see committed state ──────────────────────────

#[test]
fn a_replica_sees_the_writes_only_after_commit_prepared() {
    let g = NativeGraph::new();
    let mut replica = NativeReplica::new();
    let tx = tx_creating(&g, &["ada"]);
    g.tx_prepare(tx, "g").unwrap();
    replica.sync_from(&g).unwrap();
    assert!(titles(replica.graph()).is_empty());
    g.commit_prepared("g").unwrap();
    replica.sync_from(&g).unwrap();
    assert_eq!(titles(replica.graph()), vec!["ada"]);
}

#[test]
fn a_wal_tailing_replica_applies_the_commit_record() {
    let dir = TmpDir::new();
    let g = NativeGraph::open_durable(dir.wal()).unwrap();
    let tx = tx_creating(&g, &["ada"]);
    g.tx_prepare(tx, "g").unwrap();
    g.commit_prepared("g").unwrap();
    let ops = g.dump_wal();
    let rebuilt = NativeGraph::replay(ops);
    assert_eq!(titles(&rebuilt), vec!["ada"]);
    let mut tailer = drevo_core::replica::WalTailer::new(dir.wal());
    let tailed = NativeGraph::replay(tailer.poll().unwrap());
    assert_eq!(titles(&tailed), vec!["ada"]);
    assert!(gids(&tailed).is_empty());
}
