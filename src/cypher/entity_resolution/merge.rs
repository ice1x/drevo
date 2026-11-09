//! The plan of folding dropped nodes into a kept one: which relationships to
//! re-create on the kept node (and which would duplicate one it has), the
//! kept node's merged properties, its aliases and the ids it absorbed.

use std::collections::BTreeMap;

use crate::cypher::executor::Value;

/// The relationship type `drevo.memory.rememberEntity` records near-misses
/// with; one between two merged nodes is settled by the merge and dropped.
pub(crate) const POSSIBLY_SAME: &str = "POSSIBLY_SAME";

/// How conflicting property values are combined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Strategy {
    /// The kept node's value wins; missing ones are filled from the dropped
    /// nodes, in order.
    #[default]
    KeepFirst,
    /// The value of the most recently changed node (`updated_at`, else
    /// `created_at`) wins; the kept node wins a tie.
    Newest,
    /// Differing scalar values become a list of the distinct values, the
    /// kept node's first.
    Collect,
}

/// A node taking part in a merge.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MergeNode {
    /// The node id.
    pub(crate) id: u64,
    /// Its properties.
    pub(crate) properties: BTreeMap<String, Value>,
}

/// A relationship of a node taking part in a merge.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MergeEdge {
    /// The relationship id.
    pub(crate) id: u64,
    /// Its type.
    pub(crate) kind: String,
    /// Its start node.
    pub(crate) from: u64,
    /// Its end node.
    pub(crate) to: u64,
    /// Its properties.
    pub(crate) properties: BTreeMap<String, Value>,
}

/// A relationship the merge creates on the kept node.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlannedEdge {
    /// Its type.
    pub(crate) kind: String,
    /// Its start node.
    pub(crate) from: u64,
    /// Its end node.
    pub(crate) to: u64,
    /// Its properties.
    pub(crate) properties: BTreeMap<String, Value>,
}

/// What a merge reads.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MergeInput<'a> {
    /// The node that stays.
    pub(crate) keep: &'a MergeNode,
    /// The nodes folded into it, in the caller's order.
    pub(crate) drops: &'a [MergeNode],
    /// Every relationship of the kept node.
    pub(crate) keep_edges: &'a [MergeEdge],
    /// Every relationship of the dropped nodes.
    pub(crate) drop_edges: &'a [MergeEdge],
    /// The names of the `:Alias` nodes already pointing at the kept or a
    /// dropped node.
    pub(crate) alias_names: &'a [String],
    /// How conflicting properties are combined.
    pub(crate) strategy: Strategy,
    /// The property holding a node's name.
    pub(crate) name_property: &'a str,
    /// The property holding a node's type.
    pub(crate) type_property: &'a str,
}

/// What a merge writes.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct MergePlan {
    /// Properties of the kept node to set (only changed ones; `aliases` and
    /// `merged_ids` are separate).
    pub(crate) properties: Vec<(String, Value)>,
    /// The kept node's `aliases` after the merge.
    pub(crate) aliases: Vec<String>,
    /// The names to create an `:Alias` node for.
    pub(crate) new_aliases: Vec<String>,
    /// The kept node's `merged_ids` after the merge.
    pub(crate) merged_ids: Vec<i64>,
    /// Relationships to create on the kept node.
    pub(crate) edges: Vec<PlannedEdge>,
    /// Relationships of dropped nodes not re-created because the kept node
    /// would have had them twice.
    pub(crate) duplicates: usize,
}

/// Properties a merge never combines: identity and its own bookkeeping.
pub(crate) const RESERVED: [&str; 5] = ["id", "aliases", "merged_ids", "created_at", "updated_at"];

/// Plan folding `input.drops` into `input.keep`.
pub(crate) fn plan(input: MergeInput<'_>) -> MergePlan {
    let dropped: Vec<u64> = input.drops.iter().map(|d| d.id).collect();
    let (edges, duplicates) = plan_edges(&input, &dropped);
    let keep_name = string_at(input.keep, input.name_property);

    let mut aliases = strings_at(input.keep, "aliases");
    let mut new_aliases: Vec<String> = Vec::new();
    let mut merged_ids = integers_at(input.keep, "merged_ids");
    for drop in input.drops {
        let name = string_at(drop, input.name_property);
        let mut names: Vec<String> = name.iter().cloned().collect();
        names.extend(strings_at(drop, "aliases"));
        for alias in names {
            if Some(&alias) != keep_name.as_ref() && !aliases.contains(&alias) {
                aliases.push(alias);
            }
        }
        if let Some(name) = name {
            if Some(&name) != keep_name.as_ref()
                && !input.alias_names.contains(&name)
                && !new_aliases.contains(&name)
            {
                new_aliases.push(name);
            }
        }
        merged_ids.push(i64::try_from(drop.id).unwrap_or(i64::MAX));
        merged_ids.extend(integers_at(drop, "merged_ids"));
    }
    let mut seen = std::collections::BTreeSet::new();
    merged_ids.retain(|id| seen.insert(*id));

    MergePlan {
        properties: plan_properties(&input),
        aliases,
        new_aliases,
        merged_ids,
        edges,
        duplicates,
    }
}

/// The relationships to create on the kept node and how many were
/// duplicates.
fn plan_edges(input: &MergeInput<'_>, dropped: &[u64]) -> (Vec<PlannedEdge>, usize) {
    let keep = input.keep.id;
    let remap = |id: u64| if dropped.contains(&id) { keep } else { id };
    // The kept node's relationships that survive: those not touching a
    // dropped node (those are re-created from the dropped side).
    let mut present: Vec<PlannedEdge> = input
        .keep_edges
        .iter()
        .filter(|e| !dropped.contains(&e.from) && !dropped.contains(&e.to))
        .map(|e| PlannedEdge {
            kind: e.kind.clone(),
            from: e.from,
            to: e.to,
            properties: e.properties.clone(),
        })
        .collect();
    let mut moved_ids = std::collections::BTreeSet::new();
    let mut edges = Vec::new();
    let mut duplicates = 0;
    for edge in input.drop_edges {
        if !moved_ids.insert(edge.id) {
            continue;
        }
        let planned = PlannedEdge {
            kind: edge.kind.clone(),
            from: remap(edge.from),
            to: remap(edge.to),
            properties: edge.properties.clone(),
        };
        if planned.kind == POSSIBLY_SAME && planned.from == keep && planned.to == keep {
            continue;
        }
        if present.contains(&planned) {
            duplicates += 1;
            continue;
        }
        present.push(planned.clone());
        edges.push(planned);
    }
    (edges, duplicates)
}

/// The kept node's properties that change, by key.
fn plan_properties(input: &MergeInput<'_>) -> Vec<(String, Value)> {
    let skip = |key: &str| {
        RESERVED.contains(&key) || key == input.name_property || key == input.type_property
    };
    let mut keys: Vec<&String> = Vec::new();
    for node in std::iter::once(input.keep).chain(input.drops) {
        for (key, value) in &node.properties {
            if !skip(key) && !matches!(value, Value::Null) && !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    keys.sort();
    // Oldest first, so the newest value is the last one written; the kept
    // node goes last among equals, so it wins a tie.
    let mut by_age: Vec<(Stamp, usize, &MergeNode)> = input
        .drops
        .iter()
        .enumerate()
        .map(|(i, d)| (Stamp::of(d), i, d))
        .collect();
    by_age.push((Stamp::of(input.keep), usize::MAX, input.keep));
    by_age.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    let mut changed = Vec::new();
    for key in keys {
        let current = input
            .keep
            .properties
            .get(key)
            .filter(|v| !matches!(v, Value::Null));
        let present = |node: &MergeNode| -> Option<Value> {
            node.properties
                .get(key)
                .filter(|v| !matches!(v, Value::Null))
                .cloned()
        };
        let merged = match input.strategy {
            Strategy::KeepFirst => current
                .cloned()
                .or_else(|| input.drops.iter().find_map(present)),
            Strategy::Newest => by_age.iter().rev().find_map(|(_, _, n)| present(n)),
            Strategy::Collect => {
                let values: Vec<Value> = std::iter::once(input.keep)
                    .chain(input.drops)
                    .filter_map(present)
                    .collect();
                let mut distinct: Vec<Value> = Vec::new();
                for v in values {
                    if !distinct.contains(&v) {
                        distinct.push(v);
                    }
                }
                if distinct.len() > 1 && distinct.iter().all(is_scalar) {
                    Some(Value::List(distinct))
                } else {
                    current.cloned().or_else(|| distinct.into_iter().next())
                }
            }
        };
        if let Some(value) = merged {
            if current != Some(&value) {
                changed.push((key.clone(), value));
            }
        }
    }
    changed
}

/// When a node last changed, for [`Strategy::Newest`]: `updated_at`, else
/// `created_at`; nodes without either are oldest.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Stamp {
    Missing,
    Integer(i64),
    Text(String),
}

impl Stamp {
    fn of(node: &MergeNode) -> Self {
        let value = node
            .properties
            .get("updated_at")
            .filter(|v| !matches!(v, Value::Null))
            .or_else(|| node.properties.get("created_at"));
        match value {
            Some(Value::Integer(n)) => Self::Integer(*n),
            Some(Value::String(s)) => Self::Text(s.clone()),
            _ => Self::Missing,
        }
    }
}

/// Whether `value` is a scalar `collect` may gather into a list.
fn is_scalar(value: &Value) -> bool {
    matches!(
        value,
        Value::String(_) | Value::Integer(_) | Value::Float(_) | Value::Bool(_)
    )
}

/// `node`'s `key` as a string.
fn string_at(node: &MergeNode, key: &str) -> Option<String> {
    match node.properties.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// The strings of `node`'s list `key`.
fn strings_at(node: &MergeNode, key: &str) -> Vec<String> {
    match node.properties.get(key) {
        Some(Value::List(items)) => items
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The integers of `node`'s list `key`.
fn integers_at(node: &MergeNode, key: &str) -> Vec<i64> {
    match node.properties.get(key) {
        Some(Value::List(items)) => items
            .iter()
            .filter_map(|v| match v {
                Value::Integer(n) => Some(*n),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }

    fn node(id: u64, props: &[(&str, Value)]) -> MergeNode {
        MergeNode {
            id,
            properties: props
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        }
    }

    fn edge(id: u64, kind: &str, from: u64, to: u64, props: &[(&str, Value)]) -> MergeEdge {
        MergeEdge {
            id,
            kind: kind.to_string(),
            from,
            to,
            properties: props
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        }
    }

    fn input<'a>(
        keep: &'a MergeNode,
        drops: &'a [MergeNode],
        keep_edges: &'a [MergeEdge],
        drop_edges: &'a [MergeEdge],
        strategy: Strategy,
    ) -> MergeInput<'a> {
        MergeInput {
            keep,
            drops,
            keep_edges,
            drop_edges,
            alias_names: &[],
            strategy,
            name_property: "name",
            type_property: "type",
        }
    }

    fn planned(kind: &str, from: u64, to: u64) -> PlannedEdge {
        PlannedEdge {
            kind: kind.to_string(),
            from,
            to,
            properties: BTreeMap::new(),
        }
    }

    // ----- relationships -------------------------------------------------------

    #[test]
    fn relationships_of_dropped_nodes_move_to_the_kept_node() {
        let keep = node(1, &[("name", s("Пушкин"))]);
        let drops = [node(2, &[("name", s("А. С. Пушкин"))])];
        let drop_edges = [
            edge(10, "WROTE", 2, 7, &[("year", Value::Integer(1833))]),
            edge(11, "MENTIONS", 8, 2, &[]),
        ];
        let p = plan(input(&keep, &drops, &[], &drop_edges, Strategy::KeepFirst));
        assert_eq!(
            p.edges,
            [
                PlannedEdge {
                    kind: "WROTE".into(),
                    from: 1,
                    to: 7,
                    properties: BTreeMap::from([("year".into(), Value::Integer(1833))]),
                },
                planned("MENTIONS", 8, 1),
            ]
        );
        assert_eq!(p.duplicates, 0);
    }

    #[test]
    fn a_relationship_the_kept_node_already_has_is_collapsed() {
        let keep = node(1, &[("name", s("Пушкин"))]);
        let drops = [node(2, &[("name", s("Pushkin"))])];
        let keep_edges = [edge(20, "WROTE", 1, 7, &[])];
        let drop_edges = [
            edge(10, "WROTE", 2, 7, &[]),
            // Same type and endpoint, other properties: not a duplicate.
            edge(12, "WROTE", 2, 7, &[("draft", Value::Bool(true))]),
            // Same type and properties, other direction: not a duplicate.
            edge(13, "WROTE", 7, 2, &[]),
        ];
        let p = plan(input(
            &keep,
            &drops,
            &keep_edges,
            &drop_edges,
            Strategy::KeepFirst,
        ));
        assert_eq!(p.duplicates, 1);
        assert_eq!(p.edges.len(), 2);
        assert_eq!(p.edges[1], planned("WROTE", 7, 1));
    }

    #[test]
    fn two_dropped_nodes_with_the_same_relationship_give_it_once() {
        let keep = node(1, &[]);
        let drops = [node(2, &[]), node(3, &[])];
        let drop_edges = [edge(10, "KNOWS", 2, 9, &[]), edge(11, "KNOWS", 3, 9, &[])];
        let p = plan(input(&keep, &drops, &[], &drop_edges, Strategy::KeepFirst));
        assert_eq!(p.edges, [planned("KNOWS", 1, 9)]);
        assert_eq!(p.duplicates, 1);
    }

    #[test]
    fn merging_two_linked_nodes_keeps_the_self_loop_once() {
        let keep = node(1, &[]);
        let drops = [node(2, &[])];
        // keep -KNOWS-> drop and drop -KNOWS-> keep both become keep -> keep.
        let link = edge(10, "KNOWS", 1, 2, &[]);
        let back = edge(11, "KNOWS", 2, 1, &[]);
        let keep_edges = [link.clone(), back.clone()];
        let drop_edges = [link, back];
        let p = plan(input(
            &keep,
            &drops,
            &keep_edges,
            &drop_edges,
            Strategy::KeepFirst,
        ));
        assert_eq!(p.edges, [planned("KNOWS", 1, 1)]);
        assert_eq!(p.duplicates, 1);
    }

    #[test]
    fn a_relationship_listed_twice_is_moved_once() {
        // A relationship between two dropped nodes is a relationship of both.
        let keep = node(1, &[]);
        let drops = [node(2, &[]), node(3, &[])];
        let between = edge(10, "KNOWS", 2, 3, &[]);
        let drop_edges = [between.clone(), between];
        let p = plan(input(&keep, &drops, &[], &drop_edges, Strategy::KeepFirst));
        assert_eq!(p.edges, [planned("KNOWS", 1, 1)]);
        assert_eq!(p.duplicates, 0);
    }

    #[test]
    fn a_possibly_same_between_merged_nodes_is_settled() {
        let keep = node(1, &[]);
        let drops = [node(2, &[])];
        let near_miss = edge(10, POSSIBLY_SAME, 2, 1, &[("score", Value::Float(0.85))]);
        let elsewhere = edge(11, POSSIBLY_SAME, 2, 5, &[("score", Value::Float(0.8))]);
        let drop_edges = [near_miss.clone(), elsewhere];
        let p = plan(input(
            &keep,
            &drops,
            std::slice::from_ref(&near_miss),
            &drop_edges,
            Strategy::KeepFirst,
        ));
        assert_eq!(p.edges.len(), 1);
        assert_eq!((p.edges[0].from, p.edges[0].to), (1, 5));
        assert_eq!(p.duplicates, 0);
    }

    // ----- properties ----------------------------------------------------------

    fn three() -> (MergeNode, [MergeNode; 2]) {
        let keep = node(
            1,
            &[
                ("name", s("Пушкин")),
                ("type", s("PERSON")),
                ("born", Value::Integer(1799)),
                ("city", s("Moscow")),
                ("updated_at", s("2026-01-01T00:00:00Z")),
            ],
        );
        let drops = [
            node(
                2,
                &[
                    ("name", s("А. С. Пушкин")),
                    ("type", s("PERSON")),
                    ("city", s("St Petersburg")),
                    ("died", Value::Integer(1837)),
                    ("updated_at", s("2026-03-01T00:00:00Z")),
                ],
            ),
            node(
                3,
                &[
                    ("name", s("Pushkin")),
                    ("type", s("PERSON")),
                    ("city", s("Moscow")),
                    ("died", Value::Integer(1838)),
                    ("updated_at", s("2025-01-01T00:00:00Z")),
                ],
            ),
        ];
        (keep, drops)
    }

    fn props(p: &MergePlan) -> BTreeMap<String, Value> {
        p.properties.iter().cloned().collect()
    }

    #[test]
    fn keep_first_fills_only_missing_properties() {
        let (keep, drops) = three();
        let p = plan(input(&keep, &drops, &[], &[], Strategy::KeepFirst));
        assert_eq!(
            props(&p),
            BTreeMap::from([("died".to_string(), Value::Integer(1837))])
        );
    }

    #[test]
    fn newest_takes_the_most_recently_updated_value() {
        let (keep, drops) = three();
        let p = plan(input(&keep, &drops, &[], &[], Strategy::Newest));
        assert_eq!(
            props(&p),
            BTreeMap::from([
                ("city".to_string(), s("St Petersburg")),
                ("died".to_string(), Value::Integer(1837)),
            ])
        );
    }

    #[test]
    fn newest_falls_back_to_created_at_and_the_kept_node_wins_a_tie() {
        let keep = node(1, &[("city", s("Moscow"))]);
        let drops = [
            node(2, &[("city", s("Kazan")), ("created_at", s("2026-02-01"))]),
            node(3, &[("city", s("Tver"))]),
        ];
        let p = plan(input(&keep, &drops, &[], &[], Strategy::Newest));
        assert_eq!(
            props(&p),
            BTreeMap::from([("city".to_string(), s("Kazan"))])
        );
        // No timestamps anywhere: the kept node's value stays.
        let drops = [node(2, &[("city", s("Kazan"))])];
        let p = plan(input(&keep, &drops, &[], &[], Strategy::Newest));
        assert!(p.properties.is_empty(), "{:?}", p.properties);
    }

    #[test]
    fn collect_turns_conflicting_scalars_into_lists() {
        let (keep, drops) = three();
        let p = plan(input(&keep, &drops, &[], &[], Strategy::Collect));
        assert_eq!(
            props(&p),
            BTreeMap::from([
                (
                    "city".to_string(),
                    Value::List(vec![s("Moscow"), s("St Petersburg")])
                ),
                (
                    "died".to_string(),
                    Value::List(vec![Value::Integer(1837), Value::Integer(1838)])
                ),
            ])
        );
    }

    #[test]
    fn collect_keeps_the_kept_list_when_values_are_not_scalars() {
        let keep = node(1, &[("tags", Value::List(vec![s("poet")]))]);
        let drops = [node(2, &[("tags", Value::List(vec![s("writer")]))])];
        let p = plan(input(&keep, &drops, &[], &[], Strategy::Collect));
        assert!(p.properties.is_empty(), "{:?}", p.properties);
    }

    #[test]
    fn identity_and_bookkeeping_are_never_combined() {
        let (keep, drops) = three();
        for strategy in [Strategy::KeepFirst, Strategy::Newest, Strategy::Collect] {
            let p = plan(input(&keep, &drops, &[], &[], strategy));
            for key in ["name", "type", "id", "updated_at", "created_at", "aliases"] {
                assert!(
                    !p.properties.iter().any(|(k, _)| k == key),
                    "{strategy:?} merged {key}"
                );
            }
        }
    }

    // ----- aliases and merged ids ----------------------------------------------

    #[test]
    fn dropped_names_become_aliases_once() {
        let keep = node(
            1,
            &[
                ("name", s("Пушкин")),
                ("aliases", Value::List(vec![s("Pushkin")])),
                ("merged_ids", Value::List(vec![Value::Integer(40)])),
            ],
        );
        let drops = [
            node(2, &[("name", s("А. С. Пушкин"))]),
            node(
                3,
                &[
                    ("name", s("Pushkin")),
                    ("aliases", Value::List(vec![s("Poushkine")])),
                    ("merged_ids", Value::List(vec![Value::Integer(41)])),
                ],
            ),
            // A drop spelled exactly like the kept node adds no alias.
            node(4, &[("name", s("Пушкин"))]),
        ];
        let alias_names = vec!["Pushkin".to_string()];
        let p = plan(MergeInput {
            alias_names: &alias_names,
            ..input(&keep, &drops, &[], &[], Strategy::KeepFirst)
        });
        assert_eq!(p.aliases, ["Pushkin", "А. С. Пушкин", "Poushkine"]);
        // "Pushkin" already has an :Alias node.
        assert_eq!(p.new_aliases, ["А. С. Пушкин"]);
        assert_eq!(p.merged_ids, [40, 2, 3, 41, 4]);
    }

    #[test]
    fn a_merge_without_names_records_only_ids() {
        let keep = node(1, &[]);
        let drops = [node(2, &[])];
        let p = plan(input(&keep, &drops, &[], &[], Strategy::KeepFirst));
        assert!(p.aliases.is_empty());
        assert!(p.new_aliases.is_empty());
        assert_eq!(p.merged_ids, [2]);
    }

    #[test]
    fn keep_first_is_the_default_strategy() {
        assert_eq!(Strategy::default(), Strategy::KeepFirst);
    }
}
