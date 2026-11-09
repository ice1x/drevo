//! Entity resolution: two spellings of one entity, found, reviewed and
//! merged.
//!
//! - `CALL drevo.entity.candidates(label, config) YIELD a, b, nameScore,
//!   vectorScore, score, reasons` lists the pairs that probably name the same
//!   thing.
//! - `CALL drevo.entity.merge(keepId, dropIds, config) YIELD kept, merged,
//!   relationshipsMoved, propertiesMerged` folds the dropped nodes into the
//!   kept one and records their names as aliases.
//! - `CALL drevo.entity.aliases(name) YIELD entity, aliases` finds an entity
//!   by any of its names.
//! - `drevo.memory.rememberEntity(…, {resolve: 'alias' | 'fuzzy'})` reuses a
//!   known entity under another spelling.
//!
//! The fixture is a literature knowledge base: «Пушкин», «А. С. Пушкин» and
//! «Pushkin» are one poet; «Пушкина» is a street (a LOCATION) and must never
//! pair with him.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use drevo::bolt::packstream::Value as Bolt;
use drevo::bolt::session::{ClientMessage, ServerMessage, Session};
use drevo::cypher::executor::{ExecError, ExecResult, Value};
use drevo::cypher::parser::parse;
use drevo::native::CommitError;
use drevo::native_service::NativeService;

fn run(svc: &NativeService, q: &str) -> Result<ExecResult, ExecError> {
    svc.execute(&parse(q).expect("parse"), HashMap::new())
}

fn run_ok(svc: &NativeService, q: &str) -> ExecResult {
    run(svc, q).unwrap_or_else(|e| panic!("execute `{q}`: {e:?}"))
}

/// The `InvalidProcedureCall` message `q` fails with.
fn invalid_call(svc: &NativeService, q: &str) -> String {
    match run(svc, q) {
        Err(ExecError::InvalidProcedureCall { message, .. }) => message,
        other => panic!("`{q}`: expected InvalidProcedureCall, got {other:?}"),
    }
}

fn int(svc: &NativeService, q: &str) -> i64 {
    match run_ok(svc, q).rows.first().and_then(|r| r.first()) {
        Some(Value::Integer(n)) => *n,
        other => panic!("`{q}`: expected an integer, got {other:?}"),
    }
}

fn string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => panic!("expected a string, got {other:?}"),
    }
}

fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::List(items) => items.iter().map(string).collect(),
        other => panic!("expected a list, got {other:?}"),
    }
}

fn float(v: &Value) -> f64 {
    match v {
        Value::Float(f) => *f,
        other => panic!("expected a float, got {other:?}"),
    }
}

/// The id of the entity called `name` with `type`.
fn id_of(svc: &NativeService, name: &str, entity_type: &str) -> i64 {
    int(
        svc,
        &format!("MATCH (e:Entity {{name: '{name}', type: '{entity_type}'}}) RETURN id(e)"),
    )
}

fn remember(svc: &NativeService, name: &str, entity_type: &str, description: &str) {
    run_ok(
        svc,
        &format!(
            "CALL drevo.memory.rememberEntity(null, '{name}', '{entity_type}', '{description}') \
             YIELD node RETURN node"
        ),
    );
}

/// «Пушкин», «А. С. Пушкин», «Pushkin» (PERSON) and «Пушкина» (LOCATION).
fn poets(svc: &NativeService) {
    remember(svc, "Пушкин", "PERSON", "poet");
    remember(svc, "А. С. Пушкин", "PERSON", "author of Onegin");
    remember(svc, "Pushkin", "PERSON", "Russian poet");
    remember(svc, "Пушкина", "LOCATION", "a street");
}

/// `(a.name, b.name, nameScore, vectorScore, score, reasons)` rows.
type Pair = (String, String, f64, Value, f64, Vec<String>);

fn candidates(svc: &NativeService, config: &str) -> Vec<Pair> {
    run_ok(
        svc,
        &format!(
            "CALL drevo.entity.candidates('Entity', {config}) \
             YIELD a, b, nameScore, vectorScore, score, reasons \
             RETURN a.name, b.name, nameScore, vectorScore, score, reasons"
        ),
    )
    .rows
    .iter()
    .map(|r| {
        (
            string(&r[0]),
            string(&r[1]),
            float(&r[2]),
            r[3].clone(),
            float(&r[4]),
            strings(&r[5]),
        )
    })
    .collect()
}

// ----- candidates ----------------------------------------------------------------

#[test]
fn candidates_pair_the_cyrillic_spellings_with_a_shared_token() {
    let svc = NativeService::in_memory();
    poets(&svc);
    let pairs = candidates(&svc, "{}");
    assert_eq!(pairs.len(), 1, "{pairs:?}");
    let (a, b, name, vector, score, reasons) = &pairs[0];
    assert_eq!((a.as_str(), b.as_str()), ("Пушкин", "А. С. Пушкин"));
    assert_eq!((*name, vector.clone(), *score), (1.0, Value::Null, 1.0));
    assert_eq!(reasons, &["name:1.00", "shared_token:пушкин"]);
}

#[test]
fn the_location_never_pairs_with_the_poet() {
    let svc = NativeService::in_memory();
    poets(&svc);
    for blocking in ["prefix3", "token", "none"] {
        let pairs = candidates(&svc, &format!("{{blocking: '{blocking}', threshold: 0.0}}"));
        assert!(
            pairs
                .iter()
                .all(|(a, b, ..)| a != "Пушкина" && b != "Пушкина"),
            "{blocking}: {pairs:?}"
        );
    }
    // Without blocking every PERSON pair is scored, the Latin one too.
    let all = candidates(&svc, "{blocking: 'none', threshold: 0.0}");
    assert_eq!(all.len(), 3, "{all:?}");
}

#[test]
fn candidates_come_best_first_with_the_older_node_as_a() {
    let svc = NativeService::in_memory();
    poets(&svc);
    remember(&svc, "Пушкинъ", "PERSON", "old spelling");
    let rows = run_ok(
        &svc,
        "CALL drevo.entity.candidates('Entity', {threshold: 0.0}) YIELD a, b, score \
         RETURN id(a) < id(b), score",
    );
    assert!(rows.rows.len() >= 2, "{:?}", rows.rows);
    let scores: Vec<f64> = rows.rows.iter().map(|r| float(&r[1])).collect();
    assert!(scores.windows(2).all(|w| w[0] >= w[1]), "{scores:?}");
    assert!(rows.rows.iter().all(|r| r[0] == Value::Bool(true)));
}

#[test]
fn the_threshold_boundary_is_inclusive() {
    let svc = NativeService::in_memory();
    poets(&svc);
    assert_eq!(candidates(&svc, "{threshold: 1.0}").len(), 1);
}

#[test]
fn vector_scores_join_the_name_score_when_embeddings_are_set() {
    let svc = NativeService::in_memory();
    poets(&svc);
    run_ok(
        &svc,
        "MATCH (e:Entity) WHERE e.name IN ['Пушкин', 'Pushkin'] SET e.embedding = \
         CASE e.name WHEN 'Пушкин' THEN [1.0, 0.0, 0.1] ELSE [0.9, 0.1, 0.1] END",
    );
    let pairs = candidates(
        &svc,
        "{embeddingProperty: 'embedding', blocking: 'none', threshold: 0.5, \
          nameWeight: 0.2, vectorWeight: 0.8}",
    );
    let latin = pairs
        .iter()
        .find(|(a, b, ..)| a == "Пушкин" && b == "Pushkin")
        .unwrap_or_else(|| panic!("{pairs:?}"));
    let vector = float(&latin.3);
    assert!(vector > 0.98 && vector <= 1.0, "{vector}");
    assert!((latin.4 - (0.2 * latin.2 + 0.8 * vector)).abs() < 1e-9);
    assert!(
        latin.5.iter().any(|r| r.starts_with("vector:0.9")),
        "{:?}",
        latin.5
    );
    // A pair where one side has no embedding renormalises to the name.
    let initials = pairs
        .iter()
        .find(|(a, b, ..)| a == "Пушкин" && b == "А. С. Пушкин")
        .unwrap_or_else(|| panic!("{pairs:?}"));
    assert_eq!((initials.3.clone(), initials.4), (Value::Null, 1.0));
}

#[test]
fn candidates_with_too_many_pairs_suggest_blocking() {
    let svc = NativeService::in_memory();
    poets(&svc);
    let err = invalid_call(
        &svc,
        "CALL drevo.entity.candidates('Entity', {blocking: 'none', maxPairs: 2})",
    );
    assert_eq!(
        err,
        "blocking 'none' would compare 3 pairs, more than maxPairs = 2; \
         use blocking 'prefix3' or 'token', or raise maxPairs"
    );
}

#[test]
fn candidates_refuse_bad_arguments() {
    let svc = NativeService::in_memory();
    let err = invalid_call(
        &svc,
        "CALL drevo.entity.candidates('Entity', {cutoff: 0.5})",
    );
    assert!(
        err.starts_with("unknown config key `cutoff`; expected"),
        "{err}"
    );
    let err = invalid_call(&svc, "CALL drevo.entity.candidates(7)");
    assert_eq!(err, "label must be a string, got 7");
}

#[test]
fn candidates_without_a_config_use_the_defaults() {
    let svc = NativeService::in_memory();
    poets(&svc);
    let rows = run_ok(
        &svc,
        "CALL drevo.entity.candidates('Entity') YIELD a, b RETURN a.name, b.name",
    );
    assert_eq!(rows.rows.len(), 1);
}

#[test]
fn candidates_compare_raw_names_when_normalise_is_off() {
    let svc = NativeService::in_memory();
    poets(&svc);
    // Raw «А. С. Пушкин» shares no prefix with «Пушкин» and its initials keep
    // their dots, so only token blocking finds the pair, scored lower.
    let pairs = candidates(
        &svc,
        "{normalise: false, blocking: 'token', threshold: 0.0}",
    );
    let pair = pairs
        .iter()
        .find(|(a, b, ..)| a == "Пушкин" && b == "А. С. Пушкин")
        .unwrap_or_else(|| panic!("{pairs:?}"));
    assert!(pair.2 < 1.0, "{pair:?}");
}

// ----- merge -----------------------------------------------------------------------

fn merge(svc: &NativeService, keep: &str, drops: &[&str], config: &str) -> ExecResult {
    let drops = drops
        .iter()
        .map(|d| format!("'{d}'"))
        .collect::<Vec<_>>()
        .join(", ");
    run_ok(
        svc,
        &format!(
            "MATCH (k:Entity {{name: '{keep}'}}) \
             MATCH (d:Entity) WHERE d.name IN [{drops}] \
             WITH k, collect(d) AS ds \
             CALL drevo.entity.merge(k, ds, {config}) \
             YIELD kept, merged, relationshipsMoved, propertiesMerged \
             RETURN kept, merged, relationshipsMoved, propertiesMerged"
        ),
    )
}

#[test]
fn merge_moves_facts_and_factsat_sees_them_under_the_kept_name() {
    let svc = NativeService::in_memory();
    poets(&svc);
    remember(&svc, "Евгений Онегин", "OBJECT", "a novel in verse");
    run_ok(
        &svc,
        "CALL drevo.memory.assertFact('А. С. Пушкин', 'WROTE', 'Евгений Онегин', false) \
         YIELD rel RETURN rel",
    );
    let dropped = id_of(&svc, "А. С. Пушкин", "PERSON");
    let res = merge(&svc, "Пушкин", &["А. С. Пушкин"], "{}");
    assert_eq!(res.rows.len(), 1);
    let row = &res.rows[0];
    assert_eq!(
        &row[1..],
        &[Value::Integer(1), Value::Integer(1), Value::Integer(0)]
    );
    let Value::Node(kept) = &row[0] else {
        panic!("{row:?}")
    };
    assert_eq!(
        kept.properties.get("name"),
        Some(&Value::String("Пушкин".into()))
    );
    assert_eq!(
        kept.properties.get("aliases"),
        Some(&Value::List(vec![Value::String("А. С. Пушкин".into())]))
    );
    assert_eq!(
        kept.properties.get("merged_ids"),
        Some(&Value::List(vec![Value::Integer(dropped)]))
    );

    let facts = run_ok(
        &svc,
        "CALL drevo.memory.factsAt('Пушкин', null) YIELD subject, relation, object \
         RETURN subject.name, relation, object.name",
    );
    assert_eq!(
        facts.rows,
        [vec![
            Value::String("Пушкин".into()),
            Value::String("WROTE".into()),
            Value::String("Евгений Онегин".into()),
        ]]
    );
    assert_eq!(
        int(
            &svc,
            &format!("MATCH (n) WHERE id(n) = {dropped} RETURN count(n)")
        ),
        0
    );
    let alias = run_ok(
        &svc,
        "MATCH (a:Alias)-[:ALIAS_OF]->(k:Entity) RETURN a.name, a.normalised, k.name",
    );
    assert_eq!(
        alias.rows,
        [vec![
            Value::String("А. С. Пушкин".into()),
            Value::String("а с пушкин".into()),
            Value::String("Пушкин".into()),
        ]]
    );
}

#[test]
fn merge_counts_its_writes() {
    let svc = NativeService::in_memory();
    poets(&svc);
    let res = merge(&svc, "Пушкин", &["А. С. Пушкин", "Pushkin"], "{}");
    assert_eq!(res.rows[0][1], Value::Integer(2));
    assert_eq!(res.stats.nodes_deleted, 2);
    // One :Alias node per dropped name.
    assert_eq!(res.stats.nodes_created, 2);
    assert_eq!(res.stats.relationships_created, 2);
    assert_eq!(
        int(&svc, "MATCH (e:Entity {type: 'PERSON'}) RETURN count(e)"),
        1
    );
}

#[test]
fn merge_collapses_duplicate_relationships() {
    let svc = NativeService::in_memory();
    poets(&svc);
    remember(&svc, "Евгений Онегин", "OBJECT", "a novel in verse");
    for who in ["Пушкин", "А. С. Пушкин", "Pushkin"] {
        run_ok(
            &svc,
            &format!(
                "MATCH (p:Entity {{name: '{who}'}}), (o:Entity {{name: 'Евгений Онегин'}}) \
                 CREATE (p)-[:WROTE]->(o)"
            ),
        );
    }
    let res = merge(&svc, "Пушкин", &["А. С. Пушкин", "Pushkin"], "{}");
    assert_eq!(res.rows[0][2], Value::Integer(0));
    assert_eq!(
        int(
            &svc,
            "MATCH (:Entity {name: 'Пушкин'})-[r:WROTE]->(:Entity {name: 'Евгений Онегин'}) \
             RETURN count(r)"
        ),
        1
    );
}

#[test]
fn merging_two_linked_nodes_keeps_one_self_loop() {
    let svc = NativeService::in_memory();
    poets(&svc);
    run_ok(
        &svc,
        "MATCH (a:Entity {name: 'Пушкин'}), (b:Entity {name: 'Pushkin'}) \
         CREATE (a)-[:SAME_AUTHOR]->(b), (b)-[:SAME_AUTHOR]->(a)",
    );
    let res = merge(&svc, "Пушкин", &["Pushkin"], "{}");
    assert_eq!(res.rows[0][2], Value::Integer(1));
    assert_eq!(
        int(
            &svc,
            "MATCH (a:Entity {name: 'Пушкин'})-[r:SAME_AUTHOR]->(a) RETURN count(r)"
        ),
        1
    );
}

#[test]
fn merge_strategies_combine_properties() {
    let svc = NativeService::in_memory();
    poets(&svc);
    let res = merge(&svc, "Пушкин", &["Pushkin"], "{strategy: 'collect'}");
    assert_eq!(res.rows[0][3], Value::Integer(1));
    let description = run_ok(
        &svc,
        "MATCH (e:Entity {name: 'Пушкин'}) RETURN e.description",
    );
    assert_eq!(
        description.rows[0][0],
        Value::List(vec![
            Value::String("poet".into()),
            Value::String("Russian poet".into()),
        ])
    );
    let svc = NativeService::in_memory();
    poets(&svc);
    let res = merge(&svc, "Пушкин", &["Pushkin"], "{}");
    assert_eq!(res.rows[0][3], Value::Integer(0));
}

#[test]
fn merge_of_different_types_needs_force() {
    let svc = NativeService::in_memory();
    poets(&svc);
    let keep = id_of(&svc, "Пушкин", "PERSON");
    let street = id_of(&svc, "Пушкина", "LOCATION");
    let err = invalid_call(
        &svc,
        &format!("CALL drevo.entity.merge({keep}, [{street}], {{}})"),
    );
    assert_eq!(
        err,
        format!(
            "cannot merge nodes of different types: node {keep} has type 'PERSON', \
             node {street} has type 'LOCATION'; pass {{force: true}} to merge anyway"
        )
    );
    // Nothing was written.
    assert_eq!(int(&svc, "MATCH (e:Entity) RETURN count(e)"), 4);
    let res = run_ok(
        &svc,
        &format!(
            "CALL drevo.entity.merge({keep}, {street}, {{force: true}}) YIELD merged RETURN merged"
        ),
    );
    assert_eq!(res.rows[0][0], Value::Integer(1));
    // The kept node keeps its own type.
    assert_eq!(
        int(&svc, "MATCH (e:Entity {type: 'PERSON'}) RETURN count(e)"),
        3
    );
}

#[test]
fn merge_refuses_ids_that_are_not_nodes() {
    let svc = NativeService::in_memory();
    poets(&svc);
    let keep = id_of(&svc, "Пушкин", "PERSON");
    assert_eq!(
        invalid_call(
            &svc,
            &format!("CALL drevo.entity.merge({keep}, [987654], null)")
        ),
        "no node with id 987654"
    );
    assert_eq!(
        invalid_call(&svc, "CALL drevo.entity.merge(987654, [1], null)"),
        "no node with id 987654"
    );
    assert_eq!(
        invalid_call(
            &svc,
            &format!("CALL drevo.entity.merge({keep}, ['Pushkin'])")
        ),
        "dropIds must be a node, a node id or a list of them, got a list holding String"
    );
    assert_eq!(
        invalid_call(&svc, "CALL drevo.entity.merge('Пушкин', [])"),
        "keepId must be a node or a node id, got String"
    );
    run_ok(
        &svc,
        "MATCH (a:Entity {name: 'Пушкин'}), (b:Entity {name: 'Pushkin'}) CREATE (a)-[:KNOWS]->(b)",
    );
    assert_eq!(
        invalid_call(
            &svc,
            "MATCH (:Entity {name: 'Пушкин'})-[r:KNOWS]->(b) \
             CALL drevo.entity.merge(b, [r]) YIELD merged RETURN merged"
        ),
        "dropIds must be a node, a node id or a list of them, got a list holding Relationship"
    );
    assert_eq!(
        invalid_call(&svc, &format!("CALL drevo.entity.merge({keep}, [{keep}])")),
        format!("cannot merge node {keep} into itself")
    );
    assert_eq!(
        invalid_call(
            &svc,
            &format!("CALL drevo.entity.merge({keep}, [], {{strategy: 'oldest'}})")
        ),
        "strategy must be 'keepFirst', 'newest' or 'collect', got 'oldest'"
    );
    assert_eq!(int(&svc, "MATCH (e:Entity) RETURN count(e)"), 4);
}

#[test]
fn merge_with_nothing_to_drop_changes_nothing() {
    let svc = NativeService::in_memory();
    poets(&svc);
    let keep = id_of(&svc, "Пушкин", "PERSON");
    let res = run_ok(
        &svc,
        &format!("CALL drevo.entity.merge({keep}, []) YIELD kept, merged RETURN id(kept), merged"),
    );
    assert_eq!(res.rows[0], [Value::Integer(keep), Value::Integer(0)]);
    assert_eq!(res.stats.nodes_deleted, 0);
}

#[test]
fn a_failing_merge_writes_nothing() {
    let svc = NativeService::in_memory();
    poets(&svc);
    let keep = id_of(&svc, "Пушкин", "PERSON");
    let latin = id_of(&svc, "Pushkin", "PERSON");
    // The second id fails after the first was looked at.
    invalid_call(
        &svc,
        &format!("CALL drevo.entity.merge({keep}, [{latin}, 987654])"),
    );
    assert_eq!(int(&svc, "MATCH (e:Entity) RETURN count(e)"), 4);
    assert_eq!(int(&svc, "MATCH (a:Alias) RETURN count(a)"), 0);
}

#[test]
fn a_merge_failing_after_its_first_writes_leaves_nothing() {
    let svc = NativeService::in_memory();
    poets(&svc);
    remember(&svc, "Евгений Онегин", "OBJECT", "a novel in verse");
    run_ok(
        &svc,
        "CALL drevo.memory.assertFact('А. С. Пушкин', 'WROTE', 'Евгений Онегин', false) \
         YIELD rel RETURN rel",
    );
    // An :Alias of that name already exists, so creating the merge's own
    // :Alias fails — after the fact was moved and the node deleted.
    run_ok(
        &svc,
        "CREATE CONSTRAINT alias_name FOR (a:Alias) REQUIRE a.name IS UNIQUE",
    );
    run_ok(
        &svc,
        "CREATE (:Alias {name: 'А. С. Пушкин', normalised: 'а с пушкин'})",
    );
    let keep = id_of(&svc, "Пушкин", "PERSON");
    let initials = id_of(&svc, "А. С. Пушкин", "PERSON");
    let failed = run(
        &svc,
        &format!("CALL drevo.entity.merge({keep}, [{initials}]) YIELD merged RETURN merged"),
    );
    assert!(failed.is_err(), "{failed:?}");
    assert_eq!(
        int(
            &svc,
            &format!("MATCH (n) WHERE id(n) = {initials} RETURN count(n)")
        ),
        1
    );
    assert_eq!(
        int(
            &svc,
            "MATCH (:Entity {name: 'А. С. Пушкин'})-[r:RELATED_TO]->() RETURN count(r)"
        ),
        1
    );
    assert_eq!(
        int(
            &svc,
            "MATCH (:Entity {name: 'Пушкин'})-[r]-() RETURN count(r)"
        ),
        0
    );
    assert_eq!(
        int(
            &svc,
            "MATCH (e:Entity {name: 'Пушкин'}) RETURN size(coalesce(e.merged_ids, []))"
        ),
        0
    );
}

#[test]
fn earlier_aliases_follow_a_second_merge() {
    let svc = NativeService::in_memory();
    poets(&svc);
    merge(&svc, "Pushkin", &["А. С. Пушкин"], "{}");
    merge(&svc, "Пушкин", &["Pushkin"], "{}");
    let res = run_ok(
        &svc,
        "MATCH (a:Alias)-[:ALIAS_OF]->(k) RETURN k.name, a.name ORDER BY a.name",
    );
    assert_eq!(
        res.rows,
        [
            vec![
                Value::String("Пушкин".into()),
                Value::String("Pushkin".into())
            ],
            vec![
                Value::String("Пушкин".into()),
                Value::String("А. С. Пушкин".into())
            ],
        ]
    );
    let aliases = run_ok(
        &svc,
        "MATCH (e:Entity {name: 'Пушкин'}) RETURN e.aliases, size(e.merged_ids)",
    );
    assert_eq!(
        aliases.rows[0],
        [
            Value::List(vec![
                Value::String("Pushkin".into()),
                Value::String("А. С. Пушкин".into()),
            ]),
            Value::Integer(2),
        ]
    );
}

#[test]
fn merge_fires_triggers_like_any_write() {
    let svc = NativeService::in_memory();
    poets(&svc);
    run_ok(
        &svc,
        "CALL apoc.trigger.install('drevo', 'audit', \
         \"CREATE (:Audit {deleted: size($deletedNodes)})\", {phase: 'after'})",
    );
    merge(&svc, "Пушкин", &["А. С. Пушкин", "Pushkin"], "{}");
    assert_eq!(int(&svc, "MATCH (a:Audit) RETURN max(a.deleted)"), 2);
}

// ----- aliases -----------------------------------------------------------------------

#[test]
fn aliases_find_the_entity_by_any_spelling() {
    let svc = NativeService::in_memory();
    poets(&svc);
    merge(&svc, "Пушкин", &["А. С. Пушкин", "Pushkin"], "{}");
    for name in ["А. С. Пушкин", "а.с. пушкин", "PUSHKIN", "Пушкин"] {
        let res = run_ok(
            &svc,
            &format!(
                "CALL drevo.entity.aliases('{name}') YIELD entity, aliases \
                 RETURN entity.name, aliases"
            ),
        );
        assert_eq!(res.rows.len(), 1, "{name}: {:?}", res.rows);
        assert_eq!(res.rows[0][0], Value::String("Пушкин".into()), "{name}");
        assert_eq!(
            strings(&res.rows[0][1]),
            ["А. С. Пушкин", "Pushkin"],
            "{name}"
        );
    }
    let none = run_ok(
        &svc,
        "CALL drevo.entity.aliases('Лермонтов') YIELD entity RETURN entity",
    );
    assert!(none.rows.is_empty());
    let street = run_ok(
        &svc,
        "CALL drevo.entity.aliases('Пушкина') YIELD entity, aliases RETURN entity.type, aliases",
    );
    assert_eq!(
        street.rows,
        [vec![Value::String("LOCATION".into()), Value::List(vec![])]]
    );
}

#[test]
fn aliases_refuse_a_non_string() {
    let svc = NativeService::in_memory();
    assert_eq!(
        invalid_call(&svc, "CALL drevo.entity.aliases(3)"),
        "name must be a string, got 3"
    );
}

#[test]
fn normalise_is_a_function() {
    let svc = NativeService::in_memory();
    let res = run_ok(
        &svc,
        "RETURN drevo.entity.normalise('А. С. Пушкин'), drevo.entity.normalise(null)",
    );
    assert_eq!(
        res.rows[0],
        [Value::String("а с пушкин".into()), Value::Null]
    );
}

// ----- rememberEntity resolve ---------------------------------------------------------

fn remember_with(svc: &NativeService, name: &str, entity_type: &str, options: &str) -> ExecResult {
    run_ok(
        svc,
        &format!(
            "CALL drevo.memory.rememberEntity(null, '{name}', '{entity_type}', null, {options}) \
             YIELD node RETURN id(node), node.name"
        ),
    )
}

#[test]
fn resolve_alias_finds_the_kept_node_by_an_old_spelling() {
    let svc = NativeService::in_memory();
    poets(&svc);
    merge(&svc, "Пушкин", &["А. С. Пушкин"], "{}");
    let keep = id_of(&svc, "Пушкин", "PERSON");
    let res = remember_with(&svc, "А.С. Пушкин", "PERSON", "{resolve: 'alias'}");
    assert_eq!(res.rows[0][0], Value::Integer(keep));
    assert_eq!(res.stats.nodes_created, 0);
    // The alias is only for its type.
    let res = remember_with(&svc, "А. С. Пушкин", "LOCATION", "{resolve: 'alias'}");
    assert_eq!(res.stats.nodes_created, 1);
    // The default stays exact: the old spelling makes a new node.
    let res = remember_with(&svc, "А. С. Пушкин", "PERSON", "null");
    assert_ne!(res.rows[0][0], Value::Integer(keep));
    assert_eq!(res.stats.nodes_created, 1);
}

#[test]
fn resolve_alias_combines_with_provenance() {
    let svc = NativeService::in_memory();
    poets(&svc);
    merge(&svc, "Пушкин", &["А. С. Пушкин"], "{}");
    let keep = id_of(&svc, "Пушкин", "PERSON");
    let res = remember_with(
        &svc,
        "А. С. Пушкин",
        "PERSON",
        "{resolve: 'alias', source: {id: 'bio'}, \
          chunk: {id: 'bio#0', order: 0, text: 'А. С. Пушкин родился в Москве.'}}",
    );
    assert_eq!(res.rows[0][0], Value::Integer(keep));
    assert_eq!(
        int(
            &svc,
            "MATCH (:Entity {name: 'Пушкин'})-[m:MENTIONED_IN]->(:Chunk {id: 'bio#0'}) \
             RETURN count(m)"
        ),
        1
    );
}

/// «Пушкин» and «Pushkin» (PERSON) and the street «Пушкина» (LOCATION),
/// without «А. С. Пушкин», which a fuzzy «Пушкину» would match as well.
fn known_poet(svc: &NativeService) {
    remember(svc, "Пушкин", "PERSON", "poet");
    remember(svc, "Pushkin", "PERSON", "Russian poet");
    remember(svc, "Пушкина", "LOCATION", "a street");
}

#[test]
fn resolve_fuzzy_reuses_a_unique_match_above_the_threshold() {
    let svc = NativeService::in_memory();
    known_poet(&svc);
    let keep = id_of(&svc, "Пушкин", "PERSON");
    // «Пушкину» (dative) is close to «Пушкин» only.
    let res = remember_with(&svc, "Пушкину", "PERSON", "{resolve: 'fuzzy'}");
    assert_eq!(res.rows[0][0], Value::Integer(keep));
    assert_eq!(res.stats.nodes_created, 0);
}

#[test]
fn resolve_fuzzy_records_a_near_miss_below_the_threshold() {
    let svc = NativeService::in_memory();
    known_poet(&svc);
    let keep = id_of(&svc, "Пушкин", "PERSON");
    let res = remember_with(
        &svc,
        "Пушкину",
        "PERSON",
        "{resolve: 'fuzzy', threshold: 0.99}",
    );
    assert_ne!(res.rows[0][0], Value::Integer(keep));
    assert_eq!(res.stats.nodes_created, 1);
    let near = run_ok(
        &svc,
        "MATCH (n:Entity {name: 'Пушкину'})-[r:POSSIBLY_SAME]->(m:Entity) RETURN m.name, r.score",
    );
    assert_eq!(near.rows.len(), 1, "{:?}", near.rows);
    assert_eq!(near.rows[0][0], Value::String("Пушкин".into()));
    let score = float(&near.rows[0][1]);
    assert!(score > 0.9 && score < 0.99, "{score}");
    // A name like nothing known records no near-miss.
    let res = remember_with(&svc, "Лермонтов", "PERSON", "{resolve: 'fuzzy'}");
    assert_eq!(res.stats.nodes_created, 1);
    assert_eq!(
        int(
            &svc,
            "MATCH (:Entity {name: 'Лермонтов'})-[r:POSSIBLY_SAME]->() RETURN count(r)"
        ),
        0
    );
}

#[test]
fn resolve_fuzzy_never_reuses_an_ambiguous_match() {
    let svc = NativeService::in_memory();
    remember(&svc, "Пушкин", "PERSON", "poet");
    remember(&svc, "Пушкина", "PERSON", "his wife");
    let res = remember_with(&svc, "Пушкины", "PERSON", "{resolve: 'fuzzy'}");
    assert_eq!(res.stats.nodes_created, 1);
    assert_eq!(
        int(
            &svc,
            "MATCH (:Entity {name: 'Пушкины'})-[r:POSSIBLY_SAME]->() RETURN count(r)"
        ),
        2
    );
}

#[test]
fn resolve_fuzzy_never_crosses_types() {
    let svc = NativeService::in_memory();
    known_poet(&svc);
    let res = remember_with(&svc, "Пушкину", "LOCATION", "{resolve: 'fuzzy'}");
    let street = id_of(&svc, "Пушкина", "LOCATION");
    // «Пушкину» is close to the street «Пушкина» too: the LOCATION is reused.
    assert_eq!(res.rows[0][0], Value::Integer(street));
}

#[test]
fn resolve_options_are_checked() {
    let svc = NativeService::in_memory();
    let err = invalid_call(
        &svc,
        "CALL drevo.memory.rememberEntity(null, 'Пушкин', 'PERSON', null, {threshold: 0.5})",
    );
    assert_eq!(err, "threshold only applies with resolve: 'fuzzy'");
    let err = invalid_call(
        &svc,
        "CALL drevo.memory.rememberEntity(null, 'Пушкин', 'PERSON', null, {resolve: 'soundex'})",
    );
    assert_eq!(
        err,
        "resolve must be 'exact', 'alias' or 'fuzzy', got 'soundex'"
    );
    let err = invalid_call(
        &svc,
        "CALL drevo.memory.rememberEntity(null, 'Пушкин', 'PERSON', null, {page: 3})",
    );
    assert_eq!(
        err,
        "unknown rememberEntity option `page`; expected source, chunk, quote, offset, \
         length, surface, resolve, threshold"
    );
}

// ----- concurrency -----------------------------------------------------------------------

#[test]
fn two_merges_of_overlapping_sets_conflict_and_the_graph_stays_consistent() {
    let svc = NativeService::in_memory();
    poets(&svc);
    remember(&svc, "Евгений Онегин", "OBJECT", "a novel in verse");
    run_ok(
        &svc,
        "CALL drevo.memory.assertFact('А. С. Пушкин', 'WROTE', 'Евгений Онегин', false) \
         YIELD rel RETURN rel",
    );
    let keep = id_of(&svc, "Пушкин", "PERSON");
    let initials = id_of(&svc, "А. С. Пушкин", "PERSON");
    let latin = id_of(&svc, "Pushkin", "PERSON");

    let first = svc.begin_tx();
    let second = svc.begin_tx();
    let q1 = parse(&format!(
        "CALL drevo.entity.merge({keep}, [{initials}]) YIELD merged RETURN merged"
    ))
    .unwrap();
    let q2 = parse(&format!(
        "CALL drevo.entity.merge({latin}, [{initials}]) YIELD merged RETURN merged"
    ))
    .unwrap();
    svc.execute_in_tx(first, &q1, HashMap::new()).unwrap();
    svc.execute_in_tx(second, &q2, HashMap::new()).unwrap();
    svc.commit_tx(first).unwrap();
    let lost = svc.commit_tx(second);
    assert!(matches!(lost, Err(CommitError::Conflict)), "{lost:?}");

    // The first merge stands, whole; nothing of the second.
    assert_eq!(
        int(&svc, "MATCH (e:Entity {type: 'PERSON'}) RETURN count(e)"),
        2
    );
    assert_eq!(
        int(
            &svc,
            &format!("MATCH (n) WHERE id(n) = {initials} RETURN count(n)")
        ),
        0
    );
    assert_eq!(
        int(
            &svc,
            "MATCH (a:Alias)-[:ALIAS_OF]->(:Entity {name: 'Пушкин'}) RETURN count(a)"
        ),
        1
    );
    assert_eq!(int(&svc, "MATCH (a:Alias) RETURN count(a)"), 1);
    assert_eq!(
        int(
            &svc,
            "MATCH (:Entity {name: 'Пушкин'})-[r:RELATED_TO]->() RETURN count(r)"
        ),
        1
    );
    assert_eq!(
        int(
            &svc,
            "MATCH (e:Entity {name: 'Pushkin'}) RETURN size(coalesce(e.merged_ids, []))"
        ),
        0
    );
}

// ----- Bolt ---------------------------------------------------------------------------------

#[test]
fn bolt_candidates_carry_float_scores_a_null_vector_and_string_reasons() {
    let svc = Arc::new(NativeService::in_memory());
    poets(&svc);
    let mut s = Session::new_durable(Arc::clone(&svc));
    s.handle(ClientMessage::Hello {
        extra: BTreeMap::new(),
    });
    let mut replies = s.handle(ClientMessage::Run {
        query: "CALL drevo.entity.candidates('Entity', {}) \
                YIELD a, nameScore, vectorScore, score, reasons \
                RETURN a, nameScore, vectorScore, score, reasons"
            .to_string(),
        parameters: BTreeMap::new(),
        extra: BTreeMap::new(),
    });
    replies.extend(s.handle(ClientMessage::Pull {
        extra: BTreeMap::new(),
    }));
    let records: Vec<&Vec<Bolt>> = replies
        .iter()
        .filter_map(|m| match m {
            ServerMessage::Record { fields } => Some(fields),
            _ => None,
        })
        .collect();
    assert_eq!(records.len(), 1, "{replies:?}");
    let r = records[0];
    assert!(matches!(r[0], Bolt::Structure { .. }), "{r:?}");
    assert_eq!(r[1], Bolt::Float(1.0));
    assert_eq!(r[2], Bolt::Null);
    assert_eq!(r[3], Bolt::Float(1.0));
    assert_eq!(
        r[4],
        Bolt::List(vec![
            Bolt::String("name:1.00".into()),
            Bolt::String("shared_token:пушкин".into()),
        ])
    );
}
