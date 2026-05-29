//! Integration tests for Bolt-over-TLS — Phase 11 task `00073`.
//!
//! These tests bind a real `tokio::net::TcpListener` on loopback,
//! drive the TLS handshake (via `tokio-rustls` on both sides), and
//! then exercise the Bolt 20-byte handshake + a full HELLO → RUN →
//! PULL → GOODBYE session over the encrypted stream — proving the
//! plain-TCP code path is preserved end-to-end when wrapped in TLS.
//!
//! The cert + key are generated in-memory at the start of each test
//! via `rcgen` (dev-dep) so there is no checked-in PEM bundle to
//! rotate.

#![cfg(all(
    not(target_arch = "wasm32"),
    feature = "bolt-tls",
    feature = "redb-backend"
))]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use drevo::bolt::handshake::MAGIC_PREAMBLE;
use drevo::bolt::packstream::{decode, encode, Value};
use drevo::bolt::session::{GOODBYE, HELLO, PULL, RUN};
use drevo::bolt::tls::{accept_and_run_session_tls, accept_handshake_tls, TlsConfig};
use drevo::db::Drevo;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_rustls::client::TlsStream as ClientTlsStream;
use tokio_rustls::TlsConnector;

/// Set the rustls default crypto provider once per test process.
/// `rustls 0.23` requires an explicit provider when the crate has
/// multiple installed (or zero) — `ring` is what `bolt-tls`
/// enables, but the default-provider slot is not auto-populated.
fn install_default_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Best-effort: another test in the same process may have
        // already installed it. `install_default` returns an error
        // in that case, which we deliberately swallow.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Generate a self-signed cert + PKCS#8 key for `localhost` and
/// return them as PEM strings.
fn self_signed_pem() -> (String, String) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("rcgen self-signed cert");
    (cert.cert.pem(), cert.key_pair.serialize_pem())
}

/// Build a [`TlsConnector`] that trusts a single in-memory cert.
/// Used by the test client so the server's self-signed cert
/// verifies without any "dangerous" verifier bypass.
fn client_connector_trusting(cert_pem: &str) -> TlsConnector {
    let mut reader = std::io::Cursor::new(cert_pem.as_bytes());
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .expect("parse cert chain");
    let mut roots = RootCertStore::empty();
    for cert in certs {
        roots.add(cert).expect("trust self-signed cert");
    }
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

async fn bind_loopback() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").await.expect("bind")
}

/// Smallest possible TLS handshake test — drives only the Bolt
/// 20-byte handshake over TLS. If this passes, the TLS plumbing is
/// alive; if it hangs or errors, every richer test would too.
#[tokio::test]
async fn tls_handshake_negotiates_bolt_4_4_over_tls() {
    install_default_crypto_provider();
    let (cert_pem, key_pem) = self_signed_pem();
    let server_cfg = TlsConfig::from_pem(cert_pem.as_bytes(), key_pem.as_bytes())
        .expect("build server TLS config");
    let acceptor = server_cfg.acceptor();

    let listener = bind_loopback().await;
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("accept");
        accept_handshake_tls(socket, &acceptor).await
    });

    let connector = client_connector_trusting(&cert_pem);
    let raw = TcpStream::connect(addr).await.expect("tcp connect");
    let server_name = ServerName::try_from("localhost").expect("server name");
    let mut tls = connector
        .connect(server_name, raw)
        .await
        .expect("client TLS handshake");

    let mut payload = Vec::with_capacity(20);
    payload.extend_from_slice(&MAGIC_PREAMBLE);
    payload.extend_from_slice(&[0x00, 0x00, 0x04, 0x04]);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    tls.write_all(&payload).await.unwrap();

    let mut reply = [0u8; 4];
    timeout(Duration::from_secs(5), tls.read_exact(&mut reply))
        .await
        .expect("server reply within timeout")
        .expect("read server reply");
    assert_eq!(reply, [0x00, 0x00, 0x04, 0x04]);

    let accepted = timeout(Duration::from_secs(5), server)
        .await
        .expect("server task within timeout")
        .expect("server task join")
        .expect("server handshake ok");
    assert_eq!(
        accepted.negotiated.map(|v| (v.major, v.minor)),
        Some((4, 4)),
        "server must negotiate Bolt 4.4"
    );
}

/// Full HELLO → RUN → PULL → GOODBYE over TLS — proves that once
/// the TLS layer is in place, the existing session loop runs
/// unchanged on top.
#[tokio::test]
async fn tls_accept_and_run_session_handles_hello_run_pull_goodbye_over_tls() {
    install_default_crypto_provider();
    let (cert_pem, key_pem) = self_signed_pem();
    let server_cfg = TlsConfig::from_pem(cert_pem.as_bytes(), key_pem.as_bytes())
        .expect("build server TLS config");
    let acceptor = server_cfg.acceptor();

    let listener = bind_loopback().await;
    let addr = listener.local_addr().unwrap();

    let drevo = Arc::new(Drevo::open_in_memory().expect("open in-memory drevo"));
    let drevo_for_server = Arc::clone(&drevo);

    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("accept");
        accept_and_run_session_tls(socket, &acceptor, &drevo_for_server).await
    });

    let connector = client_connector_trusting(&cert_pem);
    let raw = TcpStream::connect(addr).await.expect("tcp connect");
    let server_name = ServerName::try_from("localhost").expect("server name");
    let mut tls = connector
        .connect(server_name, raw)
        .await
        .expect("client TLS handshake");

    // 1) Bolt 20-byte handshake.
    let mut payload = Vec::with_capacity(20);
    payload.extend_from_slice(&MAGIC_PREAMBLE);
    payload.extend_from_slice(&[0x00, 0x00, 0x04, 0x04]);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    tls.write_all(&payload).await.unwrap();

    let mut version_reply = [0u8; 4];
    timeout(Duration::from_secs(5), tls.read_exact(&mut version_reply))
        .await
        .expect("version reply")
        .expect("read version");
    assert_eq!(version_reply, [0x00, 0x00, 0x04, 0x04]);

    // 2) HELLO + RUN + PULL + GOODBYE — same framing as the
    //    plain-TCP test in `bolt_listener_tests.rs`.
    fn frame_struct(tag: u8, fields: Vec<Value>) -> Vec<u8> {
        let value = Value::Structure { tag, fields };
        let mut buf = Vec::new();
        encode(&value, &mut buf).unwrap();
        let len = buf.len() as u16;
        let mut framed = Vec::with_capacity(buf.len() + 4);
        framed.extend_from_slice(&len.to_be_bytes());
        framed.extend_from_slice(&buf);
        framed.extend_from_slice(&[0x00, 0x00]);
        framed
    }

    let empty: BTreeMap<String, Value> = BTreeMap::new();
    tls.write_all(&frame_struct(HELLO, vec![Value::Dictionary(empty.clone())]))
        .await
        .unwrap();
    tls.write_all(&frame_struct(
        RUN,
        vec![
            Value::String("RETURN 1 AS n".to_string()),
            Value::Dictionary(empty.clone()),
            Value::Dictionary(empty.clone()),
        ],
    ))
    .await
    .unwrap();
    let mut pull_extra: BTreeMap<String, Value> = BTreeMap::new();
    pull_extra.insert("n".to_string(), Value::Integer(-1));
    tls.write_all(&frame_struct(PULL, vec![Value::Dictionary(pull_extra)]))
        .await
        .unwrap();
    tls.write_all(&frame_struct(GOODBYE, vec![])).await.unwrap();

    // 3) Read server replies: SUCCESS (HELLO), SUCCESS (RUN),
    //    RECORD, SUCCESS (terminal PULL).
    async fn read_one(tls: &mut ClientTlsStream<TcpStream>) -> Value {
        let mut payload = Vec::new();
        loop {
            let mut len = [0u8; 2];
            tls.read_exact(&mut len).await.unwrap();
            let l = u16::from_be_bytes(len) as usize;
            if l == 0 {
                break;
            }
            let start = payload.len();
            payload.resize(start + l, 0);
            tls.read_exact(&mut payload[start..]).await.unwrap();
        }
        let (v, rest) = decode(&payload).unwrap();
        assert!(rest.is_empty(), "no trailing bytes");
        v
    }

    let v = timeout(Duration::from_secs(5), read_one(&mut tls))
        .await
        .expect("HELLO ack");
    assert!(
        matches!(v, Value::Structure { tag, .. } if tag == 0x70),
        "HELLO must ack with SUCCESS (0x70)"
    );
    let v = timeout(Duration::from_secs(5), read_one(&mut tls))
        .await
        .expect("RUN ack");
    assert!(
        matches!(v, Value::Structure { tag, .. } if tag == 0x70),
        "RUN must ack with SUCCESS (0x70)"
    );
    let v = timeout(Duration::from_secs(5), read_one(&mut tls))
        .await
        .expect("RECORD");
    assert!(
        matches!(v, Value::Structure { tag, .. } if tag == 0x71),
        "PULL must emit a RECORD (0x71)"
    );
    let v = timeout(Duration::from_secs(5), read_one(&mut tls))
        .await
        .expect("terminal SUCCESS");
    assert!(
        matches!(v, Value::Structure { tag, .. } if tag == 0x70),
        "PULL must terminate with SUCCESS (0x70)"
    );

    drop(tls);
    timeout(Duration::from_secs(5), server)
        .await
        .expect("server task")
        .expect("server join")
        .expect("session ok");
    let _ = drevo;
}

/// A peer that uses plain TCP against a TLS-only listener must
/// fail the TLS handshake — the bytes that look like a Bolt
/// preamble (`60 60 B0 17`) start neither a valid `ClientHello`
/// nor an SSL2 record, so rustls rejects them.
#[tokio::test]
async fn tls_listener_rejects_plain_tcp_client() {
    install_default_crypto_provider();
    let (cert_pem, key_pem) = self_signed_pem();
    let server_cfg = TlsConfig::from_pem(cert_pem.as_bytes(), key_pem.as_bytes())
        .expect("build server TLS config");
    let acceptor = server_cfg.acceptor();

    let listener = bind_loopback().await;
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("accept");
        accept_handshake_tls(socket, &acceptor).await
    });

    // Connect plain-TCP and send the Bolt preamble + four version
    // proposals. The server is in TLS-only mode — rustls will see
    // garbage where it expects a ClientHello and reject.
    let mut client = TcpStream::connect(addr).await.expect("tcp connect");
    let mut payload = Vec::with_capacity(20);
    payload.extend_from_slice(&MAGIC_PREAMBLE);
    payload.extend_from_slice(&[0x00, 0x00, 0x04, 0x04]);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    let _ = client.write_all(&payload).await;

    let r = timeout(Duration::from_secs(5), server)
        .await
        .expect("server task within timeout")
        .expect("server task join");
    // The server task must return an error. We don't pin which
    // exact variant (the underlying tokio_rustls error message
    // varies across versions) — just that it's surfaced.
    assert!(
        r.is_err(),
        "plain-TCP client against TLS listener must fail, got {r:?}"
    );
}
