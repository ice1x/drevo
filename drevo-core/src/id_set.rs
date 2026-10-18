//! A set of entity ids, ascending, that stores a single id inline.
//!
//! Index postings map a value to the ids carrying it. When values are mostly
//! distinct — coordinates, external ids, titles — almost every posting holds
//! one id, and a `BTreeSet` would allocate a whole tree node for it. An
//! [`IdSet`] keeps one id in place and only grows into a `BTreeSet` at the
//! second.

use std::collections::BTreeSet;

/// Ids in ascending order; one id costs no allocation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum IdSet {
    /// No ids.
    #[default]
    Empty,
    /// Exactly one id.
    One(u64),
    /// Two or more ids.
    Many(BTreeSet<u64>),
}

impl IdSet {
    /// Add `id`; `false` if it was already there.
    pub(crate) fn insert(&mut self, id: u64) -> bool {
        match self {
            IdSet::Empty => {
                *self = IdSet::One(id);
                true
            }
            IdSet::One(existing) if *existing == id => false,
            IdSet::One(existing) => {
                let first = *existing;
                *self = IdSet::Many(BTreeSet::from([first, id]));
                true
            }
            IdSet::Many(set) => set.insert(id),
        }
    }

    /// Remove `id`; `false` if it was not there. A set left with one id
    /// stores it inline again.
    pub(crate) fn remove(&mut self, id: &u64) -> bool {
        match self {
            IdSet::Empty => false,
            IdSet::One(existing) if existing == id => {
                *self = IdSet::Empty;
                true
            }
            IdSet::One(_) => false,
            IdSet::Many(set) => {
                let removed = set.remove(id);
                if set.len() == 1 {
                    if let Some(&last) = set.first() {
                        *self = IdSet::One(last);
                    }
                }
                removed
            }
        }
    }

    /// Whether there are no ids.
    pub(crate) fn is_empty(&self) -> bool {
        match self {
            IdSet::Empty => true,
            IdSet::One(_) => false,
            IdSet::Many(set) => set.is_empty(),
        }
    }

    /// How many ids there are.
    pub(crate) fn len(&self) -> usize {
        match self {
            IdSet::Empty => 0,
            IdSet::One(_) => 1,
            IdSet::Many(set) => set.len(),
        }
    }

    /// The lowest id.
    pub(crate) fn first(&self) -> Option<&u64> {
        match self {
            IdSet::Empty => None,
            IdSet::One(id) => Some(id),
            IdSet::Many(set) => set.first(),
        }
    }

    /// The ids, ascending.
    pub(crate) fn iter(&self) -> Iter<'_> {
        match self {
            IdSet::Empty => Iter::One(None),
            IdSet::One(id) => Iter::One(Some(id)),
            IdSet::Many(set) => Iter::Many(set.iter()),
        }
    }
}

/// The ids of an [`IdSet`], ascending.
pub(crate) enum Iter<'a> {
    One(Option<&'a u64>),
    Many(std::collections::btree_set::Iter<'a, u64>),
}

impl<'a> Iterator for Iter<'a> {
    type Item = &'a u64;

    fn next(&mut self) -> Option<&'a u64> {
        match self {
            Iter::One(id) => id.take(),
            Iter::Many(it) => it.next(),
        }
    }
}

impl<'a> IntoIterator for &'a IdSet {
    type Item = &'a u64;
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(s: &IdSet) -> Vec<u64> {
        s.iter().copied().collect()
    }

    #[test]
    fn grows_from_inline_to_a_tree_and_back() {
        let mut s = IdSet::default();
        assert!(s.is_empty());
        assert_eq!(s.first(), None);
        assert!(s.insert(7));
        assert_eq!(s, IdSet::One(7));
        assert!(!s.insert(7));
        assert!(s.insert(3));
        assert!(s.insert(9));
        assert_eq!(ids(&s), vec![3, 7, 9]);
        assert_eq!((s.len(), s.first()), (3, Some(&3)));
        assert!(s.remove(&3));
        assert!(!s.remove(&3));
        assert!(s.remove(&9));
        assert_eq!(s, IdSet::One(7), "one id left goes back inline");
        assert!(!s.remove(&8));
        assert!(s.remove(&7));
        assert!(s.is_empty());
        assert_eq!(ids(&s), Vec::<u64>::new());
    }

    #[test]
    fn behaves_like_a_btreeset() {
        // A fixed pseudo-random sequence of inserts and removes.
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut ours = IdSet::default();
        let mut reference = BTreeSet::new();
        for _ in 0..5_000 {
            let id = next() % 16;
            if next() % 3 == 0 {
                assert_eq!(ours.remove(&id), reference.remove(&id));
            } else {
                assert_eq!(ours.insert(id), reference.insert(id));
            }
            assert_eq!(ids(&ours), reference.iter().copied().collect::<Vec<_>>());
            assert_eq!(ours.len(), reference.len());
            assert_eq!(ours.first(), reference.first());
            assert_eq!(ours.is_empty(), reference.is_empty());
        }
    }
}
