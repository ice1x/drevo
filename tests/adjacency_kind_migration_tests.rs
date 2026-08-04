//! Integration tests for the #243 slice 2 kind-in-key adjacency migration.
//!
//! Exercises the public migration surface end to end on a real redb file:
//!
//! * a fresh database is already v2 and opens without migration,
//! * a database downgraded to the legacy v1 layout is **refused** by
//!   [`Drevo::open`] with [`DrevoError::NeedsMigration`],
//! * [`Drevo::migrate`] up/down round-trips losslessly and re-stamps the
//!   on-disk format version, and
//! * the kind-scoped fast path returns exactly the kind-filtered neighbours,
//!   including after a kind change moves an edge's adjacency key.
//!
//! The migration only ever rebuilds the derived adjacency index, so these
//! tests double as a guarantee that no graph data is lost across a round-trip.

#![cfg(feature = "redb-backend")]

use drevo::db::{Drevo, MigrationDirection};
use drevo::error::DrevoError;
use drevo::model::{Direction, NewEdge, NewNode, Properties};
use tempfile::TempDir;

fn temp_path() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("drevo.redb");
    (dir, path)
}

fn node(db: &Drevo, title: &str) -> u64 {
    db.create_node(NewNode {
        kind: "n".into(),
        title: title.into(),
        body: String::new(),
        body_html: String::new(),
        properties: Properties::default(),
    })
    .unwrap()
    .id
}

fn edge(db: &Drevo, from: u64, to: u64, kind: &str) {
    db.create_edge(NewEdge {
        from_id: from,
        to_id: to,
        kind: kind.into(),
        weight: 1.0,
        properties: Properties::default(),
    })
    .unwrap();
}

/// A brand-new database is written in the v2 layout, so it opens and reopens
/// cleanly with no migration required.
#[test]
fn fresh_database_is_v2_and_opens_without_migration() {
    let (_dir, path) = temp_path();
    {
        let db = Drevo::open(&path).unwrap();
        let a = node(&db, "a");
        let b = node(&db, "b");
        edge(&db, a, b, "knows");
        db.close().unwrap();
    }
    // Reopen must succeed (no NeedsMigration) and the edge survives.
    let db = Drevo::open(&path).unwrap();
    let ns = db.neighbor_ids(1, Direction::Outgoing, None).unwrap();
    assert_eq!(ns, vec![2]);
}

/// Downgrading to v1 makes `open` refuse; migrating up restores access with
/// the full graph intact.
#[test]
fn v1_database_is_refused_then_migrated_up_losslessly() {
    let (_dir, path) = temp_path();
    let (a, b, c) = {
        let db = Drevo::open(&path).unwrap();
        let a = node(&db, "a");
        let b = node(&db, "b");
        let c = node(&db, "c");
        edge(&db, a, b, "knows");
        edge(&db, a, c, "likes");
        edge(&db, b, c, "knows");
        // Simulate an old on-disk file: rewrite the index to the v1 layout and
        // stamp the format version back to 1.
        db.migrate_adjacency(1).unwrap();
        db.close().unwrap();
        (a, b, c)
    };

    // An old-layout file must be refused, not silently misread.
    match Drevo::open(&path) {
        Err(DrevoError::NeedsMigration {
            found_major,
            required_major,
        }) => {
            assert_eq!(found_major, 1);
            assert_eq!(required_major, 2);
        }
        other => panic!("expected NeedsMigration, got {other:?}"),
    }

    // Migrate up: every edge is re-indexed and the file opens again.
    let migrated = Drevo::migrate(&path, MigrationDirection::Up).unwrap();
    assert_eq!(migrated, 3);

    let db = Drevo::open(&path).unwrap();
    let mut out_a = db.neighbor_ids(a, Direction::Outgoing, None).unwrap();
    out_a.sort_unstable();
    assert_eq!(out_a, vec![b, c]);
    let in_c = db.neighbor_ids(c, Direction::Incoming, None).unwrap();
    let mut in_c = in_c;
    in_c.sort_unstable();
    assert_eq!(in_c, vec![a, b]);
    assert!(db.verify_invariants().unwrap().is_empty());
}

/// Up then down returns the file to a state an older build accepts (v1 stamp),
/// and open refuses it again — a full reversible round-trip.
#[test]
fn migrate_down_reverts_to_v1_and_is_refused_again() {
    let (_dir, path) = temp_path();
    {
        let db = Drevo::open(&path).unwrap();
        let a = node(&db, "a");
        let b = node(&db, "b");
        edge(&db, a, b, "knows");
        db.close().unwrap();
    }

    // Down-migrate the freshly-created v2 file back to v1.
    let n = Drevo::migrate(&path, MigrationDirection::Down).unwrap();
    assert_eq!(n, 1);

    // Now it looks like a legacy file again → refused.
    assert!(matches!(
        Drevo::open(&path),
        Err(DrevoError::NeedsMigration { .. })
    ));

    // And back up once more, losslessly.
    Drevo::migrate(&path, MigrationDirection::Up).unwrap();
    let db = Drevo::open(&path).unwrap();
    assert_eq!(
        db.neighbor_ids(1, Direction::Outgoing, None).unwrap(),
        vec![2]
    );
}

/// A crash mid-migration is modelled by re-running the migration: the per-edge
/// rewrite is idempotent, so running it twice yields the same, consistent
/// result and no data is lost.
#[test]
fn migration_is_idempotent_and_resumable() {
    let (_dir, path) = temp_path();
    {
        let db = Drevo::open(&path).unwrap();
        let a = node(&db, "a");
        let b = node(&db, "b");
        edge(&db, a, b, "knows");
        edge(&db, a, b, "likes");
        db.close().unwrap();
    }
    // Two up-migrations in a row (the second models resuming after a crash).
    assert_eq!(Drevo::migrate(&path, MigrationDirection::Up).unwrap(), 2);
    assert_eq!(Drevo::migrate(&path, MigrationDirection::Up).unwrap(), 2);

    let db = Drevo::open(&path).unwrap();
    // Two parallel edges → one distinct neighbour.
    assert_eq!(
        db.neighbor_ids(1, Direction::Outgoing, None).unwrap(),
        vec![2]
    );
    assert!(db.verify_invariants().unwrap().is_empty());
}

/// The kind-scoped fast path returns exactly the neighbours reachable by the
/// requested edge kind, and nothing from other kinds.
#[test]
fn kind_filtered_fan_out_returns_only_that_kind() {
    let (_dir, path) = temp_path();
    let db = Drevo::open(&path).unwrap();
    let hub = node(&db, "hub");
    let mut knows = Vec::new();
    let mut likes = Vec::new();
    for i in 0..25 {
        let k = node(&db, &format!("k{i}"));
        edge(&db, hub, k, "knows");
        knows.push(k);
    }
    for i in 0..10 {
        let l = node(&db, &format!("l{i}"));
        edge(&db, hub, l, "likes");
        likes.push(l);
    }

    let mut got_knows = db
        .neighbor_ids(hub, Direction::Outgoing, Some("knows"))
        .unwrap();
    got_knows.sort_unstable();
    knows.sort_unstable();
    assert_eq!(got_knows, knows);

    let mut got_likes = db
        .neighbor_ids(hub, Direction::Outgoing, Some("likes"))
        .unwrap();
    got_likes.sort_unstable();
    likes.sort_unstable();
    assert_eq!(got_likes, likes);

    // A kind nobody uses returns nothing.
    assert!(db
        .neighbor_ids(hub, Direction::Outgoing, Some("hates"))
        .unwrap()
        .is_empty());
}

/// Changing an edge's kind moves its adjacency key, so the old kind's
/// fast-path slice loses it and the new kind's gains it — with the plain
/// fan-out unchanged.
#[test]
fn update_edge_kind_moves_the_adjacency_key() {
    let (_dir, path) = temp_path();
    let db = Drevo::open(&path).unwrap();
    let a = node(&db, "a");
    let b = node(&db, "b");
    let e = db
        .create_edge(NewEdge {
            from_id: a,
            to_id: b,
            kind: "knows".into(),
            weight: 1.0,
            properties: Properties::default(),
        })
        .unwrap();

    assert_eq!(
        db.neighbor_ids(a, Direction::Outgoing, Some("knows"))
            .unwrap(),
        vec![b]
    );

    db.update_edge(
        e.id,
        drevo::model::EdgePatch {
            kind: Some("likes".into()),
            weight: None,
            properties: None,
        },
    )
    .unwrap();

    // Old kind slice is now empty; new kind slice has the neighbour.
    assert!(db
        .neighbor_ids(a, Direction::Outgoing, Some("knows"))
        .unwrap()
        .is_empty());
    assert_eq!(
        db.neighbor_ids(a, Direction::Outgoing, Some("likes"))
            .unwrap(),
        vec![b]
    );
    // Unfiltered fan-out is unaffected, and invariants hold.
    assert_eq!(
        db.neighbor_ids(a, Direction::Outgoing, None).unwrap(),
        vec![b]
    );
    assert!(db.verify_invariants().unwrap().is_empty());
}
