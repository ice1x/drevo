//! Strongly connected components (SCC) — RFC #307 Phase 8.
//!
//! Two nodes are in the same *strongly* connected component when each is
//! reachable from the other **following edge direction** — i.e. they lie on a
//! common directed cycle. This is the directed complement of
//! [`wcc`](crate::algorithms::wcc): `1 -> 2` with no `2 -> 1` is one *weakly*
//! connected component but two *strongly* connected ones. SCC answers "which
//! sets of nodes are mutually reachable / form a cycle" — feedback loops in a
//! dependency graph, cyclic references in a knowledge graph.
//!
//! Implemented with **iterative** Tarjan (an explicit work stack, never native
//! recursion) so a deep or adversarial graph cannot overflow the call stack.
//! Runs in linear time over the directed adjacency.
//!
//! Component IDs are assigned by the **minimum node ID** in each component, in
//! ascending order, so the labelling is independent of the order the engine
//! enumerated nodes in — the native snapshot and the KV backend produce
//! byte-identical results.
//!
//! Dependency-free, infallible, always compiled, WASM-safe.

use super::AdjacencyList;

/// The result of an [`scc`] run.
#[derive(Debug, Clone, PartialEq)]
pub struct SccResult {
    /// `(node_id, component_id)` pairs, sorted by ascending node ID. Component
    /// IDs are contiguous integers `0..component_count`, assigned by ascending
    /// minimum node ID per component (order-independent).
    pub components: Vec<(u64, usize)>,
    /// Number of distinct strongly connected components. An empty graph has
    /// `0`; an acyclic graph of `n` nodes has `n` singleton components.
    pub component_count: usize,
}

/// Compute the strongly connected components of `graph`.
///
/// Always terminates in linear time. Direction matters: a directed edge
/// `i -> j` contributes reachability from `i` to `j` only. A self-loop keeps
/// the node in its own singleton component (a node is always strongly connected
/// to itself). An empty graph yields an empty result; an acyclic graph yields
/// one singleton component per node.
pub fn scc(graph: &AdjacencyList) -> SccResult {
    let n = graph.node_count();
    if n == 0 {
        return SccResult {
            components: Vec::new(),
            component_count: 0,
        };
    }

    // Tarjan state, indexed by dense node index.
    let mut index = 0usize; // next DFS number to hand out
    let mut dfs_num: Vec<Option<usize>> = vec![None; n];
    let mut low: Vec<usize> = vec![0; n];
    let mut on_stack: Vec<bool> = vec![false; n];
    let mut tarjan_stack: Vec<usize> = Vec::new();

    // Each finished SCC is a list of dense indices; relabelled by min ID later.
    let mut raw_components: Vec<Vec<usize>> = Vec::new();

    for start in 0..n {
        if dfs_num[start].is_some() {
            continue;
        }

        // Explicit work stack of (node, next-out-edge cursor) replacing recursion.
        let mut work: Vec<(usize, usize)> = Vec::new();

        dfs_num[start] = Some(index);
        low[start] = index;
        index += 1;
        tarjan_stack.push(start);
        on_stack[start] = true;
        work.push((start, 0));

        while let Some(&(v, cursor)) = work.last() {
            let edges = graph.out_edges(v);
            if cursor < edges.len() {
                // Advance this frame's cursor before descending.
                work.last_mut().unwrap().1 = cursor + 1;
                let w = edges[cursor].0;
                match dfs_num[w] {
                    None => {
                        dfs_num[w] = Some(index);
                        low[w] = index;
                        index += 1;
                        tarjan_stack.push(w);
                        on_stack[w] = true;
                        work.push((w, 0));
                    }
                    Some(w_num) => {
                        if on_stack[w] {
                            // Back/cross edge into the current stack.
                            low[v] = low[v].min(w_num);
                        }
                    }
                }
            } else {
                // All out-edges of v explored.
                if low[v] == dfs_num[v].unwrap() {
                    // v is an SCC root: pop the component off the Tarjan stack.
                    let mut comp = Vec::new();
                    loop {
                        let x = tarjan_stack.pop().unwrap();
                        on_stack[x] = false;
                        comp.push(x);
                        if x == v {
                            break;
                        }
                    }
                    raw_components.push(comp);
                }
                work.pop();
                // Propagate v's lowlink to its parent (the tree-edge return step).
                if let Some(&(parent, _)) = work.last() {
                    low[parent] = low[parent].min(low[v]);
                }
            }
        }
    }

    // Assign each component a stable ID by the smallest node ID it contains.
    let mut labelled: Vec<(u64, Vec<usize>)> = raw_components
        .into_iter()
        .map(|comp| {
            let min_id = comp.iter().map(|&i| graph.id_at(i)).min().unwrap();
            (min_id, comp)
        })
        .collect();
    labelled.sort_unstable_by_key(|&(min_id, _)| min_id);
    let component_count = labelled.len();

    let mut components: Vec<(u64, usize)> = Vec::with_capacity(n);
    for (label, (_, comp)) in labelled.into_iter().enumerate() {
        for i in comp {
            components.push((graph.id_at(i), label));
        }
    }
    components.sort_unstable_by_key(|&(id, _)| id);

    SccResult {
        components,
        component_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comp_of(r: &SccResult, id: u64) -> usize {
        r.components
            .iter()
            .find(|&&(n, _)| n == id)
            .map(|&(_, c)| c)
            .unwrap()
    }

    #[test]
    fn empty_graph_has_no_components() {
        let g = AdjacencyList::from_parts(vec![], Vec::<(u64, u64, f32)>::new());
        let r = scc(&g);
        assert_eq!(r.component_count, 0);
        assert!(r.components.is_empty());
    }

    #[test]
    fn acyclic_graph_is_all_singletons() {
        // 1 -> 2 -> 3, no cycle: three SCCs (contrast WCC, which is one).
        let g = AdjacencyList::from_parts(vec![1, 2, 3], vec![(1, 2, 1.0), (2, 3, 1.0)]);
        let r = scc(&g);
        assert_eq!(r.component_count, 3);
        assert_ne!(comp_of(&r, 1), comp_of(&r, 2));
        assert_ne!(comp_of(&r, 2), comp_of(&r, 3));
    }

    #[test]
    fn a_directed_cycle_is_one_component() {
        let g =
            AdjacencyList::from_parts(vec![1, 2, 3], vec![(1, 2, 1.0), (2, 3, 1.0), (3, 1, 1.0)]);
        let r = scc(&g);
        assert_eq!(r.component_count, 1);
        assert_eq!(comp_of(&r, 1), comp_of(&r, 2));
        assert_eq!(comp_of(&r, 2), comp_of(&r, 3));
    }

    #[test]
    fn two_cycles_joined_one_way_stay_separate() {
        // Cycle {1,2}, cycle {3,4}, bridge 2 -> 3 (one direction only).
        // Two SCCs, even though it is a single weakly connected component.
        let g = AdjacencyList::from_parts(
            vec![1, 2, 3, 4],
            vec![
                (1, 2, 1.0),
                (2, 1, 1.0),
                (3, 4, 1.0),
                (4, 3, 1.0),
                (2, 3, 1.0),
            ],
        );
        let r = scc(&g);
        assert_eq!(r.component_count, 2);
        assert_eq!(comp_of(&r, 1), comp_of(&r, 2));
        assert_eq!(comp_of(&r, 3), comp_of(&r, 4));
        assert_ne!(comp_of(&r, 1), comp_of(&r, 3));
    }

    #[test]
    fn component_ids_are_stable_by_min_node_id() {
        // Enumeration order shuffled; the {2,3} cycle must get id 0 (min id 2),
        // the {5} singleton id 1.
        let g = AdjacencyList::from_parts(vec![5, 3, 2], vec![(3, 2, 1.0), (2, 3, 1.0)]);
        let r = scc(&g);
        assert_eq!(comp_of(&r, 2), 0);
        assert_eq!(comp_of(&r, 3), 0);
        assert_eq!(comp_of(&r, 5), 1);
        assert_eq!(r.components, vec![(2, 0), (3, 0), (5, 1)]);
    }

    #[test]
    fn self_loop_is_a_singleton_component() {
        let g = AdjacencyList::from_parts(vec![1, 2], vec![(1, 1, 1.0)]);
        let r = scc(&g);
        assert_eq!(r.component_count, 2);
        assert_ne!(comp_of(&r, 1), comp_of(&r, 2));
    }

    #[test]
    fn deep_chain_does_not_overflow_the_stack() {
        // A long acyclic chain would blow a recursive Tarjan; the iterative one
        // must handle it. 50k nodes, each -> next.
        let n = 50_000u64;
        let ids: Vec<u64> = (0..n).collect();
        let edges: Vec<(u64, u64, f32)> = (0..n - 1).map(|i| (i, i + 1, 1.0)).collect();
        let g = AdjacencyList::from_parts(ids, edges);
        let r = scc(&g);
        assert_eq!(r.component_count, n as usize);
    }
}
