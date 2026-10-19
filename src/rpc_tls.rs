//! TLS for the gRPC API and Arrow Flight, with the HTTPS certificate.
//!
//! [`incoming`](crate::rpc_tls::incoming) turns a TCP listener into the
//! stream of connections tonic's `serve_with_incoming` takes, each one past
//! the TLS handshake. The certificate offers ALPN `h2`, which gRPC clients
//! require. Handshakes run as for HTTPS: on their own tasks, at most ten
//! seconds each, and a plain-text client is dropped.
//!
//! ```no_run
//! # async fn run(registry: std::sync::Arc<drevo::database_registry::DatabaseRegistry>)
//! # -> Result<(), Box<dyn std::error::Error>> {
//! let tls = drevo::bolt::tls::TlsConfig::from_pem_files("cert.pem", "key.pem")?;
//! let tcp = tokio::net::TcpListener::bind("0.0.0.0:50051").await?;
//! drevo::grpc::serve_tls_with_security(
//!     registry,
//!     drevo::security::Security::default(),
//!     tcp,
//!     &tls,
//!     std::future::pending(),
//! )
//! .await?;
//! # Ok(()) }
//! ```

#![cfg(all(feature = "bolt-tls", any(feature = "grpc", feature = "arrow-flight")))]

use std::pin::Pin;
use std::task::{Context, Poll};

use futures::Stream;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::server::TlsStream;
use tonic::transport::server::{Connected, TcpConnectInfo};

use crate::bolt::tls::TlsConfig;

/// Connections waiting for the server to take them.
const BACKLOG: usize = 128;

/// A connection past the TLS handshake, as tonic serves it. Requests see the
/// client's address as they do over plain TCP.
#[derive(Debug)]
pub struct TlsIo(TlsStream<TcpStream>);

impl Connected for TlsIo {
    type ConnectInfo = TcpConnectInfo;

    fn connect_info(&self) -> TcpConnectInfo {
        let (tcp, _) = self.0.get_ref();
        TcpConnectInfo {
            local_addr: tcp.local_addr().ok(),
            remote_addr: tcp.peer_addr().ok(),
        }
    }
}

impl AsyncRead for TlsIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for TlsIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }
}

/// The connections on `tcp` that completed a TLS handshake with `tls`'s
/// certificate, offering ALPN `h2`. Must be called inside a Tokio runtime.
pub fn incoming(
    tcp: TcpListener,
    tls: &TlsConfig,
) -> impl Stream<Item = std::io::Result<TlsIo>> + Send + 'static {
    let acceptor = tls.with_alpn(&[b"h2"]).acceptor();
    let (send, ready) = mpsc::channel(BACKLOG);
    tokio::spawn(crate::https::accept_loop(tcp, acceptor, send));
    futures::stream::unfold(ready, |mut ready| async move {
        let (stream, _) = ready.recv().await?;
        Some((Ok(TlsIo(stream)), ready))
    })
}
