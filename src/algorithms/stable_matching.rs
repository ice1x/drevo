//! Stable matching (Gale–Shapley deferred acceptance) — issue #541.
//!
//! Two sides — **proposers** and **acceptors** — each rank members of the other
//! side. A matching pairs them one-to-one; it is **stable** when no proposer and
//! acceptor would both rather be with each other than with their assigned
//! partners (or than staying single). Gale and Shapley (1962) showed a stable
//! matching always exists and that *deferred acceptance* finds one:
//!
//! 1. every free proposer proposes to the most-preferred acceptor it has not
//!    yet proposed to;
//! 2. an acceptor holds the best proposal it has received so far and rejects
//!    the rest (a held proposer is released when a better one arrives);
//! 3. repeat until no free proposer has anyone left to propose to.
//!
//! The result is **proposer-optimal**: every proposer gets the best partner it
//! can have in *any* stable matching (and every acceptor the worst). That
//! matching is unique, so the output does not depend on processing order.
//!
//! # Conventions
//!
//! * A preference list is ordered **most-preferred first**.
//! * **Incomplete lists**: a pair is acceptable only when *each* lists the
//!   other; a proposer never proposes to an acceptor that does not rank it. An
//!   unlisted partner is worse than staying single.
//! * Sides may differ in size; members left unmatched are simply absent from
//!   the result.
//! * Ids a list names that are not on the other side, and repeats after the
//!   first mention, are ignored.
//!
//! Each proposer proposes to each acceptor at most once, so the run is
//! `O(P · A)` for `P` proposers and `A` acceptors. Dependency-free and
//! WASM-safe, like the rest of [`crate::algorithms`].

use std::collections::{HashMap, HashSet, VecDeque};

/// A proposer's or acceptor's id and its preference list (most-preferred
/// first) over the other side.
pub type Preferences = (u64, Vec<u64>);

/// Proposer-optimal stable matching of `proposers` to `acceptors`.
///
/// Gale–Shapley deferred acceptance: the result is the proposer-optimal stable
/// matching (every proposer gets the best partner it can have in any stable
/// matching), which is unique, so the output does not depend on input order.
/// Returns `(proposer, acceptor)` pairs sorted by proposer id.
///
/// Preference lists are ordered most-preferred first. Only mutually acceptable
/// pairs (each on the other's list) can be matched; sides may differ in size
/// and unmatched members are absent. Ids not on the other side, and repeats
/// after the first mention, are ignored. `O(P · A)`.
#[must_use]
pub fn stable_matching(proposers: &[Preferences], acceptors: &[Preferences]) -> Vec<(u64, u64)> {
    // Each acceptor's rank of each proposer it lists (0 = best), keeping the
    // first mention of a repeat and dropping ids that are not proposers.
    let proposer_ids: HashSet<u64> = proposers.iter().map(|(id, _)| *id).collect();
    let acceptor_rank: HashMap<u64, HashMap<u64, usize>> = acceptors
        .iter()
        .map(|(a, list)| {
            let mut ranks = HashMap::new();
            for p in list.iter().filter(|p| proposer_ids.contains(p)) {
                let next = ranks.len();
                ranks.entry(*p).or_insert(next);
            }
            (*a, ranks)
        })
        .collect();

    // Each proposer's list reduced to acceptors that accept it, in order. A
    // proposal to anyone else would be rejected outright, so dropping them
    // up front changes nothing and makes every proposal meaningful.
    let lists: HashMap<u64, Vec<u64>> = proposers
        .iter()
        .map(|(p, list)| {
            let mut seen = HashSet::new();
            let acceptable = list
                .iter()
                .copied()
                .filter(|a| acceptor_rank.get(a).is_some_and(|r| r.contains_key(p)))
                .filter(|a| seen.insert(*a))
                .collect();
            (*p, acceptable)
        })
        .collect();

    let mut next_choice: HashMap<u64, usize> = HashMap::new();
    let mut held_by: HashMap<u64, u64> = HashMap::new(); // acceptor -> proposer
    let mut ids: Vec<u64> = lists.keys().copied().collect();
    ids.sort_unstable();
    let mut free: VecDeque<u64> = ids.into();

    // Every lookup below succeeds by construction (a listed acceptor ranks the
    // proposer, and holds only proposers it ranks); `get` keeps it panic-free.
    let rank = |a: u64, p: u64| acceptor_rank.get(&a).and_then(|r| r.get(&p)).copied();
    while let Some(p) = free.pop_front() {
        let Some(list) = lists.get(&p) else {
            continue;
        };
        let cursor = next_choice.entry(p).or_insert(0);
        while let Some(&a) = list.get(*cursor) {
            *cursor += 1;
            match held_by.get(&a) {
                None => {
                    held_by.insert(a, p);
                    break;
                }
                Some(&held) if rank(a, p) < rank(a, held) => {
                    held_by.insert(a, p);
                    free.push_back(held);
                    break;
                }
                Some(_) => {} // rejected: try the next acceptor
            }
        }
    }

    let mut pairs: Vec<(u64, u64)> = held_by.into_iter().map(|(a, p)| (p, a)).collect();
    pairs.sort_unstable();
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Is `matching` a one-to-one, mutually-acceptable, stable matching of the
    /// instance? Brute force over every pair — the definition, not the
    /// algorithm.
    fn assert_stable(
        proposers: &[Preferences],
        acceptors: &[Preferences],
        matching: &[(u64, u64)],
    ) {
        let rank = |prefs: &[Preferences], who: u64, other: u64| -> Option<usize> {
            prefs
                .iter()
                .find(|(id, _)| *id == who)
                .and_then(|(_, list)| list.iter().position(|x| *x == other))
        };
        let partner_of_p: HashMap<u64, u64> = matching.iter().copied().collect();
        let partner_of_a: HashMap<u64, u64> = matching.iter().map(|&(p, a)| (a, p)).collect();
        assert_eq!(
            partner_of_p.len(),
            matching.len(),
            "a proposer matched twice"
        );
        assert_eq!(
            partner_of_a.len(),
            matching.len(),
            "an acceptor matched twice"
        );
        for &(p, a) in matching {
            assert!(rank(proposers, p, a).is_some(), "{p} does not accept {a}");
            assert!(rank(acceptors, a, p).is_some(), "{a} does not accept {p}");
        }
        for (p, _) in proposers {
            for (a, _) in acceptors {
                if partner_of_p.get(p) == Some(a) {
                    continue;
                }
                let (Some(p_rank_a), Some(a_rank_p)) =
                    (rank(proposers, *p, *a), rank(acceptors, *a, *p))
                else {
                    continue; // not mutually acceptable: cannot block
                };
                let p_prefers = partner_of_p
                    .get(p)
                    .is_none_or(|cur| p_rank_a < rank(proposers, *p, *cur).unwrap_or(usize::MAX));
                let a_prefers = partner_of_a
                    .get(a)
                    .is_none_or(|cur| a_rank_p < rank(acceptors, *a, *cur).unwrap_or(usize::MAX));
                assert!(
                    !(p_prefers && a_prefers),
                    "blocking pair ({p}, {a}) in {matching:?}"
                );
            }
        }
    }

    #[test]
    fn everyone_wants_the_same_acceptor() {
        // All proposers rank 10 > 11 > 12; every acceptor ranks 3 > 2 > 1.
        let proposers = vec![
            (1, vec![10, 11, 12]),
            (2, vec![10, 11, 12]),
            (3, vec![10, 11, 12]),
        ];
        let acceptors = vec![
            (10, vec![3, 2, 1]),
            (11, vec![3, 2, 1]),
            (12, vec![3, 2, 1]),
        ];
        let m = stable_matching(&proposers, &acceptors);
        assert_eq!(m, vec![(1, 12), (2, 11), (3, 10)]);
        assert_stable(&proposers, &acceptors, &m);
    }

    #[test]
    fn the_result_is_proposer_optimal() {
        // Two stable matchings exist: {1-10, 2-11} (best for proposers) and
        // {1-11, 2-10} (best for acceptors). Proposers get their first choices.
        let proposers = vec![(1, vec![10, 11]), (2, vec![11, 10])];
        let acceptors = vec![(10, vec![2, 1]), (11, vec![1, 2])];
        assert_eq!(
            stable_matching(&proposers, &acceptors),
            vec![(1, 10), (2, 11)]
        );
        // Swapping the roles yields the other stable matching.
        assert_eq!(
            stable_matching(&acceptors, &proposers),
            vec![(10, 2), (11, 1)]
        );
    }

    #[test]
    fn a_rejected_proposer_moves_down_its_list() {
        // 1 and 2 both start at 10; 10 prefers 2, so 1 is rejected and takes 11.
        let proposers = vec![(1, vec![10, 11]), (2, vec![10, 11])];
        let acceptors = vec![(10, vec![2, 1]), (11, vec![1, 2])];
        let m = stable_matching(&proposers, &acceptors);
        assert_eq!(m, vec![(1, 11), (2, 10)]);
        assert_stable(&proposers, &acceptors, &m);
    }

    #[test]
    fn only_mutually_acceptable_pairs_match() {
        // 1 wants 10, but 10 does not list 1; 2 is on nobody's list.
        let proposers = vec![(1, vec![10]), (2, vec![10, 11]), (3, vec![11])];
        let acceptors = vec![(10, vec![2]), (11, vec![3])];
        let m = stable_matching(&proposers, &acceptors);
        assert_eq!(m, vec![(2, 10), (3, 11)]);
        assert_stable(&proposers, &acceptors, &m);
    }

    #[test]
    fn unequal_sides_leave_the_surplus_unmatched() {
        let proposers = vec![(1, vec![10]), (2, vec![10]), (3, vec![10])];
        let acceptors = vec![(10, vec![2, 3, 1])];
        assert_eq!(stable_matching(&proposers, &acceptors), vec![(2, 10)]);

        let proposers = vec![(1, vec![11, 10, 12])];
        let acceptors = vec![(10, vec![1]), (11, vec![1]), (12, vec![1])];
        assert_eq!(stable_matching(&proposers, &acceptors), vec![(1, 11)]);
    }

    #[test]
    fn empty_sides_and_empty_lists_match_nothing() {
        assert!(stable_matching(&[], &[]).is_empty());
        assert!(stable_matching(&[(1, vec![10])], &[]).is_empty());
        assert!(stable_matching(&[(1, vec![])], &[(10, vec![1])]).is_empty());
    }

    #[test]
    fn unknown_ids_and_repeats_are_ignored() {
        let proposers = vec![(1, vec![99, 10, 10, 11]), (2, vec![10])];
        let acceptors = vec![(10, vec![2, 2, 1, 42]), (11, vec![1])];
        let m = stable_matching(&proposers, &acceptors);
        assert_eq!(m, vec![(1, 11), (2, 10)]);
        assert_stable(&proposers, &acceptors, &m);
    }

    /// A tiny deterministic LCG, so the randomised check needs no dependency
    /// and always runs the same instances.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
        fn shuffled_subset(&mut self, ids: &[u64]) -> Vec<u64> {
            let mut out: Vec<u64> = ids
                .iter()
                .copied()
                .filter(|_| self.next() % 4 != 0)
                .collect();
            for i in (1..out.len()).rev() {
                let j = usize::try_from(self.next()).unwrap_or(0) % (i + 1);
                out.swap(i, j);
            }
            out
        }
    }

    #[test]
    fn random_instances_are_always_stable() {
        let mut rng = Lcg(541);
        for _ in 0..500 {
            let n_p = usize::try_from(rng.next() % 8).unwrap_or(0);
            let n_a = usize::try_from(rng.next() % 8).unwrap_or(0);
            let p_ids: Vec<u64> = (1..=n_p as u64).collect();
            let a_ids: Vec<u64> = (100..100 + n_a as u64).collect();
            let proposers: Vec<Preferences> = p_ids
                .iter()
                .map(|&p| (p, rng.shuffled_subset(&a_ids)))
                .collect();
            let acceptors: Vec<Preferences> = a_ids
                .iter()
                .map(|&a| (a, rng.shuffled_subset(&p_ids)))
                .collect();
            let m = stable_matching(&proposers, &acceptors);
            assert_stable(&proposers, &acceptors, &m);
            // Sorted by proposer id, and order-independent.
            assert!(m.windows(2).all(|w| w[0].0 < w[1].0));
            let mut reversed = proposers.clone();
            reversed.reverse();
            assert_eq!(stable_matching(&reversed, &acceptors), m);
        }
    }
}
