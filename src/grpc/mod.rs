//! gRPC API (issue #583): run Cypher over HTTP/2 + protobuf, with the result
//! streamed back in batches.
//!
//! Compiled with the `grpc` feature and served only when `DREVO_GRPC_PORT` is
//! set. The schema is `proto/drevo.proto` (package `drevo.v1`); the Rust code
//! generated from it is checked in as `src/grpc/drevo.v1.rs` and regenerated
//! with `scripts/gen-grpc.sh`, so a build needs no `protoc`.
//!
//! `Execute` answers with one `Header` (the column names), the rows in
//! batches of 256, and one
//! `Summary` (the write counters). Values keep their Cypher type; a list
//! made only of floats — an embedding — travels as a packed `FloatList`.
//! Errors end the call with a gRPC status: `INVALID_ARGUMENT` for a query
//! that does not parse or fails, `NOT_FOUND` for an unknown database,
//! `UNAVAILABLE` while writes are paused by a prepared two-phase commit,
//! `UNIMPLEMENTED` for database administration (use HTTP or Bolt).
//!
//! When the server requires a login, `Execute` needs an `authorization`
//! metadata entry — `Basic <base64 of user:password>` or `Bearer <session
//! token>` — and answers `UNAUTHENTICATED` without a valid one. `Health`
//! needs no login.

#![cfg(feature = "grpc")]
// tonic's service trait returns `Result<_, tonic::Status>` by value, and the
// helpers here feed it directly; boxing `Status` would only add a conversion.
#![allow(clippy::result_large_err)]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use tonic::{Request, Response, Status};

use crate::cypher::executor::{ExecError, ExecResult, NodeValue, RelationshipValue, Value};
use crate::database_registry::DatabaseRegistry;
use crate::security::Security;

/// The protobuf messages and the service/client generated from
/// `proto/drevo.proto`.
#[allow(
    missing_docs,
    clippy::all,
    clippy::pedantic,
    rustdoc::bare_urls,
    rustdoc::invalid_html_tags
)]
pub mod pb {
    include!("drevo.v1.rs");
}

/// How many rows one streamed `RowBatch` carries. Not `pub`: cbindgen would
/// put a public constant into the C header `drevo.h`.
pub(crate) const ROWS_PER_BATCH: usize = 256;

/// The `drevo.v1.Drevo` service over a database registry.
#[derive(Clone)]
pub struct DrevoGrpc {
    registry: Arc<DatabaseRegistry>,
    security: Security,
}

impl DrevoGrpc {
    /// Serve the databases in `registry`, with no login.
    #[must_use]
    pub fn new(registry: Arc<DatabaseRegistry>) -> Self {
        Self {
            registry,
            security: Security::default(),
        }
    }

    /// Log calls in against `security`'s users when it requires a login.
    #[must_use]
    pub fn with_security(mut self, security: Security) -> Self {
        self.security = security;
        self
    }

    /// The tonic service, ready for `Server::add_service`.
    #[must_use]
    pub fn into_service(self) -> pb::drevo_server::DrevoServer<Self> {
        pb::drevo_server::DrevoServer::new(self)
    }
}

type ExecuteStream =
    tonic::codegen::tokio_stream::Iter<std::vec::IntoIter<Result<pb::ExecuteResponse, Status>>>;

#[tonic::async_trait]
impl pb::drevo_server::Drevo for DrevoGrpc {
    type ExecuteStream = ExecuteStream;

    async fn execute(
        &self,
        request: Request<pb::ExecuteRequest>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        let remote = request.remote_addr();
        let signed_in = crate::rpc_login::require_login(
            &self.security,
            request.metadata(),
            &crate::security::Origin::new("grpc").from_client(request.remote_addr()),
        )
        .await?;
        let pb::ExecuteRequest {
            query,
            params,
            database,
        } = request.into_inner();
        if crate::cypher::admin::parse(&query).is_some() {
            return Err(Status::unimplemented(
                "database administration is not available over gRPC; use HTTP or Bolt",
            ));
        }
        if crate::cypher::security_admin::parse(&query).is_some() {
            return Err(Status::unimplemented(
                "user management is not available over gRPC; use HTTP or Bolt",
            ));
        }
        let name = if database.is_empty() {
            self.registry.default_name().to_string()
        } else {
            database
        };
        let service = self
            .registry
            .get(&name)
            .ok_or_else(|| Status::not_found(format!("no database named `{name}`")))?;
        let params = params
            .into_iter()
            .map(|(k, v)| param_from_pb(v).map(|v| (k, v)))
            .collect::<Result<HashMap<_, _>, _>>()?;
        let ast = crate::cypher::parser::parse(&query)
            .map_err(|e| Status::invalid_argument(format!("Cypher parse error: {e}")))?;
        // The executor is synchronous and may run long: keep it off the
        // async workers, as the HTTP API does.
        // The signed-in user's privileges (#636); `None` checks nothing.
        let context = self
            .security
            .context_for(signed_in.as_ref(), self.registry.default_name());
        let audit_security = self.security.clone();
        let audit_origin = crate::security::Origin::new("grpc").from_client(remote);
        let audit_user = signed_in.as_ref().map(|u| u.name.clone());
        let result = tokio::task::spawn_blocking(move || {
            let restriction =
                context
                    .as_ref()
                    .map(|context| crate::cypher::executor::Restriction {
                        context,
                        database: &name,
                    });
            let who = crate::transactions::Caller {
                surface: audit_origin.surface,
                client: audit_origin.client.clone(),
                user: audit_user.clone(),
            };
            let r = crate::transactions::scope(who, &query, || {
                service.execute_as(&ast, params, restriction)
            });
            if let Err(ExecError::Forbidden { message }) = &r {
                audit_security.record_forbidden(
                    &audit_origin,
                    audit_user.as_deref(),
                    message,
                    Some(&query),
                );
            }
            if let Err(e) = &r {
                crate::problems::note_exec_error("grpc", &name, &query, e);
            }
            r
        })
        .await
        .map_err(|e| Status::internal(format!("query task failed: {e}")))?
        .map_err(exec_status)?;
        Ok(Response::new(tonic::codegen::tokio_stream::iter(
            stream_parts(result).into_iter().map(Ok).collect::<Vec<_>>(),
        )))
    }

    async fn health(
        &self,
        _request: Request<pb::HealthRequest>,
    ) -> Result<Response<pb::HealthResponse>, Status> {
        Ok(Response::new(pb::HealthResponse {
            ok: true,
            version: crate::VERSION.to_string(),
        }))
    }
}

/// The gRPC status for a failed statement.
fn exec_status(e: ExecError) -> Status {
    match e {
        ExecError::Storage(err @ crate::error::DrevoError::PreparedTransactionPending(_)) => {
            Status::unavailable(err.to_string())
        }
        // A missing privilege (#636).
        ExecError::Forbidden { message } => Status::permission_denied(message),
        e => Status::invalid_argument(format!("Cypher execution error: {e}")),
    }
}

/// Split a result into the streamed messages: header, row batches, summary.
fn stream_parts(result: ExecResult) -> Vec<pb::ExecuteResponse> {
    use pb::execute_response::Part;
    let mut out = vec![pb::ExecuteResponse {
        part: Some(Part::Header(pb::Header {
            columns: result.columns,
        })),
    }];
    let mut rows = result.rows.into_iter().peekable();
    while rows.peek().is_some() {
        let batch: Vec<pb::Row> = rows
            .by_ref()
            .take(ROWS_PER_BATCH)
            .map(|row| pb::Row {
                values: row.iter().map(value_to_pb).collect(),
            })
            .collect();
        out.push(pb::ExecuteResponse {
            part: Some(Part::Rows(pb::RowBatch { rows: batch })),
        });
    }
    let s = result.stats;
    let n = |v: usize| u64::try_from(v).unwrap_or(u64::MAX);
    out.push(pb::ExecuteResponse {
        part: Some(Part::Summary(pb::Summary {
            nodes_created: n(s.nodes_created),
            relationships_created: n(s.relationships_created),
            properties_set: n(s.properties_set),
            nodes_deleted: n(s.nodes_deleted),
            relationships_deleted: n(s.relationships_deleted),
            labels_added: n(s.labels_added),
            labels_removed: n(s.labels_removed),
        })),
    });
    out
}

fn kind(k: pb::value::Kind) -> pb::Value {
    pb::Value { kind: Some(k) }
}

fn props_to_pb(props: &BTreeMap<String, Value>) -> HashMap<String, pb::Value> {
    props
        .iter()
        .map(|(k, v)| (k.clone(), value_to_pb(v)))
        .collect()
}

fn node_to_pb(n: &NodeValue) -> pb::Node {
    pb::Node {
        id: n.id,
        uuid: n.uuid.to_vec(),
        labels: n.labels.clone(),
        properties: props_to_pb(&n.properties),
    }
}

fn rel_to_pb(r: &RelationshipValue) -> pb::Relationship {
    pb::Relationship {
        id: r.id,
        uuid: r.uuid.to_vec(),
        start_node_id: r.from_id,
        end_node_id: r.to_id,
        r#type: r.kind.clone(),
        properties: props_to_pb(&r.properties),
    }
}

/// A Cypher value as its protobuf message.
#[must_use]
pub fn value_to_pb(v: &Value) -> pb::Value {
    use pb::value::Kind;
    match v {
        Value::Null => kind(Kind::Null(true)),
        Value::Bool(b) => kind(Kind::Boolean(*b)),
        Value::Integer(i) => kind(Kind::Integer(*i)),
        Value::Float(f) => kind(Kind::Float(*f)),
        Value::String(s) => kind(Kind::String(s.clone())),
        Value::List(items) => {
            let floats: Option<Vec<f64>> = items
                .iter()
                .map(|x| match x {
                    Value::Float(f) => Some(*f),
                    _ => None,
                })
                .collect();
            match floats {
                Some(values) if !values.is_empty() => kind(Kind::Floats(pb::FloatList { values })),
                _ => kind(Kind::List(pb::List {
                    values: items.iter().map(value_to_pb).collect(),
                })),
            }
        }
        Value::Map(m) => kind(Kind::Map(pb::Map {
            entries: props_to_pb(m),
        })),
        Value::Node(n) => kind(Kind::Node(node_to_pb(n))),
        Value::Relationship(r) => kind(Kind::Relationship(rel_to_pb(r))),
        Value::Path(p) => kind(Kind::Path(pb::Path {
            nodes: p.nodes.iter().map(|n| node_to_pb(n)).collect(),
            relationships: p.relationships.iter().map(|r| rel_to_pb(r)).collect(),
        })),
    }
}

/// A query parameter from its protobuf message. Graph entities cannot be
/// parameters; an unset value is `null`.
///
/// # Errors
/// `INVALID_ARGUMENT` for a node, relationship or path.
pub fn param_from_pb(v: pb::Value) -> Result<Value, Status> {
    use pb::value::Kind;
    Ok(match v.kind {
        None | Some(Kind::Null(_)) => Value::Null,
        Some(Kind::Boolean(b)) => Value::Bool(b),
        Some(Kind::Integer(i)) => Value::Integer(i),
        Some(Kind::Float(f)) => Value::Float(f),
        Some(Kind::String(s)) => Value::String(s),
        Some(Kind::Floats(l)) => Value::List(l.values.into_iter().map(Value::Float).collect()),
        Some(Kind::List(l)) => Value::List(
            l.values
                .into_iter()
                .map(param_from_pb)
                .collect::<Result<_, _>>()?,
        ),
        Some(Kind::Map(m)) => Value::Map(
            m.entries
                .into_iter()
                .map(|(k, v)| param_from_pb(v).map(|v| (k, v)))
                .collect::<Result<_, _>>()?,
        ),
        Some(Kind::Node(_) | Kind::Relationship(_) | Kind::Path(_)) => {
            return Err(Status::invalid_argument(
                "a node, relationship or path cannot be a query parameter",
            ))
        }
    })
}

/// Serve the gRPC API on an already-bound `listener` until `shutdown`
/// resolves, with no login.
///
/// # Errors
/// The transport error if the server fails.
pub async fn serve(
    registry: Arc<DatabaseRegistry>,
    listener: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<(), tonic::transport::Error> {
    serve_with_security(registry, Security::default(), listener, shutdown).await
}

/// Serve the gRPC API on an already-bound `listener` until `shutdown`
/// resolves, logging calls in against `security`'s users when it requires a
/// login.
///
/// # Errors
/// The transport error if the server fails.
pub async fn serve_with_security(
    registry: Arc<DatabaseRegistry>,
    security: Security,
    listener: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<(), tonic::transport::Error> {
    let incoming = tonic::transport::server::TcpIncoming::from(listener);
    tonic::transport::Server::builder()
        .add_service(
            DrevoGrpc::new(registry)
                .with_security(security)
                .into_service(),
        )
        .serve_with_incoming_shutdown(incoming, shutdown)
        .await
}
