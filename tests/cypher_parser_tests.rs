//! Integration tests for the Cypher parser (Phase 10 task `00062`).
//!
//! These tests exercise the public `drevo::cypher::parser::parse` entry
//! point on realistic, multi-clause Cypher source. Internals of the AST
//! shape (variable names, label lists, expression structure) are asserted
//! directly so a parser regression is caught here regardless of any later
//! refactor of the AST.

use drevo::cypher::ast::{
    BinaryOp, Clause, Direction, Expression, OrderDirection, ProjectionItem, RelLength, SetItem,
    UnaryOp, UnionKind,
};
use drevo::cypher::parser::parse;

// ===== Smoke tests — every clause type round-trips ===========================

#[test]
fn parses_match_return_single_node() {
    let q = parse("MATCH (n) RETURN n").unwrap();
    assert_eq!(q.parts.len(), 1);
    let single = &q.parts[0].query;
    assert_eq!(single.clauses.len(), 2);
    match &single.clauses[0] {
        Clause::Match(m) => {
            assert!(!m.optional);
            assert_eq!(m.patterns.len(), 1);
            assert_eq!(m.patterns[0].path.head.variable.as_deref(), Some("n"));
            assert!(m.patterns[0].path.head.labels.is_empty());
            assert!(m.patterns[0].path.tail.is_empty());
            assert!(m.where_clause.is_none());
        }
        c => panic!("expected MATCH, got {c:?}"),
    }
    match &single.clauses[1] {
        Clause::Return(r) => {
            assert!(!r.distinct);
            assert_eq!(r.items.len(), 1);
            match &r.items[0] {
                ProjectionItem::Expression { expr, alias } => {
                    assert!(matches!(expr, Expression::Variable(name, _) if name == "n"));
                    assert!(alias.is_none());
                }
                p => panic!("expected expression projection, got {p:?}"),
            }
        }
        c => panic!("expected RETURN, got {c:?}"),
    }
}

#[test]
fn parses_create_with_labels_and_props() {
    let q = parse("CREATE (n:Person:Employee {name: 'Alice', age: 30})").unwrap();
    let single = &q.parts[0].query;
    assert_eq!(single.clauses.len(), 1);
    let create = match &single.clauses[0] {
        Clause::Create(c) => c,
        c => panic!("expected CREATE, got {c:?}"),
    };
    assert_eq!(create.patterns.len(), 1);
    let node = &create.patterns[0].path.head;
    assert_eq!(node.variable.as_deref(), Some("n"));
    assert_eq!(
        node.labels,
        vec!["Person".to_string(), "Employee".to_string()]
    );
    let props = node.properties.as_ref().expect("properties");
    assert_eq!(props.entries.len(), 2);
    assert_eq!(props.entries[0].0, "name");
    assert!(matches!(&props.entries[0].1, Expression::String(s, _) if s == "Alice"));
    assert_eq!(props.entries[1].0, "age");
    assert!(matches!(&props.entries[1].1, Expression::Integer(30, _)));
}

#[test]
fn parses_relationship_with_direction_outgoing() {
    let q = parse("MATCH (a)-[r:KNOWS]->(b) RETURN r").unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Match(m) => m,
        c => panic!("{c:?}"),
    };
    let path = &m.patterns[0].path;
    assert_eq!(path.head.variable.as_deref(), Some("a"));
    assert_eq!(path.tail.len(), 1);
    let rel = &path.tail[0].relationship;
    assert_eq!(rel.direction, Direction::Outgoing);
    assert_eq!(rel.variable.as_deref(), Some("r"));
    assert_eq!(rel.types, vec!["KNOWS".to_string()]);
    assert_eq!(path.tail[0].node.variable.as_deref(), Some("b"));
}

#[test]
fn parses_relationship_with_direction_incoming() {
    let q = parse("MATCH (a)<-[r]-(b) RETURN r").unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Match(m) => m,
        c => panic!("{c:?}"),
    };
    assert_eq!(
        m.patterns[0].path.tail[0].relationship.direction,
        Direction::Incoming
    );
}

#[test]
fn parses_relationship_undirected() {
    let q = parse("MATCH (a)-[r]-(b) RETURN r").unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Match(m) => m,
        c => panic!("{c:?}"),
    };
    assert_eq!(
        m.patterns[0].path.tail[0].relationship.direction,
        Direction::Undirected
    );
}

#[test]
fn parses_relationship_type_alternatives() {
    let q = parse("MATCH (a)-[r:KNOWS|FOLLOWS]->(b) RETURN r").unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Match(m) => m,
        c => panic!("{c:?}"),
    };
    let rel = &m.patterns[0].path.tail[0].relationship;
    assert_eq!(rel.types, vec!["KNOWS".to_string(), "FOLLOWS".to_string()]);
}

#[test]
fn parses_variable_length_path_exact() {
    let q = parse("MATCH (a)-[*3]->(b) RETURN b").unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Match(m) => m,
        c => panic!("{c:?}"),
    };
    let rel = &m.patterns[0].path.tail[0].relationship;
    assert_eq!(rel.length, Some(RelLength::Exact(3)));
}

#[test]
fn parses_variable_length_path_range() {
    let q = parse("MATCH (a)-[*1..3]->(b) RETURN b").unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Match(m) => m,
        c => panic!("{c:?}"),
    };
    let rel = &m.patterns[0].path.tail[0].relationship;
    assert_eq!(
        rel.length,
        Some(RelLength::Range {
            from: Some(1),
            to: Some(3)
        })
    );
}

#[test]
fn parses_variable_length_path_any() {
    let q = parse("MATCH (a)-[*]->(b) RETURN b").unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Match(m) => m,
        c => panic!("{c:?}"),
    };
    assert_eq!(
        m.patterns[0].path.tail[0].relationship.length,
        Some(RelLength::Any)
    );
}

#[test]
fn parses_optional_match() {
    let q = parse("OPTIONAL MATCH (a)-[r]->(b) RETURN a, b").unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Match(m) => m,
        c => panic!("{c:?}"),
    };
    assert!(m.optional);
}

#[test]
fn parses_match_where() {
    let q = parse("MATCH (n) WHERE n.age > 18 RETURN n").unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Match(m) => m,
        c => panic!("{c:?}"),
    };
    let pred = m.where_clause.as_ref().expect("where");
    match pred {
        Expression::Binary { op, .. } => assert_eq!(*op, BinaryOp::Gt),
        e => panic!("expected binary >, got {e:?}"),
    }
}

#[test]
fn parses_merge_with_on_create_and_on_match() {
    let q = parse("MERGE (n:Person {id: 1}) ON CREATE SET n.created = 1 ON MATCH SET n.seen = 2")
        .unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Merge(m) => m,
        c => panic!("{c:?}"),
    };
    assert_eq!(m.on_create.len(), 1);
    assert_eq!(m.on_match.len(), 1);
    assert!(matches!(&m.on_create[0], SetItem::Property { .. }));
    assert!(matches!(&m.on_match[0], SetItem::Property { .. }));
}

#[test]
fn parses_set_property_replace_merge_labels() {
    let q = parse("MATCH (n) SET n.name = 'X', n = {a: 1}, n += {b: 2}, n :Hot:Active").unwrap();
    let s = match &q.parts[0].query.clauses[1] {
        Clause::Set(s) => s,
        c => panic!("{c:?}"),
    };
    assert_eq!(s.items.len(), 4);
    assert!(matches!(&s.items[0], SetItem::Property { .. }));
    assert!(matches!(&s.items[1], SetItem::Replace { .. }));
    assert!(matches!(&s.items[2], SetItem::Merge { .. }));
    match &s.items[3] {
        SetItem::Labels { labels, .. } => {
            assert_eq!(labels, &vec!["Hot".to_string(), "Active".to_string()])
        }
        i => panic!("expected labels, got {i:?}"),
    }
}

#[test]
fn parses_delete_and_detach_delete() {
    let q1 = parse("MATCH (n) DELETE n").unwrap();
    let d1 = match &q1.parts[0].query.clauses[1] {
        Clause::Delete(d) => d,
        c => panic!("{c:?}"),
    };
    assert!(!d1.detach);

    let q2 = parse("MATCH (n) DETACH DELETE n").unwrap();
    let d2 = match &q2.parts[0].query.clauses[1] {
        Clause::Delete(d) => d,
        c => panic!("{c:?}"),
    };
    assert!(d2.detach);
}

#[test]
fn parses_remove_property_and_labels() {
    let q = parse("MATCH (n) REMOVE n.foo, n :Hot").unwrap();
    let r = match &q.parts[0].query.clauses[1] {
        Clause::Remove(r) => r,
        c => panic!("{c:?}"),
    };
    assert_eq!(r.items.len(), 2);
}

#[test]
fn parses_return_distinct_with_alias_and_order_limit_skip() {
    let q =
        parse("MATCH (n) RETURN DISTINCT n.name AS who, n.age ORDER BY n.age DESC SKIP 5 LIMIT 10")
            .unwrap();
    let r = match &q.parts[0].query.clauses[1] {
        Clause::Return(r) => r,
        c => panic!("{c:?}"),
    };
    assert!(r.distinct);
    assert_eq!(r.items.len(), 2);
    if let ProjectionItem::Expression { alias, .. } = &r.items[0] {
        assert_eq!(alias.as_deref(), Some("who"));
    } else {
        panic!("expected expression projection");
    }
    assert_eq!(r.order_by.len(), 1);
    assert_eq!(r.order_by[0].direction, OrderDirection::Desc);
    assert!(matches!(&r.skip, Some(Expression::Integer(5, _))));
    assert!(matches!(&r.limit, Some(Expression::Integer(10, _))));
}

#[test]
fn parses_return_star() {
    let q = parse("MATCH (n) RETURN *").unwrap();
    let r = match &q.parts[0].query.clauses[1] {
        Clause::Return(r) => r,
        c => panic!("{c:?}"),
    };
    assert_eq!(r.items.len(), 1);
    assert!(matches!(r.items[0], ProjectionItem::Star));
}

#[test]
fn parses_with_clause_pipelining() {
    let q = parse("MATCH (n) WITH n.age AS age WHERE age > 18 RETURN age").unwrap();
    let single = &q.parts[0].query;
    assert_eq!(single.clauses.len(), 3);
    let w = match &single.clauses[1] {
        Clause::With(w) => w,
        c => panic!("{c:?}"),
    };
    assert_eq!(w.items.len(), 1);
    assert!(w.where_clause.is_some());
}

#[test]
fn parses_unwind() {
    let q = parse("UNWIND [1, 2, 3] AS x RETURN x").unwrap();
    let u = match &q.parts[0].query.clauses[0] {
        Clause::Unwind(u) => u,
        c => panic!("{c:?}"),
    };
    assert_eq!(u.alias, "x");
    assert!(matches!(&u.expression, Expression::List { .. }));
}

#[test]
fn parses_union_distinct_and_all() {
    let q = parse("MATCH (a) RETURN a UNION MATCH (b) RETURN b").unwrap();
    assert_eq!(q.parts.len(), 2);
    assert!(q.parts[0].union.is_none());
    assert_eq!(q.parts[1].union, Some(UnionKind::Distinct));

    let q = parse("MATCH (a) RETURN a UNION ALL MATCH (b) RETURN b").unwrap();
    assert_eq!(q.parts[1].union, Some(UnionKind::All));
}

// ===== Expression tests =====================================================

#[test]
fn parses_arithmetic_precedence() {
    // 1 + 2 * 3 should bind as 1 + (2 * 3)
    let q = parse("RETURN 1 + 2 * 3").unwrap();
    let r = match &q.parts[0].query.clauses[0] {
        Clause::Return(r) => r,
        c => panic!("{c:?}"),
    };
    let expr = match &r.items[0] {
        ProjectionItem::Expression { expr, .. } => expr,
        _ => panic!(),
    };
    match expr {
        Expression::Binary {
            op: BinaryOp::Add,
            lhs,
            rhs,
            ..
        } => {
            assert!(matches!(lhs.as_ref(), Expression::Integer(1, _)));
            assert!(matches!(
                rhs.as_ref(),
                Expression::Binary {
                    op: BinaryOp::Mul,
                    ..
                }
            ));
        }
        e => panic!("expected add at top, got {e:?}"),
    }
}

#[test]
fn parses_pow_right_associative() {
    // 2 ^ 3 ^ 2 == 2 ^ (3 ^ 2)
    let q = parse("RETURN 2 ^ 3 ^ 2").unwrap();
    let r = match &q.parts[0].query.clauses[0] {
        Clause::Return(r) => r,
        c => panic!("{c:?}"),
    };
    let expr = match &r.items[0] {
        ProjectionItem::Expression { expr, .. } => expr,
        _ => panic!(),
    };
    match expr {
        Expression::Binary {
            op: BinaryOp::Pow,
            lhs,
            rhs,
            ..
        } => {
            assert!(matches!(lhs.as_ref(), Expression::Integer(2, _)));
            assert!(matches!(
                rhs.as_ref(),
                Expression::Binary {
                    op: BinaryOp::Pow,
                    ..
                }
            ));
        }
        e => panic!("expected pow at top, got {e:?}"),
    }
}

#[test]
fn parses_unary_negation_and_not() {
    let q = parse("RETURN -1, NOT TRUE").unwrap();
    let r = match &q.parts[0].query.clauses[0] {
        Clause::Return(r) => r,
        c => panic!("{c:?}"),
    };
    assert!(matches!(
        &r.items[0],
        ProjectionItem::Expression {
            expr: Expression::Unary {
                op: UnaryOp::Neg,
                ..
            },
            ..
        }
    ));
    assert!(matches!(
        &r.items[1],
        ProjectionItem::Expression {
            expr: Expression::Unary {
                op: UnaryOp::Not,
                ..
            },
            ..
        }
    ));
}

#[test]
fn parses_is_null_and_is_not_null() {
    let q = parse("MATCH (n) WHERE n.foo IS NULL AND n.bar IS NOT NULL RETURN n").unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Match(m) => m,
        c => panic!("{c:?}"),
    };
    let pred = m.where_clause.as_ref().unwrap();
    let (lhs, rhs) = match pred {
        Expression::Binary {
            op: BinaryOp::And,
            lhs,
            rhs,
            ..
        } => (lhs, rhs),
        e => panic!("{e:?}"),
    };
    match lhs.as_ref() {
        Expression::IsNull { negated, .. } => assert!(!negated),
        e => panic!("{e:?}"),
    }
    match rhs.as_ref() {
        Expression::IsNull { negated, .. } => assert!(negated),
        e => panic!("{e:?}"),
    }
}

#[test]
fn parses_in_and_string_predicates() {
    let q = parse(
        "MATCH (n) WHERE n.x IN [1, 2, 3] AND n.name STARTS WITH 'A' AND n.name ENDS WITH 'z' AND n.name CONTAINS 'li' RETURN n",
    )
    .unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Match(m) => m,
        c => panic!("{c:?}"),
    };
    let pred = m.where_clause.as_ref().unwrap();
    // Tree shape: ((x IN [..] AND name STARTS WITH 'A') AND name ENDS WITH 'z') AND name CONTAINS 'li'
    // We don't pin the exact shape — just count operators.
    let mut binops = vec![];
    collect_binops(pred, &mut binops);
    assert!(binops.contains(&BinaryOp::And));
    assert!(binops.contains(&BinaryOp::StartsWith));
    assert!(binops.contains(&BinaryOp::EndsWith));
    assert!(binops.contains(&BinaryOp::Contains));
    // Look for an In expression anywhere in the tree.
    assert!(has_in(pred));
}

fn collect_binops(e: &Expression, out: &mut Vec<BinaryOp>) {
    if let Expression::Binary { op, lhs, rhs, .. } = e {
        out.push(*op);
        collect_binops(lhs, out);
        collect_binops(rhs, out);
    }
}

fn has_in(e: &Expression) -> bool {
    match e {
        Expression::In { .. } => true,
        Expression::Binary { lhs, rhs, .. } => has_in(lhs) || has_in(rhs),
        Expression::Unary { expr, .. } => has_in(expr),
        Expression::IsNull { expr, .. } => has_in(expr),
        _ => false,
    }
}

#[test]
fn parses_property_access_chain() {
    let q = parse("RETURN n.address.city").unwrap();
    let expr = projection_expr(&q);
    match expr {
        Expression::Property { base, name, .. } => {
            assert_eq!(name, "city");
            match base.as_ref() {
                Expression::Property { name, .. } => assert_eq!(name, "address"),
                _ => panic!(),
            }
        }
        _ => panic!(),
    }
}

#[test]
fn parses_property_access_with_keyword_name() {
    // "type" is a Cypher keyword, but is allowed as a property name.
    // ("type" is actually NOT in our reserved set, but "in" is — try both)
    let q = parse("RETURN n.in").unwrap();
    let expr = projection_expr(&q);
    match expr {
        Expression::Property { name, .. } => assert_eq!(name, "in"),
        _ => panic!(),
    }
}

#[test]
fn parses_function_call_with_distinct() {
    let q = parse("RETURN count(DISTINCT n)").unwrap();
    let expr = projection_expr(&q);
    match expr {
        Expression::FunctionCall {
            name,
            distinct,
            args,
            ..
        } => {
            assert_eq!(name, &vec!["count".to_string()]);
            assert!(*distinct);
            assert_eq!(args.len(), 1);
        }
        _ => panic!(),
    }
}

#[test]
fn parses_count_star() {
    let q = parse("RETURN count(*)").unwrap();
    let expr = projection_expr(&q);
    match expr {
        Expression::FunctionCall { args, .. } => {
            assert_eq!(args.len(), 1);
            assert!(matches!(&args[0], Expression::Star(_)));
        }
        _ => panic!(),
    }
}

#[test]
fn parses_list_literal() {
    let q = parse("RETURN [1, 2, 3]").unwrap();
    let expr = projection_expr(&q);
    match expr {
        Expression::List { items, .. } => assert_eq!(items.len(), 3),
        _ => panic!(),
    }
}

#[test]
fn parses_map_literal_top_level() {
    let q = parse("RETURN {a: 1, b: 'two'}").unwrap();
    let expr = projection_expr(&q);
    match expr {
        Expression::Map(m) => {
            assert_eq!(m.entries.len(), 2);
            assert_eq!(m.entries[0].0, "a");
            assert_eq!(m.entries[1].0, "b");
        }
        _ => panic!(),
    }
}

#[test]
fn parses_list_index_and_slice() {
    let q = parse("RETURN xs[0], xs[1..3], xs[..2], xs[1..]").unwrap();
    let r = match &q.parts[0].query.clauses[0] {
        Clause::Return(r) => r,
        _ => panic!(),
    };
    assert_eq!(r.items.len(), 4);
    assert!(matches!(
        &r.items[0],
        ProjectionItem::Expression {
            expr: Expression::Index { .. },
            ..
        }
    ));
    assert!(matches!(
        &r.items[1],
        ProjectionItem::Expression {
            expr: Expression::Slice {
                from: Some(_),
                to: Some(_),
                ..
            },
            ..
        }
    ));
    assert!(matches!(
        &r.items[2],
        ProjectionItem::Expression {
            expr: Expression::Slice {
                from: None,
                to: Some(_),
                ..
            },
            ..
        }
    ));
    assert!(matches!(
        &r.items[3],
        ProjectionItem::Expression {
            expr: Expression::Slice {
                from: Some(_),
                to: None,
                ..
            },
            ..
        }
    ));
}

#[test]
fn parses_case_simple_and_generic() {
    let q1 = parse("RETURN CASE n.x WHEN 1 THEN 'one' WHEN 2 THEN 'two' ELSE 'other' END").unwrap();
    let e1 = projection_expr(&q1);
    match e1 {
        Expression::Case {
            scrutinee,
            arms,
            else_branch,
            ..
        } => {
            assert!(scrutinee.is_some());
            assert_eq!(arms.len(), 2);
            assert!(else_branch.is_some());
        }
        _ => panic!(),
    }

    let q2 = parse("RETURN CASE WHEN n.x > 0 THEN 'positive' ELSE 'non-positive' END").unwrap();
    let e2 = projection_expr(&q2);
    match e2 {
        Expression::Case {
            scrutinee, arms, ..
        } => {
            assert!(scrutinee.is_none());
            assert_eq!(arms.len(), 1);
        }
        _ => panic!(),
    }
}

#[test]
fn parses_parameter() {
    let q = parse("MATCH (n) WHERE n.id = $userId RETURN n").unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Match(m) => m,
        _ => panic!(),
    };
    let pred = m.where_clause.as_ref().unwrap();
    let rhs = match pred {
        Expression::Binary { rhs, .. } => rhs,
        _ => panic!(),
    };
    assert!(matches!(rhs.as_ref(), Expression::Parameter(name, _) if name == "userId"));
}

#[test]
fn parses_multiple_patterns_in_match() {
    let q = parse("MATCH (a), (b)-[r]->(c) RETURN a, c").unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Match(m) => m,
        _ => panic!(),
    };
    assert_eq!(m.patterns.len(), 2);
}

#[test]
fn parses_named_path() {
    let q = parse("MATCH p = (a)-[r]->(b) RETURN p").unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Match(m) => m,
        _ => panic!(),
    };
    assert_eq!(m.patterns[0].variable.as_deref(), Some("p"));
}

#[test]
fn parses_negative_number_literal() {
    let q = parse("RETURN -42").unwrap();
    let expr = projection_expr(&q);
    match expr {
        Expression::Unary {
            op: UnaryOp::Neg,
            expr,
            ..
        } => {
            assert!(matches!(expr.as_ref(), Expression::Integer(42, _)));
        }
        _ => panic!(),
    }
}

#[test]
fn parses_float_and_string_literals() {
    let q = parse("RETURN 3.14, 'hello', \"world\"").unwrap();
    let r = match &q.parts[0].query.clauses[0] {
        Clause::Return(r) => r,
        _ => panic!(),
    };
    assert!(matches!(
        &r.items[0],
        ProjectionItem::Expression {
            expr: Expression::Float(_, _),
            ..
        }
    ));
    assert!(matches!(
        &r.items[1],
        ProjectionItem::Expression { expr: Expression::String(s, _), .. } if s == "hello"
    ));
    assert!(matches!(
        &r.items[2],
        ProjectionItem::Expression { expr: Expression::String(s, _), .. } if s == "world"
    ));
}

#[test]
fn parses_comparison_chain_left_associative() {
    // a = b = c parses as (a = b) = c (Cypher does not have chained
    // comparisons; comparisons are left-associative and yield booleans).
    let q = parse("RETURN 1 = 1 = TRUE").unwrap();
    let expr = projection_expr(&q);
    match expr {
        Expression::Binary {
            op: BinaryOp::Eq,
            lhs,
            ..
        } => {
            assert!(matches!(
                lhs.as_ref(),
                Expression::Binary {
                    op: BinaryOp::Eq,
                    ..
                }
            ));
        }
        _ => panic!(),
    }
}

#[test]
fn parses_dotted_function_name() {
    let q = parse("RETURN apoc.coll.sum([1, 2, 3])").unwrap();
    let expr = projection_expr(&q);
    match expr {
        Expression::FunctionCall { name, .. } => {
            assert_eq!(
                name,
                &vec!["apoc".to_string(), "coll".to_string(), "sum".to_string()]
            );
        }
        _ => panic!(),
    }
}

// ===== Error cases ==========================================================

#[test]
fn errors_on_unterminated_pattern() {
    let err = parse("MATCH (n").unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("expected") || msg.contains("unexpected"),
        "msg={msg}"
    );
}

#[test]
fn errors_on_missing_return_expression() {
    let err = parse("RETURN").unwrap_err();
    let _ = format!("{err}");
}

#[test]
fn errors_on_dangling_keyword() {
    let err = parse("WHERE n").unwrap_err();
    let _ = format!("{err}");
}

#[test]
fn errors_on_lex_error_propagation() {
    // Unterminated string is a LexError that must surface as a ParseError.
    let err = parse("RETURN 'oops").unwrap_err();
    let _ = format!("{err}");
}

#[test]
fn errors_on_empty_input() {
    let err = parse("").unwrap_err();
    let _ = format!("{err}");
}

#[test]
fn errors_on_only_whitespace_and_comments() {
    let err = parse("  /* nothing */ // here\n").unwrap_err();
    let _ = format!("{err}");
}

// ===== Complex multi-clause integration =====================================

#[test]
fn parses_multi_clause_pipeline() {
    let q = parse(
        "MATCH (u:User)-[:WROTE]->(p:Post)
         WHERE p.created_at > 1000
         WITH u, count(p) AS posts
         WHERE posts > 5
         RETURN u.handle, posts ORDER BY posts DESC LIMIT 20",
    )
    .unwrap();
    let single = &q.parts[0].query;
    assert_eq!(single.clauses.len(), 3);
    assert!(matches!(&single.clauses[0], Clause::Match(_)));
    assert!(matches!(&single.clauses[1], Clause::With(_)));
    assert!(matches!(&single.clauses[2], Clause::Return(_)));
    if let Clause::With(w) = &single.clauses[1] {
        assert!(w.where_clause.is_some());
    } else {
        panic!();
    }
}

#[test]
fn parses_three_arm_union_all() {
    let q = parse(
        "MATCH (a:A) RETURN a.id AS id
         UNION ALL
         MATCH (b:B) RETURN b.id AS id
         UNION ALL
         MATCH (c:C) RETURN c.id AS id",
    )
    .unwrap();
    assert_eq!(q.parts.len(), 3);
    assert!(q.parts[0].union.is_none());
    assert_eq!(q.parts[1].union, Some(UnionKind::All));
    assert_eq!(q.parts[2].union, Some(UnionKind::All));
}

#[test]
fn parses_deeply_nested_arithmetic() {
    // ((1 + 2) * (3 - 4)) ^ ((5 % 6) / 7)
    let q = parse("RETURN ((1 + 2) * (3 - 4)) ^ ((5 % 6) / 7)").unwrap();
    let expr = projection_expr(&q);
    // Top operator is ^
    match expr {
        Expression::Binary {
            op: BinaryOp::Pow, ..
        } => {}
        e => panic!("expected pow at top, got {e:?}"),
    }
}

#[test]
fn parses_boolean_precedence_not_and_or() {
    // NOT a AND b OR c should parse as (NOT a AND b) OR c
    let q = parse("MATCH (n) WHERE NOT n.x AND n.y OR n.z RETURN n").unwrap();
    let m = match &q.parts[0].query.clauses[0] {
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
            // LHS should be AND
            assert!(matches!(
                lhs.as_ref(),
                Expression::Binary {
                    op: BinaryOp::And,
                    ..
                }
            ));
            // RHS should be a simple property
            assert!(matches!(rhs.as_ref(), Expression::Property { .. }));
            // Inside the AND, LHS should be NOT
            if let Expression::Binary { lhs: and_lhs, .. } = lhs.as_ref() {
                assert!(matches!(
                    and_lhs.as_ref(),
                    Expression::Unary {
                        op: UnaryOp::Not,
                        ..
                    }
                ));
            }
        }
        e => panic!("expected OR at top, got {e:?}"),
    }
}

#[test]
fn parses_three_segment_path() {
    let q = parse("MATCH (a:A)-[:R1]->(b:B)-[:R2]->(c:C)-[:R3]->(d:D) RETURN a, d").unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Match(m) => m,
        _ => panic!(),
    };
    assert_eq!(m.patterns[0].path.tail.len(), 3);
    assert_eq!(
        m.patterns[0].path.tail[0].relationship.types,
        vec!["R1".to_string()]
    );
    assert_eq!(
        m.patterns[0].path.tail[1].relationship.types,
        vec!["R2".to_string()]
    );
    assert_eq!(
        m.patterns[0].path.tail[2].relationship.types,
        vec!["R3".to_string()]
    );
}

#[test]
fn parses_deep_property_chain() {
    let q = parse("RETURN n.address.city.zip.code").unwrap();
    let expr = projection_expr(&q);
    let mut depth = 0;
    let mut cur = expr;
    while let Expression::Property { base, .. } = cur {
        depth += 1;
        cur = base;
    }
    assert_eq!(depth, 4);
    assert!(matches!(cur, Expression::Variable(name, _) if name == "n"));
}

#[test]
fn parses_nested_map_and_list() {
    let q = parse("RETURN {a: [1, 2, {b: 3}], c: {d: [4, 5]}}").unwrap();
    let expr = projection_expr(&q);
    match expr {
        Expression::Map(m) => {
            assert_eq!(m.entries.len(), 2);
            assert!(matches!(&m.entries[0].1, Expression::List { items, .. } if items.len() == 3));
            assert!(matches!(&m.entries[1].1, Expression::Map(_)));
        }
        _ => panic!(),
    }
}

#[test]
fn parses_function_call_with_complex_args() {
    let q = parse("RETURN coalesce(n.name, n.handle, 'anon')").unwrap();
    let expr = projection_expr(&q);
    match expr {
        Expression::FunctionCall {
            name,
            args,
            distinct,
            ..
        } => {
            assert_eq!(name, &vec!["coalesce".to_string()]);
            assert!(!*distinct);
            assert_eq!(args.len(), 3);
        }
        _ => panic!(),
    }
}

#[test]
fn parses_nested_function_calls() {
    let q = parse("RETURN toUpper(trim(n.name))").unwrap();
    let expr = projection_expr(&q);
    match expr {
        Expression::FunctionCall { name, args, .. } => {
            assert_eq!(name, &vec!["toUpper".to_string()]);
            assert_eq!(args.len(), 1);
            assert!(matches!(&args[0], Expression::FunctionCall { .. }));
        }
        _ => panic!(),
    }
}

#[test]
fn parses_case_in_return_with_alias() {
    let q = parse("MATCH (n) RETURN CASE WHEN n.score > 50 THEN 'pass' ELSE 'fail' END AS verdict")
        .unwrap();
    let r = match &q.parts[0].query.clauses[1] {
        Clause::Return(r) => r,
        _ => panic!(),
    };
    match &r.items[0] {
        ProjectionItem::Expression { expr, alias } => {
            assert_eq!(alias.as_deref(), Some("verdict"));
            assert!(matches!(expr, Expression::Case { .. }));
        }
        _ => panic!(),
    }
}

#[test]
fn parses_merge_with_empty_on_arms() {
    // MERGE without ON arms is the most common form.
    let q = parse("MERGE (n:Person {id: 1})").unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Merge(m) => m,
        _ => panic!(),
    };
    assert!(m.on_create.is_empty());
    assert!(m.on_match.is_empty());
}

#[test]
fn parses_delete_multiple_targets() {
    let q = parse("MATCH (a), (b), (c) DETACH DELETE a, b, c").unwrap();
    let d = match &q.parts[0].query.clauses[1] {
        Clause::Delete(d) => d,
        _ => panic!(),
    };
    assert!(d.detach);
    assert_eq!(d.targets.len(), 3);
}

#[test]
fn parses_skip_limit_with_parameters() {
    let q = parse("MATCH (n) RETURN n SKIP $offset LIMIT $page").unwrap();
    let r = match &q.parts[0].query.clauses[1] {
        Clause::Return(r) => r,
        _ => panic!(),
    };
    assert!(matches!(&r.skip, Some(Expression::Parameter(p, _)) if p == "offset"));
    assert!(matches!(&r.limit, Some(Expression::Parameter(p, _)) if p == "page"));
}

#[test]
fn parses_string_predicate_chain() {
    let q = parse(
        "MATCH (n:Post) WHERE n.title STARTS WITH 'How' AND n.body CONTAINS 'rust' RETURN n.title",
    )
    .unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Match(m) => m,
        _ => panic!(),
    };
    let pred = m.where_clause.as_ref().unwrap();
    // Top-level AND of two string predicates
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
                    op: BinaryOp::StartsWith,
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
fn parses_in_with_parameter_list() {
    let q = parse("MATCH (n) WHERE n.status IN $allowed RETURN n").unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Match(m) => m,
        _ => panic!(),
    };
    let pred = m.where_clause.as_ref().unwrap();
    assert!(matches!(pred, Expression::In { .. }));
}

#[test]
fn parses_aggregations_with_distinct() {
    let q = parse(
        "MATCH (u)-[:LIKED]->(p) RETURN u.id, count(DISTINCT p) AS unique_likes, sum(p.score) AS score_total",
    )
    .unwrap();
    let r = match &q.parts[0].query.clauses[1] {
        Clause::Return(r) => r,
        _ => panic!(),
    };
    assert_eq!(r.items.len(), 3);
    if let ProjectionItem::Expression {
        expr: Expression::FunctionCall { distinct, name, .. },
        ..
    } = &r.items[1]
    {
        assert!(*distinct);
        assert_eq!(name, &vec!["count".to_string()]);
    } else {
        panic!();
    }
}

#[test]
fn parses_grouping_parens_in_where() {
    let q = parse("MATCH (n) WHERE (n.x > 1 OR n.y > 1) AND n.z < 10 RETURN n").unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Match(m) => m,
        _ => panic!(),
    };
    let pred = m.where_clause.as_ref().unwrap();
    // Top-level AND (because of explicit parens)
    match pred {
        Expression::Binary {
            op: BinaryOp::And,
            lhs,
            ..
        } => {
            assert!(matches!(
                lhs.as_ref(),
                Expression::Binary {
                    op: BinaryOp::Or,
                    ..
                }
            ));
        }
        _ => panic!(),
    }
}

#[test]
fn parses_long_unwind_pipeline() {
    let q = parse(
        "UNWIND [1, 2, 3, 4, 5] AS i
         WITH i, i * i AS sq
         WHERE sq > 4
         RETURN i, sq ORDER BY i",
    )
    .unwrap();
    let single = &q.parts[0].query;
    assert_eq!(single.clauses.len(), 3);
    assert!(matches!(&single.clauses[0], Clause::Unwind(_)));
    assert!(matches!(&single.clauses[1], Clause::With(_)));
    assert!(matches!(&single.clauses[2], Clause::Return(_)));
}

#[test]
fn parses_multi_pattern_with_inline_props() {
    let q =
        parse("MATCH (u:User {handle: $h}), (p:Post {id: $pid}) WHERE p.author = u.id RETURN p")
            .unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Match(m) => m,
        _ => panic!(),
    };
    assert_eq!(m.patterns.len(), 2);
    assert!(m.patterns[0].path.head.properties.is_some());
    assert!(m.patterns[1].path.head.properties.is_some());
}

#[test]
fn parses_inline_comments_between_clauses() {
    let q = parse(
        "MATCH (n) // pick a node\n\
         /* multi-line\n   comment */\
         WHERE n.x = 1\n\
         RETURN n // final projection",
    )
    .unwrap();
    assert_eq!(q.parts[0].query.clauses.len(), 2);
}

#[test]
fn parses_unicode_identifiers_and_strings() {
    let q =
        parse("MATCH (пользователь:Пользователь {имя: 'Анна 🌸'}) RETURN пользователь").unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Match(m) => m,
        _ => panic!(),
    };
    assert_eq!(
        m.patterns[0].path.head.variable.as_deref(),
        Some("пользователь")
    );
    assert_eq!(
        m.patterns[0].path.head.labels,
        vec!["Пользователь".to_string()]
    );
}

#[test]
fn parses_backtick_quoted_identifier_with_spaces() {
    let q = parse("MATCH (`weird name`:Person) RETURN `weird name`").unwrap();
    let m = match &q.parts[0].query.clauses[0] {
        Clause::Match(m) => m,
        _ => panic!(),
    };
    assert_eq!(
        m.patterns[0].path.head.variable.as_deref(),
        Some("weird name")
    );
}

#[test]
fn parses_semicolon_terminator() {
    let q = parse("MATCH (n) RETURN n;").unwrap();
    assert_eq!(q.parts[0].query.clauses.len(), 2);
}

#[test]
fn parses_function_then_property_then_index() {
    // Postfix chain across function call, property access, and indexing.
    let q = parse("RETURN keys(n)[0]").unwrap();
    let expr = projection_expr(&q);
    match expr {
        Expression::Index { base, .. } => {
            assert!(matches!(base.as_ref(), Expression::FunctionCall { .. }));
        }
        _ => panic!(),
    }
}

#[test]
fn parses_optional_match_then_match_then_return() {
    let q = parse(
        "MATCH (u:User {id: $uid})
         OPTIONAL MATCH (u)-[:LIKED]->(p:Post)
         RETURN u.handle, count(p) AS likes",
    )
    .unwrap();
    let single = &q.parts[0].query;
    assert_eq!(single.clauses.len(), 3);
    if let Clause::Match(m) = &single.clauses[1] {
        assert!(m.optional);
    } else {
        panic!();
    }
}

#[test]
fn parses_create_with_existing_var_then_relate() {
    let q = parse(
        "MATCH (a:User {id: 1}), (b:User {id: 2})
         CREATE (a)-[:FOLLOWS {since: 2026}]->(b)",
    )
    .unwrap();
    let create = match &q.parts[0].query.clauses[1] {
        Clause::Create(c) => c,
        _ => panic!(),
    };
    let rel = &create.patterns[0].path.tail[0].relationship;
    assert_eq!(rel.types, vec!["FOLLOWS".to_string()]);
    assert!(rel.properties.is_some());
}

// ===== Helpers ==============================================================

fn projection_expr(q: &drevo::cypher::ast::Query) -> &Expression {
    let r = match &q.parts[0].query.clauses[0] {
        Clause::Return(r) => r,
        _ => panic!("expected RETURN"),
    };
    match &r.items[0] {
        ProjectionItem::Expression { expr, .. } => expr,
        _ => panic!("expected expression projection"),
    }
}
