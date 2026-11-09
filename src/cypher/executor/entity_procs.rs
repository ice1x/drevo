//! The `drevo.entity.*` procedures and the resolution step of
//! `drevo.memory.rememberEntity`, over the statement's view of the graph.
//! The pure parts (normalising, blocking, scoring, planning a merge) live in
//! [`crate::cypher::entity_resolution`]; this module reads the nodes, hands
//! them over and writes the result through the regular write path.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use super::{
    deadline, edge_to_value, memory_refusal, node_labels_from_storage, node_to_value, Bindings,
    CallClause, ExecError, ExecResultT, Executor, ModelDirection, NodeValue, Rows, Value,
};
use crate::cypher::entity_resolution::blocking::{candidate_pairs, Item};
use crate::cypher::entity_resolution::config::{
    drop_ids, keep_id, CandidatesConfig, MergeConfig, Resolve,
};
use crate::cypher::entity_resolution::merge::{self, MergeEdge, MergeNode, POSSIBLY_SAME};
use crate::cypher::entity_resolution::normalise::normalise;
use crate::cypher::entity_resolution::score;
use crate::cypher::provenance::describe;

/// `CALL drevo.entity.candidates(label[, config])`.
pub(super) const CANDIDATES: &str = "drevo.entity.candidates";
/// `CALL drevo.entity.merge(keepId, dropIds[, config])`.
pub(super) const MERGE: &str = "drevo.entity.merge";
/// `CALL drevo.entity.aliases(name)`.
pub(super) const ALIASES: &str = "drevo.entity.aliases";

/// The label of the nodes recording an old spelling of a merged node.
const ALIAS_LABEL: &str = "Alias";
/// The relationship from an `:Alias` to the node it names.
const ALIAS_OF: &str = "ALIAS_OF";

/// How far below the threshold a fuzzy `rememberEntity` still records a
/// `POSSIBLY_SAME` near-miss.
const NEAR_MISS_BAND: f64 = 0.1;
/// The most near-misses one fuzzy `rememberEntity` records.
const MAX_NEAR_MISSES: usize = 5;

/// What a fuzzy `rememberEntity` found.
pub(super) enum FuzzyMatch {
    /// The one entity of the type scoring at least the threshold.
    Reuse(Arc<NodeValue>),
    /// Entities to record as `POSSIBLY_SAME` with their score: every one at
    /// or above the threshold when several are, else the closest ones below.
    NearMisses(Vec<(Arc<NodeValue>, f64)>),
}

/// A node about to be compared: its value, compared name, group and
/// embedding.
struct Compared {
    node: Arc<NodeValue>,
    form: String,
    embedding: Option<Vec<f32>>,
}

/// The group key of a type value: pairs never cross groups.
fn group_key(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "null".to_string(),
        Some(Value::String(s)) => format!("s:{s}"),
        Some(other) => format!("{other:?}"),
    }
}

/// A type value as quoted in an error.
fn describe_type(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "null".to_string(),
        Some(v) => describe(v),
    }
}

/// An embedding property as floats; `None` unless it is a list of numbers.
fn embedding(value: Option<&Value>) -> Option<Vec<f32>> {
    let Some(Value::List(items)) = value else {
        return None;
    };
    items
        .iter()
        .map(|v| v.as_number().map(|n| n as f32))
        .collect()
}

fn int(n: usize) -> Value {
    Value::Integer(i64::try_from(n).unwrap_or(i64::MAX))
}

impl Executor<'_> {
    /// `CALL drevo.entity.candidates(label[, config]) YIELD a, b, nameScore,
    /// vectorScore, score, reasons` — once per input row.
    pub(super) fn run_entity_candidates(
        &mut self,
        c: &CallClause,
        columns: &[&str],
    ) -> ExecResultT<()> {
        self.run_per_row(c, columns, &mut |exec, row| {
            let label = exec.eval(&c.args[0], row)?;
            let config = match c.args.get(1) {
                Some(arg) => exec.eval(arg, row)?,
                None => Value::Null,
            };
            exec.entity_candidate_rows(&label, &config, c)
        })
    }

    /// The candidate pairs of the nodes labelled `label`, best first.
    fn entity_candidate_rows(
        &self,
        label: &Value,
        config: &Value,
        c: &CallClause,
    ) -> ExecResultT<Rows> {
        let refuse = |m: String| memory_refusal(CANDIDATES, m, c.span);
        let Value::String(label) = label else {
            return Err(refuse(format!(
                "label must be a string, got {}",
                describe(label)
            )));
        };
        let config = CandidatesConfig::from_value(config).map_err(refuse)?;
        let mut compared: Vec<(String, Compared)> = Vec::new();
        for node in self.engine().all_nodes()? {
            deadline::tick()?;
            if !node_labels_from_storage(&node).iter().any(|l| l == label) {
                continue;
            }
            let Some(nv) = self.seen_node(&node) else {
                continue;
            };
            let Some(Value::String(name)) = nv.properties.get(&config.name_property) else {
                continue;
            };
            let form = if config.normalise {
                normalise(name)
            } else {
                name.clone()
            };
            let group = group_key(nv.properties.get(&config.type_property));
            let embedding = config
                .embedding_property
                .as_ref()
                .and_then(|p| embedding(nv.properties.get(p)));
            compared.push((
                group,
                Compared {
                    node: nv,
                    form,
                    embedding,
                },
            ));
        }
        compared.sort_by_key(|(_, n)| n.node.id);
        let items: Vec<Item> = compared
            .iter()
            .map(|(group, n)| Item {
                group: group.clone(),
                name: n.form.clone(),
            })
            .collect();
        let pairs = candidate_pairs(&items, config.blocking, config.max_pairs).map_err(refuse)?;
        let mut found: Vec<(f64, u64, u64, Vec<Value>)> = Vec::new();
        for (i, j) in pairs {
            deadline::tick()?;
            let (a, b) = (&compared[i].1, &compared[j].1);
            let name_score = score::name_score(&a.form, &b.form);
            let vector_score = match (&a.embedding, &b.embedding) {
                (Some(x), Some(y)) => score::vector_score(x, y),
                _ => None,
            };
            let Some(total) = score::combine(name_score, vector_score, config.weights) else {
                continue;
            };
            if !score::passes(total, config.threshold) {
                continue;
            }
            let shared = score::shared_tokens(&a.form, &b.form);
            let reasons = score::reasons(name_score, vector_score, &shared)
                .into_iter()
                .map(Value::String)
                .collect();
            found.push((
                total,
                a.node.id,
                b.node.id,
                vec![
                    Value::Node(Arc::clone(&a.node)),
                    Value::Node(Arc::clone(&b.node)),
                    Value::Float(name_score),
                    vector_score.map_or(Value::Null, Value::Float),
                    Value::Float(total),
                    Value::List(reasons),
                ],
            ));
        }
        found.sort_by(|x, y| y.0.total_cmp(&x.0).then(x.1.cmp(&y.1)).then(x.2.cmp(&y.2)));
        Ok(found.into_iter().map(|(_, _, _, row)| row).collect())
    }

    /// `CALL drevo.entity.aliases(name) YIELD entity, aliases` — once per
    /// input row.
    pub(super) fn run_entity_aliases(
        &mut self,
        c: &CallClause,
        columns: &[&str],
    ) -> ExecResultT<()> {
        self.run_per_row(c, columns, &mut |exec, row| {
            let name = exec.eval(&c.args[0], row)?;
            exec.entity_alias_rows(&name, c)
        })
    }

    /// The nodes called `name` (compared normalised) or named by an `:Alias`
    /// of that name, with their aliases, by id.
    fn entity_alias_rows(&self, name: &Value, c: &CallClause) -> ExecResultT<Rows> {
        let Value::String(name) = name else {
            return Err(memory_refusal(
                ALIASES,
                format!("name must be a string, got {}", describe(name)),
                c.span,
            ));
        };
        let wanted = normalise(name);
        let mut targets: BTreeSet<u64> = BTreeSet::new();
        for node in self.engine().all_nodes()? {
            deadline::tick()?;
            let labels = node_labels_from_storage(&node);
            let nv = node_to_value(&node);
            if labels.iter().any(|l| l == ALIAS_LABEL) {
                if matches!(nv.properties.get("normalised"), Some(Value::String(n)) if *n == wanted)
                {
                    for edge in self.engine().edges_of(node.id, ModelDirection::Outgoing)? {
                        if edge.kind == ALIAS_OF {
                            targets.insert(edge.to_id);
                        }
                    }
                }
            } else if matches!(nv.properties.get("name"), Some(Value::String(n)) if normalise(n) == wanted)
            {
                targets.insert(node.id);
            }
        }
        let mut rows = Vec::with_capacity(targets.len());
        for id in targets {
            let Some(node) = self.engine().get_node(id)? else {
                continue;
            };
            let Some(nv) = self.seen_node(&node) else {
                continue;
            };
            let aliases = self.entity_aliases_of(&nv)?;
            rows.push(vec![
                Value::Node(nv),
                Value::List(aliases.into_iter().map(Value::String).collect()),
            ]);
        }
        Ok(rows)
    }

    /// The aliases of `node`: its `aliases` list, then the names of the
    /// `:Alias` nodes pointing at it that the list lacks.
    fn entity_aliases_of(&self, node: &NodeValue) -> ExecResultT<Vec<String>> {
        let mut aliases: Vec<String> = match node.properties.get("aliases") {
            Some(Value::List(items)) => items
                .iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        for name in self.entity_alias_names(node.id)? {
            if !aliases.contains(&name) {
                aliases.push(name);
            }
        }
        Ok(aliases)
    }

    /// The names of the `:Alias` nodes with an `ALIAS_OF` to node `id`, by
    /// alias id.
    fn entity_alias_names(&self, id: u64) -> ExecResultT<Vec<String>> {
        let mut edges = self.engine().edges_of(id, ModelDirection::Incoming)?;
        edges.retain(|e| e.kind == ALIAS_OF);
        edges.sort_by_key(|e| e.from_id);
        let mut names = Vec::new();
        for edge in edges {
            let Some(alias) = self.engine().get_node(edge.from_id)? else {
                continue;
            };
            if !node_labels_from_storage(&alias)
                .iter()
                .any(|l| l == ALIAS_LABEL)
            {
                continue;
            }
            if let Some(Value::String(name)) = node_to_value(&alias).properties.get("name") {
                names.push(name.clone());
            }
        }
        Ok(names)
    }

    /// `CALL drevo.entity.merge(keepId, dropIds[, config]) YIELD kept,
    /// merged, relationshipsMoved, propertiesMerged` — once per input row.
    pub(super) fn run_entity_merge(&mut self, c: &CallClause, columns: &[&str]) -> ExecResultT<()> {
        self.run_per_row_mut(c, columns, &mut |exec, row| {
            let keep = exec.eval(&c.args[0], row)?;
            let drops = exec.eval(&c.args[1], row)?;
            let config = match c.args.get(2) {
                Some(arg) => exec.eval(arg, row)?,
                None => Value::Null,
            };
            exec.entity_merge(&keep, &drops, &config, c)
        })
    }

    /// The node with `id` as the signed-in user sees it, or an error naming
    /// the id.
    fn entity_merge_node(&self, id: u64, c: &CallClause) -> ExecResultT<Arc<NodeValue>> {
        self.engine()
            .get_node(id)?
            .and_then(|n| self.seen_node(&n))
            .ok_or_else(|| memory_refusal(MERGE, format!("no node with id {id}"), c.span))
    }

    /// The relationships of `id`, either direction, as merge input.
    fn entity_merge_edges(&self, id: u64, into: &mut BTreeMap<u64, MergeEdge>) -> ExecResultT<()> {
        for edge in self.engine().edges_of(id, ModelDirection::Both)? {
            let rv = edge_to_value(&edge);
            into.entry(rv.id).or_insert_with(|| MergeEdge {
                id: rv.id,
                kind: rv.kind.clone(),
                from: rv.from_id,
                to: rv.to_id,
                properties: rv.properties.clone(),
            });
        }
        Ok(())
    }

    /// Fold `drops` into `keep`; one `[kept, merged, relationshipsMoved,
    /// propertiesMerged]` row.
    fn entity_merge(
        &mut self,
        keep: &Value,
        drops: &Value,
        config: &Value,
        c: &CallClause,
    ) -> ExecResultT<Rows> {
        let refuse = |m: String| memory_refusal(MERGE, m, c.span);
        let keep_id = keep_id(keep).map_err(refuse)?;
        let drop_ids = drop_ids(drops).map_err(refuse)?;
        let config = MergeConfig::from_value(config).map_err(refuse)?;
        let mut unique: Vec<u64> = Vec::with_capacity(drop_ids.len());
        for id in drop_ids {
            if id == keep_id {
                return Err(refuse(format!("cannot merge node {keep_id} into itself")));
            }
            if !unique.contains(&id) {
                unique.push(id);
            }
        }
        let keep = self.entity_merge_node(keep_id, c)?;
        let mut drops = Vec::with_capacity(unique.len());
        for id in unique {
            drops.push(self.entity_merge_node(id, c)?);
        }
        let type_property = config.type_property();
        let keep_type = keep.properties.get(type_property);
        if !config.force {
            for drop in &drops {
                let drop_type = drop.properties.get(type_property);
                if drop_type.unwrap_or(&Value::Null) != keep_type.unwrap_or(&Value::Null) {
                    return Err(refuse(format!(
                        "cannot merge nodes of different types: node {} has type {}, \
                         node {} has type {}; pass {{force: true}} to merge anyway",
                        keep.id,
                        describe_type(keep_type),
                        drop.id,
                        describe_type(drop_type)
                    )));
                }
            }
        }
        if drops.is_empty() {
            return Ok(vec![vec![Value::Node(keep), int(0), int(0), int(0)]]);
        }
        for drop in &drops {
            self.guard_node(drevo_auth::privileges::Action::Delete, &drop.labels, None)?;
        }

        let mut keep_edges = BTreeMap::new();
        self.entity_merge_edges(keep.id, &mut keep_edges)?;
        let mut drop_edges = BTreeMap::new();
        for drop in &drops {
            self.entity_merge_edges(drop.id, &mut drop_edges)?;
        }
        let mut alias_names = self.entity_alias_names(keep.id)?;
        for drop in &drops {
            alias_names.extend(self.entity_alias_names(drop.id)?);
        }
        let keep_input = MergeNode {
            id: keep.id,
            properties: keep.properties.clone(),
        };
        let drop_input: Vec<MergeNode> = drops
            .iter()
            .map(|d| MergeNode {
                id: d.id,
                properties: d.properties.clone(),
            })
            .collect();
        let keep_edges: Vec<MergeEdge> = keep_edges.into_values().collect();
        let drop_edge_list: Vec<MergeEdge> = drop_edges.values().cloned().collect();
        let plan = merge::plan(merge::MergeInput {
            keep: &keep_input,
            drops: &drop_input,
            keep_edges: &keep_edges,
            drop_edges: &drop_edge_list,
            alias_names: &alias_names,
            strategy: config.strategy,
            name_property: config.name_property(),
            type_property,
        });

        // Re-create the relationships on the kept node while every endpoint
        // still exists, then delete the dropped nodes with their own.
        let mut endpoints: HashMap<u64, Arc<NodeValue>> = HashMap::new();
        endpoints.insert(keep.id, Arc::clone(&keep));
        for edge in &plan.edges {
            for id in [edge.from, edge.to] {
                if let std::collections::hash_map::Entry::Vacant(slot) = endpoints.entry(id) {
                    let node = self.engine().get_node(id)?.ok_or_else(|| {
                        ExecError::InvalidMutation(format!("node {id} not found"))
                    })?;
                    slot.insert(node_to_value(&node));
                }
            }
        }
        for edge in &plan.edges {
            let (Some(from), Some(to)) = (endpoints.get(&edge.from), endpoints.get(&edge.to))
            else {
                continue;
            };
            let (from, to) = (Arc::clone(from), Arc::clone(to));
            let props: Vec<(&str, Value)> = edge
                .properties
                .iter()
                .map(|(k, v)| (k.as_str(), v.clone()))
                .collect();
            self.memory_link(&from, &edge.kind, &to, props, c.span)?;
        }
        for drop in &drops {
            if self.engine().get_node(drop.id)?.is_some() {
                self.engine().delete_node(drop.id)?;
                self.stats.nodes_deleted += 1;
            }
        }
        self.stats.relationships_deleted += drop_edges.len();

        let var = "__drevo_entity_kept";
        let mut row = Bindings::new();
        let mut kept = keep;
        let mut writes: Vec<(String, Value)> = plan.properties.clone();
        if !plan.aliases.is_empty() {
            writes.push((
                "aliases".to_string(),
                Value::List(plan.aliases.iter().cloned().map(Value::String).collect()),
            ));
        }
        writes.push((
            "merged_ids".to_string(),
            Value::List(
                plan.merged_ids
                    .iter()
                    .copied()
                    .map(Value::Integer)
                    .collect(),
            ),
        ));
        for (key, value) in writes {
            self.write_node_property(&kept, &key, value, &mut row, var)?;
            self.stats.properties_set += 1;
            if let Some(Value::Node(updated)) = row.remove(var) {
                kept = updated;
            }
        }
        for name in &plan.new_aliases {
            let alias = self.memory_create_node(
                &[ALIAS_LABEL],
                vec![
                    ("name", Value::String(name.clone())),
                    ("normalised", Value::String(normalise(name))),
                    ("created_at", Self::memory_now(c.span)?),
                ],
                c.span,
            )?;
            self.memory_link(&alias, ALIAS_OF, &kept, Vec::new(), c.span)?;
        }
        if let Some(node) = self.engine().get_node(kept.id)? {
            kept = node_to_value(&node);
        }
        Ok(vec![vec![
            Value::Node(kept),
            int(drops.len()),
            int(plan.edges.len()),
            int(plan.properties.len()),
        ]])
    }

    /// The `:Entity` of `entity_type` an `:Alias` called `name` (compared
    /// normalised) points at; the lowest id when several do.
    pub(super) fn entity_by_alias(
        &self,
        name: &str,
        entity_type: &str,
    ) -> ExecResultT<Option<Arc<NodeValue>>> {
        let wanted = normalise(name);
        let mut found: Option<Arc<NodeValue>> = None;
        for node in self.engine().all_nodes()? {
            if !node_labels_from_storage(&node)
                .iter()
                .any(|l| l == ALIAS_LABEL)
            {
                continue;
            }
            let nv = node_to_value(&node);
            if !matches!(nv.properties.get("normalised"), Some(Value::String(n)) if *n == wanted) {
                continue;
            }
            for edge in self.engine().edges_of(node.id, ModelDirection::Outgoing)? {
                if edge.kind != ALIAS_OF {
                    continue;
                }
                let Some(target) = self.engine().get_node(edge.to_id)? else {
                    continue;
                };
                let tv = node_to_value(&target);
                let is_entity = tv.labels.iter().any(|l| l == "Entity");
                let same_type =
                    matches!(tv.properties.get("type"), Some(Value::String(t)) if t == entity_type);
                if is_entity && same_type && found.as_ref().is_none_or(|f| tv.id < f.id) {
                    found = Some(tv);
                }
            }
        }
        Ok(found)
    }

    /// Score `name` against every `:Entity` of `entity_type`: the one at or
    /// above `threshold` is reused; several there, or the closest within
    /// [`NEAR_MISS_BAND`] below it, are near-misses.
    pub(super) fn entity_fuzzy_match(
        &self,
        name: &str,
        entity_type: &str,
        threshold: f64,
    ) -> ExecResultT<FuzzyMatch> {
        let form = normalise(name);
        let mut scored: Vec<(f64, Arc<NodeValue>)> = Vec::new();
        for node in self.engine().all_nodes()? {
            deadline::tick()?;
            if !node_labels_from_storage(&node)
                .iter()
                .any(|l| l == "Entity")
            {
                continue;
            }
            let nv = node_to_value(&node);
            if !matches!(nv.properties.get("type"), Some(Value::String(t)) if t == entity_type) {
                continue;
            }
            let Some(Value::String(other)) = nv.properties.get("name") else {
                continue;
            };
            let s = score::name_score(&form, &normalise(other));
            if s >= threshold - NEAR_MISS_BAND {
                scored.push((s, nv));
            }
        }
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.id.cmp(&b.1.id)));
        let above = scored
            .iter()
            .filter(|(s, _)| score::passes(*s, threshold))
            .count();
        if above == 1 {
            return Ok(FuzzyMatch::Reuse(Arc::clone(&scored[0].1)));
        }
        let keep = if above > 1 { above } else { MAX_NEAR_MISSES };
        Ok(FuzzyMatch::NearMisses(
            scored
                .into_iter()
                .take(keep)
                .map(|(s, nv)| (nv, s))
                .collect(),
        ))
    }

    /// Record `entity` as possibly the same as each near-miss.
    pub(super) fn entity_record_near_misses(
        &mut self,
        entity: &Arc<NodeValue>,
        near_misses: Vec<(Arc<NodeValue>, f64)>,
        span: crate::cypher::lexer::Span,
    ) -> ExecResultT<()> {
        for (other, s) in near_misses {
            self.memory_link(
                entity,
                POSSIBLY_SAME,
                &other,
                vec![("score", Value::Float(s))],
                span,
            )?;
        }
        Ok(())
    }

    /// Whether `resolve` looks further than an exact name.
    pub(super) fn entity_resolves(resolve: Resolve) -> bool {
        resolve != Resolve::Exact
    }
}
