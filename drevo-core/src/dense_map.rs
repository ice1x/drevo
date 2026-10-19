//! A map from entity id to value, stored as a vector indexed by id.
//!
//! The engine hands out node and relationship ids from counters, so the ids
//! in use are dense: a vector slot per id costs far less than a hash table
//! entry. An id far beyond the ids in use — a store imported with large ids,
//! say — goes to a small hash table instead, so a stray id never allocates a
//! huge vector.

use std::collections::HashMap;

/// How far past the current end of the vector an id may land and still be
/// stored in it (the vector grows to reach it).
const GROWTH_SLACK: u64 = 4096;

/// Entity id → `T`. Iteration is by ascending id for ids stored in the
/// vector, then the outliers in no particular order.
#[derive(Debug, Clone)]
pub(crate) struct DenseMap<T> {
    slots: Vec<Option<T>>,
    /// Ids too far past the vector to be stored in it.
    sparse: HashMap<u64, T>,
    len: usize,
}

impl<T> Default for DenseMap<T> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            sparse: HashMap::new(),
            len: 0,
        }
    }
}

impl<T> DenseMap<T> {
    /// The vector slot for `id`, if it is stored in the vector.
    fn slot(&self, id: u64) -> Option<usize> {
        usize::try_from(id).ok().filter(|&i| i < self.slots.len())
    }

    /// Whether `id` belongs in the vector, growing it as needed.
    fn dense_slot(&mut self, id: u64) -> Option<usize> {
        if let Some(i) = self.slot(id) {
            return Some(i);
        }
        let reach = (self.slots.len() as u64)
            .saturating_mul(2)
            .saturating_add(GROWTH_SLACK);
        if id > reach {
            return None;
        }
        let i = usize::try_from(id).ok()?;
        self.slots.resize_with(i + 1, || None);
        Some(i)
    }

    /// The number of entries.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// The value for `id`.
    pub(crate) fn get(&self, id: &u64) -> Option<&T> {
        match self.slot(*id) {
            Some(i) => self.slots[i].as_ref(),
            None => self.sparse.get(id),
        }
    }

    /// The value for `id`, mutably.
    pub(crate) fn get_mut(&mut self, id: &u64) -> Option<&mut T> {
        match self.slot(*id) {
            Some(i) => self.slots[i].as_mut(),
            None => self.sparse.get_mut(id),
        }
    }

    /// Whether `id` has a value.
    pub(crate) fn contains_key(&self, id: &u64) -> bool {
        self.get(id).is_some()
    }

    /// Set the value for `id`, returning the previous one.
    pub(crate) fn insert(&mut self, id: u64, value: T) -> Option<T> {
        let old = match self.dense_slot(id) {
            Some(i) => self.slots[i].replace(value),
            None => self.sparse.insert(id, value),
        };
        if old.is_none() {
            self.len += 1;
        }
        old
    }

    /// Remove and return the value for `id`.
    pub(crate) fn remove(&mut self, id: &u64) -> Option<T> {
        let old = match self.slot(*id) {
            Some(i) => self.slots[i].take(),
            None => self.sparse.remove(id),
        };
        if old.is_some() {
            self.len -= 1;
        }
        old
    }

    /// The value for `id`, inserting `T::default()` first if there is none.
    pub(crate) fn entry_or_default(&mut self, id: u64) -> &mut T
    where
        T: Default,
    {
        if !self.contains_key(&id) {
            self.insert(id, T::default());
        }
        match self.slot(id) {
            Some(i) => self.slots[i].get_or_insert_with(T::default),
            None => self.sparse.entry(id).or_default(),
        }
    }

    /// Every `(id, value)`.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (u64, &T)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, v)| v.as_ref().map(|v| (i as u64, v)))
            .chain(self.sparse.iter().map(|(&id, v)| (id, v)))
    }

    /// Every id.
    pub(crate) fn keys(&self) -> impl Iterator<Item = u64> + '_ {
        self.iter().map(|(id, _)| id)
    }

    /// Every value.
    pub(crate) fn values(&self) -> impl Iterator<Item = &T> {
        self.iter().map(|(_, v)| v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn behaves_like_a_hash_map() {
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut ours: DenseMap<u64> = DenseMap::default();
        let mut reference: HashMap<u64, u64> = HashMap::new();
        for step in 0..20_000 {
            // Mostly dense ids, now and then a far-away one.
            let id = if next() % 50 == 0 {
                next() % 1_000_000_000_000
            } else {
                next() % 3_000
            };
            match next() % 4 {
                0 => assert_eq!(ours.remove(&id), reference.remove(&id), "step {step}"),
                1 => {
                    *ours.entry_or_default(id) += 1;
                    *reference.entry(id).or_default() += 1;
                }
                _ => assert_eq!(ours.insert(id, step), reference.insert(id, step)),
            }
            assert_eq!(ours.len(), reference.len(), "step {step}");
            assert_eq!(ours.get(&id), reference.get(&id), "step {step}");
        }
        let mut all: Vec<(u64, u64)> = ours.iter().map(|(k, v)| (k, *v)).collect();
        all.sort_unstable();
        let mut expected: Vec<(u64, u64)> = reference.into_iter().collect();
        expected.sort_unstable();
        assert_eq!(all, expected);
    }

    #[test]
    fn dense_ids_iterate_in_order_and_far_ids_stay_out_of_the_vector() {
        let mut m = DenseMap::default();
        for id in [5u64, 1, 3] {
            m.insert(id, id * 10);
        }
        m.insert(u64::MAX - 1, 7);
        assert!(m.slots.len() <= 6, "a far id must not grow the vector");
        let dense: Vec<u64> = m.keys().take(3).collect();
        assert_eq!(dense, vec![1, 3, 5]);
        assert_eq!(m.get(&(u64::MAX - 1)), Some(&7));
        assert_eq!(m.len(), 4);
        assert_eq!(m.remove(&3), Some(30));
        assert_eq!(m.get(&3), None);
        assert_eq!(m.len(), 3);
    }
}
