//! End-to-end parser tests against the five drevo target scenarios
//! (Phase 10 task `00062`).
//!
//! These tests are scenario-shaped: each one is a realistic Cypher query
//! that an embedder (CBT journal app, story editor, IT task manager, ERP,
//! bug tracker) would actually send. The parser must accept them with no
//! errors and produce an AST whose structural invariants match the
//! scenario semantics (e.g. "the MATCH binds variable X to label L", "the
//! WITH projects an aggregation").
//!
//! These tests do NOT exercise execution (that lands in task `00063`).
//! They verify only that the *grammar* is rich enough to express each
//! scenario domain — once the executor lands the same queries become the
//! seed for the Phase 10 "definition of done" suite (per `README.md`).

use drevo::cypher::ast::{
    BinaryOp, Clause, Direction, Expression, ProjectionItem, Query, RelLength, SetItem, UnionKind,
};
use drevo::cypher::parser::parse;

// ===== Helpers ==============================================================

fn first_clause(q: &Query) -> &Clause {
    &q.parts[0].query.clauses[0]
}

fn clause_at(q: &Query, idx: usize) -> &Clause {
    &q.parts[0].query.clauses[idx]
}

fn clauses_len(q: &Query) -> usize {
    q.parts[0].query.clauses.len()
}

fn has_clause_kind<F: Fn(&Clause) -> bool>(q: &Query, pred: F) -> bool {
    q.parts[0].query.clauses.iter().any(pred)
}

// ===== Scenario 1 — CBT Journal ============================================
//
// Cognitive Behavioural Therapy journal: thought entries, mood ratings,
// cognitive distortions, reframes. The graph models thoughts as nodes,
// moods + distortions as adjacent nodes, and the cognitive challenge
// process as property updates.

#[test]
fn cbt_create_thought_with_mood_relationship() {
    let q = parse(
        "CREATE (t:Thought {body: 'I am terrible at work', recorded_at: 1700000000})
              -[:HAD_MOOD]->(m:Mood {valence: -0.7, intensity: 0.8})",
    )
    .unwrap();
    let create = match first_clause(&q) {
        Clause::Create(c) => c,
        _ => panic!(),
    };
    assert_eq!(create.patterns.len(), 1);
    let path = &create.patterns[0].path;
    assert_eq!(path.head.labels, vec!["Thought".to_string()]);
    assert!(path.head.properties.is_some());
    assert_eq!(path.tail.len(), 1);
    assert_eq!(
        path.tail[0].relationship.types,
        vec!["HAD_MOOD".to_string()]
    );
    assert_eq!(path.tail[0].node.labels, vec!["Mood".to_string()]);
}

#[test]
fn cbt_tag_thought_with_distortion() {
    let q = parse(
        "MATCH (t:Thought {id: $tid})
         CREATE (t)-[:HAS_DISTORTION]->(d:Distortion {kind: 'catastrophizing'})",
    )
    .unwrap();
    assert_eq!(clauses_len(&q), 2);
    assert!(matches!(clause_at(&q, 0), Clause::Match(_)));
    assert!(matches!(clause_at(&q, 1), Clause::Create(_)));
}

#[test]
fn cbt_find_thoughts_by_distortion_kind() {
    let q = parse(
        "MATCH (t:Thought)-[:HAS_DISTORTION]->(:Distortion {kind: 'mind_reading'})
         RETURN t.body AS thought, t.recorded_at AS at
         ORDER BY t.recorded_at DESC LIMIT 50",
    )
    .unwrap();
    let m = match clause_at(&q, 0) {
        Clause::Match(m) => m,
        _ => panic!(),
    };
    // Anonymous variable on Distortion node — variable is None, label
    // remains present.
    let dist_node = &m.patterns[0].path.tail[0].node;
    assert!(dist_node.variable.is_none());
    assert_eq!(dist_node.labels, vec!["Distortion".to_string()]);
    let r = match clause_at(&q, 1) {
        Clause::Return(r) => r,
        _ => panic!(),
    };
    assert!(r.limit.is_some());
    assert!(!r.order_by.is_empty());
}

#[test]
fn cbt_find_thoughts_with_negative_mood() {
    let q = parse(
        "MATCH (t:Thought)-[:HAD_MOOD]->(m:Mood)
         WHERE m.valence < 0
         RETURN t.body, m.valence
         ORDER BY m.valence ASC LIMIT 10",
    )
    .unwrap();
    let m = match clause_at(&q, 0) {
        Clause::Match(m) => m,
        _ => panic!(),
    };
    match m.where_clause.as_ref().unwrap() {
        Expression::Binary {
            op: BinaryOp::Lt, ..
        } => {}
        _ => panic!("expected `<` predicate"),
    }
}

#[test]
fn cbt_record_reframe_with_set() {
    let q = parse(
        "MATCH (t:Thought {id: $tid})
         SET t.challenge = $reframe, t.reframed = TRUE, t.reframed_at = timestamp()",
    )
    .unwrap();
    let s = match clause_at(&q, 1) {
        Clause::Set(s) => s,
        _ => panic!(),
    };
    assert_eq!(s.items.len(), 3);
    for item in &s.items {
        assert!(matches!(item, SetItem::Property { .. }));
    }
}

// ===== Scenario 2 — Story / Book editor =====================================
//
// Long-form fiction with chapters, scenes, characters, and a separate
// world-building tree. Authors care about character co-occurrence across
// chapters and being able to reorder scenes without renumbering the lot.

#[test]
fn story_create_chapter_with_first_scene() {
    let q = parse(
        "CREATE (c:Chapter {number: 1, title: 'Awakening'})
              -[:HAS_SCENE {order: 1}]->(s:Scene {summary: 'Hero wakes up to the smell of smoke.'})",
    )
    .unwrap();
    let create = match first_clause(&q) {
        Clause::Create(c) => c,
        _ => panic!(),
    };
    let rel = &create.patterns[0].path.tail[0].relationship;
    assert_eq!(rel.types, vec!["HAS_SCENE".to_string()]);
    let rel_props = rel.properties.as_ref().expect("relationship props");
    assert_eq!(rel_props.entries[0].0, "order");
}

#[test]
fn story_link_character_to_scene_via_merge() {
    // MERGE is the natural idiom — multiple FEATURES rels for the same
    // (scene, character) pair would be a bug.
    let q = parse(
        "MATCH (s:Scene {id: $sid}), (ch:Character {name: 'Aldric'})
         MERGE (s)-[:FEATURES]->(ch)",
    )
    .unwrap();
    assert!(has_clause_kind(&q, |c| matches!(c, Clause::Match(_))));
    assert!(has_clause_kind(&q, |c| matches!(c, Clause::Merge(_))));
}

#[test]
fn story_find_scenes_for_character() {
    let q = parse(
        "MATCH (s:Scene)-[:FEATURES]->(c:Character {name: 'Aldric'})
         RETURN s.summary, s.id ORDER BY s.id ASC",
    )
    .unwrap();
    let m = match first_clause(&q) {
        Clause::Match(m) => m,
        _ => panic!(),
    };
    assert_eq!(m.patterns[0].path.head.labels, vec!["Scene".to_string()]);
    assert_eq!(
        m.patterns[0].path.tail[0].relationship.types,
        vec!["FEATURES".to_string()]
    );
}

#[test]
fn story_all_characters_in_chapter_multihop() {
    let q = parse(
        "MATCH (c:Chapter {number: 1})-[:HAS_SCENE]->(s)-[:FEATURES]->(ch:Character)
         RETURN DISTINCT ch.name AS character ORDER BY character",
    )
    .unwrap();
    let r = match clause_at(&q, 1) {
        Clause::Return(r) => r,
        _ => panic!(),
    };
    assert!(r.distinct);
    let m = match first_clause(&q) {
        Clause::Match(m) => m,
        _ => panic!(),
    };
    // Three nodes => head + 2 tail segments.
    assert_eq!(m.patterns[0].path.tail.len(), 2);
}

#[test]
fn story_reorder_scene_by_updating_relationship_property() {
    let q = parse(
        "MATCH ()-[r:HAS_SCENE]->(s:Scene {id: $sid})
         SET r.order = $new_order",
    )
    .unwrap();
    let m = match first_clause(&q) {
        Clause::Match(m) => m,
        _ => panic!(),
    };
    // Anonymous head node.
    assert!(m.patterns[0].path.head.variable.is_none());
    assert_eq!(
        m.patterns[0].path.tail[0].relationship.variable.as_deref(),
        Some("r")
    );
    let s = match clause_at(&q, 1) {
        Clause::Set(s) => s,
        _ => panic!(),
    };
    assert!(matches!(&s.items[0], SetItem::Property { .. }));
}

#[test]
fn story_count_scenes_per_chapter() {
    let q = parse(
        "MATCH (c:Chapter)-[:HAS_SCENE]->(s:Scene)
         WITH c, count(s) AS scene_count
         WHERE scene_count > 0
         RETURN c.number, c.title, scene_count ORDER BY c.number",
    )
    .unwrap();
    let w = match clause_at(&q, 1) {
        Clause::With(w) => w,
        _ => panic!(),
    };
    assert!(w.where_clause.is_some());
    assert_eq!(w.items.len(), 2);
}

// ===== Scenario 3 — IT Task Manager =========================================
//
// Backlog of engineering tasks with assignees, statuses, priorities, and
// "BLOCKS" dependencies. Variable-length paths matter because a task is
// effectively blocked by every transitive blocker.

#[test]
fn taskmgr_create_task_with_inline_props() {
    let q = parse(
        "CREATE (t:Task {
            id: $id, title: $title, status: 'open',
            priority: 'high', created_at: timestamp()
        })",
    )
    .unwrap();
    let create = match first_clause(&q) {
        Clause::Create(c) => c,
        _ => panic!(),
    };
    let props = create.patterns[0].path.head.properties.as_ref().unwrap();
    assert_eq!(props.entries.len(), 5);
}

#[test]
fn taskmgr_assign_task_with_merge() {
    let q = parse(
        "MATCH (t:Task {id: $tid}), (u:User {handle: $handle})
         MERGE (t)-[:ASSIGNED_TO]->(u)
         ON CREATE SET t.assigned_at = timestamp()",
    )
    .unwrap();
    let m = match clause_at(&q, 1) {
        Clause::Merge(m) => m,
        _ => panic!(),
    };
    assert_eq!(m.on_create.len(), 1);
    assert!(m.on_match.is_empty());
}

#[test]
fn taskmgr_find_open_tasks_for_user() {
    let q = parse(
        "MATCH (u:User {handle: $handle})<-[:ASSIGNED_TO]-(t:Task)
         WHERE t.status = 'open' AND t.priority IN ['high', 'critical']
         RETURN t.id, t.title, t.priority
         ORDER BY t.priority DESC, t.created_at ASC",
    )
    .unwrap();
    let m = match first_clause(&q) {
        Clause::Match(m) => m,
        _ => panic!(),
    };
    // Incoming arrow on the relationship.
    assert_eq!(
        m.patterns[0].path.tail[0].relationship.direction,
        Direction::Incoming
    );
    let r = match clause_at(&q, 1) {
        Clause::Return(r) => r,
        _ => panic!(),
    };
    assert_eq!(r.order_by.len(), 2);
}

#[test]
fn taskmgr_find_transitive_blockers() {
    // Variable-length path: anything blocking this task up to 5 levels deep.
    let q = parse(
        "MATCH (t:Task {id: $id})<-[:BLOCKS*1..5]-(blocker:Task)
         WHERE blocker.status <> 'closed'
         RETURN DISTINCT blocker.id, blocker.title",
    )
    .unwrap();
    let m = match first_clause(&q) {
        Clause::Match(m) => m,
        _ => panic!(),
    };
    let rel = &m.patterns[0].path.tail[0].relationship;
    assert_eq!(
        rel.length,
        Some(RelLength::Range {
            from: Some(1),
            to: Some(5),
        })
    );
    assert_eq!(rel.direction, Direction::Incoming);
    let r = match clause_at(&q, 1) {
        Clause::Return(r) => r,
        _ => panic!(),
    };
    assert!(r.distinct);
}

#[test]
fn taskmgr_close_task() {
    let q = parse(
        "MATCH (t:Task {id: $tid})
         SET t.status = 'closed', t.closed_at = timestamp()
         REMOVE t.in_progress_by",
    )
    .unwrap();
    assert_eq!(clauses_len(&q), 3);
    assert!(matches!(clause_at(&q, 1), Clause::Set(_)));
    assert!(matches!(clause_at(&q, 2), Clause::Remove(_)));
}

// ===== Scenario 4 — ERP =====================================================
//
// Tiny ERP slice: customers, orders, line items, inventory. Many of the
// real-world reports are aggregations after multi-hop joins, so this
// suite stress-tests WITH-based pipelining.

#[test]
fn erp_create_order_with_line_items() {
    let q = parse(
        "CREATE (o:Order {id: $oid, customer_id: $cid, total: 0, created_at: timestamp()})
              -[:HAS_LINE]->(:LineItem {sku: 'SKU-1', qty: 2, price: 19.99})",
    )
    .unwrap();
    let create = match first_clause(&q) {
        Clause::Create(c) => c,
        _ => panic!(),
    };
    let line = &create.patterns[0].path.tail[0].node;
    assert!(line.variable.is_none());
    assert_eq!(line.labels, vec!["LineItem".to_string()]);
}

#[test]
fn erp_recent_orders_for_customer() {
    let q = parse(
        "MATCH (c:Customer {id: $cid})-[:PLACED]->(o:Order)
         WHERE o.created_at > $since
         RETURN o.id, o.total, o.created_at
         ORDER BY o.created_at DESC LIMIT 50",
    )
    .unwrap();
    let r = match clause_at(&q, 1) {
        Clause::Return(r) => r,
        _ => panic!(),
    };
    assert!(matches!(&r.limit, Some(Expression::Integer(50, _))));
}

#[test]
fn erp_orders_aggregated_by_region() {
    let q = parse(
        "MATCH (c:Customer)-[:PLACED]->(o:Order)
         WITH c.region AS region, count(o) AS orders, sum(o.total) AS revenue
         WHERE orders > 0
         RETURN region, orders, revenue
         ORDER BY revenue DESC",
    )
    .unwrap();
    let w = match clause_at(&q, 1) {
        Clause::With(w) => w,
        _ => panic!(),
    };
    assert_eq!(w.items.len(), 3);
    // Last two items are aggregations.
    if let ProjectionItem::Expression {
        expr: Expression::FunctionCall { name, .. },
        ..
    } = &w.items[1]
    {
        assert_eq!(name, &vec!["count".to_string()]);
    } else {
        panic!();
    }
    if let ProjectionItem::Expression {
        expr: Expression::FunctionCall { name, .. },
        ..
    } = &w.items[2]
    {
        assert_eq!(name, &vec!["sum".to_string()]);
    } else {
        panic!();
    }
}

#[test]
fn erp_low_stock_inventory() {
    let q = parse(
        "MATCH (i:Inventory)
         WHERE i.qty_on_hand < i.reorder_threshold AND i.discontinued IS NOT NULL
         RETURN i.sku, i.qty_on_hand ORDER BY i.qty_on_hand ASC",
    )
    .unwrap();
    let m = match first_clause(&q) {
        Clause::Match(m) => m,
        _ => panic!(),
    };
    let pred = m.where_clause.as_ref().unwrap();
    // Top is AND of `<` and `IS NOT NULL`.
    match pred {
        Expression::Binary {
            op: BinaryOp::And,
            lhs,
            rhs,
            ..
        } => {
            assert!(matches!(
                lhs.as_ref(),
                Expression::Binary {
                    op: BinaryOp::Lt,
                    ..
                }
            ));
            match rhs.as_ref() {
                Expression::IsNull { negated, .. } => assert!(negated),
                _ => panic!(),
            }
        }
        _ => panic!(),
    }
}

#[test]
fn erp_customer_summary_with_optional_orders() {
    let q = parse(
        "MATCH (c:Customer)
         OPTIONAL MATCH (c)-[:PLACED]->(o:Order)
         WITH c, count(o) AS total_orders, sum(o.total) AS revenue
         RETURN c.name, total_orders, revenue
         ORDER BY revenue DESC LIMIT 100",
    )
    .unwrap();
    let single = &q.parts[0].query;
    assert_eq!(single.clauses.len(), 4);
    if let Clause::Match(m) = &single.clauses[1] {
        assert!(m.optional);
    } else {
        panic!();
    }
}

// ===== Scenario 5 — Bug Tracker =============================================
//
// Bug filing → triage → assignment → resolution. Tracks duplicates,
// component ownership, comment threads. Heavy on string predicates
// (search by title) and conditional expressions (severity → SLA).

#[test]
fn bugs_file_new_bug_against_component() {
    let q = parse(
        "CREATE (b:Bug {
            id: $id, title: $title, severity: 'P1', status: 'open',
            filed_at: timestamp(), filed_by: $reporter
        })-[:AGAINST]->(c:Component {name: $component})",
    )
    .unwrap();
    let create = match first_clause(&q) {
        Clause::Create(c) => c,
        _ => panic!(),
    };
    let bug = &create.patterns[0].path.head;
    let props = bug.properties.as_ref().unwrap();
    assert_eq!(props.entries.len(), 6);
}

#[test]
fn bugs_find_open_p1_bugs() {
    let q = parse(
        "MATCH (b:Bug)
         WHERE b.status = 'open' AND b.severity IN ['P0', 'P1']
         RETURN b.id, b.title, b.severity
         ORDER BY b.severity ASC, b.filed_at ASC",
    )
    .unwrap();
    let m = match first_clause(&q) {
        Clause::Match(m) => m,
        _ => panic!(),
    };
    let pred = m.where_clause.as_ref().unwrap();
    // Top is AND of `=` and IN.
    match pred {
        Expression::Binary {
            op: BinaryOp::And,
            lhs,
            rhs,
            ..
        } => {
            assert!(matches!(
                lhs.as_ref(),
                Expression::Binary {
                    op: BinaryOp::Eq,
                    ..
                }
            ));
            assert!(matches!(rhs.as_ref(), Expression::In { .. }));
        }
        _ => panic!(),
    }
}

#[test]
fn bugs_mark_duplicate_with_relationship() {
    let q = parse(
        "MATCH (a:Bug {id: $aid}), (b:Bug {id: $bid})
         CREATE (a)-[:DUPLICATE_OF {marked_at: timestamp(), marked_by: $user}]->(b)
         SET a.status = 'duplicate', a.duplicate_of = $bid",
    )
    .unwrap();
    assert_eq!(clauses_len(&q), 3);
    assert!(matches!(clause_at(&q, 0), Clause::Match(_)));
    assert!(matches!(clause_at(&q, 1), Clause::Create(_)));
    assert!(matches!(clause_at(&q, 2), Clause::Set(_)));
}

#[test]
fn bugs_assignee_dashboard_with_optional_comments() {
    let q = parse(
        "MATCH (u:User {handle: $handle})<-[:ASSIGNED_TO]-(b:Bug)
         OPTIONAL MATCH (b)<-[:COMMENT_ON]-(c:Comment)
         WITH b, count(c) AS comment_count
         WHERE b.status <> 'resolved'
         RETURN b.id, b.title, comment_count
         ORDER BY b.severity ASC, comment_count DESC LIMIT 30",
    )
    .unwrap();
    let single = &q.parts[0].query;
    assert_eq!(single.clauses.len(), 4);
    if let Clause::Match(m) = &single.clauses[1] {
        assert!(m.optional);
    } else {
        panic!();
    }
}

#[test]
fn bugs_close_with_resolution_and_remove_assignee() {
    let q = parse(
        "MATCH (b:Bug {id: $bid})-[r:ASSIGNED_TO]->(u:User)
         SET b.status = 'resolved',
             b.resolved_at = timestamp(),
             b.fix_version = $version
         DELETE r",
    )
    .unwrap();
    assert_eq!(clauses_len(&q), 3);
    let s = match clause_at(&q, 1) {
        Clause::Set(s) => s,
        _ => panic!(),
    };
    assert_eq!(s.items.len(), 3);
    let d = match clause_at(&q, 2) {
        Clause::Delete(d) => d,
        _ => panic!(),
    };
    assert!(!d.detach);
    assert_eq!(d.targets.len(), 1);
}

#[test]
fn bugs_search_by_title_contains() {
    let q = parse(
        "MATCH (b:Bug)
         WHERE b.title CONTAINS $query OR b.body CONTAINS $query
         RETURN b.id, b.title, b.severity ORDER BY b.filed_at DESC LIMIT 25",
    )
    .unwrap();
    let m = match first_clause(&q) {
        Clause::Match(m) => m,
        _ => panic!(),
    };
    let pred = m.where_clause.as_ref().unwrap();
    match pred {
        Expression::Binary {
            op: BinaryOp::Or,
            lhs,
            rhs,
            ..
        } => {
            assert!(matches!(
                lhs.as_ref(),
                Expression::Binary {
                    op: BinaryOp::Contains,
                    ..
                }
            ));
            assert!(matches!(
                rhs.as_ref(),
                Expression::Binary {
                    op: BinaryOp::Contains,
                    ..
                }
            ));
        }
        _ => panic!(),
    }
}

#[test]
fn bugs_severity_sla_with_case_projection() {
    let q = parse(
        "MATCH (b:Bug)
         WHERE b.status = 'open'
         RETURN b.id,
                CASE b.severity
                    WHEN 'P0' THEN 4
                    WHEN 'P1' THEN 24
                    WHEN 'P2' THEN 72
                    ELSE 168
                END AS sla_hours",
    )
    .unwrap();
    let r = match clause_at(&q, 1) {
        Clause::Return(r) => r,
        _ => panic!(),
    };
    if let ProjectionItem::Expression { expr, alias } = &r.items[1] {
        assert_eq!(alias.as_deref(), Some("sla_hours"));
        match expr {
            Expression::Case {
                scrutinee,
                arms,
                else_branch,
                ..
            } => {
                assert!(scrutinee.is_some());
                assert_eq!(arms.len(), 3);
                assert!(else_branch.is_some());
            }
            _ => panic!(),
        }
    } else {
        panic!();
    }
}

// ===== Cross-scenario sanity =================================================

#[test]
fn cross_union_combines_two_domains_into_inbox() {
    // Realistic admin "inbox" view: list bugs assigned to me AND tasks
    // assigned to me. Same projection shape from two domain MATCHes.
    let q = parse(
        "MATCH (b:Bug)-[:ASSIGNED_TO]->(u:User {handle: $me})
         RETURN b.id AS id, b.title AS title, 'bug' AS kind
         UNION
         MATCH (t:Task)-[:ASSIGNED_TO]->(u:User {handle: $me})
         RETURN t.id AS id, t.title AS title, 'task' AS kind",
    )
    .unwrap();
    assert_eq!(q.parts.len(), 2);
    assert_eq!(q.parts[1].union, Some(UnionKind::Distinct));
}

#[test]
fn cross_unwind_then_create_bulk_import() {
    // Bulk-insert idiom — load a list of payloads then materialise nodes.
    let q = parse(
        "UNWIND $rows AS row
         CREATE (e:Entry {id: row.id, body: row.body, created_at: row.ts})",
    )
    .unwrap();
    assert!(matches!(clause_at(&q, 0), Clause::Unwind(_)));
    assert!(matches!(clause_at(&q, 1), Clause::Create(_)));
    let create = match clause_at(&q, 1) {
        Clause::Create(c) => c,
        _ => panic!(),
    };
    let props = create.patterns[0].path.head.properties.as_ref().unwrap();
    // Every value reads from `row.<x>`.
    for (_, v) in &props.entries {
        assert!(matches!(v, Expression::Property { .. }));
    }
}
