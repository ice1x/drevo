//! The gRPC API (issue #583): Cypher over HTTP/2 + protobuf, rows streamed
//! in batches. Drives a real tonic server on a loopback port with the
//! generated client.

#![cfg(feature = "grpc")]
// The helpers return tonic's `Status` by value, like the generated client.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::Arc;

use drevo::database_registry::DatabaseRegistry;
use drevo::grpc::pb::drevo_client::DrevoClient;
use drevo::grpc::pb::{self, execute_response::Part, value::Kind};
use drevo::native_service::NativeService;
use tonic::Code;

/// Start a server over a fresh in-memory registry; returns a client and the
/// registry (to set up extra databases).
async fn start() -> (
    DrevoClient<tonic::transport::Channel>,
    Arc<DatabaseRegistry>,
) {
    let registry = Arc::new(DatabaseRegistry::new(Arc::new(NativeService::in_memory())));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = Arc::clone(&registry);
    tokio::spawn(async move {
        drevo::grpc::serve(served, listener, std::future::pending())
            .await
            .unwrap();
    });
    let client = DrevoClient::connect(format!("http://{addr}"))
        .await
        .unwrap();
    (client, registry)
}

fn req(query: &str) -> pb::ExecuteRequest {
    pb::ExecuteRequest {
        query: query.to_string(),
        params: HashMap::new(),
        database: String::new(),
    }
}

/// Run a query and collect (columns, rows, summary, number of row batches).
async fn run(
    client: &mut DrevoClient<tonic::transport::Channel>,
    request: pb::ExecuteRequest,
) -> Result<(Vec<String>, Vec<pb::Row>, pb::Summary, usize), tonic::Status> {
    let mut stream = client.execute(request).await?.into_inner();
    let (mut columns, mut rows, mut summary, mut batches) = (None, Vec::new(), None, 0);
    while let Some(msg) = stream.message().await? {
        match msg.part.expect("part") {
            Part::Header(h) => {
                assert!(columns.is_none(), "one header");
                assert!(rows.is_empty(), "header first");
                columns = Some(h.columns);
            }
            Part::Rows(b) => {
                assert!(summary.is_none(), "rows before the summary");
                batches += 1;
                rows.extend(b.rows);
            }
            Part::Summary(s) => summary = Some(s),
        }
    }
    Ok((
        columns.expect("header"),
        rows,
        summary.expect("summary"),
        batches,
    ))
}

fn k(v: &pb::Value) -> &Kind {
    v.kind.as_ref().expect("kind")
}

fn int(i: i64) -> pb::Value {
    pb::Value {
        kind: Some(Kind::Integer(i)),
    }
}

#[tokio::test]
async fn scalars_keep_their_cypher_types() {
    let (mut c, _) = start().await;
    let (cols, rows, _, _) = run(
        &mut c,
        req("RETURN 1 AS i, 2.5 AS f, 'x' AS s, true AS b, null AS n, [1, 'a'] AS l, {k: 1} AS m"),
    )
    .await
    .unwrap();
    assert_eq!(cols, ["i", "f", "s", "b", "n", "l", "m"]);
    let v = &rows[0].values;
    assert_eq!(k(&v[0]), &Kind::Integer(1));
    assert_eq!(k(&v[1]), &Kind::Float(2.5));
    assert_eq!(k(&v[2]), &Kind::String("x".into()));
    assert_eq!(k(&v[3]), &Kind::Boolean(true));
    assert_eq!(k(&v[4]), &Kind::Null(true));
    let Kind::List(l) = k(&v[5]) else {
        panic!("list")
    };
    assert_eq!(l.values.len(), 2);
    let Kind::Map(m) = k(&v[6]) else {
        panic!("map")
    };
    assert_eq!(k(&m.entries["k"]), &Kind::Integer(1));
}

#[tokio::test]
async fn embeddings_travel_as_packed_float_lists() {
    let (mut c, _) = start().await;
    let (_, rows, _, _) = run(&mut c, req("RETURN [0.5, -1.25, 3.0] AS e, [] AS empty"))
        .await
        .unwrap();
    assert_eq!(
        k(&rows[0].values[0]),
        &Kind::Floats(pb::FloatList {
            values: vec![0.5, -1.25, 3.0]
        })
    );
    assert!(matches!(k(&rows[0].values[1]), Kind::List(l) if l.values.is_empty()));
}

#[tokio::test]
async fn parameters_of_every_kind() {
    let (mut c, _) = start().await;
    let mut r = req("RETURN $n + 1 AS n, $tags[1] AS tag, $meta.owner AS owner, size($vec) AS d");
    r.params = HashMap::from([
        ("n".to_string(), int(41)),
        (
            "tags".to_string(),
            pb::Value {
                kind: Some(Kind::List(pb::List {
                    values: vec![
                        pb::Value {
                            kind: Some(Kind::String("a".into())),
                        },
                        pb::Value {
                            kind: Some(Kind::String("b".into())),
                        },
                    ],
                })),
            },
        ),
        (
            "meta".to_string(),
            pb::Value {
                kind: Some(Kind::Map(pb::Map {
                    entries: HashMap::from([(
                        "owner".to_string(),
                        pb::Value {
                            kind: Some(Kind::String("ann".into())),
                        },
                    )]),
                })),
            },
        ),
        (
            "vec".to_string(),
            pb::Value {
                kind: Some(Kind::Floats(pb::FloatList {
                    values: vec![0.1, 0.2],
                })),
            },
        ),
    ]);
    let (_, rows, _, _) = run(&mut c, r).await.unwrap();
    let v = &rows[0].values;
    assert_eq!(k(&v[0]), &Kind::Integer(42));
    assert_eq!(k(&v[1]), &Kind::String("b".into()));
    assert_eq!(k(&v[2]), &Kind::String("ann".into()));
    assert_eq!(k(&v[3]), &Kind::Integer(2));
}

#[tokio::test]
async fn graph_entities_and_write_counters() {
    // Story editor: two chapters linked FOLLOWS.
    let (mut c, _) = start().await;
    let (_, _, summary, _) = run(
        &mut c,
        req("CREATE (a:Chapter {title: 'one'})-[:FOLLOWS {words: 1200}]->(b:Chapter {title: 'two'})"),
    )
    .await
    .unwrap();
    assert_eq!(summary.nodes_created, 2);
    assert_eq!(summary.relationships_created, 1);

    let (_, rows, _, _) = run(
        &mut c,
        req("MATCH p = (a:Chapter {title: 'one'})-[r:FOLLOWS]->(b) RETURN a, r, p"),
    )
    .await
    .unwrap();
    let v = &rows[0].values;
    let Kind::Node(a) = k(&v[0]) else {
        panic!("node")
    };
    assert_eq!(a.labels, ["Chapter"]);
    assert_eq!(k(&a.properties["title"]), &Kind::String("one".into()));
    assert_eq!(a.uuid.len(), 16);
    let Kind::Relationship(r) = k(&v[1]) else {
        panic!("relationship")
    };
    assert_eq!(r.r#type, "FOLLOWS");
    assert_eq!(r.start_node_id, a.id);
    assert_eq!(k(&r.properties["words"]), &Kind::Integer(1200));
    let Kind::Path(p) = k(&v[2]) else {
        panic!("path")
    };
    assert_eq!((p.nodes.len(), p.relationships.len()), (2, 1));
}

#[tokio::test]
async fn large_results_stream_in_batches() {
    let (mut c, _) = start().await;
    let (_, rows, _, batches) = run(&mut c, req("UNWIND range(1, 1000) AS i RETURN i"))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1000);
    assert_eq!(batches, 4, "1000 rows in batches of 256");
    assert_eq!(k(&rows[999].values[0]), &Kind::Integer(1000));
    let (_, rows, _, batches) = run(&mut c, req("MATCH (n:Nothing) RETURN n"))
        .await
        .unwrap();
    assert!(rows.is_empty());
    assert_eq!(batches, 0, "no rows, no batches");
}

#[tokio::test]
async fn databases_are_selected_by_name() {
    let (mut c, registry) = start().await;
    let erp = registry.create_in_memory("erp").unwrap();
    erp.execute(
        &drevo::cypher::parser::parse("CREATE (:Invoice {no: 7})").unwrap(),
        HashMap::new(),
    )
    .unwrap();
    let mut r = req("MATCH (i:Invoice) RETURN i.no AS no");
    r.database = "erp".into();
    let (_, rows, _, _) = run(&mut c, r).await.unwrap();
    assert_eq!(k(&rows[0].values[0]), &Kind::Integer(7));
    let (_, rows, _, _) = run(&mut c, req("MATCH (i:Invoice) RETURN i"))
        .await
        .unwrap();
    assert!(rows.is_empty(), "the default database is separate");
}

#[tokio::test]
async fn errors_map_to_grpc_statuses() {
    let (mut c, _) = start().await;
    let code = |r: Result<_, tonic::Status>| r.map(|_| ()).unwrap_err().code();
    assert_eq!(
        code(run(&mut c, req("MATCH (n RETURN n")).await),
        Code::InvalidArgument
    );
    assert_eq!(
        code(run(&mut c, req("RETURN nosuchfunction(1)")).await),
        Code::InvalidArgument
    );
    let mut r = req("RETURN 1");
    r.database = "missing".into();
    assert_eq!(code(run(&mut c, r).await), Code::NotFound);
    assert_eq!(
        code(run(&mut c, req("CREATE DATABASE other")).await),
        Code::Unimplemented
    );
    let mut r = req("RETURN $n");
    r.params = HashMap::from([(
        "n".to_string(),
        pb::Value {
            kind: Some(Kind::Node(pb::Node::default())),
        },
    )]);
    assert_eq!(code(run(&mut c, r).await), Code::InvalidArgument);
}

#[tokio::test]
async fn health_reports_the_version() {
    let (mut c, _) = start().await;
    let h = c.health(pb::HealthRequest {}).await.unwrap().into_inner();
    assert!(h.ok);
    assert_eq!(h.version, drevo::VERSION);
}
