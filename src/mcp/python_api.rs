//! Python-API introspection catalog backing the `python_api_*` MCP
//! tools (Phase 16 task `00121`).
//!
//! ## What this is
//!
//! The `drevo-py` package exposes a typed Python surface (the `Drevo`
//! handle, the plain-data wrappers, the exception hierarchy, and the
//! `drevo.rag` graph-RAG idioms). MCP clients (Cline, Claude Code,
//! Claude Desktop) want to introspect that surface *without leaving the
//! conversation* — "what's the signature of `create_node`?", "how do I
//! retrieve a RAG context?". This module turns the package's own
//! source-of-truth documents into a queryable catalog that the three
//! `python_api_*` tools serve.
//!
//! ## Source of truth — no hand-maintained markdown
//!
//! The catalog is built from documents that already ship with the
//! package and are themselves the contract:
//!
//! - the PEP 561 type stubs `drevo-py/python/drevo/__init__.pyi` and
//!   `drevo-py/python/drevo/rag/__init__.pyi` — every public symbol,
//!   its signature, and its docstring (the stubs are "the source of
//!   truth for signatures" per RFC §3.3);
//! - the package `drevo-py/README.md` — the fenced `python` code blocks
//!   are the curated usage examples.
//!
//! All three are pulled in at *compile time* via [`include_str!`], so
//! the catalog ships inside the `drevo-mcp` binary and re-derives itself
//! on every release: when a docstring or example changes in the stubs,
//! the next build picks it up automatically. There is no separate,
//! drift-prone copy of the API docs to maintain.
//!
//! ## Catalog shape
//!
//! - [`ApiSymbol`] — one public name (module, class, enum, exception,
//!   method, function, attribute, or constant) with its qualified name,
//!   signature, and docstring.
//! - [`ApiExample`] — one runnable snippet lifted from a README fenced
//!   block, tagged with the heading it sat under.
//!
//! The catalog is parsed once and memoised behind a [`OnceLock`]
//! ([`ApiCatalog::builtin`]); the tools borrow the shared instance.

use std::sync::OnceLock;

use serde::Serialize;

/// Type stubs for the top-level `drevo` package — the signature +
/// docstring contract for `Drevo`, the data wrappers, and the errors.
const PYI_ROOT: &str = include_str!("../../drevo-py/python/drevo/__init__.pyi");
/// Type stubs for `drevo.rag` — the graph-RAG idioms layer.
const PYI_RAG: &str = include_str!("../../drevo-py/python/drevo/rag/__init__.pyi");
/// Package README — its fenced `python` blocks are the example corpus.
const README: &str = include_str!("../../drevo-py/README.md");

/// The kind of a public Python symbol, used by clients to filter and
/// render the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    /// A module (`drevo`, `drevo.rag`) — carries the module docstring.
    Module,
    /// A plain `class` (data wrapper, protocol, retriever, …).
    Class,
    /// A class whose base is an `enum.*` type (e.g. `Direction`).
    Enum,
    /// A class in the exception hierarchy (name ends `Error` or a base
    /// is `Exception` / an `*Error`).
    Exception,
    /// A function defined inside a class body.
    Method,
    /// A module-level function.
    Function,
    /// A class-body attribute annotation (`id: int`).
    Attribute,
    /// A module-level annotated constant (`__version__: str`).
    Constant,
}

impl SymbolKind {
    /// The lowercase wire name, matching the serde representation.
    pub fn as_str(self) -> &'static str {
        match self {
            SymbolKind::Module => "module",
            SymbolKind::Class => "class",
            SymbolKind::Enum => "enum",
            SymbolKind::Exception => "exception",
            SymbolKind::Method => "method",
            SymbolKind::Function => "function",
            SymbolKind::Attribute => "attribute",
            SymbolKind::Constant => "constant",
        }
    }
}

/// One public symbol of the Python API.
#[derive(Debug, Clone, Serialize)]
pub struct ApiSymbol {
    /// Fully-qualified dotted name, e.g. `drevo.Drevo.create_node`.
    pub name: String,
    /// The owning module — `drevo` or `drevo.rag`.
    pub module: String,
    /// The enclosing class' qualified name for methods / attributes,
    /// `None` for module-level symbols.
    pub parent: Option<String>,
    /// What sort of symbol this is.
    pub kind: SymbolKind,
    /// The one-line signature, body stripped — e.g.
    /// `def create_node(self, new_node: NewNode) -> Node` or
    /// `class Direction(enum.IntEnum)`.
    pub signature: String,
    /// The associated docstring, dedented; empty string when the stub
    /// declares no docstring (most method stubs).
    pub docstring: String,
}

impl ApiSymbol {
    /// The final dotted segment — the unqualified name a user is most
    /// likely to type (`create_node`, `Drevo`, `DrevoError`).
    pub fn simple_name(&self) -> &str {
        self.name.rsplit('.').next().unwrap_or(&self.name)
    }
}

/// One usage example lifted from a README fenced `python` block.
#[derive(Debug, Clone, Serialize)]
pub struct ApiExample {
    /// The nearest preceding markdown heading — the example's intent
    /// label (e.g. "Examples — create and read a node").
    pub title: String,
    /// The example source code, verbatim (fence markers stripped).
    pub code: String,
    /// Where the snippet came from, e.g. `drevo-py/README.md`.
    pub source: String,
}

/// The parsed, queryable Python-API surface.
pub struct ApiCatalog {
    symbols: Vec<ApiSymbol>,
    examples: Vec<ApiExample>,
}

impl ApiCatalog {
    /// The process-wide catalog, parsed once from the embedded stubs
    /// and README and memoised for the lifetime of the process.
    pub fn builtin() -> &'static ApiCatalog {
        static CATALOG: OnceLock<ApiCatalog> = OnceLock::new();
        CATALOG.get_or_init(|| {
            let mut symbols = parse_pyi("drevo", PYI_ROOT);
            symbols.extend(parse_pyi("drevo.rag", PYI_RAG));
            let examples = extract_examples(README, "drevo-py/README.md");
            ApiCatalog { symbols, examples }
        })
    }

    /// Build a catalog from explicit sources — used by tests to drive
    /// the parser with fixed fixtures.
    pub fn from_sources(modules: &[(&str, &str)], example_docs: &[(&str, &str)]) -> Self {
        let mut symbols = Vec::new();
        for (module, src) in modules {
            symbols.extend(parse_pyi(module, src));
        }
        let mut examples = Vec::new();
        for (name, src) in example_docs {
            examples.extend(extract_examples(src, name));
        }
        ApiCatalog { symbols, examples }
    }

    /// Every symbol in the catalog, in declaration order.
    pub fn symbols(&self) -> &[ApiSymbol] {
        &self.symbols
    }

    /// Every example in the catalog.
    pub fn examples(&self) -> &[ApiExample] {
        &self.examples
    }

    /// Enumerate symbols whose qualified or simple name starts with
    /// `prefix` (case-insensitive). An empty prefix lists everything.
    /// Results are sorted by qualified name for stable client output.
    pub fn list(&self, prefix: &str) -> Vec<&ApiSymbol> {
        let needle = prefix.trim().to_lowercase();
        let mut out: Vec<&ApiSymbol> = self
            .symbols
            .iter()
            .filter(|s| {
                needle.is_empty()
                    || s.name.to_lowercase().starts_with(&needle)
                    || s.simple_name().to_lowercase().starts_with(&needle)
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Resolve a single symbol by name. Tries, in order, an exact
    /// qualified match, a qualified-suffix match (`.name`), then a bare
    /// simple-name match — first case-sensitively, then (if nothing
    /// matched) case-insensitively. The case-sensitive pass first is
    /// what lets the class `drevo.Drevo` and the module `drevo` —
    /// which differ only in case — both be addressable: `"Drevo"`
    /// resolves the class, `"drevo"` the module. Returns `None` when
    /// nothing matches.
    pub fn describe(&self, name: &str) -> Option<&ApiSymbol> {
        let q = name.trim();
        if q.is_empty() {
            return None;
        }
        self.resolve(q, false).or_else(|| self.resolve(q, true))
    }

    /// One resolution pass. `ci` selects case-insensitive comparison.
    fn resolve(&self, q: &str, ci: bool) -> Option<&ApiSymbol> {
        let eq = |a: &str, b: &str| {
            if ci {
                a.eq_ignore_ascii_case(b)
            } else {
                a == b
            }
        };
        let ends = |a: &str, b: &str| {
            if ci {
                a.to_lowercase().ends_with(&b.to_lowercase())
            } else {
                a.ends_with(b)
            }
        };
        // 1. exact qualified name
        if let Some(s) = self.symbols.iter().find(|s| eq(&s.name, q)) {
            return Some(s);
        }
        // 2. qualified suffix (someone typed `Drevo.create_node`)
        let suffix = format!(".{q}");
        if let Some(s) = self.symbols.iter().find(|s| ends(&s.name, &suffix)) {
            return Some(s);
        }
        // 3. bare simple name (`create_node`)
        self.symbols.iter().find(|s| eq(s.simple_name(), q))
    }

    /// Up to `limit` symbol names that look close to `name` — used to
    /// soften a [`Self::describe`] miss with "did you mean …".
    pub fn suggest(&self, name: &str, limit: usize) -> Vec<&str> {
        let q = name.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<&str> = self
            .symbols
            .iter()
            .filter(|s| {
                let simple = s.simple_name().to_lowercase();
                simple.contains(&q) || q.contains(&simple) || s.name.to_lowercase().contains(&q)
            })
            .map(|s| s.name.as_str())
            .collect();
        out.sort();
        out.dedup();
        out.truncate(limit);
        out
    }

    /// Fuzzy-search the example corpus for snippets matching `intent`.
    ///
    /// Scoring is token-overlap: the intent is split into lowercase
    /// alphanumeric tokens, and each example is scored by how many of
    /// those tokens appear in its title (weighted ×3) or its code body.
    /// Examples with a zero score are dropped; ties keep declaration
    /// order. Returns at most `limit` hits, best first.
    pub fn search_examples(&self, intent: &str, limit: usize) -> Vec<&ApiExample> {
        let tokens = tokenize(intent);
        if tokens.is_empty() || limit == 0 {
            return Vec::new();
        }
        let mut scored: Vec<(i64, usize, &ApiExample)> = self
            .examples
            .iter()
            .enumerate()
            .map(|(idx, ex)| {
                let title_tokens = tokenize(&ex.title);
                let code_tokens = tokenize(&ex.code);
                let mut score = 0i64;
                for t in &tokens {
                    if title_tokens.iter().any(|x| x == t) {
                        score += 3;
                    }
                    if code_tokens.iter().any(|x| x == t) {
                        score += 1;
                    }
                }
                (score, idx, ex)
            })
            .filter(|(score, _, _)| *score > 0)
            .collect();
        // Highest score first; stable on declaration order for ties.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        scored
            .into_iter()
            .take(limit)
            .map(|(_, _, ex)| ex)
            .collect()
    }
}

// ── Parser ─────────────────────────────────────────────────────────────

/// Parse a `.pyi` stub into a flat list of [`ApiSymbol`]s under
/// `module`.
///
/// The stubs are regular enough to parse line-by-line without a full
/// Python grammar: top-level `class`/`def`, class-body `def`/attribute,
/// module-level constants, and triple-quoted docstrings attached to the
/// preceding declaration. Decorators, imports, `__all__`, and type
/// aliases are skipped. Multi-line signatures (parenthesised parameter
/// lists) are folded into a single logical line.
fn parse_pyi(module: &str, src: &str) -> Vec<ApiSymbol> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out: Vec<ApiSymbol> = Vec::new();
    let mut i = 0;
    let mut current_class: Option<String> = None;
    let mut class_indent = 0usize;
    let mut awaiting_doc: Option<usize> = None;
    let mut module_doc_taken = false;

    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim_start();
        let indent = raw.len() - raw.trim_start().len();

        // Blank lines and comments never carry a symbol or docstring.
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }

        // Docstring — attach to the awaiting declaration, or treat as
        // the module docstring if it leads the file.
        if trimmed.starts_with("\"\"\"") || trimmed.starts_with("'''") {
            let (doc, next) = read_docstring(&lines, i);
            i = next;
            if let Some(idx) = awaiting_doc.take() {
                if out[idx].docstring.is_empty() {
                    out[idx].docstring = doc;
                }
            } else if !module_doc_taken {
                out.push(ApiSymbol {
                    name: module.to_string(),
                    module: module.to_string(),
                    parent: None,
                    kind: SymbolKind::Module,
                    signature: format!("module {module}"),
                    docstring: doc,
                });
                module_doc_taken = true;
            }
            continue;
        }

        // Decorators precede a def/class; skip them but keep waiting
        // for that declaration to set `awaiting_doc`.
        if trimmed.starts_with('@') {
            i += 1;
            continue;
        }

        // Imports / future statements — never symbols.
        if trimmed.starts_with("import ")
            || trimmed.starts_with("from ")
            || trimmed == "from __future__ import annotations"
        {
            awaiting_doc = None;
            i += 1;
            continue;
        }

        // Leaving a class scope: any statement at or below the class'
        // own indent closes the class body.
        if current_class.is_some() && indent <= class_indent {
            current_class = None;
        }

        let (stmt, next) = read_logical(&lines, i);
        let stmt = stmt.trim().to_string();

        if let Some(rest) = stmt.strip_prefix("class ") {
            let (simple, signature, kind) = parse_class_header(rest);
            current_class = Some(simple.clone());
            class_indent = indent;
            out.push(ApiSymbol {
                name: format!("{module}.{simple}"),
                module: module.to_string(),
                parent: None,
                kind,
                signature,
                docstring: String::new(),
            });
            awaiting_doc = Some(out.len() - 1);
            i = next;
            continue;
        }

        if stmt.starts_with("def ") || stmt.starts_with("async def ") {
            let simple = def_name(&stmt);
            let signature = strip_stub_body(&stmt);
            let (name, parent, kind) = match &current_class {
                Some(cls) => (
                    format!("{module}.{cls}.{simple}"),
                    Some(format!("{module}.{cls}")),
                    SymbolKind::Method,
                ),
                None => (format!("{module}.{simple}"), None, SymbolKind::Function),
            };
            out.push(ApiSymbol {
                name,
                module: module.to_string(),
                parent,
                kind,
                signature,
                docstring: String::new(),
            });
            awaiting_doc = Some(out.len() - 1);
            i = next;
            continue;
        }

        if let Some((simple, signature)) = parse_annotation(&stmt) {
            let (name, parent, kind) = match &current_class {
                Some(cls) => (
                    format!("{module}.{cls}.{simple}"),
                    Some(format!("{module}.{cls}")),
                    SymbolKind::Attribute,
                ),
                None => (format!("{module}.{simple}"), None, SymbolKind::Constant),
            };
            out.push(ApiSymbol {
                name,
                module: module.to_string(),
                parent,
                kind,
                signature,
                docstring: String::new(),
            });
            awaiting_doc = Some(out.len() - 1);
            i = next;
            continue;
        }

        // Anything else (`__all__ = [...]`, type aliases) — closes the
        // docstring window and is not itself a symbol.
        awaiting_doc = None;
        i = next;
    }

    out
}

/// Read a logical statement starting at `start`, folding continuation
/// lines while brackets are unbalanced. Returns the joined, whitespace-
/// normalised text and the index just past the statement.
fn read_logical(lines: &[&str], start: usize) -> (String, usize) {
    let mut depth = 0i32;
    let mut parts: Vec<&str> = Vec::new();
    let mut i = start;
    while i < lines.len() {
        let line = lines[i];
        parts.push(line.trim());
        depth += bracket_delta(line);
        i += 1;
        if depth <= 0 {
            break;
        }
    }
    let joined = parts.join(" ");
    (normalise_signature(&joined), i)
}

/// Net change in bracket depth across one physical line, counting
/// `()[]{}`. Stub signatures contain no string-embedded brackets, so a
/// naive count is sufficient here.
fn bracket_delta(line: &str) -> i32 {
    let mut d = 0i32;
    for c in line.chars() {
        match c {
            '(' | '[' | '{' => d += 1,
            ')' | ']' | '}' => d -= 1,
            _ => {}
        }
    }
    d
}

/// Collapse runs of whitespace to single spaces and tidy the spacing
/// that line-folding leaves around brackets and commas.
fn normalise_signature(s: &str) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .replace("( ", "(")
        .replace(" )", ")")
        .replace("[ ", "[")
        .replace(" ]", "]")
        .replace(" ,", ",")
        .replace(",)", ")")
        .replace(",]", "]")
}

/// Parse a class header (everything after `class `): returns the simple
/// name, the `class …` signature (trailing `:` stripped), and the kind
/// inferred from the base list.
fn parse_class_header(rest: &str) -> (String, String, SymbolKind) {
    // `rest` looks like `Node:` or `Direction(enum.IntEnum):` or
    // `NotFoundError(DrevoError):`.
    let head = rest.trim_end().trim_end_matches(':').trim();
    let (simple, bases) = match head.find('(') {
        Some(open) => {
            let name = head[..open].trim().to_string();
            let bases = head[open + 1..].trim_end_matches(')').to_string();
            (name, bases)
        }
        None => (head.to_string(), String::new()),
    };
    let kind =
        if simple.ends_with("Error") || bases.contains("Exception") || bases.contains("Error") {
            SymbolKind::Exception
        } else if bases.contains("Enum") || bases.contains("enum.") {
            SymbolKind::Enum
        } else {
            SymbolKind::Class
        };
    let signature = format!("class {head}");
    (simple, signature, kind)
}

/// Extract the function name from a `def …` / `async def …` statement.
fn def_name(stmt: &str) -> String {
    let after = stmt
        .strip_prefix("async def ")
        .or_else(|| stmt.strip_prefix("def "))
        .unwrap_or(stmt);
    after.split('(').next().unwrap_or(after).trim().to_string()
}

/// Strip the stub body (`: ...`) off a `def` statement, leaving just
/// the signature: `def f(self) -> Node: ...` → `def f(self) -> Node`.
fn strip_stub_body(stmt: &str) -> String {
    let mut s = stmt.trim().to_string();
    if let Some(stripped) = s.strip_suffix("...") {
        s = stripped.trim_end().to_string();
    }
    if let Some(stripped) = s.strip_suffix(':') {
        s = stripped.trim_end().to_string();
    }
    s
}

/// Parse an annotation statement (`name: type` or `name: type = default`)
/// into `(simple_name, "name: type")`. Returns `None` when the line is
/// not a bare annotated name (e.g. `__all__: Final[...] = [...]`, a call,
/// or a plain assignment).
fn parse_annotation(stmt: &str) -> Option<(String, String)> {
    let stmt = stmt.trim();
    // Find the annotation colon at bracket-depth 0.
    let mut depth = 0i32;
    let mut colon = None;
    for (idx, c) in stmt.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ':' if depth == 0 => {
                colon = Some(idx);
                break;
            }
            '=' if depth == 0 => return None, // assignment without annotation
            _ => {}
        }
    }
    let colon = colon?;
    let name = stmt[..colon].trim();
    // A bare identifier only — reject anything with spaces, calls, etc.
    if name.is_empty()
        || name == "__all__"
        || !name.chars().all(|c| c.is_alphanumeric() || c == '_')
    {
        return None;
    }
    // Signature = `name: type`, dropping any `= default` tail.
    let rhs = stmt[colon + 1..].trim();
    let typ = match rhs.find('=') {
        Some(eq) => rhs[..eq].trim(),
        None => rhs,
    };
    if typ.is_empty() {
        return None;
    }
    Some((name.to_string(), format!("{name}: {typ}")))
}

/// Read a triple-quoted docstring starting at `start` (whose trimmed
/// line opens with `"""` or `'''`). Returns the dedented text and the
/// index just past the closing quote.
fn read_docstring(lines: &[&str], start: usize) -> (String, usize) {
    let first = lines[start].trim_start();
    let quote = if first.starts_with("\"\"\"") {
        "\"\"\""
    } else {
        "'''"
    };
    let after = &first[3..];
    // Single-line docstring: `"""text"""` on one line.
    if let Some(end) = after.find(quote) {
        return (after[..end].trim().to_string(), start + 1);
    }
    let mut body: Vec<String> = Vec::new();
    if !after.trim().is_empty() {
        body.push(after.trim().to_string());
    }
    let mut i = start + 1;
    while i < lines.len() {
        let line = lines[i];
        if let Some(end) = line.find(quote) {
            let pre = &line[..end];
            if !pre.trim().is_empty() {
                body.push(pre.to_string());
            }
            i += 1;
            return (dedent(&body), i);
        }
        body.push(line.to_string());
        i += 1;
    }
    // Unterminated docstring (malformed stub) — return what we have.
    (dedent(&body), i)
}

/// Strip the common leading-whitespace prefix from a docstring body and
/// join it back into a single string, trimming surrounding blank lines.
fn dedent(body: &[String]) -> String {
    let min_indent = body
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let joined = body
        .iter()
        .map(|l| {
            if l.len() >= min_indent {
                l[min_indent..].trim_end().to_string()
            } else {
                l.trim().to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    joined.trim().to_string()
}

// ── Examples ───────────────────────────────────────────────────────────

/// Extract fenced ```` ```python ```` blocks from a markdown document.
/// Each block's title is the nearest preceding ATX heading (`#`..`####`).
fn extract_examples(src: &str, source: &str) -> Vec<ApiExample> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    let mut heading = String::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        // Track the most recent heading.
        if let Some(rest) = trimmed.strip_prefix('#') {
            let title = rest.trim_start_matches('#');
            if title.starts_with(' ') {
                heading = title.trim().to_string();
            }
            i += 1;
            continue;
        }
        // Open a python fence.
        if trimmed.starts_with("```python") || trimmed.starts_with("```py") {
            i += 1;
            let mut code: Vec<&str> = Vec::new();
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                code.push(lines[i]);
                i += 1;
            }
            i += 1; // step past the closing fence
            let code = code.join("\n").trim_end().to_string();
            if !code.trim().is_empty() {
                out.push(ApiExample {
                    title: if heading.is_empty() {
                        "example".to_string()
                    } else {
                        heading.clone()
                    },
                    code,
                    source: source.to_string(),
                });
            }
            continue;
        }
        i += 1;
    }
    out
}

/// Lowercase the input and split it into alphanumeric tokens — the unit
/// of comparison for [`ApiCatalog::search_examples`].
fn tokenize(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#""""Module-level docstring for the sample."""

from __future__ import annotations

import enum


class Direction(enum.IntEnum):
    """Traversal direction.

    Spread across two lines.
    """

    OUT = 0
    IN = 1


class Node:
    """A node row."""

    id: int
    title: str

    def __repr__(self) -> str: ...


class DrevoError(Exception):
    """Root error."""


class NodeNotFoundError(DrevoError):
    """Missing node."""


class Drevo:
    """Handle docstring."""

    @classmethod
    def open(cls, path: str) -> Drevo: ...
    def create_node(self, new_node: NewNode) -> Node: ...
    def bfs(
        self,
        start_id: int,
        max_depth: int,
        direction: Direction,
        edge_kind: Optional[str] = ...,
    ) -> list[Node]: ...


__version__: str

__all__: Final[list[str]] = [
    "Drevo",
    "Node",
]
"#;

    fn sample_catalog() -> ApiCatalog {
        ApiCatalog::from_sources(&[("drevo", SAMPLE)], &[])
    }

    #[test]
    fn parses_module_docstring() {
        let cat = sample_catalog();
        let module = cat
            .symbols()
            .iter()
            .find(|s| s.kind == SymbolKind::Module)
            .expect("module symbol");
        assert_eq!(module.name, "drevo");
        assert_eq!(module.docstring, "Module-level docstring for the sample.");
    }

    #[test]
    fn parses_enum_class_with_multiline_docstring() {
        let cat = sample_catalog();
        let dir = cat.describe("Direction").expect("Direction");
        assert_eq!(dir.kind, SymbolKind::Enum);
        assert_eq!(dir.name, "drevo.Direction");
        assert_eq!(dir.signature, "class Direction(enum.IntEnum)");
        assert!(
            dir.docstring.contains("Spread across two lines."),
            "multi-line docstring not captured: {:?}",
            dir.docstring
        );
    }

    #[test]
    fn classifies_exceptions_by_base_and_name() {
        let cat = sample_catalog();
        assert_eq!(
            cat.describe("DrevoError").unwrap().kind,
            SymbolKind::Exception
        );
        assert_eq!(
            cat.describe("NodeNotFoundError").unwrap().kind,
            SymbolKind::Exception
        );
    }

    #[test]
    fn parses_class_attributes() {
        let cat = sample_catalog();
        let id_attr = cat.describe("Node.id").expect("Node.id");
        assert_eq!(id_attr.kind, SymbolKind::Attribute);
        assert_eq!(id_attr.signature, "id: int");
        assert_eq!(id_attr.parent.as_deref(), Some("drevo.Node"));
    }

    #[test]
    fn parses_module_constant() {
        let cat = sample_catalog();
        let v = cat.describe("__version__").expect("__version__");
        assert_eq!(v.kind, SymbolKind::Constant);
        assert_eq!(v.signature, "__version__: str");
    }

    #[test]
    fn skips_dunder_all() {
        let cat = sample_catalog();
        assert!(
            cat.describe("__all__").is_none(),
            "__all__ must not be a catalog symbol"
        );
    }

    #[test]
    fn folds_multiline_method_signature() {
        let cat = sample_catalog();
        let bfs = cat.describe("Drevo.bfs").expect("bfs");
        assert_eq!(bfs.kind, SymbolKind::Method);
        assert_eq!(
            bfs.signature,
            "def bfs(self, start_id: int, max_depth: int, direction: Direction, edge_kind: Optional[str] = ...) -> list[Node]"
        );
    }

    #[test]
    fn classmethod_decorator_does_not_break_method_parse() {
        let cat = sample_catalog();
        let open = cat.describe("Drevo.open").expect("open");
        assert_eq!(open.kind, SymbolKind::Method);
        assert_eq!(open.signature, "def open(cls, path: str) -> Drevo");
    }

    #[test]
    fn describe_resolves_by_simple_qualified_and_suffix() {
        let cat = sample_catalog();
        assert_eq!(
            cat.describe("create_node").unwrap().name,
            "drevo.Drevo.create_node"
        );
        assert_eq!(
            cat.describe("drevo.Drevo.create_node").unwrap().name,
            "drevo.Drevo.create_node"
        );
        assert_eq!(
            cat.describe("Drevo.create_node").unwrap().name,
            "drevo.Drevo.create_node"
        );
        // Exact-case disambiguates the module `drevo` from the class
        // `drevo.Drevo` (they collide case-insensitively).
        assert_eq!(cat.describe("drevo").unwrap().kind, SymbolKind::Module);
        assert_eq!(cat.describe("Drevo").unwrap().name, "drevo.Drevo");
        // Case-insensitive fallback still resolves an unambiguous name.
        assert_eq!(
            cat.describe("CREATE_NODE").unwrap().name,
            "drevo.Drevo.create_node"
        );
    }

    #[test]
    fn describe_miss_returns_none_and_suggests() {
        let cat = sample_catalog();
        assert!(cat.describe("create_nope").is_none());
        let suggestions = cat.suggest("node", 5);
        assert!(
            suggestions.iter().any(|s| s.contains("Node")),
            "expected a Node-ish suggestion, got {suggestions:?}"
        );
    }

    #[test]
    fn list_filters_by_prefix_and_sorts() {
        let cat = sample_catalog();
        let all = cat.list("");
        assert!(
            all.len() >= 6,
            "expected the full surface, got {}",
            all.len()
        );
        // Sorted by qualified name.
        let names: Vec<&str> = all.iter().map(|s| s.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        // Prefix filter on the qualified path.
        let drevo_methods = cat.list("drevo.Drevo");
        assert!(drevo_methods
            .iter()
            .all(|s| s.name.starts_with("drevo.Drevo")));
        assert!(drevo_methods.iter().any(|s| s.name == "drevo.Drevo.bfs"));
    }

    #[test]
    fn extract_examples_pulls_python_fences_with_headings() {
        let md = "# Title\n\n## Quickstart\n\nText.\n\n```python\nimport drevo\nx = 1\n```\n\nMore.\n\n```bash\nnot python\n```\n";
        let examples = extract_examples(md, "README.md");
        assert_eq!(examples.len(), 1, "only the python fence counts");
        assert_eq!(examples[0].title, "Quickstart");
        assert_eq!(examples[0].code, "import drevo\nx = 1");
        assert_eq!(examples[0].source, "README.md");
    }

    #[test]
    fn search_examples_ranks_by_token_overlap() {
        let md = "## Create a node\n\n```python\ndb.create_node(NewNode(kind=\"note\"))\n```\n\n## Vector search\n\n```python\ndrevo.rag.vector_search(db, query)\n```\n";
        let cat = ApiCatalog::from_sources(&[], &[("README.md", md)]);
        let hits = cat.search_examples("how do I create a node", 5);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].title, "Create a node");

        let vec_hits = cat.search_examples("vector search query", 5);
        assert_eq!(vec_hits[0].title, "Vector search");

        // No token overlap → no hits.
        assert!(cat
            .search_examples("kubernetes deployment yaml", 5)
            .is_empty());
    }

    #[test]
    fn builtin_catalog_parses_real_stubs() {
        let cat = ApiCatalog::builtin();
        // Headline symbols from the real stubs must resolve.
        for name in [
            "drevo.Drevo",
            "drevo.Drevo.create_node",
            "drevo.Drevo.search_fts",
            "drevo.Node",
            "drevo.Direction",
            "drevo.DrevoError",
            "drevo.rag.Retriever",
            "drevo.rag.ingest_documents",
            "drevo.rag.vector_search",
        ] {
            assert!(
                cat.describe(name).is_some(),
                "builtin catalog missing {name}"
            );
        }
        // The Direction enum must classify correctly off the real stub.
        assert_eq!(cat.describe("Direction").unwrap().kind, SymbolKind::Enum);
        // And the real README must yield at least one example.
        assert!(
            !cat.examples().is_empty(),
            "expected README python examples"
        );
    }

    #[test]
    fn builtin_describe_carries_signature_and_docstring() {
        let cat = ApiCatalog::builtin();
        let create = cat.describe("create_node").expect("create_node");
        assert_eq!(
            create.signature,
            "def create_node(self, new_node: NewNode) -> Node"
        );
        let drevo = cat.describe("drevo.Drevo").expect("Drevo");
        assert!(
            drevo.docstring.contains("Embedded graph database handle"),
            "Drevo docstring not captured: {:?}",
            drevo.docstring
        );
    }
}
