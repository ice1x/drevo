//! Async TCP listener — accepts Bolt connections and performs the
//! handshake.
//!
//! Gated behind the `http` Cargo feature because that's what already
//! pulls in `tokio`. The Bolt session layer (task `00071`) will run
//! on top of [`crate::bolt::listener::accept_handshake`]: it returns
//! the negotiated version plus the still-open socket so the session
//! loop can start reading chunked PackStream messages from the same
//! connection — via [`crate::bolt::chunked`] once an async reader
//! wrapper lands.
//!
//! ## What's intentionally minimal
//!
//! * No session loop yet — that lands with `00071` HELLO / RUN / PULL.
//! * No TLS — `rustls` integration lands with `00073`.
//! * No authentication — basic auth + session tokens land with `00074`.
//! * No back-pressure / connection limit — added when MVCC concurrency
//!   work (`00080`+) gives us a meaningful budget to enforce.
//!
//! The split is deliberate: task `00070` ships the bytes-on-the-wire
//! piece (PackStream + framing + handshake + listener). The session
//! layer lands separately so it can be reviewed against the Cypher
//! executor without re-reviewing the codec.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::error::BoltResult;
use super::handshake::{parse_client_handshake, select_version, BoltVersion, HANDSHAKE_LEN};

/// Result of a completed Bolt handshake.
#[derive(Debug)]
pub struct AcceptedConnection {
    /// The socket, returned to the caller so it can drive a session
    /// loop on top of [`super::chunked`]. Already past the 20-byte
    /// handshake and the 4-byte server reply.
    pub socket: TcpStream,
    /// The selected version, or `None` if no proposal was supported.
    /// When `None`, the connection has had `00 00 00 00` written to
    /// it and the caller should close it.
    pub negotiated: Option<BoltVersion>,
}

/// Read the 20-byte client handshake from `socket`, choose a version,
/// and write the 4-byte server reply.
///
/// On success returns the still-open socket plus the chosen version
/// (or `None` if no proposal was supported — in that case the server
/// reply was `0x00 0x00 0x00 0x00` and the caller is expected to
/// close the socket).
///
/// # Errors
///
/// * [`super::error::BoltError::InvalidMagic`] — bad preamble.
/// * [`super::error::BoltError::Io`] — socket read or write failure.
pub async fn accept_handshake(mut socket: TcpStream) -> BoltResult<AcceptedConnection> {
    let mut buf = [0u8; HANDSHAKE_LEN];
    socket.read_exact(&mut buf).await?;
    let parsed = parse_client_handshake(&buf)?;
    let negotiated = select_version(&parsed.versions);
    let reply = negotiated.unwrap_or(BoltVersion::NONE).to_be_bytes();
    socket.write_all(&reply).await?;
    Ok(AcceptedConnection { socket, negotiated })
}
