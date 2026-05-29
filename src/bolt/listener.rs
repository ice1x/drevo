//! Async TCP listener — accepts Bolt connections, performs the
//! handshake, then drives the session loop.
//!
//! Gated behind the `http` Cargo feature because that's what already
//! pulls in `tokio`. [`crate::bolt::listener::accept_handshake`] runs
//! the 20-byte exchange and returns the still-open socket so callers
//! can layer their own protocol on top;
//! [`crate::bolt::listener::accept_and_run_session`] (task `00071`)
//! bundles handshake + session loop in one call so a `tokio::spawn`-
//! per-connection server is a one-liner.
//!
//! ## What's intentionally minimal
//!
//! * No authentication — basic auth + session tokens land with `00074`.
//! * No back-pressure / connection limit — added when MVCC concurrency
//!   work (`00080`+) gives us a meaningful budget to enforce.
//!
//! TLS lands as task `00073` in the sibling `crate::bolt::tls`
//! module (feature `bolt-tls`; link not generated under default
//! features so this rustdoc compiles either way). It is layered on
//! top of the generic
//! [`crate::bolt::listener::accept_handshake_on`] /
//! [`crate::bolt::listener::run_session_on`] entry points exposed
//! here — those work over *any* `AsyncRead + AsyncWrite + Unpin`
//! stream, not just [`tokio::net::TcpStream`], so the only TLS-aware
//! code path is the wrapper that calls `tokio_rustls::TlsAcceptor`
//! before invoking the same handshake + session loop.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use super::chunked::MAX_CHUNK_SIZE;
use super::error::{BoltError, BoltResult};
use super::handshake::{parse_client_handshake, select_version, BoltVersion, HANDSHAKE_LEN};
use super::packstream::{decode, encode};
use super::session::{decode_client, encode_server, ServerMessage, Session, State};
use crate::db::Drevo;

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

/// Generic counterpart of [`AcceptedConnection`] returned by
/// [`accept_handshake_on`]. Exists so the TLS module (task `00073`)
/// can drive the same handshake over a `tokio_rustls::server::TlsStream`
/// without paying for a separate code path. The `S` type parameter is
/// whatever async stream the caller passed in — most commonly
/// [`tokio::net::TcpStream`] (the back-compat alias [`AcceptedConnection`]
/// remains) or a TLS-wrapped variant.
#[derive(Debug)]
pub struct AcceptedConnectionOn<S> {
    /// The stream, returned past the handshake + server reply.
    pub stream: S,
    /// Negotiated version, or `None` if no client proposal was
    /// supported (reply `00 00 00 00` was written; caller closes).
    pub negotiated: Option<BoltVersion>,
}

/// Read the 20-byte client handshake from `socket`, choose a version,
/// and write the 4-byte server reply.
///
/// Thin wrapper over [`accept_handshake_on`] preserved for callers
/// that work with [`tokio::net::TcpStream`] directly. New code that
/// needs to support both plain TCP and TLS should call
/// [`accept_handshake_on`] instead.
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
pub async fn accept_handshake(socket: TcpStream) -> BoltResult<AcceptedConnection> {
    let accepted = accept_handshake_on(socket).await?;
    Ok(AcceptedConnection {
        socket: accepted.stream,
        negotiated: accepted.negotiated,
    })
}

/// Same as [`accept_handshake`] but generic over any
/// `AsyncRead + AsyncWrite + Unpin` stream — TCP, TLS-wrapped TCP,
/// in-memory duplex, etc. The bytes-on-the-wire layer is identical;
/// the only thing that changes is what `S` is.
///
/// Used as the foundation for both the plain-TCP entry point and the
/// TLS entry point (task `00073`).
///
/// # Errors
///
/// Same as [`accept_handshake`].
pub async fn accept_handshake_on<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
) -> BoltResult<AcceptedConnectionOn<S>> {
    let mut buf = [0u8; HANDSHAKE_LEN];
    stream.read_exact(&mut buf).await?;
    let parsed = parse_client_handshake(&buf)?;
    let negotiated = select_version(&parsed.versions);
    let reply = negotiated.unwrap_or(BoltVersion::NONE).to_be_bytes();
    stream.write_all(&reply).await?;
    Ok(AcceptedConnectionOn { stream, negotiated })
}

/// Bundle handshake + session loop in a single call. Suitable as the
/// body of a `tokio::spawn`-per-connection server. Returns once the
/// peer hits clean EOF, sends `GOODBYE`, or the connection errors.
///
/// On handshake failure (bad magic, no compatible version) the socket
/// is closed; the function returns `Ok(())` for the "no compatible
/// version" case (the spec reply `00 00 00 00` was already written by
/// [`accept_handshake`]) and `Err` for malformed handshakes.
///
/// # Errors
///
/// Propagates handshake / I/O errors from [`accept_handshake`]; Cypher
/// errors are surfaced as `FAILURE` messages on the wire and do *not*
/// abort this loop.
pub async fn accept_and_run_session(socket: TcpStream, drevo: &Drevo) -> BoltResult<()> {
    let accepted = accept_handshake_on(socket).await?;
    if accepted.negotiated.is_none() {
        return Ok(());
    }
    let mut stream = accepted.stream;
    run_session_on(&mut stream, drevo).await
}

/// Post-handshake session loop. Drives a [`Session`] over any
/// `AsyncRead + AsyncWrite + Unpin` stream — TCP, TLS, etc. The
/// stream must already be past the 20-byte handshake (call
/// [`accept_handshake_on`] first).
///
/// Used directly by the TLS module (task `00073`) so the same
/// session-loop bytes flow whether the underlying transport is plain
/// TCP or TLS-wrapped TCP.
///
/// Returns once the peer hits clean EOF, sends `GOODBYE`, or the
/// connection errors. Cypher errors are reported on the wire as
/// `FAILURE` messages and do *not* abort the loop.
///
/// # Errors
///
/// Propagates I/O failures from the stream. Cypher / protocol errors
/// are surfaced as Bolt `FAILURE` messages.
pub async fn run_session_on<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    drevo: &Drevo,
) -> BoltResult<()> {
    let mut session = Session::new(drevo);
    loop {
        let payload = match read_message_async(stream).await {
            Ok(p) => p,
            Err(BoltError::Eof) => return Ok(()),
            Err(e) => return Err(e),
        };
        let (value, rest) = decode(&payload)?;
        if !rest.is_empty() {
            return Err(BoltError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "trailing bytes after Bolt message",
            )));
        }
        let msg = match decode_client(&value) {
            Ok(m) => m,
            Err(e) => {
                let reply = ServerMessage::Failure {
                    metadata: super::session::protocol_failure_metadata(&format!("{e}")),
                };
                write_server_async(&reply, stream).await?;
                continue;
            }
        };
        let replies = session.handle(msg);
        for reply in &replies {
            write_server_async(reply, stream).await?;
        }
        if session.state() == State::Defunct {
            return Ok(());
        }
    }
}

async fn write_server_async<W: AsyncWrite + Unpin>(
    msg: &ServerMessage,
    writer: &mut W,
) -> BoltResult<()> {
    let mut payload = Vec::new();
    encode(&encode_server(msg), &mut payload)?;
    write_message_async(&payload, writer).await
}

async fn read_message_async<R: AsyncRead + Unpin>(reader: &mut R) -> BoltResult<Vec<u8>> {
    let mut payload = Vec::new();
    loop {
        let mut len_bytes = [0u8; 2];
        match reader.read_exact(&mut len_bytes).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                if payload.is_empty() {
                    return Err(BoltError::Eof);
                }
                return Err(BoltError::Eof);
            }
            Err(e) => return Err(BoltError::Io(e)),
        }
        let len = u16::from_be_bytes(len_bytes) as usize;
        if len == 0 {
            return Ok(payload);
        }
        let start = payload.len();
        payload.resize(start + len, 0);
        reader
            .read_exact(&mut payload[start..])
            .await
            .map_err(|e| {
                if e.kind() == io::ErrorKind::UnexpectedEof {
                    BoltError::Eof
                } else {
                    BoltError::Io(e)
                }
            })?;
    }
}

async fn write_message_async<W: AsyncWrite + Unpin>(
    payload: &[u8],
    writer: &mut W,
) -> BoltResult<()> {
    for chunk in payload.chunks(MAX_CHUNK_SIZE) {
        let len = chunk.len() as u16;
        writer.write_all(&len.to_be_bytes()).await?;
        writer.write_all(chunk).await?;
    }
    writer.write_all(&[0x00, 0x00]).await?;
    Ok(())
}
