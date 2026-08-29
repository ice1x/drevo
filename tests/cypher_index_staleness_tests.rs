//! Index-staleness guards for mixed write-then-read statements (RFC #307,
//! durability track): the native secondary indexes are synced from the
//! change-feed *between* statements, so **within** a statement that has
//! already written, index-narrowed candidate sets can be missing this
//! statement's own writes — a false negative that would return wrong rows.
//!
//! The rule under test: once a statement has performed any write, the
//! executor must stop trusting index narrowing (falling back to the scan +
//! exact filter, which reads the live engine), so a stale index can only
//! ever cost speed, never answers — the same invariant the value cache and
//! the read mirror hold.
//!
//! Every case compares the KV engine (always index-free at this layer)
//! against a native engine executing THROUGH the indexed entry point with
//! indexes synced before the statement — the exact serving shape of the
//! durable-native track.

use std::collections::HashMap;

use drevo::cypher::executor::{
    execute, execute_on_engine_with_indexes_and_values, ExecResult, Value,
};
use drevo::cypher::parser::parse;
use drevo::db::Drevo;
use drevo::native::NativeGraph;
use drevo::native_label_index::NativeLabelIndex;
use drevo::native_property_index::NativePropertyIndex;
use drevo::native_value_cache::NativeValueCache;

/// A KV + indexed-native pair where every statement (seeds included) runs
/// through the indexed entry, with all indexes re-synced before each
/// statement — statements are the staleness unit, exactly like the serving
/// layer.
struct IndexedPair {
    kv: Drevo,
    native: NativeGraph,
    labels: NativeLabelIndex,
    props: NativePropertyIndex,
    values: NativeValueCache,
}

impl IndexedPair {
    fn new() -> Self {
        Self {
            kv: Drevo::open_in_memory().expect("open"),
            native: NativeGraph::new(),
            labels: NativeLabelIndex::new(),
            props: NativePropertyIndex::new(),
            values: NativeValueCache::new(),
        }
    }

    /// Run one statement on both engines (indexes synced up to — but not
    /// within — the statement) and assert identical rows.
    fn check(&mut self, source: &str) -> ExecResult {
        self.labels.sync(&self.native);
        self.props.sync(&self.native);
        self.values.sync(&self.native);
        let q = parse(source).expect("parse");
        let kv =
            execute(&q, &self.kv, HashMap::new()).unwrap_or_else(|e| panic!("kv `{source}`: {e}"));
        let native = execute_on_engine_with_indexes_and_values(
            &q,
            &self.native,
            None,
            Some(&self.labels),
            Some(&self.props),
            Some(&self.values),
            HashMap::new(),
        )
        .unwrap_or_else(|e| panic!("native `{source}`: {e}"));
        assert_eq!(kv.rows, native.rows, "engines disagree on `{source}`");
        native
    }
}

fn int(result: &ExecResult) -> i64 {
    match result.rows[0].as_slice() {
        [Value::Integer(n)] => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn create_then_property_map_match_in_one_statement() {
    let mut pair = IndexedPair::new();
    let r = pair.check("CREATE (:W {k: 7}) WITH 1 AS one MATCH (m {k: 7}) RETURN count(*)");
    assert_eq!(int(&r), 1, "the statement must see its own write");
}

#[test]
fn create_then_where_equality_in_one_statement() {
    let mut pair = IndexedPair::new();
    let r = pair.check(
        "CREATE (:V {p: 'z', title: 'v1'}) WITH 1 AS one \
         MATCH (n) WHERE n.p = 'z' RETURN count(*)",
    );
    assert_eq!(int(&r), 1);
}

#[test]
fn create_then_secondary_label_match_in_one_statement() {
    let mut pair = IndexedPair::new();
    let r = pair.check(
        "CREATE (:Base:Extra {title: 'm'}) WITH 1 AS one \
         MATCH (x:Extra) RETURN count(*)",
    );
    assert_eq!(
        int(&r),
        1,
        "the secondary label written this statement must match"
    );
}

#[test]
fn update_then_match_new_value_in_one_statement() {
    let mut pair = IndexedPair::new();
    pair.check("CREATE (:Item {title: 'it', k: 1})");
    let r = pair.check("MATCH (n:Item) SET n.k = 8 WITH 1 AS one MATCH (m {k: 8}) RETURN count(*)");
    assert_eq!(
        int(&r),
        1,
        "the updated value must be matchable in-statement"
    );
}

#[test]
fn delete_then_match_old_value_in_one_statement() {
    let mut pair = IndexedPair::new();
    pair.check("CREATE (:Gone {title: 'g', k: 1})");
    // A stale index can also over-approximate (the deleted node is still in
    // its postings) — the exact filter must drop it. Guards the superset
    // direction stays safe too.
    let r = pair.check(
        "MATCH (n {k: 1}) DETACH DELETE n WITH 1 AS one \
         MATCH (m {k: 1}) RETURN count(*)",
    );
    assert_eq!(int(&r), 0);
}

#[test]
fn reads_before_any_write_still_use_the_indexes_between_statements() {
    // Sanity: the staleness rule is per-statement — pure reads after a
    // separately-applied write (with a re-sync in between) keep index
    // parity, matching the differential corpus.
    let mut pair = IndexedPair::new();
    pair.check("CREATE (:P {title: 'a', k: 3}), (:P {title: 'b', k: 4})");
    let r = pair.check("MATCH (n {k: 3}) RETURN count(*)");
    assert_eq!(int(&r), 1);
}
