//! Async TCP listener — accepts Bolt connections, performs the
//! handshake, then drives the session loop.
//!
//! Gated behind the `http` Cargo feature because that's what already
//! pulls in `tokio`. [`accept_handshake`] runs the 20-byte exchange
//! and returns the still-open socket so callers can layer their own
//! protocol on top; [`accept_and_run_session`] (task `00071`) bundles
//! handshake + session loop in one call so a `tokio::spawn`-per-
//! connection server is a one-liner.
//!
//! ## What's intentionally minimal
//!
//! * No TLS — `rustls` integration lands with `00073`.
//! * No authentication — basic auth + session tokens land with `00074`.
//! * No back-pressure / connection limit — added when MVCC concurrency
//!   work (`00080`+) gives us a meaningful budget to enforce.

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
    let accepted = accept_handshake(socket).await?;
    let mut socket = accepted.socket;
    if accepted.negotiated.is_none() {
        return Ok(());
    }
    let mut session = Session::new(drevo);
    loop {
        let payload = match read_message_async(&mut socket).await {
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
                write_server_async(&reply, &mut socket).await?;
                continue;
            }
        };
        let replies = session.handle(msg);
        for reply in &replies {
            write_server_async(reply, &mut socket).await?;
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
