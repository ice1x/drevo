//! gRPC and Arrow Flight over TLS, with the HTTPS certificate.
//!
//! With `DREVO_TLS_CERT` / `DREVO_TLS_KEY` set, the gRPC API and the Flight
//! endpoint are served over TLS (ALPN `h2`) like HTTPS and `bolt+s://`. A
//! tonic client trusting the certificate logs in and runs Cypher; a
//! plain-text client gets no answer.
//!
//! The scenario is an ERP: a reporting job pulls invoice totals over Flight
//! and the order service queries over gRPC.

#![cfg(all(
    feature = "grpc",
    feature = "arrow-flight",
    feature = "bolt-tls",
    feature = "auth"
))]
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arrow_flight::{FlightClient, Ticket};
use drevo::bolt::tls::TlsConfig;
use drevo::database_registry::DatabaseRegistry;
use drevo::grpc::pb::{self, drevo_client::DrevoClient};
use drevo::native_service::NativeService;
use drevo::security::Security;
use drevo_auth::catalog::MemoryStore;
use drevo_auth::users::{AuthManager, NewUser};
use futures::TryStreamExt;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tonic::transport::{Channel, Endpoint};

const PW: &str = "erp-reporting-pw";

fn crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A self-signed certificate for `localhost`: (cert PEM, key PEM, cert DER).
fn certificate() -> (String, String, CertificateDer<'static>) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    (
        cert.cert.pem(),
        cert.key_pair.serialize_pem(),
        cert.cert.der().clone(),
    )
}

fn security() -> Security {
    let users = Arc::new(AuthManager::open(Arc::new(MemoryStore::new())).unwrap());
    users
        .create_user(NewUser::new("reports", PW).change_required(false))
        .unwrap();
    users.grant_role("reader", "reports").unwrap();
    Security::new(users)
}

fn registry() -> Arc<DatabaseRegistry> {
    let svc = Arc::new(NativeService::in_memory());
    svc.execute(
        &drevo::cypher::parser::parse(
            "CREATE (:Invoice {title: 'INV-1', total: 120}), (:Invoice {title: 'INV-2', total: 80})",
        )
        .unwrap(),
        Default::default(),
    )
    .unwrap();
    Arc::new(DatabaseRegistry::new(svc))
}

fn basic(user: &str, password: &str) -> String {
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
            out.push(if i <= chunk.len() {
                char::from(A[((n >> (18 - 6 * i)) & 63) as usize])
            } else {
                '='
            });
        }
    }
    format!("Basic {out}")
}

/// A TLS connector trusting `cert` and offering ALPN `h2`.
fn connector(cert: &CertificateDer<'static>) -> TlsConnector {
    let mut roots = RootCertStore::empty();
    roots.add(cert.clone()).unwrap();
    let mut config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    TlsConnector::from(Arc::new(config))
}

/// A tonic channel to `localhost:port` over TLS, trusting `cert`.
async fn tls_channel(port: u16, cert: &CertificateDer<'static>) -> Channel {
    let connector = connector(cert);
    Endpoint::from_shared(format!("http://localhost:{port}"))
        .unwrap()
        .connect_with_connector(tower::service_fn(move |_| {
            let connector = connector.clone();
            async move {
                let tcp = TcpStream::connect(("127.0.0.1", port)).await?;
                let tls = connector
                    .connect(ServerName::try_from("localhost").unwrap(), tcp)
                    .await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(tls))
            }
        }))
        .await
        .unwrap()
}

fn execute(query: &str, authorization: Option<&str>) -> tonic::Request<pb::ExecuteRequest> {
    let mut r = tonic::Request::new(pb::ExecuteRequest {
        query: query.into(),
        params: HashMap::new(),
        database: String::new(),
    });
    if let Some(value) = authorization {
        r.metadata_mut()
            .insert("authorization", value.parse().unwrap());
    }
    r
}

/// Start the gRPC API and the Flight endpoint over TLS; their ports.
async fn start(tls: &TlsConfig) -> (u16, u16) {
    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let flight = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ports = (
        grpc.local_addr().unwrap().port(),
        flight.local_addr().unwrap().port(),
    );
    let registry = registry();
    let (grpc_tls, flight_tls) = (tls.clone(), tls.clone());
    let (grpc_registry, grpc_security) = (Arc::clone(&registry), security());
    tokio::spawn(async move {
        drevo::grpc::serve_tls_with_security(
            grpc_registry,
            grpc_security,
            grpc,
            &grpc_tls,
            std::future::pending(),
        )
        .await
        .unwrap();
    });
    tokio::spawn(async move {
        drevo::flight::serve_tls_with_security(
            registry,
            security(),
            flight,
            &flight_tls,
            std::future::pending(),
        )
        .await
        .unwrap();
    });
    ports
}

#[tokio::test]
async fn grpc_over_tls_logs_in_and_runs_cypher() {
    crypto();
    let (cert_pem, key_pem, cert) = certificate();
    let tls = TlsConfig::from_pem(cert_pem.as_bytes(), key_pem.as_bytes()).unwrap();
    let (grpc, _) = start(&tls).await;

    let mut client = DrevoClient::new(tls_channel(grpc, &cert).await);
    let refused = client
        .execute(execute("RETURN 1", None))
        .await
        .expect_err("a call without a login is refused");
    assert_eq!(refused.code(), tonic::Code::Unauthenticated);

    let parts: Vec<_> = client
        .execute(execute(
            "MATCH (i:Invoice) RETURN i.title AS invoice",
            Some(&basic("reports", PW)),
        ))
        .await
        .unwrap()
        .into_inner()
        .try_collect()
        .await
        .unwrap();
    let rows: usize = parts
        .iter()
        .map(|p| match &p.part {
            Some(pb::execute_response::Part::Rows(batch)) => batch.rows.len(),
            _ => 0,
        })
        .sum();
    assert_eq!(rows, 2, "{parts:?}");
}

#[tokio::test]
async fn the_server_offers_h2_in_alpn() {
    crypto();
    let (cert_pem, key_pem, cert) = certificate();
    let tls = TlsConfig::from_pem(cert_pem.as_bytes(), key_pem.as_bytes()).unwrap();
    let (grpc, flight) = start(&tls).await;
    for port in [grpc, flight] {
        let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let stream = connector(&cert)
            .connect(ServerName::try_from("localhost").unwrap(), tcp)
            .await
            .unwrap();
        assert_eq!(stream.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    }
}

#[tokio::test]
async fn flight_over_tls_streams_record_batches() {
    crypto();
    let (cert_pem, key_pem, cert) = certificate();
    let tls = TlsConfig::from_pem(cert_pem.as_bytes(), key_pem.as_bytes()).unwrap();
    let (_, flight) = start(&tls).await;

    let mut client = FlightClient::new(tls_channel(flight, &cert).await);
    client
        .add_header("authorization", &basic("reports", PW))
        .unwrap();
    let batches: Vec<_> = client
        .do_get(Ticket::new(
            "MATCH (i:Invoice) RETURN i.title AS invoice, i.total AS total ORDER BY invoice",
        ))
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 2);
}

#[tokio::test]
async fn a_plain_text_client_gets_no_answer() {
    crypto();
    let (cert_pem, key_pem, _) = certificate();
    let tls = TlsConfig::from_pem(cert_pem.as_bytes(), key_pem.as_bytes()).unwrap();
    let (grpc, flight) = start(&tls).await;

    let call = async {
        let mut client = DrevoClient::connect(format!("http://127.0.0.1:{grpc}")).await?;
        client
            .execute(execute("RETURN 1", Some(&basic("reports", PW))))
            .await
            .map_err(Into::<Box<dyn std::error::Error + Send + Sync>>::into)?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    };
    let outcome = timeout(Duration::from_secs(15), call)
        .await
        .expect("no hang");
    assert!(outcome.is_err(), "a plain-text gRPC call succeeded");

    let call = async {
        let channel = Channel::from_shared(format!("http://127.0.0.1:{flight}"))?
            .connect()
            .await?;
        let mut client = FlightClient::new(channel);
        client.add_header("authorization", &basic("reports", PW))?;
        client.do_get(Ticket::new("RETURN 1 AS one")).await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    };
    let outcome = timeout(Duration::from_secs(15), call)
        .await
        .expect("no hang");
    assert!(outcome.is_err(), "a plain-text Flight call succeeded");
}

/// `drevo-server` serves gRPC and Flight over TLS when `DREVO_TLS_*` is set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_server_serves_grpc_and_flight_over_tls_with_its_certificate() {
    crypto();
    let (cert_pem, key_pem, cert) = certificate();
    let dir = tempfile::TempDir::new().unwrap();
    let (cert_file, key_file) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert_file, &cert_pem).unwrap();
    std::fs::write(&key_file, &key_pem).unwrap();

    let data = tempfile::TempDir::new().unwrap();
    let env: HashMap<&str, String> = HashMap::from([
        ("DREVO_HOST", "127.0.0.1".to_string()),
        ("DREVO_GRPC_PORT", "1".to_string()),
        ("DREVO_FLIGHT_PORT", "1".to_string()),
        ("DREVO_DATA_DIR", data.path().to_string_lossy().to_string()),
        ("DREVO_AUTH", format!("reports/{PW}")),
        ("DREVO_TLS_CERT", cert_file.to_string_lossy().to_string()),
        ("DREVO_TLS_KEY", key_file.to_string_lossy().to_string()),
    ]);
    let mut cfg = drevo::server::Config::from_env(move |k| env.get(k).cloned()).unwrap();
    // Port 0: the system picks free ports, the server reports them.
    cfg.port = 0;
    cfg.grpc_port = Some(0);
    cfg.flight_port = Some(0);
    let (listening, announced) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move { drevo::server::run_announcing(cfg, listening).await });
    let listening = timeout(Duration::from_secs(30), announced)
        .await
        .expect("the server started in time")
        .expect("the server started");
    let grpc = listening.grpc.expect("gRPC is served").port();
    let flight = listening.flight.expect("Flight is served").port();

    let mut client = DrevoClient::new(tls_channel(grpc, &cert).await);
    assert!(client
        .execute(execute("RETURN 1", Some(&basic("reports", PW))))
        .await
        .is_ok());
    let mut flight_client = FlightClient::new(tls_channel(flight, &cert).await);
    flight_client
        .add_header("authorization", &basic("reports", PW))
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
