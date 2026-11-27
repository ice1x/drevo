//! Which variables a query has at each clause, checked without running it.
//!
//! `EXPLAIN` runs nothing, so a variable that was never bound
//! (`MATCH (p) RETURN q.name`) would go unnoticed: when the query runs, the
//! error comes from evaluating the first row.
//! [`unbound_variable`](crate::cypher::scope::unbound_variable) finds it
//! from the text alone.
//!
//! ```
//! use drevo::cypher::parser::parse;
//! use drevo::cypher::scope::unbound_variable;
//!
//! let query = parse("MATCH (p:Person) RETURN q.name").unwrap();
//! assert_eq!(unbound_variable(&query).unwrap().0, "q");
//! let query = parse("MATCH (p:Person) RETURN p.name").unwrap();
//! assert!(unbound_variable(&query).is_none());
//! ```
//!
//! The check is careful rather than complete: it reports a variable only
//! where it is sure of the scope, and stops at the first clause whose
//! bindings it does not model, so it never refuses a query that would run.

use std::collections::HashSet;

use crate::cypher::ast::{
    Clause, Expression, MapProjectionSelector, NamedPattern, PathPattern, ProjectionItem, Query,
    SubqueryImports,
};
use crate::cypher::lexer::Span;

/// The first variable `query` reads that no clause before it has bound,
/// with where it stands; `None` when there is none, or none the check is
/// sure of.
#[must_use]
pub fn unbound_variable(query: &Query) -> Option<(String, Span)> {
    query
        .parts
        .iter()
        .find_map(|part| check_clauses(&part.query.clauses, HashSet::new()).err())
}

type Scope = HashSet<String>;
type Found = Result<(), (String, Span)>;

/// Walk `clauses` from `bound`. `Ok(scope)` is the scope after the last
/// clause the check models.
fn check_clauses(clauses: &[Clause], mut bound: Scope) -> Result<Scope, (String, Span)> {
    for clause in clauses {
        match clause {
            Clause::Match(m) => {
                for pattern in &m.patterns {
                    bind_pattern(pattern, &mut bound);
                }
                if let Some(predicate) = &m.where_clause {
                    check(predicate, &bound, &mut Vec::new())?;
                }
            }
            Clause::Create(c) => {
                for pattern in &c.patterns {
                    bind_pattern(pattern, &mut bound);
                }
            }
            Clause::Merge(m) => bind_pattern(&m.pattern, &mut bound),
            Clause::Unwind(u) => {
                check(&u.expression, &bound, &mut Vec::new())?;
                bound.insert(u.alias.clone());
            }
            Clause::With(w) => {
                let mut next = Scope::new();
                for item in &w.items {
                    match item {
                        ProjectionItem::Star => next.extend(bound.iter().cloned()),
                        ProjectionItem::Expression { expr, alias, .. } => {
                            check(expr, &bound, &mut Vec::new())?;
                            match (alias, expr) {
                                (Some(alias), _) => next.insert(alias.clone()),
                                (None, Expression::Variable(name, _)) => next.insert(name.clone()),
                                (None, _) => false,
                            };
                        }
                    }
                }
                bound = next;
                if let Some(predicate) = &w.where_clause {
                    check(predicate, &bound, &mut Vec::new())?;
                }
            }
            Clause::Return(r) => {
                for item in &r.items {
                    if let ProjectionItem::Expression { expr, .. } = item {
                        check(expr, &bound, &mut Vec::new())?;
                    }
                }
            }
            Clause::Delete(d) => {
                for target in &d.targets {
                    check(target, &bound, &mut Vec::new())?;
                }
            }
            Clause::Call(call) => match &call.yields {
                Some(yields) => {
                    for item in yields {
                        bound.insert(item.alias.clone().unwrap_or_else(|| item.name.clone()));
                    }
                }
                // Without YIELD the call ends the query with its own columns.
                None => return Ok(bound),
            },
            Clause::CallSubquery(sub) => {
                let inner = match &sub.imports {
                    SubqueryImports::All | SubqueryImports::LeadingWith => bound.clone(),
                    SubqueryImports::Variables(names) => names.iter().cloned().collect(),
                };
                for part in &sub.body.parts {
                    let clauses = &part.query.clauses;
                    check_clauses(clauses, inner.clone())?;
                    if let Some(Clause::Return(r)) = clauses.last() {
                        for item in &r.items {
                            match item {
                                ProjectionItem::Expression {
                                    alias: Some(alias), ..
                                } => {
                                    bound.insert(alias.clone());
                                }
                                ProjectionItem::Expression {
                                    expr: Expression::Variable(name, _),
                                    ..
                                } => {
                                    bound.insert(name.clone());
                                }
                                // A column named by its text, or `*`: not
                                // modelled.
                                _ => return Ok(bound),
                            }
                        }
                    }
                }
            }
            // Writes that bind nothing new.
            Clause::Set(_) | Clause::Remove(_) | Clause::Foreach(_) => {}
            // Bindings that are not modelled: say nothing about the rest.
            Clause::Search(_)
            | Clause::Explain(_)
            | Clause::CreateVectorIndex(_)
            | Clause::CreatePathIndex(_)
            | Clause::DropIndex(_)
            | Clause::CreateConstraint(_)
            | Clause::DropConstraint(_)
            | Clause::SchemaNoop(_) => return Ok(bound),
        }
    }
    Ok(bound)
}

/// Add the variables a pattern binds: its path, nodes and relationships.
fn bind_pattern(pattern: &NamedPattern, bound: &mut Scope) {
    bound.extend(pattern.variable.iter().cloned());
    bound.extend(path_variables(&pattern.path));
}

fn path_variables(path: &PathPattern) -> Vec<String> {
    let mut names: Vec<String> = path.head.variable.iter().cloned().collect();
    for segment in &path.tail {
        names.extend(segment.relationship.variable.iter().cloned());
        names.extend(segment.node.variable.iter().cloned());
    }
    names
}

/// Report the first variable of `expr` that is neither in `bound` nor one
/// of the `locals` an enclosing comprehension or subquery introduced.
fn check(expr: &Expression, bound: &Scope, locals: &mut Vec<String>) -> Found {
    /// Check `parts` with `names` added to the locals.
    fn with_locals(
        names: Vec<String>,
        parts: &[Option<&Expression>],
        bound: &Scope,
        locals: &mut Vec<String>,
    ) -> Found {
        let before = locals.len();
        locals.extend(names);
        let found = parts
            .iter()
            .flatten()
            .try_for_each(|part| check(part, bound, locals));
        locals.truncate(before);
        found
    }
    match expr {
        Expression::Variable(name, span) => {
            if bound.contains(name) || locals.contains(name) {
                Ok(())
            } else {
                Err((name.clone(), *span))
            }
        }
        Expression::Integer(..)
        | Expression::Float(..)
        | Expression::String(..)
        | Expression::True(_)
        | Expression::False(_)
        | Expression::Null(_)
        | Expression::Parameter(..)
        | Expression::Star(_) => Ok(()),
        Expression::Property { base, .. } => check(base, bound, locals),
        Expression::List { items, .. } => items.iter().try_for_each(|e| check(e, bound, locals)),
        Expression::Map(map) => map
            .entries
            .iter()
            .try_for_each(|(_, e)| check(e, bound, locals)),
        Expression::FunctionCall { args, .. } => {
            args.iter().try_for_each(|e| check(e, bound, locals))
        }
        Expression::Unary { expr, .. }
        | Expression::IsNull { expr, .. }
        | Expression::HasLabels { expr, .. } => check(expr, bound, locals),
        Expression::Binary { lhs, rhs, .. } => {
            check(lhs, bound, locals)?;
            check(rhs, bound, locals)
        }
        Expression::In { expr, list, .. } => {
            check(expr, bound, locals)?;
            check(list, bound, locals)
        }
        Expression::Index { base, index, .. } => {
            check(base, bound, locals)?;
            check(index, bound, locals)
        }
        Expression::Slice { base, from, to, .. } => {
            check(base, bound, locals)?;
            [from, to]
                .into_iter()
                .flatten()
                .try_for_each(|e| check(e, bound, locals))
        }
        Expression::Case {
            scrutinee,
            arms,
            else_branch,
            ..
        } => {
            [scrutinee, else_branch]
                .into_iter()
                .flatten()
                .try_for_each(|e| check(e, bound, locals))?;
            arms.iter().try_for_each(|(when, then)| {
                check(when, bound, locals)?;
                check(then, bound, locals)
            })
        }
        Expression::ListComprehension {
            variable,
            list,
            predicate,
            projection,
            ..
        } => {
            check(list, bound, locals)?;
            with_locals(
                vec![variable.clone()],
                &[predicate.as_deref(), projection.as_deref()],
                bound,
                locals,
            )
        }
        Expression::ListPredicate {
            variable,
            list,
            predicate,
            ..
        } => {
            check(list, bound, locals)?;
            with_locals(vec![variable.clone()], &[Some(predicate)], bound, locals)
        }
        Expression::Reduce {
            accumulator,
            init,
            variable,
            list,
            expr,
            ..
        } => {
            check(init, bound, locals)?;
            check(list, bound, locals)?;
            with_locals(
                vec![accumulator.clone(), variable.clone()],
                &[Some(expr)],
                bound,
                locals,
            )
        }
        Expression::MapProjection {
            base, selectors, ..
        } => {
            check(base, bound, locals)?;
            selectors.iter().try_for_each(|selector| match selector {
                MapProjectionSelector::Literal(_, e) => check(e, bound, locals),
                _ => Ok(()),
            })
        }
        Expression::PatternComprehension {
            pattern,
            predicate,
            projection,
            ..
        } => with_locals(
            path_variables(pattern),
            &[predicate.as_deref(), Some(projection)],
            bound,
            locals,
        ),
        Expression::ExistsSubquery {
            pattern, predicate, ..
        }
        | Expression::CountSubquery {
            pattern, predicate, ..
        } => with_locals(
            path_variables(pattern),
            &[predicate.as_deref()],
            bound,
            locals,
        ),
        Expression::PatternPredicate { .. } => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::parser::parse;

    fn unbound(query: &str) -> Option<String> {
        unbound_variable(&parse(query).unwrap_or_else(|e| panic!("`{query}`: {e}"))).map(|f| f.0)
    }

    #[test]
    fn a_variable_no_clause_bound_is_found() {
        for (query, name) in [
            ("MATCH (p:Person) RETURN q.name", "q"),
            ("MATCH (p:Person) WHERE q.age > 1 RETURN p", "q"),
            ("UNWIND xs AS x RETURN x", "xs"),
            ("MATCH (p) WITH p.name AS n RETURN p", "p"),
            ("MATCH (p) WITH p.name AS n WHERE p.age > 1 RETURN n", "p"),
            ("MATCH (p) RETURN [x IN [1] | y]", "y"),
            ("MATCH (p) DELETE q", "q"),
            ("RETURN 1 AS x UNION RETURN y AS x", "y"),
            ("MATCH (p) CALL (p) { RETURN q.name AS n } RETURN n", "q"),
            ("MATCH (p) CALL () { RETURN p.name AS n } RETURN n", "p"),
            ("CALL db.labels() YIELD label RETURN lbl", "lbl"),
        ] {
            assert_eq!(unbound(query).as_deref(), Some(name), "{query}");
        }
    }

    #[test]
    fn every_way_to_bind_a_variable_is_known() {
        for query in [
            "MATCH path = (a:Person)-[r:KNOWS]->(b) WHERE r.since > 1 RETURN path, a, r, b",
            "MATCH (p) WITH p, p.name AS n WHERE n <> '' RETURN p, n",
            "MATCH (p) WITH * RETURN p",
            "UNWIND [1, 2] AS x RETURN x * 2",
            "CREATE (a:X)-[r:Y]->(b) RETURN a, r, b",
            "MERGE (a:X {k: 1}) RETURN a",
            "RETURN [x IN [1, 2] WHERE x > 1 | x * 2], reduce(s = 0, x IN [1] | s + x)",
            "RETURN all(x IN [1] WHERE x > 0), $param, count(*)",
            "MATCH (p) RETURN [(p)-[k:KNOWS]->(f) WHERE f.age > k.since | f.name]",
            "MATCH (p) WHERE EXISTS { (p)-[:KNOWS]->(f) WHERE f.age > 1 } RETURN p {.name, n: p.age}",
            "MATCH (p) RETURN COUNT { (p)-->(x) WHERE x.a = p.a } AS c ORDER BY c",
            "CALL db.labels() YIELD label AS l RETURN l",
            "CALL db.labels()",
            "MATCH (p) CALL (p) { MATCH (p)-->(f) RETURN count(f) AS c } RETURN p, c",
            "MATCH (p) CALL { WITH p RETURN p.name AS n } RETURN n",
            "MATCH (p) CALL (*) { RETURN p.name AS n } RETURN n",
            "MATCH (p) FOREACH (x IN [1] | SET p.x = x) RETURN p",
            "MATCH (p) RETURN p.name AS n ORDER BY p.age",
            "MATCH (p) RETURN CASE p.x WHEN 1 THEN p.a ELSE p.b END, p.xs[0], p.xs[0..1]",
        ] {
            assert_eq!(unbound(query), None, "{query}");
        }
    }

    #[test]
    fn a_clause_that_is_not_modelled_ends_the_check() {
        // A subquery whose column is named by its text: the rest is not
        // judged, though `nope` is not bound.
        assert_eq!(unbound("CALL { RETURN 1 + 1 } RETURN nope"), None);
    }
}
