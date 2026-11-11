//! The serialiser of `drevo.rag.context`: a ranked [`Subgraph`] written as
//! LLM-ready text within a token budget.
//!
//! Nodes are taken best-ranked first. Each brings its own block (a bullet or
//! a line built from its text properties) plus the relationships to the nodes
//! already taken; a node whose block would push the text over the budget is
//! skipped whole — never cut mid-sentence — and the fill goes on with the
//! next one. Markdown has the layout of drevo-py's
//! `Context.to_text('markdown')`: the nodes taken are written under
//! `## Seeds` / `## Neighbours` sorted by kind, name and id, and their
//! relationships under `## Edges` sorted by ends and id.
//!
//! ```
//! use std::collections::BTreeMap;
//! use std::sync::Arc;
//! use drevo::cypher::executor::{NodeValue, Value};
//! use drevo::rag::serialize::{render, RenderOptions};
//! use drevo::rag::{RankedNode, Subgraph};
//!
//! let anna = NodeValue {
//!     id: 1,
//!     uuid: [0; 16],
//!     labels: vec!["Person".to_string()],
//!     properties: BTreeMap::from([
//!         ("name".to_string(), Value::String("Anna".to_string())),
//!         ("description".to_string(), Value::String("Keeps a journal".to_string())),
//!     ]),
//! };
//! let subgraph = Subgraph {
//!     nodes: vec![RankedNode::seed(Arc::new(anna), 1.0)],
//!     rels: Vec::new(),
//!     rel_weights: BTreeMap::new(),
//!     truncated: false,
//! };
//! let out = render(&subgraph, &RenderOptions::default());
//! let seeds = "## Seeds\n- Anna (kind=Person, id=1): Keeps a journal\n";
//! let rest = "\n## Neighbours\n- (none)\n\n## Edges\n- (none)\n";
//! assert_eq!(out.text, format!("{seeds}{rest}"));
//! assert_eq!(out.node_ids, [1]);
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::cypher::executor::{NodeValue, RelationshipValue, Value};
use crate::rag::tokens::{Measure, Tokenizer};
use crate::rag::Subgraph;

/// The properties a node's block is built from when the config names none,
/// in order: the first one present is its name.
pub(crate) const DEFAULT_TEXT_PROPERTIES: [&str; 5] =
    ["name", "title", "description", "body", "text"];

/// The default token budget of a context.
pub(crate) const DEFAULT_TOKEN_BUDGET: usize = 2000;

/// The default cap on one text value, in characters.
pub(crate) const DEFAULT_MAX_BODY_CHARS: usize = 600;

/// How a context is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// The layout of drevo-py's `Context.to_text('markdown')`: `## Seeds`
    /// and `## Neighbours` with a bullet per node
    /// (`- name (kind=Label, id=7): text`), sorted by kind, name and id, then
    /// `## Edges` with a line per relationship
    /// (`- 7 -[REL]-> 9 (id=12, weight=1.0)`), sorted by its ends and id; an
    /// empty section reads `- (none)`.
    Markdown,
    /// One `|`-separated line per node (`name | Label | text`) followed by
    /// the lines of its relationships (`A | REL | B`) — the fewest tokens.
    Triples,
    /// One JSON object: `{"nodes":[{id, labels, props}], "rels":[{from, id,
    /// props, to, type}], "stats":{nodes, rels}}`, keys sorted.
    Json,
}

impl Format {
    /// The format's name as written in a config map and reported per row.
    ///
    /// ```
    /// use drevo::rag::serialize::Format;
    /// assert_eq!(Format::Triples.as_str(), "triples");
    /// ```
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Triples => "triples",
            Self::Json => "json",
        }
    }

    /// The format named `name`, if any.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "markdown" => Some(Self::Markdown),
            "triples" => Some(Self::Triples),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

/// How [`render`] writes a context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderOptions {
    /// The text format.
    pub format: Format,
    /// How tokens are counted.
    pub tokenizer: Tokenizer,
    /// The most tokens the text may take.
    pub token_budget: usize,
    /// The properties a node's block is built from, in order; the first one
    /// present names the node.
    pub text_properties: Vec<String>,
    /// Each `body`, `text`, `description` or `quote` value longer than this
    /// many characters is cut to it, with `…` marking the cut; `name`,
    /// `title` and other properties are written whole.
    pub max_body_chars: usize,
    /// Whether `valid_from` / `valid_until`, `quote` and `source_id` are
    /// written when present.
    pub include_provenance: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            format: Format::Markdown,
            tokenizer: Tokenizer::Approx,
            token_budget: DEFAULT_TOKEN_BUDGET,
            text_properties: DEFAULT_TEXT_PROPERTIES.map(str::to_string).to_vec(),
            max_body_chars: DEFAULT_MAX_BODY_CHARS,
            include_provenance: true,
        }
    }
}

/// What [`render`] wrote.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Rendered {
    /// The context; empty when no node fits.
    pub text: String,
    /// The text's size under the options' tokenizer.
    pub tokens: usize,
    /// The ids of the nodes written, in the order they were taken (rank
    /// order).
    pub node_ids: Vec<u64>,
    /// The ids of the relationships written, in the order they were taken.
    pub rel_ids: Vec<u64>,
    /// How many nodes did not fit the budget.
    pub skipped: usize,
    /// The tokens each written node added to the text, aligned with
    /// [`node_ids`](Self::node_ids): its block, its relationships and any
    /// framing it brought. They sum to [`tokens`](Self::tokens).
    pub node_tokens: Vec<usize>,
    /// The ids of the nodes that did not fit the budget, in rank order.
    pub skipped_ids: Vec<u64>,
}

/// Write `subgraph` within `options.token_budget`: nodes in their rank
/// order, each with the relationships to the nodes before it; a node that
/// does not fit is skipped and the next one tried.
#[must_use]
pub fn render(subgraph: &Subgraph, options: &RenderOptions) -> Rendered {
    let names: HashMap<u64, String> = subgraph
        .nodes
        .iter()
        .map(|n| (n.node.id, node_name(&n.node, options)))
        .collect();
    let mut by_node: HashMap<u64, Vec<&RelationshipValue>> = HashMap::new();
    for rel in &subgraph.rels {
        by_node.entry(rel.from_id).or_default().push(rel);
        if rel.to_id != rel.from_id {
            by_node.entry(rel.to_id).or_default().push(rel);
        }
    }

    let mut text = match options.format {
        Format::Markdown => Writer::Markdown(Markdown::default()),
        Format::Triples => Writer::Lines(Text::new(false)),
        Format::Json => Writer::Lines(Text::new(true)),
    };
    let weight = |rel: &RelationshipValue| subgraph.rel_weights.get(&rel.id).copied();
    let mut out = Rendered::default();
    let mut taken: HashSet<u64> = HashSet::new();
    let mut written = 0;
    for ranked in &subgraph.nodes {
        let node = &ranked.node;
        if taken.contains(&node.id) {
            continue;
        }
        let mut rels: Vec<&RelationshipValue> = by_node
            .get(&node.id)
            .map(|rels| {
                rels.iter()
                    .copied()
                    .filter(|r| {
                        let other = if r.from_id == node.id {
                            r.to_id
                        } else {
                            r.from_id
                        };
                        other == node.id || taken.contains(&other)
                    })
                    .collect()
            })
            .unwrap_or_default();
        rels.sort_by_key(|r| r.id);
        let block = node_piece(node, &names, options);
        let rel_pieces: Vec<String> = rels
            .iter()
            .map(|r| rel_piece(r, weight(r), &names, options))
            .collect();
        let tokens = match &mut text {
            Writer::Lines(text) => text
                .measure_with(&block, &rel_pieces)
                .tokens(options.tokenizer),
            Writer::Markdown(md) => {
                let key = (
                    node_kind(node),
                    names.get(&node.id).cloned().unwrap_or_default(),
                    node.id,
                );
                let edges = rels.iter().map(|r| (r.from_id, r.to_id, r.id));
                md.insert(
                    ranked.seed_score.is_some(),
                    key,
                    block.clone(),
                    edges,
                    rel_pieces.clone(),
                );
                let tokens = options.tokenizer.count(&md.text());
                if tokens > options.token_budget {
                    md.undo();
                }
                tokens
            }
        };
        if tokens > options.token_budget {
            out.skipped += 1;
            out.skipped_ids.push(node.id);
            continue;
        }
        if let Writer::Lines(text) = &mut text {
            text.push(block, rel_pieces);
        }
        taken.insert(node.id);
        out.node_ids.push(node.id);
        out.node_tokens.push(tokens.saturating_sub(written));
        written = tokens;
        out.rel_ids.extend(rels.iter().map(|r| r.id));
    }
    out.text = match text {
        Writer::Lines(text) => text.finish(),
        Writer::Markdown(_) if out.node_ids.is_empty() => String::new(),
        Writer::Markdown(md) => md.text(),
    };
    out.tokens = options.tokenizer.count(&out.text);
    out
}

/// What a node's kind is written as: its labels joined by `:` (its kind
/// alone when it has one label, as drevo-py writes `Node.kind`).
fn node_kind(node: &NodeValue) -> String {
    node.labels.join(":")
}

/// A float as Python's `repr` writes it (`1.0`, `0.5`, `1e-05`, `1e+16`), so
/// edge weights read as in drevo-py's `Context.to_text`.
fn py_float(value: f64) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .to_string();
    }
    // Shortest round-trip digits and exponent, e.g. `1.5e-7`.
    let sci = format!("{value:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let sign = if negative { "-" } else { "" };
    if (-4..16).contains(&exp) {
        let point = exp + 1; // digits before the decimal point
        let body = if point <= 0 {
            format!("0.{}{digits}", "0".repeat(point.unsigned_abs() as usize))
        } else {
            let point = point as usize;
            if digits.len() <= point {
                format!("{digits}{}.0", "0".repeat(point - digits.len()))
            } else {
                format!("{}.{}", &digits[..point], &digits[point..])
            }
        };
        return format!("{sign}{body}");
    }
    let (head, tail) = digits.split_at(1);
    let mantissa = if tail.is_empty() {
        head.to_string()
    } else {
        format!("{head}.{tail}")
    };
    let exp_sign = if exp < 0 { '-' } else { '+' };
    format!("{sign}{mantissa}e{exp_sign}{:02}", exp.unsigned_abs())
}

/// `text` cut to at most `max_chars` characters (never inside a UTF-8
/// sequence), with `…` appended when anything was cut.
pub(crate) fn cut(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

/// The provenance properties written when present.
const PROVENANCE: [&str; 4] = ["quote", "source_id", "valid_from", "valid_until"];

/// A property value as text: scalars as written, lists comma-separated;
/// `None` for an empty string, `null`, maps and graph values.
fn value_text(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(s) => s.clone(),
        Value::Integer(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::List(items) => items
            .iter()
            .filter_map(value_text)
            .collect::<Vec<_>>()
            .join(", "),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

/// The node's text values, in `text_properties` order, each cut.
fn text_values(node: &NodeValue, options: &RenderOptions) -> Vec<String> {
    options
        .text_properties
        .iter()
        .filter_map(|key| {
            let text = node.properties.get(key).and_then(value_text)?;
            Some(cut(&text, cap(key, options)))
        })
        .collect()
}

/// The properties [`RenderOptions::max_body_chars`] applies to: long text.
/// Every other property — `name` and `title` above all, which say what a
/// node is — is written whole.
const LONG_TEXT: [&str; 4] = ["body", "text", "description", "quote"];

/// How many characters of property `key` are written.
fn cap(key: &str, options: &RenderOptions) -> usize {
    if LONG_TEXT.contains(&key) {
        options.max_body_chars
    } else {
        usize::MAX
    }
}

/// What a node is called in its block and in relationship lines: its first
/// text value, else `#id`.
fn node_name(node: &NodeValue, options: &RenderOptions) -> String {
    text_values(node, options)
        .into_iter()
        .next()
        .unwrap_or_else(|| format!("#{}", node.id))
}

/// The provenance segments of an element: `valid from→until` and
/// `«quote» (source)`, each when present and asked for.
fn provenance(props: &BTreeMap<String, Value>, options: &RenderOptions) -> Vec<String> {
    if !options.include_provenance {
        return Vec::new();
    }
    let text = |key: &str| props.get(key).and_then(value_text);
    let mut segments = Vec::new();
    let (from, until) = (text("valid_from"), text("valid_until"));
    if from.is_some() || until.is_some() {
        segments.push(format!(
            "valid {}→{}",
            from.unwrap_or_default(),
            until.unwrap_or_default()
        ));
    }
    let quote = text("quote").map(|q| format!("«{}»", cut(&q, options.max_body_chars)));
    let source = text("source_id").map(|s| format!("({s})"));
    match (quote, source) {
        (Some(q), Some(s)) => segments.push(format!("{q} {s}")),
        (Some(one), None) | (None, Some(one)) => segments.push(one),
        (None, None) => {}
    }
    segments
}

/// The name a relationship is written with: a `RELATED_TO` fact's own
/// `type` property, else its type.
fn rel_name(rel: &RelationshipValue) -> &str {
    match rel.properties.get("type") {
        Some(Value::String(t)) if rel.kind == "RELATED_TO" && !t.is_empty() => t,
        _ => &rel.kind,
    }
}

/// A property value as JSON, strings cut.
fn json_value(value: &Value, max_chars: usize) -> serde_json::Value {
    match value {
        Value::String(s) => serde_json::Value::String(cut(s, max_chars)),
        Value::Integer(i) => serde_json::Value::from(*i),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::List(items) => items.iter().map(|v| json_value(v, max_chars)).collect(),
        _ => serde_json::Value::Null,
    }
}

/// `entries` as a JSON object, keys sorted.
fn json_object(entries: BTreeMap<&str, serde_json::Value>) -> String {
    let mut object = serde_json::Map::new();
    for (key, value) in entries {
        object.insert(key.to_string(), value);
    }
    serde_json::Value::Object(object).to_string()
}

/// A JSON string literal.
fn json_string(text: &str) -> String {
    serde_json::Value::String(text.to_string()).to_string()
}

/// A node's block in the options' format (without separators).
fn node_piece(node: &NodeValue, names: &HashMap<u64, String>, options: &RenderOptions) -> String {
    if options.format == Format::Json {
        let mut props: BTreeMap<&str, serde_json::Value> = BTreeMap::new();
        for key in &options.text_properties {
            if let Some(v) = node.properties.get(key).filter(|v| value_text(v).is_some()) {
                props.insert(key, json_value(v, cap(key, options)));
            }
        }
        if options.include_provenance {
            for key in PROVENANCE {
                if let Some(v) = node.properties.get(key).filter(|v| value_text(v).is_some()) {
                    props.insert(key, json_value(v, cap(key, options)));
                }
            }
        }
        let labels: Vec<serde_json::Value> = node
            .labels
            .iter()
            .map(|l| serde_json::Value::String(l.clone()))
            .collect();
        return format!(
            r#"{{"id":{},"labels":{},"props":{}}}"#,
            node.id,
            serde_json::Value::Array(labels),
            json_object(props)
        );
    }
    let name = names
        .get(&node.id)
        .cloned()
        .unwrap_or_else(|| format!("#{}", node.id));
    let rest: Vec<String> = text_values(node, options)
        .into_iter()
        .skip(1)
        .filter(|v| *v != name)
        .collect();
    let labels = node.labels.join(":");
    let prov = provenance(&node.properties, options);
    let mut line = String::new();
    match options.format {
        Format::Markdown => {
            line.push_str(&format!("- {name} (kind={labels}, id={})", node.id));
            if !rest.is_empty() {
                line.push_str(&format!(": {}", rest.join(" — ")));
            }
            for p in prov {
                line.push(' ');
                line.push_str(&p);
            }
        }
        Format::Triples | Format::Json => {
            line.push_str(&name);
            if !labels.is_empty() {
                line.push_str(&format!(" | {labels}"));
            }
            if !rest.is_empty() {
                line.push_str(&format!(" | {}", rest.join(" — ")));
            }
            for p in prov {
                line.push_str(&format!(" | {p}"));
            }
        }
    }
    line
}

/// A relationship's line (or JSON object) in the options' format.
fn rel_piece(
    rel: &RelationshipValue,
    weight: Option<f32>,
    names: &HashMap<u64, String>,
    options: &RenderOptions,
) -> String {
    let name = rel_name(rel);
    if options.format == Format::Json {
        let mut props: BTreeMap<&str, serde_json::Value> = BTreeMap::new();
        if options.include_provenance {
            for key in PROVENANCE {
                if let Some(v) = rel.properties.get(key).filter(|v| value_text(v).is_some()) {
                    props.insert(key, json_value(v, cap(key, options)));
                }
            }
        }
        return format!(
            r#"{{"from":{},"id":{},"props":{},"to":{},"type":{}}}"#,
            rel.from_id,
            rel.id,
            json_object(props),
            rel.to_id,
            json_string(name)
        );
    }
    let end = |id: u64| names.get(&id).cloned().unwrap_or_else(|| format!("#{id}"));
    let (from, to) = (end(rel.from_id), end(rel.to_id));
    let prov = provenance(&rel.properties, options);
    match options.format {
        Format::Markdown => {
            let weight = py_float(f64::from(weight.unwrap_or(1.0)));
            let mut line = format!(
                "- {} -[{name}]-> {} (id={}, weight={weight})",
                rel.from_id, rel.to_id, rel.id
            );
            for p in prov {
                line.push(' ');
                line.push_str(&p);
            }
            line
        }
        Format::Triples | Format::Json => {
            let mut line = format!("{from} | {name} | {to}");
            for p in prov {
                line.push_str(&format!(" | {p}"));
            }
            line
        }
    }
}

/// A run of pieces joined by a separator, and its measure.
#[derive(Default)]
struct Section {
    pieces: Vec<String>,
    measure: Measure,
}

impl Section {
    /// The measure after appending `more` (joined by `sep`).
    fn measure_with<'p>(&self, more: impl IntoIterator<Item = &'p str>, sep: &str) -> Measure {
        let sep = Measure::of(sep);
        let mut m = self.measure;
        let mut empty = self.pieces.is_empty();
        for piece in more {
            if !empty {
                m = m.then(sep);
            }
            m = m.then(Measure::of(piece));
            empty = false;
        }
        m
    }

    fn push(&mut self, piece: String, sep: &str) {
        self.measure = self.measure_with([piece.as_str()], sep);
        self.pieces.push(piece);
    }

    fn len(&self) -> usize {
        self.pieces.len()
    }
}

/// The context being written: triples / JSON lines measured as they grow,
/// or markdown sections kept sorted.
enum Writer {
    Lines(Text),
    Markdown(Markdown),
}

/// A triples or JSON context being written, measured as it grows.
struct Text {
    json: bool,
    /// Node blocks (JSON) or every line (triples).
    nodes: Section,
    /// Relationship objects (JSON).
    rels: Section,
}

impl Text {
    fn new(json: bool) -> Self {
        Self {
            json,
            nodes: Section::default(),
            rels: Section::default(),
        }
    }

    /// The separator between pieces of a section.
    fn sep(&self) -> &'static str {
        if self.json {
            ","
        } else {
            "\n"
        }
    }

    /// The measure of the whole text once `block` and `rels` are added.
    fn measure_with(&self, block: &str, rels: &[String]) -> Measure {
        let sep = self.sep();
        if !self.json {
            let lines = self.nodes.measure_with(
                std::iter::once(block).chain(rels.iter().map(String::as_str)),
                sep,
            );
            return lines.then(Measure::of("\n"));
        }
        let nodes = self.nodes.measure_with([block], sep);
        let rel_count = self.rels.len() + rels.len();
        let rels = self.rels.measure_with(rels.iter().map(String::as_str), sep);
        Self::json_measure(nodes, rels, self.nodes.len() + 1, rel_count)
    }

    fn json_measure(nodes: Measure, rels: Measure, node_count: usize, rel_count: usize) -> Measure {
        Measure::of(JSON_NODES)
            .then(nodes)
            .then(Measure::of(JSON_RELS))
            .then(rels)
            .then(Measure::of(&json_stats(node_count, rel_count)))
    }

    fn push(&mut self, block: String, rels: Vec<String>) {
        let sep = self.sep();
        self.nodes.push(block, sep);
        for rel in rels {
            if self.json {
                self.rels.push(rel, sep);
            } else {
                self.nodes.push(rel, sep);
            }
        }
    }

    fn finish(self) -> String {
        if self.nodes.pieces.is_empty() {
            return String::new();
        }
        let sep = self.sep();
        let nodes = self.nodes.pieces.join(sep);
        if !self.json {
            return format!("{nodes}\n");
        }
        let rels = self.rels.pieces.join(sep);
        format!(
            "{JSON_NODES}{nodes}{JSON_RELS}{rels}{}",
            json_stats(self.nodes.len(), self.rels.len())
        )
    }
}

/// How a markdown node line sorts: kind, name, id.
type NodeKey = (String, String, u64);

/// How a markdown edge line sorts: from id, to id, id.
type EdgeKey = (u64, u64, u64);

/// A markdown context being written: each section kept sorted, so the text
/// is the same whatever order the nodes were taken in.
#[derive(Default)]
struct Markdown {
    seeds: Vec<(NodeKey, String)>,
    neighbours: Vec<(NodeKey, String)>,
    edges: Vec<(EdgeKey, String)>,
    /// What the last [`Self::insert`] added, for [`Self::undo`]: whether the
    /// node is a seed, its key, and the keys of its edges.
    last: Option<(bool, NodeKey, Vec<EdgeKey>)>,
}

impl Markdown {
    /// Add a node line and its edge lines, each in sorted place.
    fn insert(
        &mut self,
        seed: bool,
        key: NodeKey,
        line: String,
        edge_keys: impl Iterator<Item = EdgeKey>,
        edge_lines: Vec<String>,
    ) {
        let section = if seed {
            &mut self.seeds
        } else {
            &mut self.neighbours
        };
        let at = section.partition_point(|(k, _)| *k < key);
        section.insert(at, (key.clone(), line));
        let mut keys = Vec::new();
        for (edge, line) in edge_keys.zip(edge_lines) {
            let at = self.edges.partition_point(|(k, _)| *k < edge);
            self.edges.insert(at, (edge, line));
            keys.push(edge);
        }
        self.last = Some((seed, key, keys));
    }

    /// Take back what the last [`Self::insert`] added.
    fn undo(&mut self) {
        let Some((seed, key, edges)) = self.last.take() else {
            return;
        };
        let section = if seed {
            &mut self.seeds
        } else {
            &mut self.neighbours
        };
        if let Some(at) = section.iter().position(|(k, _)| *k == key) {
            section.remove(at);
        }
        for edge in edges {
            if let Some(at) = self.edges.iter().position(|(k, _)| *k == edge) {
                self.edges.remove(at);
            }
        }
    }

    fn text(&self) -> String {
        let mut out = String::new();
        let sections: [(&str, Vec<&String>); 3] = [
            ("## Seeds", self.seeds.iter().map(|(_, l)| l).collect()),
            (
                "## Neighbours",
                self.neighbours.iter().map(|(_, l)| l).collect(),
            ),
            ("## Edges", self.edges.iter().map(|(_, l)| l).collect()),
        ];
        for (i, (heading, lines)) in sections.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(heading);
            out.push('\n');
            if lines.is_empty() {
                out.push_str("- (none)\n");
            }
            for line in lines {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }
}

const JSON_NODES: &str = r#"{"nodes":["#;
const JSON_RELS: &str = r#"],"rels":["#;

/// The closing `stats` of a JSON context.
fn json_stats(nodes: usize, rels: usize) -> String {
    format!(r#"],"stats":{{"nodes":{nodes},"rels":{rels}}}}}"#)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::*;
    use crate::cypher::executor::{NodeValue, RelationshipValue, Value};
    use crate::rag::RankedNode;

    fn node(id: u64, label: &str, props: &[(&str, &str)]) -> Arc<NodeValue> {
        Arc::new(NodeValue {
            id,
            uuid: [0; 16],
            labels: vec![label.to_string()],
            properties: props
                .iter()
                .map(|(k, v)| ((*k).to_string(), Value::String((*v).to_string())))
                .collect(),
        })
    }

    fn rel(
        id: u64,
        from: u64,
        kind: &str,
        to: u64,
        props: &[(&str, &str)],
    ) -> Arc<RelationshipValue> {
        Arc::new(RelationshipValue {
            id,
            uuid: [0; 16],
            from_id: from,
            to_id: to,
            kind: kind.to_string(),
            properties: props
                .iter()
                .map(|(k, v)| ((*k).to_string(), Value::String((*v).to_string())))
                .collect(),
        })
    }

    fn reached(node: Arc<NodeValue>, distance: usize) -> RankedNode {
        RankedNode {
            node,
            seed_score: None,
            distance,
            ppr_score: None,
            via: None,
        }
    }

    /// A six-node CBT-journal slice, in rank order: a person, two entries
    /// (one in Russian), a thought carrying a quote, a distortion and a
    /// second person; five relationships, two carrying provenance.
    fn journal() -> Subgraph {
        Subgraph {
            nodes: vec![
                RankedNode::seed(
                    node(
                        1,
                        "Person",
                        &[("name", "Anna"), ("description", "Keeps a CBT journal")],
                    ),
                    0.5,
                ),
                reached(
                    node(
                        2,
                        "Entry",
                        &[
                            ("title", "Monday entry"),
                            ("body", "Felt anxious before the meeting."),
                            ("mood", "low"),
                        ],
                    ),
                    1,
                ),
                reached(
                    node(
                        3,
                        "Thought",
                        &[
                            ("text", "I will fail"),
                            ("quote", "I will fail at this"),
                            ("source_id", "journal-1"),
                        ],
                    ),
                    1,
                ),
                reached(
                    node(
                        4,
                        "Distortion",
                        &[
                            ("name", "Catastrophizing"),
                            ("description", "Expecting the worst"),
                        ],
                    ),
                    2,
                ),
                reached(node(5, "Person", &[("name", "Boris")]), 2),
                reached(
                    node(
                        6,
                        "Entry",
                        &[
                            ("title", "Пятница"),
                            ("body", "Сегодня было тяжело, но я справилась."),
                        ],
                    ),
                    2,
                ),
            ],
            rels: vec![
                rel(10, 1, "WROTE", 2, &[]),
                rel(11, 2, "CONTAINS", 3, &[]),
                rel(
                    12,
                    3,
                    "IS_A",
                    4,
                    &[
                        ("valid_from", "2025-01-01"),
                        ("quote", "always the worst"),
                        ("source_id", "journal-1"),
                    ],
                ),
                rel(
                    13,
                    1,
                    "RELATED_TO",
                    5,
                    &[
                        ("type", "KNOWS"),
                        ("valid_from", "2024-05-01"),
                        ("valid_until", "2025-02-01"),
                    ],
                ),
                rel(14, 1, "WROTE", 6, &[]),
            ],
            truncated: false,
            rel_weights: BTreeMap::from([(13, 0.5)]),
        }
    }

    fn opts(format: Format) -> RenderOptions {
        RenderOptions {
            format,
            token_budget: 100_000,
            ..RenderOptions::default()
        }
    }

    const MARKDOWN: &str = "## Seeds\n\
- Anna (kind=Person, id=1): Keeps a CBT journal\n\
\n\
## Neighbours\n\
- Catastrophizing (kind=Distortion, id=4): Expecting the worst\n\
- Monday entry (kind=Entry, id=2): Felt anxious before the meeting.\n\
- Пятница (kind=Entry, id=6): Сегодня было тяжело, но я справилась.\n\
- Boris (kind=Person, id=5)\n\
- I will fail (kind=Thought, id=3) «I will fail at this» (journal-1)\n\
\n\
## Edges\n\
- 1 -[WROTE]-> 2 (id=10, weight=1.0)\n\
- 1 -[KNOWS]-> 5 (id=13, weight=0.5) valid 2024-05-01→2025-02-01\n\
- 1 -[WROTE]-> 6 (id=14, weight=1.0)\n\
- 2 -[CONTAINS]-> 3 (id=11, weight=1.0)\n\
- 3 -[IS_A]-> 4 (id=12, weight=1.0) valid 2025-01-01→ «always the worst» (journal-1)\n";

    const TRIPLES: &str = "Anna | Person | Keeps a CBT journal\n\
Monday entry | Entry | Felt anxious before the meeting.\n\
Anna | WROTE | Monday entry\n\
I will fail | Thought | «I will fail at this» (journal-1)\n\
Monday entry | CONTAINS | I will fail\n\
Catastrophizing | Distortion | Expecting the worst\n\
I will fail | IS_A | Catastrophizing | valid 2025-01-01→ | «always the worst» (journal-1)\n\
Boris | Person\n\
Anna | KNOWS | Boris | valid 2024-05-01→2025-02-01\n\
Пятница | Entry | Сегодня было тяжело, но я справилась.\n\
Anna | WROTE | Пятница\n";

    const JSON: &str = concat!(
        r#"{"nodes":["#,
        r#"{"id":1,"labels":["Person"],"props":{"description":"Keeps a CBT journal","name":"Anna"}},"#,
        r#"{"id":2,"labels":["Entry"],"props":{"body":"Felt anxious before the meeting.","title":"Monday entry"}},"#,
        r#"{"id":3,"labels":["Thought"],"props":{"quote":"I will fail at this","source_id":"journal-1","text":"I will fail"}},"#,
        r#"{"id":4,"labels":["Distortion"],"props":{"description":"Expecting the worst","name":"Catastrophizing"}},"#,
        r#"{"id":5,"labels":["Person"],"props":{"name":"Boris"}},"#,
        r#"{"id":6,"labels":["Entry"],"props":{"body":"Сегодня было тяжело, но я справилась.","title":"Пятница"}}"#,
        r#"],"rels":["#,
        r#"{"from":1,"id":10,"props":{},"to":2,"type":"WROTE"},"#,
        r#"{"from":2,"id":11,"props":{},"to":3,"type":"CONTAINS"},"#,
        r#"{"from":3,"id":12,"props":{"quote":"always the worst","source_id":"journal-1","valid_from":"2025-01-01"},"to":4,"type":"IS_A"},"#,
        r#"{"from":1,"id":13,"props":{"valid_from":"2024-05-01","valid_until":"2025-02-01"},"to":5,"type":"KNOWS"},"#,
        r#"{"from":1,"id":14,"props":{},"to":6,"type":"WROTE"}"#,
        r#"],"stats":{"nodes":6,"rels":5}}"#,
    );

    #[test]
    fn markdown_snapshot() {
        let out = render(&journal(), &opts(Format::Markdown));
        assert_eq!(out.text, MARKDOWN);
        assert_eq!(out.node_ids, [1, 2, 3, 4, 5, 6]);
        assert_eq!(out.rel_ids, [10, 11, 12, 13, 14]);
        assert_eq!(out.skipped, 0);
        assert_eq!(out.tokens, Tokenizer::Approx.count(MARKDOWN));
    }

    #[test]
    fn markdown_is_sorted_whatever_the_rank_order() {
        let mut reversed = journal();
        let seed = reversed.nodes.remove(0);
        reversed.nodes.reverse();
        reversed.nodes.insert(0, seed);
        let out = render(&reversed, &opts(Format::Markdown));
        assert_eq!(out.text, MARKDOWN);
        assert_eq!(out.node_ids, [1, 6, 5, 4, 3, 2], "taken in rank order");
    }

    #[test]
    fn weights_are_written_as_python_writes_floats() {
        assert_eq!(py_float(1.0), "1.0");
        assert_eq!(py_float(0.5), "0.5");
        assert_eq!(py_float(f64::from(0.1_f32)), "0.10000000149011612");
        assert_eq!(py_float(123_456.0), "123456.0");
        assert_eq!(py_float(0.0001), "0.0001");
        assert_eq!(py_float(0.00001), "1e-05");
        assert_eq!(py_float(1.5e-7), "1.5e-07");
        assert_eq!(py_float(1e16), "1e+16");
        assert_eq!(py_float(1_234_567_890_123_456.0), "1234567890123456.0");
        assert_eq!(py_float(-2.5), "-2.5");
        assert_eq!(py_float(0.0), "0.0");
    }

    #[test]
    fn triples_snapshot() {
        let out = render(&journal(), &opts(Format::Triples));
        assert_eq!(out.text, TRIPLES);
        assert_eq!(out.rel_ids, [10, 11, 12, 13, 14]);
        assert_eq!(out.tokens, Tokenizer::Approx.count(TRIPLES));
    }

    #[test]
    fn json_snapshot_has_sorted_keys_and_parses() {
        let out = render(&journal(), &opts(Format::Json));
        assert_eq!(out.text, JSON);
        let parsed: serde_json::Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(parsed["stats"]["nodes"], 6);
        assert_eq!(out.tokens, Tokenizer::Approx.count(JSON));
    }

    #[test]
    fn triples_take_fewer_tokens_than_markdown_and_json() {
        let tokens = |f| render(&journal(), &opts(f)).tokens;
        assert!(tokens(Format::Triples) < tokens(Format::Markdown));
        assert!(tokens(Format::Markdown) < tokens(Format::Json));
    }

    #[test]
    fn rendering_twice_gives_the_same_bytes() {
        for format in [Format::Markdown, Format::Triples, Format::Json] {
            assert_eq!(
                render(&journal(), &opts(format)),
                render(&journal(), &opts(format))
            );
        }
    }

    #[test]
    fn provenance_is_left_out_when_not_asked_for() {
        let options = RenderOptions {
            include_provenance: false,
            ..opts(Format::Triples)
        };
        let out = render(&journal(), &options);
        assert!(out.text.contains("I will fail | Thought\n"), "{}", out.text);
        assert!(
            out.text.contains("I will fail | IS_A | Catastrophizing\n"),
            "{}",
            out.text
        );
        assert!(!out.text.contains('«'), "{}", out.text);
        assert!(!out.text.contains("valid "), "{}", out.text);
        let json = render(
            &journal(),
            &RenderOptions {
                include_provenance: false,
                ..opts(Format::Json)
            },
        );
        assert!(!json.text.contains("source_id"), "{}", json.text);
        assert!(!json.text.contains("valid_from"), "{}", json.text);
    }

    #[test]
    fn text_properties_choose_and_order_the_block() {
        let options = RenderOptions {
            text_properties: vec!["body".to_string(), "title".to_string()],
            ..opts(Format::Triples)
        };
        let out = render(&journal(), &options);
        // The first property present names the node; nodes with none of them
        // are named by id.
        assert!(
            out.text.starts_with(
                "#1 | Person\nFelt anxious before the meeting. | Entry | Monday entry\n"
            ),
            "{}",
            out.text
        );
    }

    #[test]
    fn max_body_chars_cuts_at_a_character_boundary() {
        assert_eq!(cut("Сегодня было тяжело", 7), "Сегодня…");
        assert_eq!(cut("Сегодня", 7), "Сегодня");
        assert_eq!(cut("abc", 10), "abc");
        assert_eq!(cut("😀😀😀", 2), "😀😀…");
        let options = RenderOptions {
            max_body_chars: 7,
            ..opts(Format::Triples)
        };
        let out = render(&journal(), &options);
        assert!(
            out.text.contains("Пятница | Entry | Сегодня…\n"),
            "{}",
            out.text
        );
        // `title` names the node and is never cut; `body` is.
        assert!(
            out.text.contains("Monday entry | Entry | Felt an…\n"),
            "{}",
            out.text
        );
    }

    #[test]
    fn max_body_chars_keeps_names_and_titles_whole() {
        let name = "Анастасия Владимировна Пожарская-Константинопольская";
        let title = "Понедельник: тревога перед совещанием";
        let graph = Subgraph {
            nodes: vec![
                RankedNode::seed(
                    node(
                        1,
                        "Person",
                        &[
                            ("name", name),
                            ("description", "Ведёт дневник мыслей каждый вечер"),
                        ],
                    ),
                    1.0,
                ),
                reached(
                    node(
                        2,
                        "Entry",
                        &[
                            ("title", title),
                            ("body", "Сердце колотилось всё утро"),
                            ("text", "Я опозорюсь"),
                        ],
                    ),
                    1,
                ),
                reached(
                    node(
                        3,
                        "Thought",
                        &[("text", "Все заметят, что я не справляюсь")],
                    ),
                    2,
                ),
            ],
            rels: vec![
                rel(10, 1, "WROTE", 2, &[("quote", "мне было очень страшно")]),
                rel(11, 2, "CONTAINS", 3, &[]),
            ],
            truncated: false,
            rel_weights: BTreeMap::new(),
        };
        let options = RenderOptions {
            max_body_chars: 4,
            ..opts(Format::Triples)
        };
        let out = render(&graph, &options);
        assert_eq!(
            out.text,
            format!(
                "{name} | Person | Ведё…\n\
                 {title} | Entry | Серд… — Я оп…\n\
                 {name} | WROTE | {title} | «мне …»\n\
                 Все … | Thought\n\
                 {title} | CONTAINS | Все …\n"
            )
        );
        let json = render(
            &graph,
            &RenderOptions {
                max_body_chars: 4,
                ..opts(Format::Json)
            },
        );
        assert!(
            json.text.contains(&format!(r#""name":"{name}""#)),
            "{}",
            json.text
        );
        assert!(
            json.text.contains(&format!(r#""title":"{title}""#)),
            "{}",
            json.text
        );
        assert!(json.text.contains(r#""body":"Серд…""#), "{}", json.text);
    }

    /// A seed and four equally long neighbours.
    fn star() -> Subgraph {
        let mut nodes = vec![RankedNode::seed(
            node(1, "Topic", &[("name", "Sleep")]),
            1.0,
        )];
        let mut rels = Vec::new();
        for i in 2..=5 {
            nodes.push(reached(
                node(
                    i,
                    "Entry",
                    &[
                        ("title", &format!("Night {i}")),
                        ("body", "slept badly again"),
                    ],
                ),
                1,
            ));
            rels.push(rel(100 + i, 1, "ABOUT", i, &[]));
        }
        Subgraph {
            nodes,
            rels,
            truncated: false,
            rel_weights: BTreeMap::new(),
        }
    }

    #[test]
    fn the_greedy_fill_skips_the_lowest_ranked_that_do_not_fit() {
        for format in [Format::Markdown, Format::Triples, Format::Json] {
            let mut first_three = star();
            first_three.nodes.truncate(3);
            first_three.rels.truncate(2);
            let budget = render(&first_three, &opts(format)).tokens;
            let options = RenderOptions {
                token_budget: budget,
                ..opts(format)
            };
            let out = render(&star(), &options);
            assert_eq!(out.node_ids, [1, 2, 3], "{format:?}");
            assert_eq!(out.rel_ids, [102, 103], "{format:?}");
            assert_eq!(out.skipped, 2, "{format:?}");
            assert_eq!(out.skipped_ids, [4, 5], "{format:?}");
            assert!(out.tokens <= budget, "{format:?}");
            assert_eq!(out.tokens, options.tokenizer.count(&out.text), "{format:?}");
        }
    }

    #[test]
    fn a_node_too_big_to_fit_is_skipped_and_the_fill_goes_on() {
        let mut graph = star();
        graph.nodes[1] = reached(
            node(
                2,
                "Entry",
                &[("title", "Night 2"), ("body", &"long ".repeat(200))],
            ),
            1,
        );
        let mut without = star();
        without.nodes.remove(1);
        without.rels.remove(0);
        let budget = render(&without, &opts(Format::Markdown)).tokens;
        let out = render(
            &graph,
            &RenderOptions {
                token_budget: budget,
                ..opts(Format::Markdown)
            },
        );
        assert_eq!(out.node_ids, [1, 3, 4, 5]);
        assert_eq!(out.skipped, 1);
        assert_eq!(out.skipped_ids, [2]);
        assert!(out.tokens <= budget);
    }

    #[test]
    fn the_budget_holds_under_both_tokenizers() {
        for tokenizer in [Tokenizer::Approx, Tokenizer::Words] {
            for format in [Format::Markdown, Format::Triples, Format::Json] {
                for budget in [50, 60, 80, 120, 200] {
                    let options = RenderOptions {
                        format,
                        tokenizer,
                        token_budget: budget,
                        ..RenderOptions::default()
                    };
                    let out = render(&journal(), &options);
                    assert!(out.tokens <= budget, "{format:?} {tokenizer:?} {budget}");
                    assert_eq!(out.tokens, tokenizer.count(&out.text));
                    assert_eq!(out.node_ids.len() + out.skipped, 6);
                    assert_eq!(out.skipped_ids.len(), out.skipped);
                    assert_eq!(out.node_tokens.len(), out.node_ids.len());
                    assert_eq!(
                        out.node_tokens.iter().sum::<usize>(),
                        out.tokens,
                        "{format:?} {tokenizer:?} {budget}"
                    );
                }
            }
        }
    }

    #[test]
    fn nothing_fits_gives_empty_text() {
        let huge = Subgraph {
            nodes: vec![RankedNode::seed(
                node(1, "Entry", &[("title", &"word ".repeat(400))]),
                1.0,
            )],
            rels: Vec::new(),
            truncated: false,
            rel_weights: BTreeMap::new(),
        };
        let out = render(
            &huge,
            &RenderOptions {
                token_budget: 50,
                max_body_chars: 10_000,
                ..RenderOptions::default()
            },
        );
        assert_eq!(out.text, "");
        assert_eq!(out.tokens, 0);
        assert!(out.node_ids.is_empty());
        assert_eq!(out.skipped, 1);
        assert_eq!(out.skipped_ids, [1]);
        assert!(out.node_tokens.is_empty());
        let empty = render(&Subgraph::default(), &RenderOptions::default());
        assert_eq!(empty, Rendered::default());
    }

    #[test]
    fn non_string_values_are_written_as_text() {
        let mut props = BTreeMap::new();
        props.insert("name".to_string(), Value::String("Dose".to_string()));
        props.insert("body".to_string(), Value::Integer(20));
        props.insert(
            "text".to_string(),
            Value::List(vec![Value::String("a".to_string()), Value::Float(1.5)]),
        );
        props.insert("description".to_string(), Value::Bool(true));
        let graph = Subgraph {
            nodes: vec![RankedNode::seed(
                Arc::new(NodeValue {
                    id: 9,
                    uuid: [0; 16],
                    labels: vec!["Med".to_string(), "Note".to_string()],
                    properties: props,
                }),
                1.0,
            )],
            rels: Vec::new(),
            truncated: false,
            rel_weights: BTreeMap::new(),
        };
        let out = render(&graph, &opts(Format::Markdown));
        assert_eq!(
            out.text,
            "## Seeds\n- Dose (kind=Med:Note, id=9): true — 20 — a, 1.5\n\n\
             ## Neighbours\n- (none)\n\n## Edges\n- (none)\n"
        );
    }

    #[test]
    fn each_written_node_carries_the_tokens_it_added() {
        for tokenizer in [Tokenizer::Approx, Tokenizer::Words] {
            for format in [Format::Markdown, Format::Triples, Format::Json] {
                let options = RenderOptions {
                    format,
                    tokenizer,
                    ..RenderOptions::default()
                };
                let out = render(&star(), &options);
                assert_eq!(out.node_ids, [1, 2, 3, 4, 5], "{format:?}");
                assert_eq!(out.node_tokens.len(), 5, "{format:?}");
                assert!(out.node_tokens.iter().all(|t| *t > 0), "{format:?}");
                assert_eq!(
                    out.node_tokens.iter().sum::<usize>(),
                    out.tokens,
                    "{format:?} {tokenizer:?}"
                );
                assert!(out.skipped_ids.is_empty());
            }
        }
    }

    #[test]
    fn format_names_round_trip() {
        for f in [Format::Markdown, Format::Triples, Format::Json] {
            assert_eq!(Format::parse(f.as_str()), Some(f));
        }
        assert_eq!(Format::parse("yaml"), None);
    }
}
