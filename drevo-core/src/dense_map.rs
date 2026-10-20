//! A map from entity id to value, stored as a vector indexed by id.
//!
//! The engine hands out node and relationship ids from counters, so the ids
//! in use are dense: a vector slot per id costs far less than a hash table
//! entry. An id far beyond the ids in use — a store imported with large ids,
//! say — goes to a small hash table instead, so a stray id never allocates a
//! huge vector.
//!
//! The slots are kept in shared chunks of 64: cloning the map, as a
//! transaction does to get its own copy of the store, copies one pointer per
//! chunk, and the first write to a chunk copies that chunk alone.

use std::collections::HashMap;
use std::sync::Arc;

/// How far past the current end of the vector an id may land and still be
/// stored in it (the vector grows to reach it).
const GROWTH_SLACK: u64 = 4096;

/// Slots per shared chunk.
const CHUNK: usize = 64;

/// Entity id → `T`. Iteration is by ascending id for ids stored in the
/// vector, then the outliers in no particular order.
#[derive(Debug, Clone)]
pub(crate) struct DenseMap<T> {
    /// Slots `CHUNK * i ..` in chunk `i`, each `CHUNK` long.
    chunks: Vec<Arc<Vec<Option<T>>>>,
    /// Ids too far past the vector to be stored in it.
    sparse: HashMap<u64, T>,
    /// The smallest id in `sparse` (`u64::MAX` when it is empty). Every id
    /// below the vector's end is in the vector: once the vector grows past
    /// this one, the outliers it reaches move into it.
    sparse_min: u64,
    len: usize,
}

impl<T> Default for DenseMap<T> {
    fn default() -> Self {
        Self {
            chunks: Vec::new(),
            sparse: HashMap::new(),
            sparse_min: u64::MAX,
            len: 0,
        }
    }
}

impl<T: Clone> DenseMap<T> {
    /// The number of ids the vector covers.
    fn end(&self) -> usize {
        self.chunks.len() * CHUNK
    }

    /// The vector slot for `id`, if it is stored in the vector.
    fn slot(&self, id: u64) -> Option<usize> {
        usize::try_from(id).ok().filter(|&i| i < self.end())
    }

    /// The slot `i`, for writing: its chunk is copied first if a clone of
    /// the map shares it.
    fn slot_mut(&mut self, i: usize) -> &mut Option<T> {
        &mut Arc::make_mut(&mut self.chunks[i / CHUNK])[i % CHUNK]
    }

    /// Whether `id` belongs in the vector, growing it as needed.
    fn dense_slot(&mut self, id: u64) -> Option<usize> {
        if let Some(i) = self.slot(id) {
            return Some(i);
        }
        let reach = (self.end() as u64)
            .saturating_mul(2)
            .saturating_add(GROWTH_SLACK);
        if id > reach {
            return None;
        }
        let i = usize::try_from(id).ok()?;
        while self.end() <= i {
            self.chunks.push(Arc::new(vec![None; CHUNK]));
        }
        // An id below the vector's end is looked up in the vector only, so
        // the outliers the vector now reaches move into it.
        let end = self.end() as u64;
        if self.sparse_min < end {
            let reached: Vec<u64> = self.sparse.keys().copied().filter(|&k| k < end).collect();
            for k in reached {
                if let Some(value) = self.sparse.remove(&k) {
                    *self.slot_mut(k as usize) = Some(value);
                }
            }
            self.sparse_min = self.sparse.keys().copied().min().unwrap_or(u64::MAX);
        }
        Some(i)
    }

    /// The number of entries.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// The value for `id`.
    pub(crate) fn get(&self, id: &u64) -> Option<&T> {
        match self.slot(*id) {
            Some(i) => self.chunks[i / CHUNK][i % CHUNK].as_ref(),
            None => self.sparse.get(id),
        }
    }

    /// The value for `id`, mutably.
    pub(crate) fn get_mut(&mut self, id: &u64) -> Option<&mut T> {
        match self.slot(*id) {
            Some(i) => {
                // Copy the chunk only when there is a value to change.
                self.chunks[i / CHUNK][i % CHUNK].as_ref()?;
                self.slot_mut(i).as_mut()
            }
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
            Some(i) => self.slot_mut(i).replace(value),
            None => {
                self.sparse_min = self.sparse_min.min(id);
                self.sparse.insert(id, value)
            }
        };
        if old.is_none() {
            self.len += 1;
        }
        old
    }

    /// Remove and return the value for `id`.
    pub(crate) fn remove(&mut self, id: &u64) -> Option<T> {
        let old = match self.slot(*id) {
            Some(i) => {
                self.chunks[i / CHUNK][i % CHUNK].as_ref()?;
                self.slot_mut(i).take()
            }
            None => {
                let old = self.sparse.remove(id);
                if *id == self.sparse_min {
                    self.sparse_min = self.sparse.keys().copied().min().unwrap_or(u64::MAX);
                }
                old
            }
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
            Some(i) => self.slot_mut(i).get_or_insert_with(T::default),
            None => self.sparse.entry(id).or_default(),
        }
    }

    /// Every `(id, value)`.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (u64, &T)> {
        self.chunks
            .iter()
            .flat_map(|chunk| chunk.iter())
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
    fn an_outlier_stays_visible_once_the_vector_grows_past_it() {
        let mut m = DenseMap::default();
        m.insert(10_000, "far");
        assert!(m.end() < 10_000, "first stored as an outlier");
        // Ids that grow the vector step by step until it covers 10_000.
        let mut id = 0;
        while (m.end() as u64) <= 10_000 {
            m.insert(id, "near");
            id = m.end() as u64 * 2;
        }
        assert_eq!(m.get(&10_000), Some(&"far"));
        assert_eq!(m.get_mut(&10_000).copied(), Some("far"));
        assert!(m.contains_key(&10_000));
        assert!(m.keys().any(|k| k == 10_000));
        *m.entry_or_default(10_000) = "updated";
        assert_eq!(m.get(&10_000), Some(&"updated"));
        assert_eq!(m.remove(&10_000), Some("updated"));
        assert_eq!(m.get(&10_000), None);
        assert!(m.sparse.is_empty());
    }

    #[test]
    fn far_apart_ids_in_an_empty_map_behave_like_a_hash_map() {
        // Adjacency of a fresh store: random endpoints over a wide id range.
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut ours: DenseMap<u64> = DenseMap::default();
        let mut reference: HashMap<u64, u64> = HashMap::new();
        for step in 0..20_000u64 {
            let id = next() % 200_000;
            match next() % 5 {
                0 => assert_eq!(ours.remove(&id), reference.remove(&id), "step {step}"),
                1 | 2 => {
                    *ours.entry_or_default(id) += 1;
                    *reference.entry(id).or_default() += 1;
                }
                _ => assert_eq!(ours.insert(id, step), reference.insert(id, step)),
            }
            assert_eq!(ours.len(), reference.len(), "step {step}");
        }
        for (id, value) in &reference {
            assert_eq!(ours.get(id), Some(value), "id {id}");
        }
        assert_eq!(ours.iter().count(), reference.len());
        let end = ours.end() as u64;
        assert!(
            ours.sparse.keys().all(|&k| k >= end),
            "no outlier below the vector's end"
        );
    }

    #[test]
    fn a_clone_shares_chunks_until_one_is_written() {
        let mut original: DenseMap<u64> = DenseMap::default();
        for id in 0..1_000u64 {
            original.insert(id, id);
        }
        let mut copy = original.clone();
        let shared = |a: &DenseMap<u64>, b: &DenseMap<u64>| {
            a.chunks
                .iter()
                .zip(&b.chunks)
                .filter(|(x, y)| Arc::ptr_eq(x, y))
                .count()
        };
        assert_eq!(shared(&original, &copy), original.chunks.len());
        // One write copies one chunk; the original is unchanged.
        *copy.get_mut(&5).unwrap() = 50;
        copy.insert(700, 7_000);
        copy.remove(&999);
        assert_eq!(shared(&original, &copy), original.chunks.len() - 3);
        assert_eq!(original.get(&5), Some(&5));
        assert_eq!(original.get(&700), Some(&700));
        assert_eq!(original.get(&999), Some(&999));
        assert_eq!(copy.get(&5), Some(&50));
        assert_eq!(copy.get(&999), None);
        // Reading, or touching an empty slot, copies nothing.
        let before = shared(&original, &copy);
        assert!(copy.get_mut(&5_000).is_none());
        assert!(copy.remove(&5_000).is_none());
        assert_eq!(copy.iter().count(), 999);
        assert_eq!(shared(&original, &copy), before);
    }

    #[test]
    fn dense_ids_iterate_in_order_and_far_ids_stay_out_of_the_vector() {
        let mut m = DenseMap::default();
        for id in [5u64, 1, 3] {
            m.insert(id, id * 10);
        }
        m.insert(u64::MAX - 1, 7);
        assert!(m.end() <= 64, "a far id must not grow the vector");
        let dense: Vec<u64> = m.keys().take(3).collect();
        assert_eq!(dense, vec![1, 3, 5]);
        assert_eq!(m.get(&(u64::MAX - 1)), Some(&7));
        assert_eq!(m.len(), 4);
        assert_eq!(m.remove(&3), Some(30));
        assert_eq!(m.get(&3), None);
        assert_eq!(m.len(), 3);
    }
}
