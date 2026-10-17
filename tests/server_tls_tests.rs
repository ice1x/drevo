//! The server over TLS: with `DREVO_TLS_CERT` and `DREVO_TLS_KEY` set,
//! `drevo::server::run` serves HTTPS on the HTTP port and `bolt+s://` on the
//! Bolt port, and the Web UI's session cookie is marked `Secure`. Drives the
//! real entry point on loopback ports with a self-signed certificate made by
//! `rcgen`.
#![cfg(feature = "bolt-tls")]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use drevo::bolt::handshake::MAGIC_PREAMBLE;
use drevo::bolt::packstream::{decode, encode, Value};
use drevo::bolt::session::HELLO;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

fn install_default_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A self-signed certificate and key for `localhost`, as PEM.
fn self_signed_pem() -> (String, String) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    (cert.cert.pem(), cert.key_pair.serialize_pem())
}

fn connector_trusting(cert_pem: &str) -> TlsConnector {
    let mut reader = std::io::Cursor::new(cert_pem.as_bytes());
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<_, _>>()
        .unwrap();
    let mut roots = RootCertStore::empty();
    for cert in certs {
        roots.add(cert).unwrap();
    }
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

fn free_port() -> u16 {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    probe.local_addr().unwrap().port()
}

/// A running server; aborted on drop.
struct Server {
    http: u16,
    bolt: u16,
    cert_pem: String,
    task: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Start `server::run` with TLS, a Bolt port and `extra` settings.
async fn start(extra: &[(&'static str, &str)]) -> Server {
    install_default_crypto_provider();
    let dir = tempfile::TempDir::new().unwrap();
    let (cert_pem, key_pem) = self_signed_pem();
    let cert = dir.path().join("cert.pem");
    let key = dir.path().join("key.pem");
    std::fs::write(&cert, &cert_pem).unwrap();
    std::fs::write(&key, &key_pem).unwrap();
    let (http, bolt) = (free_port(), free_port());
    let mut env: HashMap<&str, String> = HashMap::from([
        ("DREVO_HOST", "127.0.0.1".to_string()),
        ("DREVO_PORT", http.to_string()),
        ("DREVO_BOLT_PORT", bolt.to_string()),
        (
            "DREVO_DATA_DIR",
            dir.path().join("data").to_string_lossy().into(),
        ),
        ("DREVO_TLS_CERT", cert.to_string_lossy().into()),
        ("DREVO_TLS_KEY", key.to_string_lossy().into()),
    ]);
    for (k, v) in extra {
        env.insert(k, (*v).to_string());
    }
    std::fs::create_dir_all(dir.path().join("data")).unwrap();
    let cfg = drevo::server::Config::from_env(move |k| env.get(k).cloned()).unwrap();
    let task = tokio::spawn(async move { drevo::server::run(cfg).await.unwrap() });
    let mut up = false;
    for _ in 0..250 {
        if TcpStream::connect(("127.0.0.1", http)).await.is_ok() {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(up, "the server did not start");
    Server {
        http,
        bolt,
        cert_pem,
        task,
        _dir: dir,
    }
}

async fn tls_connect(port: u16, cert_pem: &str) -> TlsStream<TcpStream> {
    let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    connector_trusting(cert_pem)
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .expect("TLS handshake")
}

/// One HTTP/1.1 request over TLS; the whole response as text.
async fn https(server: &Server, method: &str, path: &str, body: &str) -> String {
    let mut tls = tls_connect(server.http, &server.cert_pem).await;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    tls.write_all(request.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    timeout(Duration::from_secs(10), tls.read_to_end(&mut out))
        .await
        .expect("response in time")
        .ok();
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test]
async fn http_is_served_over_tls() {
    let server = start(&[("DREVO_AUTH", "none")]).await;
    let reply = https(&server, "GET", "/health", "").await;
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
}

#[tokio::test]
async fn plain_http_on_the_tls_port_gets_no_http_answer() {
    let server = start(&[("DREVO_AUTH", "none")]).await;
    let mut tcp = TcpStream::connect(("127.0.0.1", server.http))
        .await
        .unwrap();
    tcp.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = Vec::new();
    let _ = timeout(Duration::from_secs(10), tcp.read_to_end(&mut out)).await;
    assert!(
        !String::from_utf8_lossy(&out).contains("200 OK"),
        "a plain-text request must not be answered"
    );
    // The server keeps serving TLS clients afterwards.
    let reply = https(&server, "GET", "/health", "").await;
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
}

#[cfg(feature = "auth")]
#[tokio::test]
async fn the_session_cookie_is_secure_over_tls() {
    let server = start(&[("DREVO_AUTH", "ann/ann-password-1")]).await;
    let reply = https(
        &server,
        "POST",
        "/auth/login",
        r#"{"username":"ann","password":"ann-password-1"}"#,
    )
    .await;
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    let cookie = reply
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .expect("a session cookie");
    assert!(cookie.contains("; Secure"), "{cookie}");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
}

// ── bolt+s:// ────────────────────────────────────────────────────────

fn frame(tag: u8, fields: Vec<Value>) -> Vec<u8> {
    let mut buf = Vec::new();
    encode(&Value::Structure { tag, fields }, &mut buf).unwrap();
    let mut framed = (buf.len() as u16).to_be_bytes().to_vec();
    framed.extend_from_slice(&buf);
    framed.extend_from_slice(&[0, 0]);
    framed
}

async fn read_message(tls: &mut TlsStream<TcpStream>) -> Value {
    let mut payload = Vec::new();
    loop {
        let mut len = [0u8; 2];
        tls.read_exact(&mut len).await.unwrap();
        let len = u16::from_be_bytes(len) as usize;
        if len == 0 {
            break;
        }
        let mut chunk = vec![0u8; len];
        tls.read_exact(&mut chunk).await.unwrap();
        payload.extend_from_slice(&chunk);
    }
    decode(&payload).unwrap().0
}

/// The Bolt handshake and a `HELLO` over TLS; the server's reply tag
/// (`0x70` SUCCESS, `0x7F` FAILURE).
async fn bolt_hello(server: &Server, extra: BTreeMap<String, Value>) -> u8 {
    let mut tls = tls_connect(server.bolt, &server.cert_pem).await;
    let mut handshake = MAGIC_PREAMBLE.to_vec();
    handshake.extend_from_slice(&[0, 0, 4, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    tls.write_all(&handshake).await.unwrap();
    let mut version = [0u8; 4];
    tls.read_exact(&mut version).await.unwrap();
    assert_eq!(version, [0, 0, 4, 4]);
    tls.write_all(&frame(HELLO, vec![Value::Dictionary(extra)]))
        .await
        .unwrap();
    match timeout(Duration::from_secs(10), read_message(&mut tls))
        .await
        .expect("HELLO reply in time")
    {
        Value::Structure { tag, .. } => tag,
        other => panic!("unexpected reply {other:?}"),
    }
}

#[cfg(feature = "auth")]
fn basic(user: &str, password: &str) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("scheme".to_string(), Value::String("basic".into())),
        ("principal".to_string(), Value::String(user.into())),
        ("credentials".to_string(), Value::String(password.into())),
    ])
}

#[tokio::test]
async fn bolt_is_served_over_tls() {
    let server = start(&[("DREVO_AUTH", "none")]).await;
    assert_eq!(bolt_hello(&server, BTreeMap::new()).await, 0x70);
}

#[cfg(feature = "auth")]
#[tokio::test]
async fn bolt_over_tls_logs_in_against_the_servers_users() {
    let server = start(&[("DREVO_AUTH", "ann/ann-password-1")]).await;
    assert_eq!(
        bolt_hello(&server, basic("ann", "ann-password-1")).await,
        0x70
    );
    assert_eq!(
        bolt_hello(&server, basic("ann", "wrong-password")).await,
        0x7F
    );
}

// ── Startup errors ───────────────────────────────────────────────────

#[tokio::test]
async fn an_unreadable_certificate_stops_the_server_at_startup() {
    install_default_crypto_provider();
    let dir = tempfile::TempDir::new().unwrap();
    let missing = dir.path().join("missing.pem").to_string_lossy().to_string();
    let port = free_port().to_string();
    let data = dir.path().to_string_lossy().to_string();
    let cfg = drevo::server::Config::from_env(move |k| match k {
        "DREVO_HOST" => Some("127.0.0.1".into()),
        "DREVO_PORT" => Some(port.clone()),
        "DREVO_DATA_DIR" => Some(data.clone()),
        "DREVO_AUTH" => Some("none".into()),
        "DREVO_TLS_CERT" | "DREVO_TLS_KEY" => Some(missing.clone()),
        _ => None,
    })
    .unwrap();
    let err = timeout(Duration::from_secs(10), drevo::server::run(cfg))
        .await
        .expect("run returns instead of serving")
        .expect_err("a missing certificate is a startup error");
    assert!(err.to_string().contains("TLS"), "{err}");
}
