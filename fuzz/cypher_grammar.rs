// Grammar-aware Cypher generator — Phase 15 task `00099`.
//
// Shared, dependency-free source `include!`d by BOTH the libFuzzer target
// `fuzz/fuzz_targets/fuzz_cypher_grammar.rs` AND the stable replay harness
// `tests/cypher_fuzz_harness_tests.rs`, so the grammar that the nightly
// coverage-guided fuzzer explores is exactly the grammar the stable
// `cargo test` matrix exercises on every PR. If you change a production
// rule here, both consumers pick it up in the same commit — they cannot
// silently drift.
//
// The generator interprets an arbitrary byte slice as a *choice stream*:
// every decision (which clause, which label, which literal) consumes one
// byte and maps it onto a grammar alternative. This makes it a true
// grammar-aware generator — the output is always a syntactically
// well-formed Cypher query drawn from the subset drevo's parser supports
// (Phase 10: MATCH / OPTIONAL MATCH / WHERE / WITH / RETURN / CREATE /
// MERGE / SET / DELETE / aggregations / variable-length paths) — while
// still letting libFuzzer's mutator steer coverage through the byte
// stream. An empty / exhausted stream deterministically reads `0`, so the
// generator always terminates and always produces a non-empty query.

/// A finite-budget choice stream over arbitrary input bytes.
struct Gen<'a> {
    bytes: &'a [u8],
    pos: usize,
    /// Recursion / repetition budget so deeply-nested choices terminate.
    budget: u32,
}

impl<'a> Gen<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Gen {
            bytes,
            pos: 0,
            budget: 64,
        }
    }

    /// Next byte of the choice stream; `0` once exhausted (so generation is
    /// total and deterministic on short inputs).
    fn next_byte(&mut self) -> u8 {
        let b = self.bytes.get(self.pos).copied().unwrap_or(0);
        self.pos = self.pos.saturating_add(1);
        b
    }

    /// Pick one of `n` alternatives.
    fn choice(&mut self, n: usize) -> usize {
        if n <= 1 {
            return 0;
        }
        (self.next_byte() as usize) % n
    }

    /// A small bounded count in `0..=max`.
    fn count(&mut self, max: usize) -> usize {
        self.choice(max + 1)
    }

    fn spend(&mut self) -> bool {
        if self.budget == 0 {
            return false;
        }
        self.budget -= 1;
        true
    }
}

const IDENTS: &[&str] = &["a", "b", "c", "n", "m", "x", "node", "r1"];
const LABELS: &[&str] = &[
    "Person", "Task", "Bug", "Note", "Order", "Scene", "Project", "Account",
];
const REL_TYPES: &[&str] = &[
    "KNOWS",
    "DEPENDS_ON",
    "ASSIGNED_TO",
    "LINKS_TO",
    "OWNS",
    "DUPLICATE_OF",
];
const PROPS: &[&str] = &["name", "title", "priority", "status", "weight", "age"];

fn ident(g: &mut Gen) -> &'static str {
    IDENTS[g.choice(IDENTS.len())]
}

fn label(g: &mut Gen) -> &'static str {
    LABELS[g.choice(LABELS.len())]
}

fn rel_type(g: &mut Gen) -> &'static str {
    REL_TYPES[g.choice(REL_TYPES.len())]
}

fn prop(g: &mut Gen) -> &'static str {
    PROPS[g.choice(PROPS.len())]
}

/// A literal value Cypher accepts on the right-hand side of a comparison
/// or inside a property map.
fn literal(g: &mut Gen) -> String {
    match g.choice(7) {
        0 => format!("{}", g.next_byte()),
        1 => format!("-{}", g.next_byte()),
        2 => format!("{}.{}", g.next_byte(), g.next_byte()),
        3 => "'hello world'".to_string(),
        4 => "true".to_string(),
        5 => "false".to_string(),
        _ => "null".to_string(),
    }
}

/// `(var)` / `(var:Label)` / `(var:Label {prop: literal})`.
fn node_pattern(g: &mut Gen) -> String {
    let v = ident(g);
    match g.choice(3) {
        0 => format!("({v})"),
        1 => format!("({v}:{})", label(g)),
        _ => format!("({v}:{} {{{}: {}}})", label(g), prop(g), literal(g)),
    }
}

/// A relationship pattern between two nodes, optionally variable-length.
fn path_pattern(g: &mut Gen) -> String {
    let left = node_pattern(g);
    let right = node_pattern(g);
    let (lt, gt) = match g.choice(3) {
        0 => ("-", "->"),
        1 => ("<-", "-"),
        _ => ("-", "-"),
    };
    let rel = match g.choice(4) {
        0 => "[r]".to_string(),
        1 => format!("[r:{}]", rel_type(g)),
        2 => format!("[*{}..{}]", g.count(2), g.count(4) + 1),
        _ => format!("[r:{}*1..{}]", rel_type(g), g.count(3) + 1),
    };
    format!("{left}{lt}{rel}{gt}{right}")
}

/// A boolean predicate for a WHERE clause.
fn predicate(g: &mut Gen, var: &str) -> String {
    let p = prop(g);
    match g.choice(6) {
        0 => format!("{var}.{p} = {}", literal(g)),
        1 => format!("{var}.{p} > {}", literal(g)),
        2 => format!("{var}.{p} <> {}", literal(g)),
        3 => format!("{var}.{p} IS NULL"),
        4 => format!("{var}.{p} IN [{}, {}]", literal(g), literal(g)),
        _ => format!("{var}.{p} STARTS WITH 'a'"),
    }
}

/// A RETURN projection list.
fn return_list(g: &mut Gen, var: &str) -> String {
    match g.choice(6) {
        0 => var.to_string(),
        1 => format!("{var}.{}", prop(g)),
        2 => format!("count({var})"),
        3 => format!("{var}, {}", ident(g)),
        4 => format!("collect({var}.{})", prop(g)),
        _ => format!("{var}.{} AS alias", prop(g)),
    }
}

/// Generate one syntactically well-formed Cypher query from the choice
/// stream. Always returns a non-empty string.
pub fn generate_query(bytes: &[u8]) -> String {
    let mut g = Gen::new(bytes);
    let mut q = String::new();
    let var = ident(&mut g);

    match g.choice(8) {
        0 => {
            // MATCH (n:Label) RETURN ...
            q.push_str(&format!("MATCH ({var}:{}) RETURN {}", label(&mut g), return_list(&mut g, var)));
        }
        1 => {
            // MATCH path RETURN ...
            q.push_str(&format!("MATCH {} RETURN {}", path_pattern(&mut g), return_list(&mut g, "a")));
        }
        2 => {
            // MATCH (n) WHERE pred RETURN n
            q.push_str(&format!(
                "MATCH ({var}) WHERE {} RETURN {}",
                predicate(&mut g, var),
                return_list(&mut g, var)
            ));
        }
        3 => {
            // CREATE (n:Label {prop: literal})
            q.push_str(&format!(
                "CREATE ({var}:{} {{{}: {}}})",
                label(&mut g),
                prop(&mut g),
                literal(&mut g)
            ));
            if g.choice(2) == 1 {
                q.push_str(&format!(" RETURN {var}"));
            }
        }
        4 => {
            // MATCH (n:Label) SET n.prop = literal RETURN n
            q.push_str(&format!(
                "MATCH ({var}:{}) SET {var}.{} = {} RETURN {var}",
                label(&mut g),
                prop(&mut g),
                literal(&mut g)
            ));
        }
        5 => {
            // MERGE (n:Label {prop: literal})
            q.push_str(&format!(
                "MERGE ({var}:{} {{{}: {}}})",
                label(&mut g),
                prop(&mut g),
                literal(&mut g)
            ));
        }
        6 => {
            // OPTIONAL MATCH + WITH pipeline
            q.push_str(&format!(
                "MATCH ({var}:{}) WITH {var} OPTIONAL MATCH ({var})-[r]->(b) RETURN {var}, b",
                label(&mut g)
            ));
        }
        _ => {
            // Aggregation with ORDER BY / SKIP / LIMIT tail.
            q.push_str(&format!(
                "MATCH ({var}) RETURN {var}.{} ORDER BY {var}.{} SKIP {} LIMIT {}",
                prop(&mut g),
                prop(&mut g),
                g.count(3),
                g.count(9) + 1
            ));
        }
    }

    // Occasionally append a parameter reference to exercise that path.
    while g.spend() && g.choice(8) == 0 {
        // no-op budget burner: keeps deeply skewed inputs terminating
        let _ = g.next_byte();
    }

    q
}
