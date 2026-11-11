//! The `drevo.community.*` procedures and the global scope of the graph-RAG
//! pipeline, over the statement's view of the graph. The pure parts (config
//! maps, partition, ranking, weights, summarisers) live in
//! [`crate::community`]; this module reads the nodes, hands them over and
//! writes the result through the regular write path.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use super::{
    deadline, edge_to_value, memory_refusal, node_labels_from_storage, node_to_value,
    value_to_json, CallClause, ExecResultT, Executor, HybridItems, ModelDirection, NodeValue, Rows,
    Value,
};
use crate::community::extractive::ExtractiveSummarizer;
use crate::community::llm::{
    finish_plan, in_job, is_planning, is_prepared, max_llm_sync, LlmSummarizer,
};
use crate::community::{
    community_id, community_keywords, current_members, effective_mode, heaviest_rel_types,
    member_ids_hash, member_name, member_ranks, partition, related_weights, CommunitySummarizer,
    Member, QueryConfig, SummarizeConfig, SummaryInput, SummaryMode, TrigramCorpus,
    COMMUNITY_LABEL, DEFAULT_EMBEDDING_PROPERTY, DEFAULT_LAYER, KEYWORDS_PER_COMMUNITY, MEMBER_OF,
    RELATED_COMMUNITY, REL_TYPES_PER_SUMMARY,
};
use crate::cypher::hybrid::{parse_k, HybridConfig, HybridMode};
use crate::cypher::lexer::Span;
use crate::cypher::provenance::describe;
use crate::rag::config::{global_hint_matches, RagConfig, Scope};
use crate::rag::serialize::DEFAULT_TEXT_PROPERTIES;
use crate::rag::{source_admits, RankedNode, Subgraph};

/// `CALL drevo.community.summarize([config])`.
pub(super) const SUMMARIZE: &str = "drevo.community.summarize";
/// `CALL drevo.community.query(query, k[, config])`.
pub(super) const QUERY: &str = "drevo.community.query";
/// `CALL drevo.community.of(node)`.
pub(super) const OF: &str = "drevo.community.of";
/// `CALL drevo.community.stale()`.
pub(super) const STALE: &str = "drevo.community.stale";

/// The capability `mode: 'llm'` needs: the chat model `drevo.extract.*`
/// uses, which `drevo.extract.status()` reports.
const LLM_SUMMARIES: &str = "LLM community summaries (no chat upstream is configured: set \
     DREVO_EXTRACT_UPSTREAM or DREVO_TEXT2CYPHER_UPSTREAM; see CALL drevo.extract.status())";

/// How a planning run of `drevo.community.summarize` ends: the statement
/// stops there, nothing written ([`crate::community::llm::planning`]).
const PLANNED: &str = "planned before the write";

/// Why `mode: 'llm'` is refused where the model cannot be asked before the
/// write.
const LLM_NEEDS_OPENING_CALL: &str = "mode: 'llm' asks the model before the write begins, so \
     the statement must open with CALL drevo.community.summarize and run in auto-commit; \
     pass async: true to run it as a background job, or use mode: 'extractive'";

/// The span of the `drevo.community.summarize` call `query` opens with: one
/// part (no `UNION`) whose first clause is the call, so its model calls can
/// be made before its write. `None` for any other statement.
pub(crate) fn opening_community_summarize(query: &crate::cypher::ast::Query) -> Option<Span> {
    let [part] = query.parts.as_slice() else {
        return None;
    };
    match part.query.clauses.first() {
        Some(crate::cypher::ast::Clause::Call(c)) if c.name.join(".") == SUMMARIZE => Some(c.span),
        _ => None,
    }
}

/// Why `async: true` is refused to a user who may not read everything.
const ASYNC_NEEDS_FULL_READ: &str = "async: true summarises the whole graph in a background job, \
     so it needs a user who may read everything; leave async out to summarise what you may see";

/// Communities found by a search, best first, with their fused score.
type CommunityHits = Vec<(Arc<NodeValue>, f64)>;

/// A community about to be written.
struct Planned {
    members: Vec<(u64, f64)>,
    summary: Option<String>,
    keywords: Vec<String>,
}

fn int(n: usize) -> Value {
    Value::Integer(i64::try_from(n).unwrap_or(i64::MAX))
}

fn text(s: &str) -> Value {
    Value::String(s.to_string())
}

/// The `layer` property of a community or `MEMBER_OF` relationship.
fn layer_of(props: &BTreeMap<String, Value>) -> Option<&str> {
    match props.get("layer") {
        Some(Value::String(layer)) => Some(layer),
        _ => None,
    }
}

/// The text values of `node` keywords are drawn from.
fn texts_of(node: &NodeValue) -> impl Iterator<Item = String> + '_ {
    DEFAULT_TEXT_PROPERTIES
        .iter()
        .filter_map(|key| match node.properties.get(*key) {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            _ => None,
        })
}

impl Executor<'_> {
    /// `CALL drevo.community.summarize([config]) YIELD community, size,
    /// summary, keywords, mode` — once per input row.
    pub(super) fn run_community_summarize(
        &mut self,
        c: &CallClause,
        columns: &[&str],
    ) -> ExecResultT<()> {
        self.run_per_row_mut(c, columns, &mut |exec, row| {
            let config = match c.args.first() {
                Some(arg) => exec.eval(arg, row)?,
                None => Value::Null,
            };
            exec.community_summarize(&config, c.span)
        })
    }

    /// `CALL drevo.community.query(query, k[, config]) YIELD community,
    /// score, summary` — once per input row.
    pub(super) fn run_community_query(
        &mut self,
        c: &CallClause,
        columns: &[&str],
    ) -> ExecResultT<()> {
        self.run_per_row(c, columns, &mut |exec, row| {
            let query = exec.eval(&c.args[0], row)?;
            let k = exec.eval(&c.args[1], row)?;
            let config = match c.args.get(2) {
                Some(arg) => exec.eval(arg, row)?,
                None => Value::Null,
            };
            exec.community_query_rows(&query, &k, &config, c.span)
        })
    }

    /// `CALL drevo.community.of(node) YIELD community, level` — once per
    /// input row.
    pub(super) fn run_community_of(&mut self, c: &CallClause, columns: &[&str]) -> ExecResultT<()> {
        self.run_per_row(c, columns, &mut |exec, row| {
            let node = exec.eval(&c.args[0], row)?;
            exec.community_of_rows(&node, c.span)
        })
    }

    /// `CALL drevo.community.stale() YIELD community, size, changed` — once
    /// per input row.
    pub(super) fn run_community_stale(
        &mut self,
        c: &CallClause,
        columns: &[&str],
    ) -> ExecResultT<()> {
        self.run_per_row(c, columns, &mut |exec, _| exec.community_stale_rows())
    }

    /// The summariser of `mode: 'llm'`: the chat model `drevo.extract.*`
    /// uses, asked for `model` instead of its own when given. `None` when no
    /// chat model is configured, which makes `'auto'` write extractive
    /// summaries and `'llm'` fail.
    fn community_llm_summarizer(model: Option<&str>) -> Option<Box<dyn CommunitySummarizer>> {
        let completer = crate::extract::installed()?;
        Some(Box::new(LlmSummarizer::new(
            completer,
            model.map(str::to_string),
        )))
    }

    /// `drevo.community.summarize({async: true, …})`: check the call as the
    /// synchronous one would be checked, then store a job of the
    /// `community` pipeline that runs it with the config less `async`. One
    /// row: every summary column `null`, `jobId` the job's id.
    fn community_submit(
        &mut self,
        raw: &Value,
        config: &SummarizeConfig,
        span: Span,
    ) -> ExecResultT<Rows> {
        let refuse = |m: String| memory_refusal(SUMMARIZE, m, span);
        if self.read_filter.is_some() {
            return Err(refuse(ASYNC_NEEDS_FULL_READ.to_string()));
        }
        let community = [COMMUNITY_LABEL.to_string()];
        self.guard_node(drevo_auth::privileges::Action::Create, &community, None)?;
        for kind in [MEMBER_OF, RELATED_COMMUNITY] {
            self.guard_relationship(drevo_auth::privileges::Action::Create, kind, None)?;
        }
        let existing = self.community_nodes(Some(&config.layer))?;
        if !existing.is_empty() {
            if !config.replace {
                return Err(refuse(Self::layer_taken(config, existing.len())));
            }
            self.guard_node(drevo_auth::privileges::Action::Delete, &community, None)?;
        }
        let Value::Map(map) = raw else {
            // `null`: the defaults.
            let id = self.ingest_store_community_job("{}".to_string(), span)?;
            return Ok(vec![Self::job_row(id)]);
        };
        let mut map = map.clone();
        map.remove("async");
        let json = value_to_json(&Value::Map(map))
            .ok_or_else(|| refuse("the config must hold plain values".to_string()))?;
        let id = self.ingest_store_community_job(json.to_string(), span)?;
        Ok(vec![Self::job_row(id)])
    }

    /// The row of a submitted community job.
    fn job_row(id: String) -> Vec<Value> {
        vec![
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::String(id),
        ]
    }

    /// Why `replace: false` refuses a layer that already has `count`
    /// communities.
    fn layer_taken(config: &SummarizeConfig, count: usize) -> String {
        format!(
            "layer '{}' already has {count} communities; pass replace: true to recompute it, \
             or name another layer",
            config.layer
        )
    }

    /// Whether `layer` holds a `:Community` the caller may see — whether the
    /// global scope of `drevo.rag.context` has anything to retrieve there.
    pub(super) fn has_visible_community(&self, layer: &str) -> ExecResultT<bool> {
        Ok(self
            .community_nodes(Some(layer))?
            .into_iter()
            .any(|c| self.visible_node(c).is_some()))
    }

    /// Every `:Community` of `layer` (of every layer when `None`), as
    /// stored, by id.
    fn community_nodes(&self, layer: Option<&str>) -> ExecResultT<Vec<Arc<NodeValue>>> {
        let mut out = Vec::new();
        for node in self.engine().all_nodes()? {
            deadline::tick()?;
            if !node_labels_from_storage(&node)
                .iter()
                .any(|l| l == COMMUNITY_LABEL)
            {
                continue;
            }
            let nv = node_to_value(&node);
            if layer.is_none_or(|wanted| layer_of(&nv.properties) == Some(wanted)) {
                out.push(nv);
            }
        }
        out.sort_by_key(|n| n.id);
        Ok(out)
    }

    /// Partition the graph, summarise each community and write the layer;
    /// one row per community, largest first.
    fn community_summarize(&mut self, config: &Value, span: Span) -> ExecResultT<Rows> {
        let refuse = |m: String| memory_refusal(SUMMARIZE, m, span);
        let raw = config;
        let config = SummarizeConfig::from_value(raw).map_err(refuse)?;
        let llm = Self::community_llm_summarizer(config.model.as_deref());
        let mode = effective_mode(config.mode, llm.is_some())
            .map_err(|()| Self::engine_capability(LLM_SUMMARIES))?;
        if config.run_async {
            if is_planning() {
                finish_plan(false);
                return Err(refuse(PLANNED.to_string()));
            }
            return self.community_submit(raw, &config, span);
        }
        let existing = self.community_nodes(Some(&config.layer))?;
        if !config.replace && !existing.is_empty() {
            return Err(refuse(Self::layer_taken(&config, existing.len())));
        }

        // The graph to partition, as the user may see it.
        let mut nodes: BTreeMap<u64, Arc<NodeValue>> = BTreeMap::new();
        for node in self.engine().all_nodes()? {
            deadline::tick()?;
            if !config.admits_labels(&node_labels_from_storage(&node)) {
                continue;
            }
            if let Some(nv) = self.seen_node(&node) {
                nodes.insert(nv.id, nv);
            }
        }
        let mut edges: Vec<(u64, u64, f32)> = Vec::new();
        let mut kinds: Vec<String> = Vec::new();
        for edge in self.engine().all_edges()? {
            deadline::tick()?;
            if nodes.contains_key(&edge.from_id)
                && nodes.contains_key(&edge.to_id)
                && config.admits_rel_type(&edge.kind)
                && self.edge_traversable(&edge)
            {
                edges.push((edge.from_id, edge.to_id, edge.weight));
                kinds.push(edge.kind.clone());
            }
        }
        let found = partition(
            nodes.keys().copied().collect(),
            &edges,
            config.algorithm,
            config.resolution,
        )
        .map_err(refuse)?;
        deadline::check()?;

        // A synchronous call asks the model for at most so many summaries:
        // more fail with `'llm'`, and `'auto'` writes extractive ones.
        let summarised = found
            .groups
            .iter()
            .filter(|g| g.len() >= config.min_size)
            .count();
        // The model is asked only between a plan and the write (see
        // `crate::community::llm`); elsewhere `'llm'` fails and `'auto'`
        // writes extractive summaries.
        let cap = max_llm_sync();
        let within = summarised <= cap || in_job();
        let staged = is_planning() || is_prepared();
        let summarizer: Box<dyn CommunitySummarizer> = match llm {
            Some(llm) if mode == SummaryMode::Llm && within && staged => llm,
            Some(_) if config.mode == SummaryMode::Llm && !within => {
                return Err(refuse(format!(
                    "mode: 'llm' would summarise {summarised} communities, more than the {cap} \
                     one synchronous call may (DREVO_COMMUNITY_MAX_LLM_SYNC); pass async: true \
                     to summarise them in a background job, or use mode: 'extractive'"
                )))
            }
            Some(_) if config.mode == SummaryMode::Llm => {
                return Err(refuse(LLM_NEEDS_OPENING_CALL.to_string()))
            }
            _ => Box::new(ExtractiveSummarizer),
        };
        if is_planning() && summarizer.mode() != SummaryMode::Llm {
            finish_plan(false);
            return Err(refuse(PLANNED.to_string()));
        }

        // Rank and summarise each community.
        let group_of: HashMap<u64, usize> = found
            .groups
            .iter()
            .enumerate()
            .flat_map(|(i, members)| members.iter().map(move |&id| (id, i)))
            .collect();
        let mut inner_kinds: Vec<Vec<&str>> = vec![Vec::new(); found.groups.len()];
        for ((from, to, _), kind) in edges.iter().zip(&kinds) {
            if let (Some(a), Some(b)) = (group_of.get(from), group_of.get(to)) {
                if a == b {
                    inner_kinds[*a].push(kind);
                }
            }
        }
        let corpus = TrigramCorpus::new(
            nodes
                .values()
                .map(|n| texts_of(n).collect::<Vec<_>>().join(" ")),
        );
        let mut planned = Vec::with_capacity(found.groups.len());
        for (group, kinds) in found.groups.iter().zip(&inner_kinds) {
            let ranks = member_ranks(group, &edges);
            let (summary, keywords) = if group.len() >= config.min_size {
                let texts: Vec<String> = group
                    .iter()
                    .filter_map(|id| nodes.get(id))
                    .flat_map(|n| texts_of(n))
                    .collect();
                let keywords =
                    community_keywords(&texts, KEYWORDS_PER_COMMUNITY, corpus.doc_count(), &|t| {
                        Ok(corpus.df(t))
                    })?;
                let rel_types = heaviest_rel_types(kinds.iter().copied(), REL_TYPES_PER_SUMMARY);
                let members: Vec<Member> = ranks
                    .iter()
                    .filter_map(|(id, rank)| {
                        let node = nodes.get(id)?;
                        Some(Member {
                            name: member_name(node),
                            label: node
                                .labels
                                .first()
                                .cloned()
                                .unwrap_or_else(|| "Node".to_string()),
                            rank: *rank,
                        })
                    })
                    .collect();
                let ids: Vec<u64> = ranks.iter().map(|(id, _)| *id).collect();
                let hash = member_ids_hash(&ids);
                let summary = summarizer
                    .summarize(&SummaryInput {
                        size: group.len(),
                        member_ids_hash: &hash,
                        members: &members,
                        keywords: &keywords,
                        rel_types: &rel_types,
                        max_members: config.max_members,
                        token_budget: config.token_budget,
                    })
                    .map_err(refuse)?;
                (Some(summary), keywords)
            } else {
                (None, Vec::new())
            };
            planned.push(Planned {
                members: ranks,
                summary,
                keywords,
            });
            deadline::check()?;
        }
        if is_planning() {
            finish_plan(true);
            return Err(refuse(PLANNED.to_string()));
        }

        // Replace the layer.
        let mut dropped_edges: BTreeSet<u64> = BTreeSet::new();
        for old in &existing {
            self.guard_node(drevo_auth::privileges::Action::Delete, &old.labels, None)?;
            for edge in self.engine().edges_of(old.id, ModelDirection::Both)? {
                dropped_edges.insert(edge.id);
            }
        }
        for old in &existing {
            self.engine().delete_node(old.id)?;
            self.stats.nodes_deleted += 1;
        }
        self.stats.relationships_deleted += dropped_edges.len();

        let computed_at = Self::memory_now(span)?;
        let mut created: Vec<Arc<NodeValue>> = Vec::with_capacity(planned.len());
        let mut rows = Vec::with_capacity(planned.len());
        for (index, plan) in planned.into_iter().enumerate() {
            let ids: Vec<u64> = plan.members.iter().map(|(id, _)| *id).collect();
            let keywords = Value::List(plan.keywords.into_iter().map(Value::String).collect());
            let mut props = vec![
                ("id", Value::String(community_id(&config.layer, index))),
                ("layer", text(&config.layer)),
                ("size", int(ids.len())),
                ("modularity", Value::Float(found.modularity)),
                ("keywords", keywords.clone()),
                ("computed_at", computed_at.clone()),
                ("member_ids_hash", Value::String(member_ids_hash(&ids))),
                ("config", Value::String(config.scope_json())),
            ];
            let mode = match &plan.summary {
                Some(summary) => {
                    props.push(("summary", text(summary)));
                    props.push(("mode", text(summarizer.mode().as_str())));
                    text(summarizer.mode().as_str())
                }
                None => Value::Null,
            };
            let community = self.memory_create_node(&[COMMUNITY_LABEL], props, span)?;
            for (id, rank) in &plan.members {
                let Some(member) = nodes.get(id).cloned() else {
                    continue;
                };
                self.memory_link(
                    &member,
                    MEMBER_OF,
                    &community,
                    vec![
                        ("layer", text(&config.layer)),
                        ("rank", Value::Float(*rank)),
                    ],
                    span,
                )?;
            }
            rows.push(vec![
                Value::Node(Arc::clone(&community)),
                int(ids.len()),
                plan.summary.map_or(Value::Null, Value::String),
                keywords,
                mode,
                Value::Null,
            ]);
            created.push(community);
        }
        for (a, b, weight) in related_weights(&found.groups, &edges) {
            let (Some(from), Some(to)) = (created.get(a).cloned(), created.get(b).cloned()) else {
                continue;
            };
            self.memory_link(
                &from,
                RELATED_COMMUNITY,
                &to,
                vec![("weight", Value::Integer(weight))],
                span,
            )?;
        }
        Ok(rows)
    }

    /// The communities of `layer` best matching `query`: BM25 over their
    /// summaries fused with the vector leg over `property` when they carry
    /// embeddings; at most `k`, best first, with their fused score. Also
    /// returns the search mode that ran.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn community_hits(
        &self,
        name: &str,
        query: &str,
        k: usize,
        layer: &str,
        property: &str,
        hybrid: &HybridConfig,
        span: Span,
    ) -> ExecResultT<(CommunityHits, HybridMode)> {
        let hybrid = HybridConfig {
            fts_filter: Some(COMMUNITY_LABEL.to_string()),
            ..hybrid.clone()
        };
        // Read past the communities of other layers.
        let fetch = hybrid.depth(k).max(k.saturating_mul(4));
        let (hits, mode) = self.hybrid_hits(
            name,
            HybridItems::Nodes,
            query,
            Some(COMMUNITY_LABEL),
            property,
            fetch,
            &hybrid,
            span,
        )?;
        let mut out = Vec::new();
        for (item, fused) in hits {
            if out.len() == k {
                break;
            }
            let Value::Node(node) = item else {
                continue;
            };
            if !node.labels.iter().any(|l| l == COMMUNITY_LABEL)
                || layer_of(&node.properties) != Some(layer)
            {
                continue;
            }
            if let Some(node) = self.visible_node(node) {
                out.push((node, fused.score));
            }
        }
        Ok((out, mode))
    }

    /// The `drevo.community.query` rows of one call.
    fn community_query_rows(
        &self,
        query: &Value,
        k: &Value,
        config: &Value,
        span: Span,
    ) -> ExecResultT<Rows> {
        let refuse = |m: String| memory_refusal(QUERY, m, span);
        let Value::String(query) = query else {
            return Err(refuse(format!(
                "query must be a string, got {}",
                query.type_name()
            )));
        };
        let k = parse_k(k).map_err(refuse)?;
        let config = QueryConfig::from_value(config).map_err(refuse)?;
        let (hits, _) = self.community_hits(
            QUERY,
            query,
            k,
            &config.layer,
            &config.embedding_property,
            &config.hybrid,
            span,
        )?;
        Ok(hits
            .into_iter()
            .map(|(node, score)| {
                let summary = node.properties.get("summary").cloned();
                vec![
                    Value::Node(node),
                    Value::Float(score),
                    summary.unwrap_or(Value::Null),
                ]
            })
            .collect())
    }

    /// The communities `node` (a node or a node id) is a member of, one row
    /// per layer, by layer; `null` gives none.
    fn community_of_rows(&self, node: &Value, span: Span) -> ExecResultT<Rows> {
        let refuse = |m: String| memory_refusal(OF, m, span);
        let id = match node {
            Value::Null => return Ok(Vec::new()),
            Value::Node(n) => n.id,
            Value::Integer(i) if *i >= 0 => i.unsigned_abs(),
            other => {
                return Err(refuse(format!(
                    "node must be a node or a node id, got {}",
                    describe(other)
                )))
            }
        };
        let seen = self.engine().get_node(id)?.and_then(|n| self.seen_node(&n));
        if seen.is_none() {
            return Err(refuse(format!("no node with id {id}")));
        }
        let mut found: Vec<(String, u64, Arc<NodeValue>)> = Vec::new();
        for edge in self.engine().edges_of(id, ModelDirection::Outgoing)? {
            if edge.kind != MEMBER_OF || !self.edge_traversable(&edge) {
                continue;
            }
            let Some(target) = self.engine().get_node(edge.to_id)? else {
                continue;
            };
            let Some(community) = self.seen_node(&target) else {
                continue;
            };
            if !community.labels.iter().any(|l| l == COMMUNITY_LABEL) {
                continue;
            }
            let rel = edge_to_value(&edge);
            let level = layer_of(&rel.properties)
                .or_else(|| layer_of(&community.properties))
                .unwrap_or(DEFAULT_LAYER)
                .to_string();
            found.push((level, community.id, community));
        }
        found.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        Ok(found
            .into_iter()
            .map(|(level, _, community)| vec![Value::Node(community), Value::String(level)])
            .collect())
    }

    /// Every community the user may see with its member count now and
    /// whether its member set changed since it was computed: a member
    /// deleted or unlinked, or a node with no community of the layer linked
    /// to members (it counts towards the community it links into most).
    fn community_stale_rows(&self) -> ExecResultT<Rows> {
        let mut by_layer: BTreeMap<String, Vec<Arc<NodeValue>>> = BTreeMap::new();
        for community in self.community_nodes(None)? {
            let Some(community) = self.visible_node(community) else {
                continue;
            };
            let layer = layer_of(&community.properties)
                .unwrap_or(DEFAULT_LAYER)
                .to_string();
            by_layer.entry(layer).or_default().push(community);
        }
        let mut rows = Vec::new();
        for (layer, communities) in by_layer {
            // The labels and relationship types the layer was computed with.
            let scope = SummarizeConfig::from_scope_json(communities.iter().find_map(|c| {
                match c.properties.get("config") {
                    Some(Value::String(json)) => Some(json.as_str()),
                    _ => None,
                }
            }));
            let mut stored: BTreeMap<u64, BTreeSet<u64>> = BTreeMap::new();
            for community in &communities {
                let members = stored.entry(community.id).or_default();
                for edge in self
                    .engine()
                    .edges_of(community.id, ModelDirection::Incoming)?
                {
                    deadline::tick()?;
                    let rel = edge_to_value(&edge);
                    if edge.kind == MEMBER_OF && layer_of(&rel.properties) == Some(&layer) {
                        members.insert(edge.from_id);
                    }
                }
            }
            let assigned: HashSet<u64> = stored.values().flatten().copied().collect();
            let mut links: Vec<(u64, u64)> = Vec::new();
            for node in self.engine().all_nodes()? {
                deadline::tick()?;
                if assigned.contains(&node.id)
                    || !scope.admits_labels(&node_labels_from_storage(&node))
                    || self.seen_node(&node).is_none()
                {
                    continue;
                }
                for edge in self.engine().edges_of(node.id, ModelDirection::Both)? {
                    if !scope.admits_rel_type(&edge.kind) || !self.edge_traversable(&edge) {
                        continue;
                    }
                    let other = if edge.from_id == node.id {
                        edge.to_id
                    } else {
                        edge.from_id
                    };
                    links.push((node.id, other));
                }
            }
            let now = current_members(&stored, &links);
            for community in communities {
                let members: Vec<u64> = now
                    .get(&community.id)
                    .map(|m| m.iter().copied().collect())
                    .unwrap_or_default();
                let changed = !matches!(
                    community.properties.get("member_ids_hash"),
                    Some(Value::String(hash)) if *hash == member_ids_hash(&members)
                );
                rows.push(vec![
                    Value::Node(community),
                    int(members.len()),
                    Value::Bool(changed),
                ]);
            }
        }
        Ok(rows)
    }

    /// The graph-RAG pipeline in the config's scope: the local pipeline
    /// ([`Self::rag_subgraph`]), the global one
    /// ([`Self::rag_global_subgraph`]), or — under `'auto'` — global when
    /// the local search finds no seed, the question matches the global hint,
    /// or both search legs ran and the best seed scores below the global
    /// threshold; local when that finds no community.
    /// Returns what [`Self::rag_subgraph`] returns and the scope that ran.
    pub(super) fn rag_scoped(
        &self,
        name: &str,
        query: &str,
        config: &RagConfig,
        span: Span,
    ) -> ExecResultT<(Subgraph, HybridMode, Option<Arc<NodeValue>>, Scope)> {
        let refuse = |m: String| memory_refusal(name, m, span);
        let global = |exec: &Self| exec.rag_global_subgraph(name, query, config, span);
        match config.scope {
            Scope::Local => {
                let (subgraph, mode, cut) = self.rag_subgraph(name, query, config, span)?;
                Ok((subgraph, mode, cut, Scope::Local))
            }
            Scope::Global => {
                let (subgraph, mode) = global(self)?;
                Ok((subgraph.unwrap_or_default(), mode, None, Scope::Global))
            }
            Scope::Auto => {
                let hinted = global_hint_matches(&config.global_hint, query).map_err(refuse)?;
                if hinted {
                    if let (Some(subgraph), mode) = global(self)? {
                        return Ok((subgraph, mode, None, Scope::Global));
                    }
                }
                let (subgraph, mode, cut) = self.rag_subgraph(name, query, config, span)?;
                if !hinted {
                    let best = subgraph
                        .nodes
                        .iter()
                        .filter_map(|n| n.seed_score)
                        .reduce(f64::max);
                    // One BM25 leg's RRF score carries no confidence: the
                    // threshold applies only when both legs ran.
                    let weak = match best {
                        None => true,
                        Some(best) => mode == HybridMode::Hybrid && best < config.global_threshold,
                    };
                    if weak {
                        if let (Some(global), mode) = global(self)? {
                            return Ok((global, mode, None, Scope::Global));
                        }
                    }
                }
                Ok((subgraph, mode, cut, Scope::Local))
            }
        }
    }

    /// The global scope: the `k` communities of `communityLayer` best
    /// matching `query` (topped up with the largest others when fewer
    /// match) as seeds, then
    /// their members — best in-community rank first, taken in turns across
    /// the communities up to `maxNodes` — and the relationships between
    /// them all. `None` when the layer has no community.
    fn rag_global_subgraph(
        &self,
        name: &str,
        query: &str,
        config: &RagConfig,
        span: Span,
    ) -> ExecResultT<(Option<Subgraph>, HybridMode)> {
        let (mut hits, mode) = self.community_hits(
            name,
            query,
            config.k,
            &config.community_layer,
            DEFAULT_EMBEDDING_PROPERTY,
            &config.hybrid,
            span,
        )?;
        if hits.len() < config.k {
            let chosen: HashSet<u64> = hits.iter().map(|(c, _)| c.id).collect();
            let mut largest: Vec<(i64, Arc<NodeValue>)> = self
                .community_nodes(Some(&config.community_layer))?
                .into_iter()
                .filter(|c| !chosen.contains(&c.id))
                .filter_map(|c| self.visible_node(c))
                .map(|c| {
                    let size = match c.properties.get("size") {
                        Some(Value::Integer(n)) => *n,
                        _ => 0,
                    };
                    (size, c)
                })
                .collect();
            largest.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.id.cmp(&b.1.id)));
            let missing = config.k - hits.len();
            hits.extend(largest.into_iter().take(missing).map(|(_, c)| (c, 0.0)));
        }
        if hits.is_empty() {
            return Ok((None, mode));
        }
        deadline::check()?;
        let sources = config.sources.as_deref();
        let mut lists: Vec<Vec<(Arc<NodeValue>, f64)>> = Vec::with_capacity(hits.len());
        for (community, _) in &hits {
            let mut members = Vec::new();
            for edge in self
                .engine()
                .edges_of(community.id, ModelDirection::Incoming)?
            {
                deadline::tick()?;
                if edge.kind != MEMBER_OF || !self.edge_traversable(&edge) {
                    continue;
                }
                let rel = edge_to_value(&edge);
                if layer_of(&rel.properties) != Some(config.community_layer.as_str()) {
                    continue;
                }
                let rank = match rel.properties.get("rank") {
                    Some(Value::Float(f)) => *f,
                    _ => 0.0,
                };
                let Some(node) = self.engine().get_node(edge.from_id)? else {
                    continue;
                };
                let Some(node) = self.seen_node(&node) else {
                    continue;
                };
                let excluded = node
                    .labels
                    .iter()
                    .any(|l| config.expand.exclude_labels.contains(l));
                if excluded || !source_admits(&node.properties, sources) {
                    continue;
                }
                members.push((node, rank));
            }
            members.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.id.cmp(&b.0.id)));
            lists.push(members);
        }
        let mut nodes: Vec<RankedNode> = hits
            .iter()
            .map(|(c, score)| RankedNode::seed(Arc::clone(c), *score))
            .collect();
        let mut taken: HashSet<u64> = nodes.iter().map(|n| n.node.id).collect();
        let cap = config.expand.max_nodes.max(nodes.len());
        let mut truncated = false;
        let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
        'fill: for position in 0..longest {
            for list in &lists {
                let Some((node, rank)) = list.get(position) else {
                    continue;
                };
                if taken.contains(&node.id) {
                    continue;
                }
                if nodes.len() >= cap {
                    truncated = true;
                    break 'fill;
                }
                taken.insert(node.id);
                nodes.push(RankedNode {
                    node: Arc::clone(node),
                    seed_score: None,
                    distance: 1,
                    ppr_score: Some(*rank),
                    via: None,
                });
            }
        }
        let ids: HashSet<u64> = nodes.iter().map(|n| n.node.id).collect();
        let (rels, weighted) = self.rag_edges(&ids, config)?;
        deadline::check()?;
        Ok((
            Some(Subgraph {
                nodes,
                rel_weights: crate::rag::rel_weights(&rels, &weighted),
                rels,
                truncated,
            }),
            mode,
        ))
    }
}
