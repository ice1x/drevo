//! Chunked message framing — the transport envelope every Bolt
//! message rides on after the handshake.
//!
//! ## Wire format
//!
//! ```text
//! +-----------+--------+
//! | length: 16-bit BE  |
//! | payload (≤ 65 535) |
//! +-----------+--------+
//! …more chunks…
//! +---------+
//! |  0x00   |
//! |  0x00   |
//! +---------+
//! ```
//!
//! A logical Bolt message is one or more chunks followed by a
//! zero-length chunk (the two bytes `0x00 0x00`). Splitting messages
//! into chunks lets Bolt stream very large records (Cypher result
//! rows, vector blobs) without buffering the entire message in
//! memory on the producer side.
//!
//! ## Why sync I/O
//!
//! The framing layer is deliberately sync (`std::io::Read` /
//! `Write`) so it can be unit-tested against a `Cursor<Vec<u8>>`
//! without a `tokio` runtime. The async TCP listener
//! ([`super::listener`]) reads the handshake and the message body
//! into a buffer first, then runs this sync codec on the buffer —
//! the same trick the MCP module uses.

use std::io::{self, Read, Write};

use super::error::{BoltError, BoltResult};

/// Maximum bytes carried in a single chunk (the 16-bit length field's
/// max value). Messages longer than this are split across multiple
/// chunks.
pub const MAX_CHUNK_SIZE: usize = u16::MAX as usize;

/// Encode `payload` into a sequence of chunks terminated by a zero
/// chunk, written to `writer`.
///
/// # Errors
///
/// Returns [`BoltError::Io`] if the writer fails. The encoder never
/// fails on its own — chunk boundaries are derived purely from the
/// payload length.
pub fn write_message<W: Write>(payload: &[u8], writer: &mut W) -> BoltResult<()> {
    for chunk in payload.chunks(MAX_CHUNK_SIZE) {
        let len = chunk.len() as u16;
        writer.write_all(&len.to_be_bytes())?;
        writer.write_all(chunk)?;
    }
    writer.write_all(&[0x00, 0x00])?;
    Ok(())
}

/// Read one logical message from `reader`. Stops on the first
/// zero-length chunk and returns the concatenated payload of every
/// preceding chunk.
///
/// # Errors
///
/// * [`BoltError::Eof`] — the reader hit EOF before a complete
///   message was assembled (e.g. a half-written length prefix).
/// * [`BoltError::Io`] — propagated from the underlying reader.
pub fn read_message<R: Read>(reader: &mut R) -> BoltResult<Vec<u8>> {
    let mut payload = Vec::new();
    loop {
        let mut len_bytes = [0u8; 2];
        match read_exact_or_eof(reader, &mut len_bytes)? {
            ReadOutcome::Eof => return Err(BoltError::Eof),
            ReadOutcome::Ok => {}
        }
        let len = u16::from_be_bytes(len_bytes) as usize;
        if len == 0 {
            return Ok(payload);
        }
        let start = payload.len();
        payload.resize(start + len, 0);
        reader
            .read_exact(&mut payload[start..])
            .map_err(map_unexpected_eof)?;
    }
}

enum ReadOutcome {
    Ok,
    Eof,
}

/// Read exactly `buf.len()` bytes, but treat an *initial* `UnexpectedEof`
/// (no bytes read at all) as a clean end-of-stream signal. A partial
/// read after at least one byte is still an error.
fn read_exact_or_eof<R: Read>(reader: &mut R, buf: &mut [u8]) -> BoltResult<ReadOutcome> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Ok(ReadOutcome::Eof);
                }
                return Err(BoltError::Eof);
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(BoltError::Io(e)),
        }
    }
    Ok(ReadOutcome::Ok)
}

fn map_unexpected_eof(err: io::Error) -> BoltError {
    if err.kind() == io::ErrorKind::UnexpectedEof {
        BoltError::Eof
    } else {
        BoltError::Io(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn small_payload_yields_three_byte_overhead_plus_terminator() {
        let mut buf = Vec::new();
        write_message(&[0xAB], &mut buf).unwrap();
        assert_eq!(buf, vec![0x00, 0x01, 0xAB, 0x00, 0x00]);
    }

    #[test]
    fn payload_at_max_chunk_size_fits_in_one_chunk() {
        let payload = vec![0x55; MAX_CHUNK_SIZE];
        let mut buf = Vec::new();
        write_message(&payload, &mut buf).unwrap();
        // 2-byte prefix + max payload + 2-byte terminator
        assert_eq!(buf.len(), 2 + MAX_CHUNK_SIZE + 2);
        assert_eq!(
            u16::from_be_bytes([buf[0], buf[1]]) as usize,
            MAX_CHUNK_SIZE
        );
    }

    #[test]
    fn payload_above_max_chunk_size_is_split() {
        let payload = vec![0xAA; MAX_CHUNK_SIZE + 1];
        let mut buf = Vec::new();
        write_message(&payload, &mut buf).unwrap();
        // first chunk MAX, second chunk 1, then terminator
        let first_len = u16::from_be_bytes([buf[0], buf[1]]);
        assert_eq!(first_len as usize, MAX_CHUNK_SIZE);
        let second_off = 2 + MAX_CHUNK_SIZE;
        assert_eq!(
            u16::from_be_bytes([buf[second_off], buf[second_off + 1]]),
            1
        );
    }

    #[test]
    fn read_clean_eof_before_any_bytes_is_error() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        let err = read_message(&mut cursor).unwrap_err();
        assert!(matches!(err, BoltError::Eof));
    }

    #[test]
    fn read_terminates_on_zero_chunk() {
        // payload "ab" then zero terminator
        let stream = vec![0x00, 0x02, b'a', b'b', 0x00, 0x00];
        let mut cursor = Cursor::new(stream);
        let out = read_message(&mut cursor).unwrap();
        assert_eq!(out, b"ab".to_vec());
    }
}
