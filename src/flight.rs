//! Arrow Flight endpoint (issue #584): Cypher results as Arrow record batches,
//! for pandas / polars / DuckDB and anything else that speaks Flight.
//!
//! Compiled with the `arrow-flight` feature and served only when
//! `DREVO_FLIGHT_PORT` is set. A client calls `DoGet` with a ticket that holds
//! the statement: either the Cypher text itself, or a JSON object
//! `{"query": …, "params": {…}, "database": …}`. The answer is the result as
//! Arrow record batches of up to 8192 rows each.
//!
//! Column types come from the values:
//!
//! | Values in the column | Arrow type |
//! |---|---|
//! | integers | `Int64` |
//! | floats, or integers and floats | `Float64` |
//! | booleans | `Boolean` |
//! | strings | `Utf8` |
//! | lists of numbers (embeddings) | `List<Float64>` |
//! | anything else — nodes, maps, mixed types | `Utf8` holding JSON |
//!
//! `null` fits any column. Only `DoGet` (and an empty `Handshake`) are
//! served; every other Flight call answers `UNIMPLEMENTED`. Errors use the
//! same statuses as the gRPC API.
//!
//! When the server requires a login, `DoGet` needs an `authorization`
//! header — `Basic <base64 of user:password>` or `Bearer <session token>` —
//! as the gRPC API does. `Handshake` with such a header answers with a
//! session token, both as the response payload and as an
//! `authorization: Bearer <token>` header, so clients that log in once with
//! a password (pyarrow's `authenticate_basic_token`) send only the token
//! afterwards.

#![cfg(feature = "arrow-flight")]
// tonic's service trait returns `Result<_, tonic::Status>` by value.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use arrow_array::builder::{Float64Builder, ListBuilder};
use arrow_array::{ArrayRef, BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray};
use arrow_flight::encode::FlightDataEncoderBuilder;
use arrow_flight::flight_service_server::{FlightService, FlightServiceServer};
use arrow_flight::{
    Action, ActionType, Criteria, Empty, FlightData, FlightDescriptor, FlightInfo,
    HandshakeRequest, HandshakeResponse, PollInfo, PutResult, SchemaResult, Ticket,
};
use arrow_schema::{DataType, Field, Schema};
use futures::stream::{self, BoxStream, StreamExt};
use tonic::{Request, Response, Status, Streaming};

use crate::cypher::executor::{ExecError, ExecResult, Value};
use crate::database_registry::DatabaseRegistry;
use crate::security::Security;

/// Rows per Arrow record batch.
const ROWS_PER_BATCH: usize = 8192;

/// The Flight service over a database registry.
#[derive(Clone)]
pub struct DrevoFlight {
    registry: Arc<DatabaseRegistry>,
    security: Security,
}

impl DrevoFlight {
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
    pub fn into_service(self) -> FlightServiceServer<Self> {
        FlightServiceServer::new(self)
    }
}

/// What a `DoGet` ticket asks for.
#[derive(serde::Deserialize)]
struct TicketQuery {
    query: String,
    #[serde(default)]
    params: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    database: Option<String>,
}

fn parse_ticket(bytes: &[u8]) -> Result<TicketQuery, Status> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| Status::invalid_argument("the ticket must be UTF-8"))?
        .trim();
    if text.starts_with('{') {
        serde_json::from_str(text)
            .map_err(|e| Status::invalid_argument(format!("invalid JSON ticket: {e}")))
    } else {
        Ok(TicketQuery {
            query: text.to_string(),
            params: serde_json::Map::new(),
            database: None,
        })
    }
}

fn exec_status(e: ExecError) -> Status {
    match e {
        ExecError::Storage(err @ crate::error::DrevoError::PreparedTransactionPending(_)) => {
            Status::unavailable(err.to_string())
        }
        // An explicit transaction over its write limit (#751).
        ExecError::Storage(err @ crate::error::DrevoError::TransactionTooLarge { .. }) => {
            Status::resource_exhausted(err.to_string())
        }
        // A missing privilege (#636).
        ExecError::Forbidden { message } => Status::permission_denied(message),
        e => Status::invalid_argument(format!("Cypher execution error: {e}")),
    }
}

/// The Arrow type a column of `values` gets (see the [module docs](self)).
fn column_type<'a>(values: impl Iterator<Item = &'a Value>) -> DataType {
    #[derive(PartialEq)]
    enum Seen {
        Nothing,
        Int,
        Float,
        Bool,
        Str,
        NumList,
        Other,
    }
    let mut seen = Seen::Nothing;
    for v in values {
        let this = match v {
            Value::Null => continue,
            Value::Integer(_) => Seen::Int,
            Value::Float(_) => Seen::Float,
            Value::Bool(_) => Seen::Bool,
            Value::String(_) => Seen::Str,
            Value::List(items)
                if items
                    .iter()
                    .all(|x| matches!(x, Value::Integer(_) | Value::Float(_) | Value::Null)) =>
            {
                Seen::NumList
            }
            _ => Seen::Other,
        };
        seen = match (seen, this) {
            (Seen::Nothing, t) => t,
            (a, b) if a == b => a,
            (Seen::Int, Seen::Float) | (Seen::Float, Seen::Int) => Seen::Float,
            _ => Seen::Other,
        };
        if seen == Seen::Other {
            break;
        }
    }
    match seen {
        Seen::Int => DataType::Int64,
        Seen::Float => DataType::Float64,
        Seen::Bool => DataType::Boolean,
        Seen::NumList => DataType::List(Arc::new(Field::new("item", DataType::Float64, true))),
        // An all-null column is typed Utf8 (all nulls).
        Seen::Str | Seen::Nothing | Seen::Other => DataType::Utf8,
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Integer(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// Build one Arrow column of `ty` from `values`.
fn column(ty: &DataType, values: &[&Value]) -> ArrayRef {
    match ty {
        DataType::Int64 => Arc::new(Int64Array::from_iter(values.iter().map(|v| match v {
            Value::Integer(i) => Some(*i),
            _ => None,
        }))),
        DataType::Float64 => Arc::new(Float64Array::from_iter(values.iter().map(|v| as_f64(v)))),
        DataType::Boolean => Arc::new(BooleanArray::from_iter(values.iter().map(|v| match v {
            Value::Bool(b) => Some(*b),
            _ => None,
        }))),
        DataType::List(_) => {
            let mut b = ListBuilder::new(Float64Builder::new());
            for v in values {
                match v {
                    Value::List(items) => {
                        for x in items {
                            b.values().append_option(as_f64(x));
                        }
                        b.append(true);
                    }
                    _ => b.append(false),
                }
            }
            Arc::new(b.finish())
        }
        _ => Arc::new(StringArray::from_iter(values.iter().map(|v| match v {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            other => Some(crate::api::value_to_json(other).to_string()),
        }))),
    }
}

/// Turn a result into an Arrow schema and record batches.
///
/// # Errors
/// An Arrow error if a batch cannot be assembled (a bug, not bad input).
fn to_batches(result: &ExecResult) -> Result<(Arc<Schema>, Vec<RecordBatch>), Status> {
    let types: Vec<DataType> = (0..result.columns.len())
        .map(|c| column_type(result.rows.iter().map(|r| &r[c])))
        .collect();
    let schema = Arc::new(Schema::new(
        result
            .columns
            .iter()
            .zip(&types)
            .map(|(name, ty)| Field::new(name, ty.clone(), true))
            .collect::<Vec<_>>(),
    ));
    let mut batches = Vec::new();
    for chunk in result.rows.chunks(ROWS_PER_BATCH) {
        let columns: Vec<ArrayRef> = types
            .iter()
            .enumerate()
            .map(|(c, ty)| column(ty, &chunk.iter().map(|r| &r[c]).collect::<Vec<_>>()))
            .collect();
        let batch = RecordBatch::try_new(Arc::clone(&schema), columns)
            .map_err(|e| Status::internal(format!("building a record batch: {e}")))?;
        batches.push(batch);
    }
    Ok((schema, batches))
}

type FlightStream<T> = Pin<Box<dyn futures::Stream<Item = Result<T, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl FlightService for DrevoFlight {
    type HandshakeStream = FlightStream<HandshakeResponse>;
    type ListFlightsStream = FlightStream<FlightInfo>;
    type DoGetStream = FlightStream<FlightData>;
    type DoPutStream = FlightStream<PutResult>;
    type DoActionStream = FlightStream<arrow_flight::Result>;
    type ListActionsStream = FlightStream<ActionType>;
    type DoExchangeStream = FlightStream<FlightData>;

    async fn handshake(
        &self,
        request: Request<Streaming<HandshakeRequest>>,
    ) -> Result<Response<Self::HandshakeStream>, Status> {
        // Without a login requirement: an empty handshake, as on HTTP.
        let Some(user) = crate::rpc_login::require_login(
            &self.security,
            request.metadata(),
            &crate::security::Origin::new("flight").from_client(request.remote_addr()),
        )
        .await?
        else {
            let out: BoxStream<'static, Result<HandshakeResponse, Status>> =
                stream::empty().boxed();
            return Ok(Response::new(out));
        };
        let token = self
            .security
            .issue_session(&user.name)
            .ok_or_else(|| Status::unauthenticated(crate::security::LOGIN_REFUSED))?;
        let bearer = format!("Bearer {token}")
            .parse()
            .map_err(|_| Status::internal("a session token is not a valid header value"))?;
        let reply = HandshakeResponse {
            protocol_version: 0,
            payload: token.into_bytes().into(),
        };
        let out: BoxStream<'static, Result<HandshakeResponse, Status>> =
            stream::once(async move { Ok(reply) }).boxed();
        let mut response = Response::new(out);
        response.metadata_mut().insert("authorization", bearer);
        Ok(response)
    }

    async fn do_get(
        &self,
        request: Request<Ticket>,
    ) -> Result<Response<Self::DoGetStream>, Status> {
        let remote = request.remote_addr();
        let signed_in = crate::rpc_login::require_login(
            &self.security,
            request.metadata(),
            &crate::security::Origin::new("flight").from_client(request.remote_addr()),
        )
        .await?;
        let TicketQuery {
            query,
            params,
            database,
        } = parse_ticket(&request.into_inner().ticket)?;
        if crate::cypher::admin::parse(&query).is_some() {
            return Err(Status::unimplemented(
                "database administration is not available over Flight; use HTTP or Bolt",
            ));
        }
        if crate::cypher::security_admin::parse(&query).is_some() {
            return Err(Status::unimplemented(
                "user management is not available over Flight; use HTTP or Bolt",
            ));
        }
        let name = database
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| self.registry.default_name().to_string());
        let service = self
            .registry
            .get(&name)
            .ok_or_else(|| Status::not_found(format!("no database named `{name}`")))?;
        let params: HashMap<String, Value> = params
            .into_iter()
            .map(|(k, v)| (k, crate::api::json_to_cypher_value(v)))
            .collect();
        let ast = crate::cypher::parser::parse(&query)
            .map_err(|e| Status::invalid_argument(format!("Cypher parse error: {e}")))?;
        // The signed-in user's privileges (#636); `None` checks nothing.
        let context = self
            .security
            .context_for(signed_in.as_ref(), self.registry.default_name());
        let audit_security = self.security.clone();
        let audit_origin = crate::security::Origin::new("flight").from_client(remote);
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
                connection: None,
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
                crate::problems::note_exec_error("flight", &name, &query, e);
            }
            r
        })
        .await
        .map_err(|e| Status::internal(format!("query task failed: {e}")))?
        .map_err(exec_status)?;
        let (schema, batches) = to_batches(&result)?;
        let encoded = FlightDataEncoderBuilder::new()
            .with_schema(schema)
            .build(stream::iter(batches.into_iter().map(Ok)))
            .map(|r| r.map_err(|e| Status::internal(e.to_string())));
        Ok(Response::new(encoded.boxed()))
    }

    async fn list_flights(
        &self,
        _request: Request<Criteria>,
    ) -> Result<Response<Self::ListFlightsStream>, Status> {
        Err(unimplemented())
    }

    async fn get_flight_info(
        &self,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        Err(unimplemented())
    }

    async fn poll_flight_info(
        &self,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<PollInfo>, Status> {
        Err(unimplemented())
    }

    async fn get_schema(
        &self,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<SchemaResult>, Status> {
        Err(unimplemented())
    }

    async fn do_put(
        &self,
        _request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoPutStream>, Status> {
        Err(unimplemented())
    }

    async fn do_action(
        &self,
        _request: Request<Action>,
    ) -> Result<Response<Self::DoActionStream>, Status> {
        Err(unimplemented())
    }

    async fn list_actions(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<Self::ListActionsStream>, Status> {
        Err(unimplemented())
    }

    async fn do_exchange(
        &self,
        _request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoExchangeStream>, Status> {
        Err(unimplemented())
    }
}

fn unimplemented() -> Status {
    Status::unimplemented("drevo serves DoGet only: send the Cypher statement as the ticket")
}

/// Serve the Flight endpoint on an already-bound `listener` until `shutdown`
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

/// Serve the Flight endpoint on an already-bound `listener` until `shutdown`
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
            DrevoFlight::new(registry)
                .with_security(security)
                .into_service(),
        )
        .serve_with_incoming_shutdown(incoming, shutdown)
        .await
}

/// [`serve_with_security`] over TLS with `tls`'s certificate (ALPN `h2`); a
/// plain-text client gets no answer. See [`crate::rpc_tls`].
///
/// # Errors
/// The transport error if the server fails.
#[cfg(feature = "bolt-tls")]
pub async fn serve_tls_with_security(
    registry: Arc<DatabaseRegistry>,
    security: Security,
    listener: tokio::net::TcpListener,
    tls: &crate::bolt::tls::TlsConfig,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<(), tonic::transport::Error> {
    tonic::transport::Server::builder()
        .add_service(
            DrevoFlight::new(registry)
                .with_security(security)
                .into_service(),
        )
        .serve_with_incoming_shutdown(crate::rpc_tls::incoming(listener, tls), shutdown)
        .await
}
