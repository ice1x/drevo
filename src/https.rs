//! HTTPS for the HTTP API: a listener that completes the TLS handshake
//! before handing a connection to the router.
//!
//! [`TlsListener`](crate::https::TlsListener) implements [`axum::serve::Listener`], so the router is
//! served over TLS with the same `axum::serve` call as over plain TCP:
//!
//! ```no_run
//! # async fn run(router: axum::Router) -> std::io::Result<()> {
//! let tls = drevo::bolt::tls::TlsConfig::from_pem_files("cert.pem", "key.pem")
//!     .map_err(std::io::Error::other)?;
//! let tcp = tokio::net::TcpListener::bind("0.0.0.0:8443").await?;
//! axum::serve(drevo::https::TlsListener::new(tcp, &tls), router).await
//! # }
//! ```
//!
//! Handshakes run on their own tasks, so a client that connects and stays
//! silent holds up nobody; one that has not finished within ten seconds is
//! dropped. A failed handshake — a plain-text client, an unknown protocol
//! version — is logged at DEBUG and the connection closed.

#![cfg(feature = "bolt-tls")]

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

use crate::bolt::tls::TlsConfig;

/// How long a client may take to finish the TLS handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Connections waiting for the router to accept them.
const BACKLOG: usize = 128;

/// A TCP listener whose connections arrive already past the TLS handshake.
#[derive(Debug)]
pub struct TlsListener {
    local_addr: std::io::Result<SocketAddr>,
    ready: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
}

impl TlsListener {
    /// Accept connections on `tcp` and complete each TLS handshake with
    /// `tls`'s certificate. Must be called inside a Tokio runtime.
    #[must_use]
    pub fn new(tcp: TcpListener, tls: &TlsConfig) -> Self {
        let local_addr = tcp.local_addr();
        let acceptor = tls.acceptor();
        let (send, ready) = mpsc::channel(BACKLOG);
        tokio::spawn(accept_loop(tcp, acceptor, send));
        Self { local_addr, ready }
    }
}

async fn accept_loop(
    tcp: TcpListener,
    acceptor: TlsAcceptor,
    send: mpsc::Sender<(TlsStream<TcpStream>, SocketAddr)>,
) {
    loop {
        let (socket, peer) = match tcp.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                tracing::warn!(error = %err, "https accept failed");
                // Out of file descriptors and the like: back off briefly.
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        if send.is_closed() {
            return;
        }
        let acceptor = acceptor.clone();
        let send = send.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(socket)).await {
                Ok(Ok(stream)) => {
                    let _ = send.send((stream, peer)).await;
                }
                Ok(Err(err)) => tracing::debug!(%peer, error = %err, "TLS handshake failed"),
                Err(_) => tracing::debug!(%peer, "TLS handshake timed out"),
            }
        });
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.ready.recv().await {
            Some(connection) => connection,
            // The accept loop only stops when this listener is dropped, so
            // the channel never closes while it is being polled.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        match &self.local_addr {
            Ok(addr) => Ok(*addr),
            Err(err) => Err(std::io::Error::new(err.kind(), err.to_string())),
        }
    }
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, TlsListener>>
    for crate::native_api::ClientAddr
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, TlsListener>) -> Self {
        Self(*stream.remote_addr())
    }
}
