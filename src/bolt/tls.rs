//! TLS for Bolt — Phase 11 task `00073`.
//!
//! Wraps the existing async TCP listener ([`crate::bolt::listener`])
//! in a [`tokio_rustls::TlsAcceptor`] so the same handshake +
//! session loop work over an encrypted transport. Gated behind the
//! `bolt-tls` Cargo feature (which itself implies `http`) — default
//! builds keep their dep graph free of `rustls`, `tokio-rustls`, and
//! `rustls-pemfile`.
//!
//! ## Why rustls, not openssl-sys
//!
//! drevo's deployment story is "ship a static binary or an embedded
//! library that runs on any of {Linux glibc, macOS, Windows, iOS,
//! Android}". OpenSSL bindings drag in a system library that breaks
//! that property — and on the embedded targets (iOS / Android via C
//! FFI) it isn't even available. `rustls` is pure Rust, uses the
//! same `ring` crypto provider the rest of the Rust ecosystem
//! already pins, and integrates cleanly with `tokio` via
//! `tokio-rustls`.
//!
//! ## Why a String-payload `BoltError::Tls`
//!
//! The Bolt module deliberately keeps its error enum free of
//! optional-feature types so the rest of the codebase (FFI, WASM,
//! HTTP) can match against it without `cfg` guards. The TLS error
//! path converts `rustls::Error` / `std::io::Error` from the
//! handshake into a `String` payload at the boundary — we never
//! re-emit it as a structured `rustls::Error` further up.
//!
//! ## Public surface
//!
//! * [`crate::bolt::tls::TlsConfig`] — owns an
//!   `Arc<rustls::ServerConfig>`. Built via
//!   [`crate::bolt::tls::TlsConfig::from_pem`] (in-memory bytes —
//!   used by the test suite, FFI callers that load certs themselves,
//!   and Tauri-style embedders) or
//!   [`crate::bolt::tls::TlsConfig::from_pem_files`] (paths on disk
//!   — the deployment-time entry point).
//! * [`crate::bolt::tls::accept_handshake_tls`] — performs the TLS
//!   handshake on a freshly accepted [`tokio::net::TcpStream`], then
//!   runs the Bolt 20-byte handshake on the encrypted stream.
//! * [`crate::bolt::tls::accept_and_run_session_tls`] — bundles TLS
//!   handshake + Bolt handshake + session loop into one call,
//!   suitable as a `tokio::spawn`-per-connection body.
//!
//! Both `_tls` entry points are thin wrappers over the generic
//! [`crate::bolt::listener::accept_handshake_on`] /
//! [`crate::bolt::listener::run_session_on`] helpers — the only
//! TLS-specific code lives here. Plain-TCP and TLS code paths share
//! the same session-loop bytes.

use std::io;
use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

use super::error::{BoltError, BoltResult};
use super::listener::{accept_handshake_on, run_session_on, AcceptedConnectionOn};
use crate::db::Drevo;

/// Server-side TLS configuration for the Bolt listener.
///
/// Backed by an [`Arc<rustls::ServerConfig>`] so cloning is cheap
/// and the same config can be handed to a `tokio::spawn`-per-
/// connection accept loop without re-loading certs.
///
/// `Debug` is implemented by hand because [`rustls::ServerConfig`]
/// does not derive `Debug` itself (its internal types intentionally
/// avoid the trait so cert / key material doesn't accidentally leak
/// through log statements).
#[derive(Clone)]
pub struct TlsConfig {
    config: Arc<ServerConfig>,
}

impl std::fmt::Debug for TlsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsConfig")
            .field("config", &"<rustls::ServerConfig>")
            .finish()
    }
}

impl TlsConfig {
    /// Build a config from PEM-encoded certificate chain and private
    /// key bytes (in-memory).
    ///
    /// The certificate chain is expected to be a concatenation of
    /// one or more `BEGIN CERTIFICATE` blocks (leaf first, intermediate
    /// certificates after). The key may be PKCS#8, PKCS#1 (RSA), or
    /// SEC1 (EC) — whichever form is found first wins.
    ///
    /// # Errors
    ///
    /// * [`BoltError::Tls`] — empty / malformed PEM, missing key,
    ///   or rustls config build failure (e.g. cert/key mismatch).
    pub fn from_pem(cert_pem: &[u8], key_pem: &[u8]) -> BoltResult<Self> {
        let certs = parse_cert_chain(cert_pem)?;
        if certs.is_empty() {
            return Err(BoltError::Tls(
                "no CERTIFICATE blocks found in cert PEM".to_string(),
            ));
        }
        let key = parse_private_key(key_pem)?;

        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| BoltError::Tls(format!("rustls config build failed: {e}")))?;

        Ok(TlsConfig {
            config: Arc::new(config),
        })
    }

    /// Build a config from PEM-encoded certificate chain and private
    /// key files on disk.
    ///
    /// Convenience wrapper around [`TlsConfig::from_pem`] that
    /// reads both files via [`std::fs::read`].
    ///
    /// # Errors
    ///
    /// * [`BoltError::Io`] — either file cannot be opened or read.
    /// * [`BoltError::Tls`] — see [`TlsConfig::from_pem`].
    pub fn from_pem_files<P: AsRef<Path>, Q: AsRef<Path>>(
        cert_path: P,
        key_path: Q,
    ) -> BoltResult<Self> {
        let cert_pem = std::fs::read(cert_path.as_ref())?;
        let key_pem = std::fs::read(key_path.as_ref())?;
        Self::from_pem(&cert_pem, &key_pem)
    }

    /// Borrow the underlying `Arc<ServerConfig>` — exposed so
    /// embedders that already drive their own `TlsAcceptor` can
    /// reuse the same parsed certs.
    pub fn server_config(&self) -> Arc<ServerConfig> {
        Arc::clone(&self.config)
    }

    /// Build a [`TlsAcceptor`] from this config. Cheap (clones an
    /// `Arc`); call per accept loop, not per connection.
    pub fn acceptor(&self) -> TlsAcceptor {
        TlsAcceptor::from(Arc::clone(&self.config))
    }
}

fn parse_cert_chain(pem: &[u8]) -> BoltResult<Vec<CertificateDer<'static>>> {
    let mut reader = io::Cursor::new(pem);
    let mut chain = Vec::new();
    for item in rustls_pemfile::certs(&mut reader) {
        let cert = item.map_err(|e| BoltError::Tls(format!("failed to parse certificate: {e}")))?;
        chain.push(cert);
    }
    Ok(chain)
}

fn parse_private_key(pem: &[u8]) -> BoltResult<PrivateKeyDer<'static>> {
    // Try in turn: PKCS#8, PKCS#1 (RSA), SEC1 (EC). The first form
    // that yields a key wins. This matches the behaviour of the
    // official `rustls-pemfile::private_key` helper but is spelled
    // out here so the error message names what was expected.
    let mut reader = io::Cursor::new(pem);
    if let Some(item) = rustls_pemfile::pkcs8_private_keys(&mut reader).next() {
        let key = item.map_err(|e| BoltError::Tls(format!("failed to parse PKCS#8 key: {e}")))?;
        return Ok(PrivateKeyDer::Pkcs8(key));
    }

    let mut reader = io::Cursor::new(pem);
    if let Some(item) = rustls_pemfile::rsa_private_keys(&mut reader).next() {
        let key = item.map_err(|e| BoltError::Tls(format!("failed to parse PKCS#1 key: {e}")))?;
        return Ok(PrivateKeyDer::Pkcs1(key));
    }

    let mut reader = io::Cursor::new(pem);
    if let Some(item) = rustls_pemfile::ec_private_keys(&mut reader).next() {
        let key = item.map_err(|e| BoltError::Tls(format!("failed to parse SEC1 key: {e}")))?;
        return Ok(PrivateKeyDer::Sec1(key));
    }

    Err(BoltError::Tls(
        "no PRIVATE KEY block (PKCS#8 / PKCS#1 / SEC1) found in key PEM".to_string(),
    ))
}

/// Perform the TLS handshake on `socket` using the supplied
/// [`TlsAcceptor`], then run the Bolt 20-byte handshake on top of
/// the encrypted stream.
///
/// Returns the accepted TLS stream past both handshakes, plus the
/// negotiated Bolt version. When the Bolt version is `None`, the
/// reply `00 00 00 00` has already been written and the caller
/// should drop the stream.
///
/// # Errors
///
/// * [`BoltError::Tls`] — TLS handshake failure (bad client cert
///   if mTLS is configured, peer used plain TCP against a TLS-only
///   port, unsupported protocol version, etc.).
/// * [`BoltError::InvalidMagic`] / [`BoltError::Io`] — propagated
///   from [`crate::bolt::listener::accept_handshake_on`].
pub async fn accept_handshake_tls(
    socket: TcpStream,
    acceptor: &TlsAcceptor,
) -> BoltResult<AcceptedConnectionOn<TlsStream<TcpStream>>> {
    let tls_stream = acceptor
        .accept(socket)
        .await
        .map_err(|e| BoltError::Tls(format!("TLS handshake failed: {e}")))?;
    accept_handshake_on(tls_stream).await
}

/// Bundle TLS handshake + Bolt handshake + session loop into a
/// single call. Drop-in TLS equivalent of
/// [`crate::bolt::listener::accept_and_run_session`].
///
/// Suitable as the body of a `tokio::spawn`-per-connection accept
/// loop on a `bolt+s://` / `bolt+ssc://` listener.
///
/// # Errors
///
/// Propagates errors from [`accept_handshake_tls`]; Cypher errors
/// are surfaced as `FAILURE` messages and do *not* abort the loop.
pub async fn accept_and_run_session_tls(
    socket: TcpStream,
    acceptor: &TlsAcceptor,
    drevo: &Drevo,
) -> BoltResult<()> {
    let accepted = accept_handshake_tls(socket, acceptor).await?;
    if accepted.negotiated.is_none() {
        return Ok(());
    }
    let mut stream = accepted.stream;
    run_session_on(&mut stream, drevo).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Empty cert PEM is the simplest "did the helper actually parse
    /// the bytes" check — guards against returning a default config
    /// with zero certs (which would compile but fail every handshake).
    #[test]
    fn from_pem_rejects_empty_cert_chain() {
        let result = TlsConfig::from_pem(
            b"",
            b"-----BEGIN PRIVATE KEY-----\n-----END PRIVATE KEY-----\n",
        );
        let err = result.expect_err("expected error for empty cert chain");
        match err {
            BoltError::Tls(msg) => assert!(
                msg.contains("CERTIFICATE"),
                "error message should name what was missing: got {msg}"
            ),
            other => panic!("expected BoltError::Tls, got {other:?}"),
        }
    }

    /// Malformed cert PEM (a single `BEGIN CERTIFICATE` header with
    /// no matching `END`) must be rejected with a `Tls` error, not
    /// silently treated as empty.
    #[test]
    fn from_pem_rejects_malformed_cert_pem() {
        let bad_cert = b"-----BEGIN CERTIFICATE-----\nnot-base64-at-all\n";
        let result = TlsConfig::from_pem(
            bad_cert,
            b"-----BEGIN PRIVATE KEY-----\n-----END PRIVATE KEY-----\n",
        );
        let err = result.expect_err("expected error for malformed cert PEM");
        assert!(
            matches!(err, BoltError::Tls(_)),
            "expected BoltError::Tls, got {err:?}"
        );
    }

    /// Even with a valid cert chain, missing the private key must
    /// fail at the parse stage (before rustls ever sees it).
    #[test]
    fn from_pem_rejects_missing_private_key() {
        let (cert_pem, _key_pem) = generate_self_signed_pem();
        let result = TlsConfig::from_pem(cert_pem.as_bytes(), b"");
        let err = result.expect_err("expected error for missing key");
        match err {
            BoltError::Tls(msg) => assert!(
                msg.contains("PRIVATE KEY"),
                "error message should name what was missing: got {msg}"
            ),
            other => panic!("expected BoltError::Tls, got {other:?}"),
        }
    }

    /// Happy path: a freshly generated self-signed PKCS#8 key + cert
    /// pair builds a config and yields a usable `TlsAcceptor`. This
    /// is the same setup the integration tests use over a real TCP
    /// loopback in `tests/bolt_tls_tests.rs`.
    #[test]
    fn from_pem_builds_config_for_self_signed_pkcs8() {
        let (cert_pem, key_pem) = generate_self_signed_pem();
        let cfg = TlsConfig::from_pem(cert_pem.as_bytes(), key_pem.as_bytes())
            .expect("valid self-signed pair");
        // Acceptor build is cheap (Arc clone); just exercise it.
        let _acceptor = cfg.acceptor();
        // server_config returns the same Arc — sanity-check pointer
        // identity so cloning never re-parses the cert chain.
        let a = cfg.server_config();
        let b = cfg.server_config();
        assert!(Arc::ptr_eq(&a, &b));
    }

    /// `from_pem_files` is a thin wrapper over `from_pem` — exercise
    /// the file-read path against a tempdir so a later refactor that
    /// drops the I/O conversion is caught.
    #[test]
    fn from_pem_files_loads_self_signed_pair_from_disk() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        let (cert_pem, key_pem) = generate_self_signed_pem();
        std::fs::File::create(&cert_path)
            .and_then(|mut f| f.write_all(cert_pem.as_bytes()))
            .expect("write cert");
        std::fs::File::create(&key_path)
            .and_then(|mut f| f.write_all(key_pem.as_bytes()))
            .expect("write key");
        let cfg =
            TlsConfig::from_pem_files(&cert_path, &key_path).expect("load PEM files from disk");
        let _acceptor = cfg.acceptor();
    }

    /// Missing files surface as `BoltError::Io`, not `Tls` — the
    /// I/O layer is the right place for a "file not found" message.
    #[test]
    fn from_pem_files_returns_io_error_for_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = TlsConfig::from_pem_files(
            dir.path().join("does-not-exist-cert.pem"),
            dir.path().join("does-not-exist-key.pem"),
        );
        let err = result.expect_err("expected error for missing file");
        assert!(
            matches!(err, BoltError::Io(_)),
            "expected BoltError::Io, got {err:?}"
        );
    }

    /// Helper: generate a self-signed RSA cert + PKCS#8 key via
    /// rcgen and return them as PEM strings. Used by the inline
    /// tests above and (re-implemented because dev-deps don't
    /// cross test crates) by the integration suite.
    fn generate_self_signed_pem() -> (String, String) {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("rcgen self-signed cert");
        (cert.cert.pem(), cert.key_pair.serialize_pem())
    }
}
