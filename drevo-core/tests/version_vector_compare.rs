//! Comparing two [`VersionVector`]s (#799): a sync host merges a delta only
//! when its own vector already covers the vector the delta was computed
//! against, and needs the entries of a vector to show or send it.

use std::collections::BTreeMap;

use drevo_core::delta::VersionVector;
use drevo_core::hlc::Hlc;
use drevo_core::lww::{OriginId, Stamp};

fn stamp(origin: u64, wall_ms: i64, counter: u32) -> Stamp {
    Stamp::new(Hlc::new(wall_ms, counter), OriginId(origin))
}

fn vector(stamps: &[Stamp]) -> VersionVector {
    let mut v = VersionVector::new();
    for s in stamps {
        v.observe(*s);
    }
    v
}

#[test]
fn the_empty_vector_is_covered_by_every_vector_and_covers_only_itself() {
    let empty = VersionVector::new();
    let some = vector(&[stamp(1, 10, 0)]);
    assert!(empty.covers(&empty));
    assert!(some.covers(&empty));
    assert!(!empty.covers(&some));
}

#[test]
fn a_vector_covers_itself() {
    let v = vector(&[stamp(1, 10, 0), stamp(2, 20, 3)]);
    assert!(v.covers(&v));
}

#[test]
fn covering_needs_an_equal_or_later_clock_for_every_origin() {
    let base = vector(&[stamp(1, 10, 0), stamp(2, 20, 0)]);
    let later = vector(&[stamp(1, 11, 0), stamp(2, 20, 0)]);
    let behind_on_two = vector(&[stamp(1, 30, 0), stamp(2, 19, 9)]);
    assert!(later.covers(&base));
    assert!(!base.covers(&later));
    assert!(
        !behind_on_two.covers(&base),
        "ahead on origin 1 does not make up for behind on origin 2"
    );
}

#[test]
fn the_counter_breaks_a_tie_on_the_wall_clock() {
    let base = vector(&[stamp(1, 10, 2)]);
    assert!(vector(&[stamp(1, 10, 2)]).covers(&base));
    assert!(vector(&[stamp(1, 10, 3)]).covers(&base));
    assert!(!vector(&[stamp(1, 10, 1)]).covers(&base));
}

#[test]
fn an_origin_never_seen_is_not_covered() {
    let base = vector(&[stamp(1, 10, 0), stamp(7, 5, 0)]);
    let without_seven = vector(&[stamp(1, 99, 0)]);
    assert!(!without_seven.covers(&base));
    let extra_origin = vector(&[stamp(1, 10, 0), stamp(7, 5, 0), stamp(9, 1, 0)]);
    assert!(
        extra_origin.covers(&base),
        "origins the base has not seen do not matter"
    );
}

#[test]
fn the_missed_push_of_the_issue_is_caught() {
    // Alice pushes twice. Bob misses the first push (base {A: 10}) and gets
    // the second, computed against {A: 20}. Merging it would leave Bob at
    // {A: 30} with the writes of 10..20 never resent.
    let bob = vector(&[stamp(1, 10, 0)]);
    let second_push_base = vector(&[stamp(1, 20, 0)]);
    assert!(
        !bob.covers(&second_push_base),
        "Bob must ask for the missed writes first"
    );
}

#[test]
fn iter_lists_every_origin_once_with_its_greatest_clock() {
    let v = vector(&[
        stamp(2, 20, 0),
        stamp(1, 10, 0),
        stamp(2, 25, 1),
        stamp(1, 5, 9),
        stamp(3, 1, 0),
    ]);
    let mut entries: Vec<(OriginId, Hlc)> = v.iter().collect();
    entries.sort_by_key(|(o, _)| o.0);
    assert_eq!(
        entries,
        vec![
            (OriginId(1), Hlc::new(10, 0)),
            (OriginId(2), Hlc::new(25, 1)),
            (OriginId(3), Hlc::new(1, 0)),
        ]
    );
    assert_eq!(v.iter().count(), v.len());
    assert_eq!(VersionVector::new().iter().count(), 0);
}

#[test]
fn a_vector_rebuilt_from_its_entries_is_equal() {
    let v = vector(&[stamp(4, 40, 0), stamp(8, 80, 2)]);
    let rebuilt = vector(
        &v.iter()
            .map(|(origin, hlc)| Stamp::new(hlc, origin))
            .collect::<Vec<_>>(),
    );
    assert_eq!(rebuilt, v);
}

/// A small deterministic generator, so the sweep below needs no extra crate.
struct Lcg(u64);
impl Lcg {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }
}

fn random_stamps(rng: &mut Lcg) -> Vec<Stamp> {
    (0..rng.below(12))
        .map(|_| {
            stamp(
                rng.below(5),
                i64::try_from(rng.below(50)).unwrap(),
                u32::try_from(rng.below(3)).unwrap(),
            )
        })
        .collect()
}

/// What `covers` must say, from the entries alone.
fn covers_by_entries(a: &VersionVector, b: &VersionVector) -> bool {
    let a: BTreeMap<u64, Hlc> = a.iter().map(|(o, h)| (o.0, h)).collect();
    b.iter()
        .all(|(o, h)| a.get(&o.0).is_some_and(|mine| *mine >= h))
}

#[test]
fn covers_matches_the_entries_and_is_a_partial_order() {
    let mut rng = Lcg(0x0799);
    for _ in 0..3_000 {
        let (a, b, c) = (
            random_stamps(&mut rng),
            random_stamps(&mut rng),
            random_stamps(&mut rng),
        );
        let (va, vb, vc) = (vector(&a), vector(&b), vector(&c));
        assert_eq!(va.covers(&vb), covers_by_entries(&va, &vb), "{va:?} {vb:?}");
        assert!(va.covers(&va));
        if va.covers(&vb) && vb.covers(&va) {
            assert_eq!(va, vb);
        }
        if va.covers(&vb) && vb.covers(&vc) {
            assert!(va.covers(&vc), "{va:?} {vb:?} {vc:?}");
        }
        // A vector dominates every stamp it was built from.
        for s in &a {
            assert!(va.dominates(*s));
        }
        // Observing more only ever widens what is covered.
        let mut wider = va.clone();
        for s in &b {
            wider.observe(*s);
        }
        assert!(wider.covers(&va) && wider.covers(&vb));
    }
}
