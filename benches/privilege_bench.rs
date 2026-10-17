//! The cost of privilege checks on reads: the same queries over the same
//! graph with no checks (no login), as a `reader` (graph-wide grants, so
//! nothing is filtered), and as a role with denies, whose reads are filtered
//! node by node and property by property.
//!
//! Run: `cargo bench --features auth --bench privilege_bench`

use std::collections::HashMap;
use std::hint::black_box;
use std::time::Duration;

use criterion::Criterion;

use drevo::cypher::executor::Restriction;
use drevo::cypher::parser::parse;
use drevo::native_service::NativeService;
use drevo_auth::privileges::SecurityContext;
use drevo_auth::privileges::{builtin_privileges, parse as parse_privilege, Access, Op, Privilege};

/// A task board: `n` tasks, one in ten secret, each with a title, an
/// estimate and a cost.
fn seed(n: usize) -> NativeService {
    let service = NativeService::in_memory();
    let q = parse(
        "UNWIND range(1, $n) AS i \
         CREATE (t:Task {title: 'task ' + toString(i), estimate: i % 13, cost: i * 3}) \
         WITH t, i WHERE i % 10 = 0 SET t:Secret",
    )
    .unwrap();
    let params = HashMap::from([(
        "n".to_string(),
        drevo::cypher::executor::Value::Integer(n as i64),
    )]);
    service.execute(&q, params).unwrap();
    service
}

fn context(commands: &[&str]) -> SecurityContext {
    let mut held: Vec<(Access, Privilege)> = ["PUBLIC", "reader"]
        .iter()
        .flat_map(|r| builtin_privileges(r).unwrap())
        .map(|p| (Access::Grant, p))
        .collect();
    for text in commands {
        let c = parse_privilege(text).unwrap();
        let access = if c.op == Op::Deny {
            Access::Deny
        } else {
            Access::Grant
        };
        held.extend(c.privileges.into_iter().map(|p| (access, p)));
    }
    SecurityContext::new(held, "drevo")
}

fn main() {
    let mut c = Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3))
        .sample_size(20)
        .configure_from_args();

    let n = 20_000;
    let service = seed(n);
    let reader = context(&[]);
    let filtered = context(&[
        "DENY TRAVERSE ON GRAPH * NODES Secret TO r",
        "DENY READ {cost} ON GRAPH * NODES Task TO r",
    ]);
    let queries = [
        (
            "scan",
            "MATCH (t:Task) WHERE t.estimate > 6 RETURN count(t)",
        ),
        ("project", "MATCH (t:Task) RETURN t.title, t.cost"),
    ];
    for (name, text) in queries {
        let query = parse(text).unwrap();
        let mut group = c.benchmark_group(format!("privileges_{name}_{n}"));
        let run = |restriction: Option<Restriction<'_>>| {
            black_box(
                service
                    .execute_as(&query, HashMap::new(), restriction)
                    .unwrap()
                    .rows
                    .len(),
            )
        };
        group.bench_function("no_checks", |b| b.iter(|| run(None)));
        group.bench_function("reader", |b| {
            b.iter(|| {
                run(Some(Restriction {
                    context: &reader,
                    database: "drevo",
                }))
            })
        });
        group.bench_function("filtered", |b| {
            b.iter(|| {
                run(Some(Restriction {
                    context: &filtered,
                    database: "drevo",
                }))
            })
        });
        group.finish();
    }
    c.final_summary();
}
