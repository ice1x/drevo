//! Logging in over gRPC and Arrow Flight against the server's users: when the
//! server requires a login, every call carries `authorization` metadata —
//! `Basic` (user:password) or `Bearer <session token>` — exactly as an HTTP
//! request does. Flight's `Handshake` trades a password for a session token.
//! Drives real tonic servers on loopback ports.
#![cfg(all(feature = "auth", any(feature = "grpc", feature = "arrow-flight")))]
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use drevo::database_registry::DatabaseRegistry;
use drevo::native_service::NativeService;
use drevo::security::{Security, LOGIN_REFUSED};
use drevo_auth::catalog::MemoryStore;
use drevo_auth::users::{AuthManager, NewUser};

const ANN_PW: &str = "ann-password";
const BOB_PW: &str = "bob-password";

/// A login-requiring server with `ann` (ready to use) and `bob` (must change
/// his password).
fn security() -> (Security, Arc<AuthManager>) {
    let users = Arc::new(AuthManager::open(Arc::new(MemoryStore::new())).unwrap());
    users
        .create_user(NewUser::new("ann", ANN_PW).change_required(false))
        .unwrap();
    users.create_user(NewUser::new("bob", BOB_PW)).unwrap();
    (Security::new(Arc::clone(&users)), users)
}

fn registry() -> Arc<DatabaseRegistry> {
    Arc::new(DatabaseRegistry::new(Arc::new(NativeService::in_memory())))
}

fn basic(user: &str, password: &str) -> String {
    // A minimal base64 encoder for the test (the server decodes it).
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let raw = format!("{user}:{password}").into_bytes();
    let mut out = String::new();
    for chunk in raw.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(A[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    format!("Basic {out}")
}

// ── gRPC ─────────────────────────────────────────────────────────────

#[cfg(feature = "grpc")]
mod grpc {
    use super::*;
    use std::collections::HashMap;

    use drevo::grpc::pb::drevo_client::DrevoClient;
    use drevo::grpc::pb::{self, execute_response::Part};
    use tonic::{Code, Status};

    type Client = DrevoClient<tonic::transport::Channel>;

    async fn start(security: Security) -> Client {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let registry = registry();
        tokio::spawn(async move {
            drevo::grpc::serve_with_security(registry, security, listener, std::future::pending())
                .await
                .unwrap();
        });
        DrevoClient::connect(format!("http://{addr}"))
            .await
            .unwrap()
    }

    fn request(query: &str, authorization: Option<&str>) -> tonic::Request<pb::ExecuteRequest> {
        let mut r = tonic::Request::new(pb::ExecuteRequest {
            query: query.to_string(),
            params: HashMap::new(),
            database: String::new(),
        });
        if let Some(value) = authorization {
            r.metadata_mut()
                .insert("authorization", value.parse().unwrap());
        }
        r
    }

    /// Run `query`; the number of rows, or the status it failed with.
    async fn rows(
        c: &mut Client,
        query: &str,
        authorization: Option<&str>,
    ) -> Result<usize, Status> {
        let mut stream = c.execute(request(query, authorization)).await?.into_inner();
        let mut n = 0;
        while let Some(msg) = stream.message().await? {
            if let Some(Part::Rows(b)) = msg.part {
                n += b.rows.len();
            }
        }
        Ok(n)
    }

    #[tokio::test]
    async fn a_call_without_a_login_is_unauthenticated() {
        let (security, _) = security();
        let mut c = start(security).await;
        let err = rows(&mut c, "RETURN 1", None).await.unwrap_err();
        assert_eq!(err.code(), Code::Unauthenticated);
        assert_eq!(err.message(), LOGIN_REFUSED);
    }

    #[tokio::test]
    async fn basic_credentials_run_the_query() {
        let (security, _) = security();
        let mut c = start(security).await;
        let n = rows(
            &mut c,
            "UNWIND [1, 2, 3] AS x RETURN x",
            Some(&basic("ann", ANN_PW)),
        )
        .await
        .unwrap();
        assert_eq!(n, 3);
    }

    #[tokio::test]
    async fn a_session_token_runs_the_query() {
        let (security, users) = security();
        let token = users.issue_session("ann").unwrap();
        let mut c = start(security).await;
        let n = rows(&mut c, "RETURN 1", Some(&format!("Bearer {token}")))
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn wrong_unknown_and_malformed_logins_are_refused_alike() {
        let (security, users) = security();
        let mut c = start(security).await;
        let revoked = users.issue_session("ann").unwrap();
        assert!(users.revoke_session(&revoked));
        for authorization in [
            basic("ann", "wrong-password"),
            basic("zed", ANN_PW),
            "Basic !!!not-base64".to_string(),
            "Digest whatever".to_string(),
            format!("Bearer {revoked}"),
        ] {
            let err = rows(&mut c, "RETURN 1", Some(&authorization))
                .await
                .unwrap_err();
            assert_eq!(err.code(), Code::Unauthenticated, "{authorization}");
            assert_eq!(err.message(), LOGIN_REFUSED, "{authorization}");
        }
    }

    #[tokio::test]
    async fn a_refused_login_never_echoes_the_password() {
        let (security, _) = security();
        let mut c = start(security).await;
        let err = rows(&mut c, "RETURN 1", Some(&basic("ann", "hunter2-guess")))
            .await
            .unwrap_err();
        assert!(!format!("{err:?}").contains("hunter2"));
    }

    #[tokio::test]
    async fn repeated_failures_rate_limit_the_user() {
        let (security, _) = security();
        let mut c = start(security).await;
        for _ in 0..3 {
            let _ = rows(&mut c, "RETURN 1", Some(&basic("ann", "wrong-password"))).await;
        }
        let err = rows(&mut c, "RETURN 1", Some(&basic("ann", ANN_PW)))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::ResourceExhausted);
        assert!(err.message().contains("try again"), "{}", err.message());
    }

    #[tokio::test]
    async fn a_user_who_must_change_the_password_is_sent_to_http_or_bolt() {
        let (security, _) = security();
        let mut c = start(security).await;
        let err = rows(&mut c, "RETURN 1", Some(&basic("bob", BOB_PW)))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
        assert!(
            err.message().contains("ALTER CURRENT USER"),
            "{}",
            err.message()
        );
    }

    #[tokio::test]
    async fn health_needs_no_login() {
        let (security, _) = security();
        let mut c = start(security).await;
        let reply = c.health(pb::HealthRequest {}).await.unwrap().into_inner();
        assert!(reply.ok);
    }

    #[tokio::test]
    async fn a_server_that_needs_no_login_takes_calls_without_one() {
        let (security, _) = security();
        let mut c = start(security.without_login()).await;
        assert_eq!(rows(&mut c, "RETURN 1", None).await.unwrap(), 1);
        let mut c = start(Security::default()).await;
        assert_eq!(rows(&mut c, "RETURN 1", None).await.unwrap(), 1);
    }
}

// ── Arrow Flight ─────────────────────────────────────────────────────

#[cfg(feature = "arrow-flight")]
mod flight {
    use super::*;

    use arrow_array::RecordBatch;
    use arrow_flight::error::FlightError;
    use arrow_flight::{FlightClient, Ticket};
    use futures::TryStreamExt;
    use tonic::Code;

    async fn start(security: Security) -> FlightClient {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let registry = registry();
        tokio::spawn(async move {
            drevo::flight::serve_with_security(
                registry,
                security,
                listener,
                std::future::pending(),
            )
            .await
            .unwrap();
        });
        let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        FlightClient::new(channel)
    }

    fn status(e: FlightError) -> tonic::Status {
        match e {
            FlightError::Tonic(status) => *status,
            other => panic!("expected a gRPC status, got {other:?}"),
        }
    }

    async fn get(c: &mut FlightClient, ticket: &str) -> Result<Vec<RecordBatch>, tonic::Status> {
        let stream = c
            .do_get(Ticket::new(ticket.to_string()))
            .await
            .map_err(status)?;
        stream.try_collect().await.map_err(status)
    }

    fn row_count(batches: &[RecordBatch]) -> usize {
        batches.iter().map(RecordBatch::num_rows).sum()
    }

    #[tokio::test]
    async fn do_get_without_a_login_is_unauthenticated() {
        let (security, _) = security();
        let mut c = start(security).await;
        let err = get(&mut c, "RETURN 1 AS one").await.unwrap_err();
        assert_eq!(err.code(), Code::Unauthenticated);
        assert_eq!(err.message(), LOGIN_REFUSED);
    }

    #[tokio::test]
    async fn do_get_with_basic_credentials_returns_the_rows() {
        let (security, _) = security();
        let mut c = start(security).await;
        c.add_header("authorization", &basic("ann", ANN_PW))
            .unwrap();
        let batches = get(&mut c, "UNWIND range(1, 5) AS x RETURN x")
            .await
            .unwrap();
        assert_eq!(row_count(&batches), 5);
    }

    #[tokio::test]
    async fn the_handshake_trades_a_password_for_a_session_token() {
        let (security, users) = security();
        let mut c = start(security).await;
        c.add_header("authorization", &basic("ann", ANN_PW))
            .unwrap();
        let token = c.handshake(Vec::<u8>::new()).await.unwrap();
        let token = String::from_utf8(token.to_vec()).unwrap();
        assert_eq!(token.len(), 64, "a 256-bit token as hex");
        let who = users
            .authenticate(&drevo_auth::authn::Credentials::Token(token.clone()))
            .unwrap();
        assert_eq!(who.name, "ann");

        // A client on the same server that only has the token.
        let mut c2 = FlightClient::new_from_inner(c.inner().clone());
        c2.add_header("authorization", &format!("Bearer {token}"))
            .unwrap();
        let batches = get(&mut c2, "RETURN 1 AS one").await.unwrap();
        assert_eq!(row_count(&batches), 1);
    }

    #[tokio::test]
    async fn a_handshake_with_wrong_credentials_is_refused() {
        let (security, _) = security();
        let mut c = start(security).await;
        c.add_header("authorization", &basic("ann", "wrong-password"))
            .unwrap();
        let err = status(c.handshake(Vec::<u8>::new()).await.unwrap_err());
        assert_eq!(err.code(), Code::Unauthenticated);
        assert_eq!(err.message(), LOGIN_REFUSED);
    }

    #[tokio::test]
    async fn a_user_who_must_change_the_password_cannot_query() {
        let (security, _) = security();
        let mut c = start(security).await;
        c.add_header("authorization", &basic("bob", BOB_PW))
            .unwrap();
        let err = get(&mut c, "RETURN 1 AS one").await.unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
        assert!(err.message().contains("ALTER CURRENT USER"));
    }

    #[tokio::test]
    async fn a_server_that_needs_no_login_takes_do_get_without_one() {
        let (security, _) = security();
        let mut c = start(security.without_login()).await;
        let batches = get(&mut c, "RETURN 1 AS one").await.unwrap();
        assert_eq!(row_count(&batches), 1);
    }
}

// ── The server wires its users into gRPC and Flight ──────────────────

/// `drevo::server::run` with `DREVO_AUTH`, gRPC and Flight on free ports:
/// both refuse a call without a login and run one with the administrator's
/// password. Guards the wiring in `server::run`, not just the services.
#[cfg(all(feature = "grpc", feature = "arrow-flight"))]
#[tokio::test]
async fn the_server_requires_a_login_on_grpc_and_flight() {
    use std::collections::HashMap;
    use std::time::Duration;

    use arrow_flight::{FlightClient, Ticket};
    use drevo::grpc::pb::{self, drevo_client::DrevoClient};
    use futures::TryStreamExt;

    fn free_port() -> u16 {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    }
    let dir = tempfile::TempDir::new().unwrap();
    let (http, grpc, flight) = (free_port(), free_port(), free_port());
    let env: HashMap<&str, String> = HashMap::from([
        ("DREVO_HOST", "127.0.0.1".to_string()),
        ("DREVO_PORT", http.to_string()),
        ("DREVO_GRPC_PORT", grpc.to_string()),
        ("DREVO_FLIGHT_PORT", flight.to_string()),
        ("DREVO_DATA_DIR", dir.path().to_string_lossy().to_string()),
        ("DREVO_AUTH", format!("ann/{ANN_PW}")),
    ]);
    let cfg = drevo::server::Config::from_env(move |k| env.get(k).cloned()).unwrap();
    let server = tokio::spawn(async move { drevo::server::run(cfg).await.unwrap() });
    // The HTTP listener binds last, so once it answers gRPC and Flight are up.
    let mut up = false;
    for _ in 0..250 {
        if tokio::net::TcpStream::connect(("127.0.0.1", http))
            .await
            .is_ok()
        {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(up, "the server did not start");

    let mut grpc_client = DrevoClient::connect(format!("http://127.0.0.1:{grpc}"))
        .await
        .unwrap();
    let request = |authorization: Option<String>| {
        let mut r = tonic::Request::new(pb::ExecuteRequest {
            query: "RETURN 1".into(),
            params: HashMap::new(),
            database: String::new(),
        });
        if let Some(value) = authorization {
            r.metadata_mut()
                .insert("authorization", value.parse().unwrap());
        }
        r
    };
    let refused = grpc_client.execute(request(None)).await.unwrap_err();
    assert_eq!(refused.code(), tonic::Code::Unauthenticated);
    assert!(grpc_client
        .execute(request(Some(basic("ann", ANN_PW))))
        .await
        .is_ok());

    let channel = tonic::transport::Channel::from_shared(format!("http://127.0.0.1:{flight}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut flight_client = FlightClient::new(channel);
    let refused = flight_client
        .do_get(Ticket::new("RETURN 1 AS one"))
        .await
        .expect_err("a call without a login is refused");
    assert!(refused.to_string().contains(LOGIN_REFUSED), "{refused}");
    flight_client
        .add_header("authorization", &basic("ann", ANN_PW))
        .unwrap();
    let batches: Vec<_> = flight_client
        .do_get(Ticket::new("RETURN 1 AS one"))
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 1);

    server.abort();
    let _ = server.await;
}
