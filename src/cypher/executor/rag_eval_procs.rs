//! `drevo.rag.evaluate` and `drevo.rag.evaluateSummary`: each retrieval
//! strategy run over a gold set with one token budget, scored on the context
//! it would hand to the model. The pure parts (reading the gold set, the
//! metrics, the aggregation) live in [`crate::rag::eval`]; this module
//! resolves the gold nodes, runs the strategies through the pipeline's own
//! stages and builds the rows. It reads only: the pipeline runs without a
//! session, so no trace is recorded.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::{
    deadline, Bindings, ExecError, ExecResultT, Executor, Expression, ModelDirection, NodeValue,
    Span, Value,
};
use crate::cypher::hybrid::HybridMode;
use crate::rag::config::{RagConfig, RankMode, Scope};
use crate::rag::eval::{
    parse_questions, score, split_config, summarize, ExpectedRef, Gold, Run, Strategy,
};
use crate::rag::serialize::{render, Rendered};
use crate::rag::{RankedNode, Subgraph};

/// `CALL drevo.rag.evaluate(questions[, config])`.
pub(super) const EVALUATE: &str = "drevo.rag.evaluate";
/// `CALL drevo.rag.evaluateSummary(questions[, config])`.
pub(super) const EVALUATE_SUMMARY: &str = "drevo.rag.evaluateSummary";

/// The columns of `drevo.rag.evaluate`.
pub(super) const EVALUATE_COLUMNS: &[&str] = &[
    "question",
    "strategy",
    "hit",
    "recall",
    "precision",
    "mrr",
    "tokens",
    "durationMs",
    "nodeIds",
    "recallRetrieved",
];

/// The columns of `drevo.rag.evaluateSummary`.
pub(super) const SUMMARY_COLUMNS: &[&str] = &[
    "strategy",
    "questions",
    "hitRate",
    "meanRecall",
    "meanPrecision",
    "mrr",
    "p50Ms",
    "p95Ms",
    "meanTokens",
];

/// One strategy's run on one question, with what it handed over.
struct Evaluated {
    /// The question asked.
    question: String,
    /// The run's strategy, score, tokens and duration.
    run: Run,
    /// The ids of the nodes written into the context, in order.
    node_ids: Vec<u64>,
}

/// A question ready to run: its text and the gold node ids.
struct Resolved {
    query: String,
    expected: Vec<u64>,
}

fn int(n: u64) -> Value {
    Value::Integer(i64::try_from(n).unwrap_or(i64::MAX))
}

fn size(n: usize) -> Value {
    int(u64::try_from(n).unwrap_or(u64::MAX))
}

impl Executor<'_> {
    /// `CALL drevo.rag.evaluate(questions[, config]) YIELD question,
    /// strategy, hit, recall, precision, mrr, tokens, durationMs, nodeIds,
    /// recallRetrieved` — one row per question and strategy, questions in
    /// order, strategies in the config's order.
    pub(super) fn proc_rag_evaluate(
        &self,
        args: &[Expression],
        span: Span,
    ) -> ExecResultT<Vec<Vec<Value>>> {
        let (evaluated, _) = self.rag_evaluate(EVALUATE, args, span)?;
        Ok(evaluated
            .into_iter()
            .map(|e| {
                let s = e.run.score;
                vec![
                    Value::String(e.question),
                    Value::String(e.run.strategy.as_str().to_string()),
                    Value::Bool(s.hit),
                    Value::Float(s.recall),
                    Value::Float(s.precision),
                    Value::Float(s.mrr),
                    size(e.run.tokens),
                    int(e.run.duration_ms),
                    Value::List(e.node_ids.into_iter().map(int).collect()),
                    Value::Float(s.recall_retrieved),
                ]
            })
            .collect())
    }

    /// `CALL drevo.rag.evaluateSummary(questions[, config]) YIELD strategy,
    /// questions, hitRate, meanRecall, meanPrecision, mrr, p50Ms, p95Ms,
    /// meanTokens` — the runs of `drevo.rag.evaluate` averaged, one row per
    /// strategy.
    pub(super) fn proc_rag_evaluate_summary(
        &self,
        args: &[Expression],
        span: Span,
    ) -> ExecResultT<Vec<Vec<Value>>> {
        let (evaluated, strategies) = self.rag_evaluate(EVALUATE_SUMMARY, args, span)?;
        let runs: Vec<Run> = evaluated.into_iter().map(|e| e.run).collect();
        Ok(summarize(&runs, &strategies)
            .into_iter()
            .map(|s| {
                vec![
                    Value::String(s.strategy.as_str().to_string()),
                    size(s.questions),
                    Value::Float(s.hit_rate),
                    Value::Float(s.mean_recall),
                    Value::Float(s.mean_precision),
                    Value::Float(s.mrr),
                    Value::Float(s.p50_ms),
                    Value::Float(s.p95_ms),
                    Value::Float(s.mean_tokens),
                ]
            })
            .collect())
    }

    /// Read the arguments of `name`, resolve the gold set and run every
    /// strategy on every question. Returns the runs and the strategies run.
    fn rag_evaluate(
        &self,
        name: &str,
        args: &[Expression],
        span: Span,
    ) -> ExecResultT<(Vec<Evaluated>, Vec<Strategy>)> {
        let refuse = |message: String| ExecError::InvalidProcedureCall {
            name: name.to_string(),
            message,
            span,
        };
        // Arity (1 or 2) is enforced by the upfront validation sweep.
        let empty = Bindings::new();
        let questions = parse_questions(&self.eval(&args[0], &empty)?).map_err(refuse)?;
        let config = match args.get(1) {
            Some(arg) => self.eval(arg, &empty)?,
            None => Value::Null,
        };
        let (config, strategies) = split_config(&config).map_err(refuse)?;
        let config =
            RagConfig::from_value(&config, crate::cypher::expand::ceilings()).map_err(refuse)?;
        let strategies = self
            .rag_eval_strategies(strategies, &config)?
            .map_err(refuse)?;
        let resolved = self.rag_eval_gold(&questions)?.map_err(refuse)?;

        let mut evaluated = Vec::with_capacity(resolved.len() * strategies.len());
        for question in &resolved {
            for &strategy in &strategies {
                deadline::check()?;
                let started = std::time::Instant::now();
                let (rendered, retrieved) =
                    self.rag_eval_run(name, strategy, &question.query, &config, span)?;
                let duration = started.elapsed();
                evaluated.push(Evaluated {
                    question: question.query.clone(),
                    run: Run {
                        strategy,
                        score: score(&rendered.node_ids, &retrieved, &question.expected),
                        tokens: rendered.tokens,
                        duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
                    },
                    node_ids: rendered.node_ids,
                });
            }
        }
        Ok((evaluated, strategies))
    }

    /// The strategies to run: those named, each checked to be runnable here,
    /// or by default every strategy this server can run with `config` —
    /// `'vector'` only with an embedder and a `label`, `'rag-global'` only
    /// when the config's `communityLayer` holds a community. The outer error
    /// is a storage failure; the inner one names the strategy that cannot
    /// run.
    fn rag_eval_strategies(
        &self,
        named: Option<Vec<Strategy>>,
        config: &RagConfig,
    ) -> ExecResultT<Result<Vec<Strategy>, String>> {
        let has_embedder = self.native_embedder.is_some();
        let Some(named) = named else {
            let vector = (has_embedder && config.label.is_some()).then_some(Strategy::Vector);
            let global = self
                .has_visible_community(&config.community_layer)?
                .then_some(Strategy::RagGlobal);
            return Ok(Ok(vector
                .into_iter()
                .chain([
                    Strategy::Fts,
                    Strategy::Hybrid,
                    Strategy::HybridExpand,
                    Strategy::Rag,
                ])
                .chain(global)
                .collect()));
        };
        for strategy in &named {
            let refusal = match strategy {
                Strategy::RagGlobal if !self.has_visible_community(&config.community_layer)? => {
                    format!(
                        "strategy 'rag-global' needs community summaries, and layer '{}' has \
                         none: run `drevo.community.summarize` first",
                        config.community_layer
                    )
                }
                Strategy::Vector if config.label.is_none() => "strategy 'vector' needs a \
                     `label` whose `embeddingProperty` vectors it ranks"
                    .to_string(),
                Strategy::Vector if !has_embedder => "strategy 'vector' needs a server-side \
                     embedder (DREVO_EMBEDDINGS_UPSTREAM), and this server has none; leave it \
                     out of `strategies`"
                    .to_string(),
                _ => continue,
            };
            return Ok(Err(refusal));
        }
        Ok(Ok(named))
    }

    /// Every question with its text and gold node ids: ids checked to be
    /// nodes the caller may see, `{name, label}` resolved by an exact `name`
    /// or `title` match, a trace's `INCLUDED` nodes read from it. The outer error is a
    /// storage failure; the inner one names what did not resolve.
    #[allow(clippy::type_complexity)]
    fn rag_eval_gold(
        &self,
        questions: &[crate::rag::eval::Question],
    ) -> ExecResultT<Result<Vec<Resolved>, String>> {
        // The nodes the named references may resolve to, by the `name` or
        // `title` they carry.
        let wanted: HashSet<&str> = questions
            .iter()
            .filter_map(|q| match &q.gold {
                Gold::Expected(refs) => Some(refs),
                Gold::Trace(_) => None,
            })
            .flatten()
            .filter_map(|r| match r {
                ExpectedRef::Named { name, .. } => Some(name.as_str()),
                ExpectedRef::Id(_) => None,
            })
            .collect();
        let mut by_name: HashMap<String, Vec<Arc<NodeValue>>> = HashMap::new();
        if !wanted.is_empty() {
            for node in self.engine().all_nodes()? {
                let Some(node) = self.seen_node(&node) else {
                    continue;
                };
                // A node whose name and title are both wanted is listed
                // under each, once.
                for wanted_name in &wanted {
                    if crate::rag::eval::names(&node.properties, wanted_name) {
                        by_name
                            .entry((*wanted_name).to_string())
                            .or_default()
                            .push(Arc::clone(&node));
                    }
                }
            }
        }
        let traces: HashMap<String, Arc<NodeValue>> =
            if questions.iter().any(|q| matches!(q.gold, Gold::Trace(_))) {
                self.rag_trace_nodes()?
                    .into_iter()
                    .filter_map(|t| match t.properties.get("id") {
                        Some(Value::String(id)) => Some((id.clone(), Arc::clone(&t))),
                        _ => None,
                    })
                    .collect()
            } else {
                HashMap::new()
            };

        let mut unresolved: Vec<String> = Vec::new();
        let mut resolved = Vec::with_capacity(questions.len());
        for (i, question) in questions.iter().enumerate() {
            let at = format!("questions[{i}]");
            let (query, expected) = match &question.gold {
                Gold::Expected(refs) => {
                    let mut ids = Vec::with_capacity(refs.len());
                    for (j, r) in refs.iter().enumerate() {
                        let found = match r {
                            ExpectedRef::Id(id) => {
                                let seen = self
                                    .engine()
                                    .get_node(*id)?
                                    .and_then(|n| self.seen_node(&n));
                                seen.map(|n| n.id)
                            }
                            ExpectedRef::Named { name, label } => {
                                let matches: Vec<u64> = by_name
                                    .get(name)
                                    .into_iter()
                                    .flatten()
                                    .filter(|n| label.as_ref().is_none_or(|l| n.labels.contains(l)))
                                    .map(|n| n.id)
                                    .collect();
                                if matches.len() > 1 {
                                    let ids = matches
                                        .iter()
                                        .map(u64::to_string)
                                        .collect::<Vec<_>>()
                                        .join(", ");
                                    return Ok(Err(format!(
                                        "{at}.expected[{j}] {} matches {} nodes (ids {ids}); \
                                         add a label",
                                        r.describe(),
                                        matches.len()
                                    )));
                                }
                                matches.first().copied()
                            }
                        };
                        match found {
                            Some(id) => ids.push(id),
                            None => unresolved.push(format!("{at}.expected[{j}] {}", r.describe())),
                        }
                    }
                    (question.query.clone().unwrap_or_default(), ids)
                }
                Gold::Trace(id) => {
                    let Some(trace) = traces.get(id) else {
                        return Ok(Err(format!("{at}: no context trace with id `{id}`")));
                    };
                    let query = match (&question.query, trace.properties.get("query")) {
                        (Some(query), _) => query.clone(),
                        (None, Some(Value::String(query))) => query.clone(),
                        (None, _) => {
                            return Ok(Err(format!(
                                "{at}: context trace `{id}` has no query; give one"
                            )))
                        }
                    };
                    let included = self.rag_trace_included(trace)?;
                    if included.is_empty() {
                        return Ok(Err(format!("{at}: context trace `{id}` included no nodes")));
                    }
                    (query, included)
                }
            };
            resolved.push(Resolved { query, expected });
        }
        if !unresolved.is_empty() {
            let count = unresolved.len();
            let noun = if count == 1 { "node" } else { "nodes" };
            return Ok(Err(format!(
                "{count} expected {noun} not found: {}",
                unresolved.join(", ")
            )));
        }
        Ok(Ok(resolved))
    }

    /// The ids of the nodes a context trace links to with `INCLUDED` that
    /// the caller may see.
    fn rag_trace_included(&self, trace: &NodeValue) -> ExecResultT<Vec<u64>> {
        let mut ids = Vec::new();
        for edge in self
            .engine()
            .edges_of(trace.id, ModelDirection::Outgoing)?
            .iter()
        {
            if edge.kind != crate::rag::trace::INCLUDED || !self.edge_traversable(edge) {
                continue;
            }
            let seen = self
                .engine()
                .get_node(edge.to_id)?
                .and_then(|n| self.seen_node(&n));
            if let Some(node) = seen {
                ids.push(node.id);
            }
        }
        Ok(ids)
    }

    /// Run `strategy` for `query`: the context it writes within the
    /// config's budget and the ids of every node it retrieved, in rank
    /// order. Each strategy is a stage (or the whole) of the graph-RAG
    /// pipeline, run without a session.
    fn rag_eval_run(
        &self,
        name: &str,
        strategy: Strategy,
        query: &str,
        config: &RagConfig,
        span: Span,
    ) -> ExecResultT<(Rendered, Vec<u64>)> {
        let seeds = match strategy {
            Strategy::Vector => {
                let label = config.label.as_deref().unwrap_or_default();
                let vector = self.hybrid_query_vector(name, query, span)?;
                let rows = self.vector_scan(
                    label,
                    &config.embedding_property,
                    &vector,
                    config.hybrid.depth(config.k),
                    span,
                )?;
                rows.into_iter()
                    .filter_map(|row| {
                        let mut row = row.into_iter();
                        let node = self.rag_admit(row.next()?, config)?;
                        let score = match row.next() {
                            Some(Value::Float(score)) => score,
                            _ => 0.0,
                        };
                        Some((node, score))
                    })
                    .take(config.k)
                    .collect()
            }
            Strategy::Fts => {
                let fts = RagConfig {
                    hybrid: crate::cypher::hybrid::HybridConfig {
                        mode: HybridMode::Fts,
                        ..config.hybrid.clone()
                    },
                    ..config.clone()
                };
                self.rag_seeds(name, query, &fts, span)?.0
            }
            Strategy::Hybrid => self.rag_seeds(name, query, config, span)?.0,
            Strategy::HybridExpand | Strategy::Rag | Strategy::RagGlobal => {
                let (rank, scope) = match strategy {
                    Strategy::HybridExpand => (RankMode::Distance, Scope::Local),
                    Strategy::RagGlobal => (RankMode::Ppr, Scope::Global),
                    _ => (RankMode::Ppr, Scope::Local),
                };
                let config = RagConfig {
                    rank,
                    scope,
                    session: None,
                    trace: false,
                    ..config.clone()
                };
                let built = self.rag_build(name, query, &config, span)?;
                let retrieved = built.subgraph.nodes.iter().map(|n| n.node.id).collect();
                return Ok((built.rendered, retrieved));
            }
        };
        // A search strategy hands over its hits alone, with the
        // relationships between them.
        let nodes: Vec<RankedNode> = seeds
            .into_iter()
            .map(|(node, score)| RankedNode::seed(node, score))
            .collect();
        let ids: HashSet<u64> = nodes.iter().map(|n| n.node.id).collect();
        let (rels, weighted) = self.rag_edges(&ids, config)?;
        let subgraph = Subgraph {
            nodes,
            rel_weights: crate::rag::rel_weights(&rels, &weighted),
            rels,
            truncated: false,
        };
        let rendered = render(&subgraph, &config.render);
        let retrieved = subgraph.nodes.iter().map(|n| n.node.id).collect();
        Ok((rendered, retrieved))
    }
}
