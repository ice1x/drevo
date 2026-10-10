//! The Arrow Flight endpoint (issue #584): `DoGet` with a Cypher ticket
//! returns the result as Arrow record batches. Drives a real server on a
//! loopback port with arrow-flight's own client.

#![cfg(feature = "arrow-flight")]
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, Int64Type};
use arrow_array::{Array, RecordBatch};
use arrow_flight::{FlightClient, Ticket};
use arrow_schema::DataType;
use drevo::database_registry::DatabaseRegistry;
use drevo::native_service::NativeService;
use futures::TryStreamExt;

async fn start() -> (FlightClient, Arc<DatabaseRegistry>) {
    let registry = Arc::new(DatabaseRegistry::new(Arc::new(NativeService::in_memory())));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = Arc::clone(&registry);
    tokio::spawn(async move {
        drevo::flight::serve(served, listener, std::future::pending())
            .await
            .unwrap();
    });
    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    (FlightClient::new(channel), registry)
}

async fn get(client: &mut FlightClient, ticket: &str) -> Result<Vec<RecordBatch>, String> {
    let stream = client
        .do_get(Ticket::new(ticket.to_string()))
        .await
        .map_err(|e| e.to_string())?;
    stream.try_collect().await.map_err(|e| e.to_string())
}

fn seed(registry: &DatabaseRegistry, q: &str) {
    registry
        .default_service()
        .execute(&drevo::cypher::parser::parse(q).unwrap(), HashMap::new())
        .unwrap();
}

#[tokio::test]
async fn columns_get_arrow_types_from_their_values() {
    let (mut c, registry) = start().await;
    // IT task manager: tasks with estimates, progress, flags, owners, vectors.
    seed(
        &registry,
        "UNWIND range(1, 3) AS i CREATE (:Task {title: 'task-' + toString(i), estimate: i * 2, \
         progress: i / 4.0, done: i = 3, owner: CASE WHEN i = 2 THEN null ELSE 'ann' END, \
         vec: [toFloat(i), 0.5]})",
    );
    let batches = get(
        &mut c,
        "MATCH (t:Task) RETURN t.title AS title, t.estimate AS estimate, t.progress AS progress, \
         t.done AS done, t.owner AS owner, t.vec AS vec, t AS task ORDER BY title",
    )
    .await
    .unwrap();
    assert_eq!(batches.len(), 1);
    let b = &batches[0];
    let types: Vec<DataType> = b
        .schema()
        .fields()
        .iter()
        .map(|f| f.data_type().clone())
        .collect();
    assert_eq!(types[0], DataType::Utf8);
    assert_eq!(types[1], DataType::Int64);
    assert_eq!(types[2], DataType::Float64);
    assert_eq!(types[3], DataType::Boolean);
    assert_eq!(types[4], DataType::Utf8);
    assert!(matches!(&types[5], DataType::List(f) if f.data_type() == &DataType::Float64));
    assert_eq!(types[6], DataType::Utf8, "nodes travel as JSON");

    assert_eq!(b.num_rows(), 3);
    assert_eq!(b.column(1).as_primitive::<Int64Type>().values(), &[2, 4, 6]);
    assert_eq!(b.column(2).as_primitive::<Float64Type>().value(0), 0.25);
    assert!(b.column(3).as_boolean().value(2));
    assert!(b.column(4).is_null(1), "null owner");
    let vecs = b.column(5).as_list::<i32>();
    assert_eq!(
        vecs.value(1).as_primitive::<Float64Type>().values(),
        &[2.0, 0.5]
    );
    let node: serde_json::Value =
        serde_json::from_str(b.column(6).as_string::<i32>().value(0)).unwrap();
    assert_eq!(node["properties"]["title"], "task-1", "{node}");
}

#[tokio::test]
async fn integers_and_floats_share_a_float_column_and_mixed_types_become_json() {
    let (mut c, _) = start().await;
    let batches = get(
        &mut c,
        "UNWIND [1, 2.5] AS x WITH x, CASE WHEN x = 1 THEN 'one' ELSE 2 END AS mixed RETURN x, mixed",
    )
    .await
    .unwrap();
    let b = &batches[0];
    assert_eq!(b.schema().field(0).data_type(), &DataType::Float64);
    assert_eq!(
        b.column(0).as_primitive::<Float64Type>().values(),
        &[1.0, 2.5]
    );
    assert_eq!(b.schema().field(1).data_type(), &DataType::Utf8);
    let mixed = b.column(1).as_string::<i32>();
    assert_eq!((mixed.value(0), mixed.value(1)), ("one", "2"));
}

#[tokio::test]
async fn large_results_come_in_several_batches() {
    let (mut c, _) = start().await;
    let batches = get(&mut c, "UNWIND range(1, 20000) AS i RETURN i")
        .await
        .unwrap();
    let rows: usize = batches.iter().map(RecordBatch::num_rows).sum();
    assert_eq!(rows, 20000);
    assert_eq!(batches.len(), 3, "batches of 8192 rows");
}

#[tokio::test]
async fn json_tickets_carry_parameters_and_a_database() {
    let (mut c, registry) = start().await;
    let erp = registry.create_in_memory("erp").unwrap();
    erp.execute(
        &drevo::cypher::parser::parse("UNWIND range(1, 5) AS i CREATE (:Invoice {no: i})").unwrap(),
        HashMap::new(),
    )
    .unwrap();
    let ticket = serde_json::json!({
        "query": "MATCH (i:Invoice) WHERE i.no >= $min RETURN i.no AS no ORDER BY no",
        "params": {"min": 4},
        "database": "erp"
    })
    .to_string();
    let batches = get(&mut c, &ticket).await.unwrap();
    assert_eq!(
        batches[0].column(0).as_primitive::<Int64Type>().values(),
        &[4, 5]
    );
}

#[tokio::test]
async fn empty_results_still_carry_the_schema() {
    let (mut c, _) = start().await;
    let stream = c
        .do_get(Ticket::new("MATCH (n:Nothing) RETURN n.title AS title"))
        .await
        .unwrap();
    let batches: Vec<RecordBatch> = stream.try_collect().await.unwrap();
    assert!(batches.iter().all(|b| b.num_rows() == 0));
}

#[tokio::test]
async fn errors_and_unsupported_calls() {
    let (mut c, _) = start().await;
    let err = get(&mut c, "MATCH (n RETURN n").await.unwrap_err();
    assert!(err.contains("parse error"), "{err}");
    let err = get(&mut c, r#"{"query": "RETURN 1", "database": "missing"}"#)
        .await
        .unwrap_err();
    assert!(err.contains("no database named"), "{err}");
    let err = c
        .get_flight_info(arrow_flight::FlightDescriptor::new_cmd("RETURN 1"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("DoGet"), "{err}");
}
