//! Integration tests for the WAL-based MAIN / REPLICA replication engine
//! (Phase 15 task `00095`).
//!
//! Where the inline unit tests pin individual methods, these exercise the
//! [`Primary`] + [`Replica`] pair as an operator would: a writable MAIN node
//! produces a write-ahead log, one or more read-only replicas pull the delta
//! and replay it, and reads served from a caught-up replica match the MAIN.
//! Each test frames the topology around one of drevo's five target scenarios
//! (CBT journal, story / book editor, IT task manager, ERP, bug tracker).

use drevo::replication::{Lsn, Primary, Replica, ReplicationError, Role};
use drevo::storage::MemoryBackend;

/// Pull everything past the replica's cursor from the MAIN and replay it —
/// the canonical sync round. Returns the number of records newly applied.
fn sync(primary: &Primary<MemoryBackend>, replica: &mut Replica<MemoryBackend>) -> usize {
    let delta = primary
        .replicate_since(replica.applied_lsn())
        .expect("replica should be serveable");
    replica.apply_stream(&delta).expect("replay should succeed")
}

/// CBT journal — a single therapist writes session notes on the MAIN; a
/// read-only analytics replica catches up and sees an identical view. The
/// replica refuses to be written to directly, so the client's private graph
/// cannot diverge through the read path.
#[test]
fn cbt_journal_main_writes_replica_reads() {
    let mut main = Primary::new(MemoryBackend::new());
    let mut analytics = Replica::new(MemoryBackend::new());

    assert_eq!(main.role(), Role::Main);
    assert_eq!(analytics.role(), Role::Replica);

    main.put(b"entry:1", b"felt anxious before standup")
        .unwrap();
    main.put(b"entry:2", b"reframed the thought").unwrap();
    main.put(b"mood:1", b"4/10").unwrap();

    let applied = sync(&main, &mut analytics);
    assert_eq!(applied, 3);
    assert_eq!(analytics.applied_lsn(), Lsn(3));
    assert_eq!(
        analytics.get(b"entry:1").unwrap(),
        Some(b"felt anxious before standup".to_vec())
    );
    assert_eq!(analytics.get(b"mood:1").unwrap(), Some(b"4/10".to_vec()));

    // The read replica cannot be written to — reads scale, writes do not fork.
    assert!(matches!(
        analytics.put(b"entry:3", b"forged"),
        Err(ReplicationError::ReadOnly)
    ));
}

/// Story / book editor — incremental editing. The author keeps writing chapters
/// on the MAIN while a preview replica re-syncs between edits, each round
/// pulling only the new records and converging on the latest manuscript.
#[test]
fn story_editor_incremental_catch_up() {
    let mut main = Primary::new(MemoryBackend::new());
    let mut preview = Replica::new(MemoryBackend::new());

    main.put(b"ch:1", b"It was a dark and stormy night.")
        .unwrap();
    sync(&main, &mut preview);
    assert_eq!(preview.applied_lsn(), Lsn(1));

    // Author revises chapter 1 and adds chapter 2 before the next preview pull.
    main.put(b"ch:1", b"It was a bright and calm morning.")
        .unwrap();
    main.put(b"ch:2", b"The detective arrived at noon.")
        .unwrap();

    let applied = sync(&main, &mut preview);
    assert_eq!(applied, 2);
    assert_eq!(preview.applied_lsn(), Lsn(3));
    assert_eq!(
        preview.get(b"ch:1").unwrap(),
        Some(b"It was a bright and calm morning.".to_vec())
    );
    assert_eq!(
        preview.get(b"ch:2").unwrap(),
        Some(b"The detective arrived at noon.".to_vec())
    );
}

/// IT task manager — read scaling across two replicas at different positions.
/// A fast dashboard stays current while a slow reporting replica lags; both
/// eventually converge on the MAIN's state without the MAIN blocking on either.
#[test]
fn task_manager_two_replicas_independent_cursors() {
    let mut main = Primary::new(MemoryBackend::new());
    let mut dashboard = Replica::new(MemoryBackend::new());
    let mut reporting = Replica::new(MemoryBackend::new());

    main.put(b"task:1", b"open").unwrap();
    main.put(b"task:2", b"open").unwrap();
    sync(&main, &mut dashboard); // dashboard now at LSN 2

    // More writes land; only the dashboard has seen the first two.
    main.put(b"task:1", b"in_progress").unwrap();
    main.put(b"task:3", b"open").unwrap();

    sync(&main, &mut dashboard); // dashboard catches the new two
    assert_eq!(dashboard.applied_lsn(), Lsn(4));

    // Reporting starts cold and pulls the whole history in one round.
    let applied = sync(&main, &mut reporting);
    assert_eq!(applied, 4);
    assert_eq!(reporting.applied_lsn(), Lsn(4));

    // Both replicas agree with the MAIN.
    for r in [&dashboard, &reporting] {
        assert_eq!(r.get(b"task:1").unwrap(), Some(b"in_progress".to_vec()));
        assert_eq!(r.get(b"task:3").unwrap(), Some(b"open".to_vec()));
    }
}

/// ERP — a deletion (e.g. voiding a posted invoice) replicates as a tombstone,
/// and a batched ledger posting replicates as one atomic record. The replica's
/// view reflects both the removal and the bulk insert.
#[test]
fn erp_delete_and_batch_replicate() {
    let mut main = Primary::new(MemoryBackend::new());
    let mut replica = Replica::new(MemoryBackend::new());

    main.put(b"invoice:1001", b"posted:500.00").unwrap();
    main.put(b"invoice:1002", b"posted:250.00").unwrap();
    sync(&main, &mut replica);
    assert_eq!(
        replica.get(b"invoice:1001").unwrap(),
        Some(b"posted:500.00".to_vec())
    );

    // Void invoice 1001 and post a batch of three ledger lines atomically.
    main.delete(b"invoice:1001").unwrap();
    main.put_batch(&[
        (b"ledger:1".to_vec(), b"debit:500.00".to_vec()),
        (b"ledger:2".to_vec(), b"credit:250.00".to_vec()),
        (b"ledger:3".to_vec(), b"credit:250.00".to_vec()),
    ])
    .unwrap();

    sync(&main, &mut replica);
    assert_eq!(replica.get(b"invoice:1001").unwrap(), None);
    assert_eq!(
        replica.get(b"ledger:1").unwrap(),
        Some(b"debit:500.00".to_vec())
    );
    assert_eq!(
        replica.get(b"ledger:3").unwrap(),
        Some(b"credit:250.00".to_vec())
    );
    assert_eq!(replica.applied_lsn(), main.last_lsn());
}

/// Bug tracker — a high-volume MAIN checkpoints away an applied log prefix to
/// reclaim memory. A current replica keeps syncing fine; a replica that lagged
/// past the checkpoint is correctly told it must perform a full resync.
#[test]
fn bug_tracker_checkpoint_forces_resync_for_laggard() {
    let mut main = Primary::new(MemoryBackend::new());
    let mut current = Replica::new(MemoryBackend::new());
    let laggard = Replica::new(MemoryBackend::new()); // never syncs

    for i in 0..6u8 {
        let key = format!("bug:{i}");
        main.put(key.as_bytes(), b"new").unwrap();
        // Keep the current replica fully caught up after each write.
        sync(&main, &mut current);
    }
    assert_eq!(current.applied_lsn(), Lsn(6));

    // Operator checkpoints everything the current replica has applied.
    let dropped = main.checkpoint(current.applied_lsn());
    assert_eq!(dropped, 6);
    assert_eq!(main.last_lsn(), Lsn(6)); // high-water survives

    // The current replica is up to date — still serveable (empty delta).
    assert!(main
        .replicate_since(current.applied_lsn())
        .unwrap()
        .is_empty());

    // The laggard is still at LSN 0 and the records it needs were dropped.
    let err = main.replicate_since(laggard.applied_lsn()).unwrap_err();
    assert!(matches!(
        err,
        ReplicationError::Truncated {
            requested: Lsn(0),
            earliest: None
        }
    ));
}

/// A corrupted / reordered stream is rejected at the replica boundary: applying
/// a record that skips a sequence number raises a gap error and leaves the
/// replica's state untouched, so a follower can never silently diverge.
#[test]
fn out_of_order_stream_is_rejected() {
    let mut main = Primary::new(MemoryBackend::new());
    let mut replica = Replica::new(MemoryBackend::new());

    main.put(b"a", b"1").unwrap();
    main.put(b"b", b"2").unwrap();
    main.put(b"c", b"3").unwrap();

    let mut delta = main.replicate_since(Lsn::ZERO).unwrap();
    // Simulate a transport that drops the middle record.
    delta.remove(1); // now [LSN 1, LSN 3]

    let err = replica.apply_stream(&delta).unwrap_err();
    assert!(matches!(
        err,
        ReplicationError::LsnGap {
            applied: Lsn(1),
            expected: Lsn(2),
            got: Lsn(3)
        }
    ));
    // The first record applied; the gap stopped the rest.
    assert_eq!(replica.applied_lsn(), Lsn(1));
    assert_eq!(replica.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(replica.get(b"c").unwrap(), None);
}
