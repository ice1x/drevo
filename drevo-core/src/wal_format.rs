//! Encodings of the write-ahead log.
//!
//! The log is JSON Lines by default: one [`WalOp`] object, or one JSON array of
//! ops for a transaction, per line. Two binary encodings can be chosen instead,
//! each behind a Cargo feature:
//!
//! - [`WalFormat::Cbor`] (`wal-cbor`) — floats are written in the shortest
//!   form that keeps their value, so an `f32` embedding component costs 5
//!   bytes instead of ~20 characters;
//! - [`WalFormat::MsgPack`] (`wal-msgpack`).
//!
//! A binary log starts with an 8-byte header naming its format. Each record is
//! framed as `length (u32 LE) · CRC-32 of the payload (u32 LE) · payload`,
//! where the payload is the encoded array of the record's ops. The frame gives
//! binary logs the same recovery rule JSON Lines has: an incomplete or
//! checksum-failing **last** record is a torn tail and is dropped, while a bad
//! record with data after it is corruption and the log refuses to open.
//!
//! A JSON log has no header, so logs written before binary formats existed
//! keep opening unchanged. The format of an existing log is detected from its
//! first bytes; a log is only ever rewritten in another format by compaction.

use std::fmt;
use std::str::FromStr;

use crate::error::{CoreError, Result};
use crate::native::WalOp;

/// The first six header bytes of a binary log, followed by a version byte and
/// a format byte.
const MAGIC: &[u8; 6] = b"DRVWAL";
/// Header format version.
const HEADER_VERSION: u8 = 1;
/// Length of a binary log's header.
pub const HEADER_LEN: usize = 8;
/// Length of a binary record's frame (length + checksum).
const FRAME_LEN: usize = 8;

/// How the write-ahead log encodes its records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum WalFormat {
    /// JSON Lines — the default, always available.
    #[default]
    Json,
    /// Framed CBOR records, behind the `wal-cbor` feature.
    Cbor,
    /// Framed MessagePack records, behind the `wal-msgpack` feature.
    MsgPack,
}

/// Why a WAL format name was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WalFormatError {
    /// Not a known format name.
    #[error("unknown WAL format `{0}` (known: json, cbor, msgpack)")]
    Unknown(String),
    /// A known format this build was compiled without.
    #[error("WAL format `{0}` is not compiled in")]
    NotCompiled(String),
}

impl WalFormat {
    /// The name used in configuration.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            WalFormat::Json => "json",
            WalFormat::Cbor => "cbor",
            WalFormat::MsgPack => "msgpack",
        }
    }

    /// Whether this build can read and write the format.
    #[must_use]
    pub fn compiled_in(self) -> bool {
        match self {
            WalFormat::Json => true,
            WalFormat::Cbor => cfg!(feature = "wal-cbor"),
            WalFormat::MsgPack => cfg!(feature = "wal-msgpack"),
        }
    }

    fn tag(self) -> u8 {
        match self {
            WalFormat::Json => b'J',
            WalFormat::Cbor => b'C',
            WalFormat::MsgPack => b'M',
        }
    }

    /// The header a new log in this format starts with; empty for JSON.
    #[must_use]
    pub fn header(self) -> Vec<u8> {
        if self == WalFormat::Json {
            return Vec::new();
        }
        let mut h = MAGIC.to_vec();
        h.push(HEADER_VERSION);
        h.push(self.tag());
        h
    }

    /// Encode one record holding `ops` (one op for a direct write, the whole
    /// write set for a transaction), including its framing.
    ///
    /// # Errors
    /// [`CoreError::Json`] if an op cannot be serialised, or
    /// [`CoreError::Backend`] for a binary encoding failure or a format this
    /// build lacks.
    pub fn encode_record(self, ops: &[WalOp]) -> Result<Vec<u8>> {
        if self == WalFormat::Json {
            let mut line = match ops {
                [] => return Ok(Vec::new()),
                [op] => serde_json::to_vec(op)?,
                many => serde_json::to_vec(many)?,
            };
            line.push(b'\n');
            return Ok(line);
        }
        if ops.is_empty() {
            return Ok(Vec::new());
        }
        // Through `serde_json::Value`, whose serializer is human-readable, so
        // property maps keep their map form (the compact path wraps them as
        // JSON text for bincode).
        let value = serde_json::to_value(ops)?;
        let payload = encode_value(self, &value)?;
        let len = u32::try_from(payload.len())
            .map_err(|_| CoreError::Backend("WAL record over 4 GiB".into()))?;
        let mut out = Vec::with_capacity(FRAME_LEN + payload.len());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&crc32(&payload).to_le_bytes());
        out.extend_from_slice(&payload);
        Ok(out)
    }
}

impl fmt::Display for WalFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for WalFormat {
    type Err = WalFormatError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let name = s.trim().to_ascii_lowercase();
        let format = [WalFormat::Json, WalFormat::Cbor, WalFormat::MsgPack]
            .into_iter()
            .find(|f| f.name() == name)
            .ok_or_else(|| WalFormatError::Unknown(name.clone()))?;
        if !format.compiled_in() {
            return Err(WalFormatError::NotCompiled(name));
        }
        Ok(format)
    }
}

/// What the start of a log says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detected {
    /// A log in `format`; records start at byte `body_start`.
    Log {
        /// The log's format.
        format: WalFormat,
        /// Where the first record starts (after the header, if any).
        body_start: usize,
    },
    /// The file holds only the beginning of a binary header — a crash while a
    /// fresh log was being created. Nothing in it was ever acknowledged.
    TornHeader,
}

/// Detect the format of a log from its first bytes.
///
/// # Errors
/// [`CoreError::Io`] when the header names a format this build lacks or an
/// unknown version, so a log is never misread.
pub fn detect(bytes: &[u8]) -> Result<Detected> {
    if bytes.len() < HEADER_LEN {
        if !bytes.is_empty() && is_header_prefix(bytes) {
            return Ok(Detected::TornHeader);
        }
        return Ok(Detected::Log {
            format: WalFormat::Json,
            body_start: 0,
        });
    }
    if &bytes[..MAGIC.len()] != MAGIC {
        return Ok(Detected::Log {
            format: WalFormat::Json,
            body_start: 0,
        });
    }
    let (version, tag) = (bytes[MAGIC.len()], bytes[MAGIC.len() + 1]);
    let format = match tag {
        b'C' => WalFormat::Cbor,
        b'M' => WalFormat::MsgPack,
        _ => return Err(invalid(format!("unknown WAL format tag {tag:#04x}"))),
    };
    if version != HEADER_VERSION {
        return Err(invalid(format!("unsupported WAL header version {version}")));
    }
    if !format.compiled_in() {
        return Err(invalid(format!(
            "this write-ahead log is in {format} format, which this build was compiled without"
        )));
    }
    Ok(Detected::Log {
        format,
        body_start: HEADER_LEN,
    })
}

fn is_header_prefix(bytes: &[u8]) -> bool {
    let magic_part = bytes.len().min(MAGIC.len());
    bytes[..magic_part] == MAGIC[..magic_part]
}

fn invalid(message: String) -> CoreError {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message).into()
}

/// The outcome of reading one record at a position in a log.
#[derive(Debug)]
pub enum Record {
    /// A complete, valid record; the next one starts at `next`.
    Ops {
        /// The record's ops (empty for a blank JSON line).
        ops: Vec<WalOp>,
        /// Byte offset just past the record.
        next: usize,
    },
    /// The bytes from here do not form a valid record. Whether that is a torn
    /// tail or corruption depends on what follows `next`.
    Invalid {
        /// Byte offset just past the invalid record (or the end of the log).
        next: usize,
    },
    /// The record is cut off by the end of the log.
    Incomplete,
}

/// Read the record of a `format` log starting at `pos`.
#[must_use]
pub fn read_record(format: WalFormat, bytes: &[u8], pos: usize) -> Record {
    if format == WalFormat::Json {
        let (line_end, next) = match bytes[pos..].iter().position(|b| *b == b'\n') {
            Some(i) => (pos + i, pos + i + 1),
            None => (bytes.len(), bytes.len()),
        };
        return match parse_json_line(&bytes[pos..line_end]) {
            Some(ops) => Record::Ops { ops, next },
            None => Record::Invalid { next },
        };
    }
    let rest = &bytes[pos..];
    if rest.len() < FRAME_LEN {
        return Record::Incomplete;
    }
    let len = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
    let crc = u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]);
    let Some(end) = FRAME_LEN.checked_add(len).filter(|e| *e <= rest.len()) else {
        return Record::Incomplete;
    };
    let payload = &rest[FRAME_LEN..end];
    let next = pos + end;
    if crc32(payload) != crc {
        return Record::Invalid { next };
    }
    match decode_ops(format, payload) {
        Some(ops) => Record::Ops { ops, next },
        None => Record::Invalid { next },
    }
}

/// What [`replay`] found in a log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replayed {
    /// The file holds only the beginning of a binary header; nothing in it
    /// was ever acknowledged.
    TornHeader,
    /// The log is empty.
    Empty,
    /// A log in `format`. Its valid records end at byte `valid_end`; `torn`
    /// says whether an unacknowledged tail follows them.
    Log {
        /// The log's format.
        format: WalFormat,
        /// Byte offset just past the last valid record.
        valid_end: u64,
        /// Whether bytes after `valid_end` form a torn tail to drop.
        torn: bool,
    },
}

/// The bytes [`replay`] reads at a time.
pub(crate) const REPLAY_CHUNK: usize = 4 << 20;

/// Read a log from `reader` record by record, `chunk` bytes at a time, and
/// hand every op of every valid record to `apply`, in order. Memory stays
/// at about one chunk plus the largest record, whatever the size of the log.
///
/// The recovery rule is that of reading the whole log at once: an incomplete
/// or invalid **last** record is a torn tail; an invalid record with data
/// after it (anything but whitespace, for JSON Lines) is corruption.
///
/// # Errors
/// [`CoreError::Io`] on a read failure, an unreadable header (see
/// [`detect`]) or a corrupt record; `path` names the log in the message.
pub(crate) fn replay<R: std::io::Read>(
    mut reader: R,
    chunk: usize,
    path: &std::path::Path,
    mut apply: impl FnMut(WalOp),
) -> Result<Replayed> {
    let chunk = chunk.max(1);
    let mut buf: Vec<u8> = Vec::new();
    // Absolute offset of `buf[0]` in the log.
    let mut base: u64 = 0;
    let mut eof = false;
    // Append up to `chunk` more bytes; `false` once the log is exhausted.
    let mut fill = |buf: &mut Vec<u8>, eof: &mut bool| -> Result<bool> {
        if *eof {
            return Ok(false);
        }
        let start = buf.len();
        buf.resize(start + chunk, 0);
        let mut got = 0;
        while got < chunk {
            let n = reader.read(&mut buf[start + got..])?;
            if n == 0 {
                *eof = true;
                break;
            }
            got += n;
        }
        buf.truncate(start + got);
        Ok(got > 0)
    };
    while buf.len() < HEADER_LEN && fill(&mut buf, &mut eof)? {}
    let (format, body_start) = match detect(&buf)? {
        Detected::TornHeader => return Ok(Replayed::TornHeader),
        Detected::Log { .. } if buf.is_empty() => return Ok(Replayed::Empty),
        Detected::Log { format, body_start } => (format, body_start),
    };
    let mut pos = body_start;
    let mut valid_end = body_start as u64;
    loop {
        if pos >= buf.len() {
            buf.clear();
            base += pos as u64;
            pos = 0;
            if !fill(&mut buf, &mut eof)? {
                return Ok(Replayed::Log {
                    format,
                    valid_end,
                    torn: false,
                });
            }
            continue;
        }
        // A JSON record ends at a newline: without one in the window, the
        // record may continue in the next chunk.
        if format == WalFormat::Json && !eof && !buf[pos..].contains(&b'\n') {
            buf.drain(..pos);
            base += pos as u64;
            pos = 0;
            fill(&mut buf, &mut eof)?;
            continue;
        }
        match read_record(format, &buf, pos) {
            Record::Ops { ops, next } => {
                for op in ops {
                    apply(op);
                }
                valid_end = base + next as u64;
                pos = next;
            }
            Record::Incomplete if !eof => {
                buf.drain(..pos);
                base += pos as u64;
                pos = 0;
                fill(&mut buf, &mut eof)?;
            }
            Record::Incomplete => {
                return Ok(Replayed::Log {
                    format,
                    valid_end,
                    torn: true,
                })
            }
            Record::Invalid { next } => {
                // A torn tail only if nothing follows: look at the rest of the
                // window, then at the rest of the log.
                let follows = |bytes: &[u8]| match format {
                    WalFormat::Json => !bytes.iter().all(u8::is_ascii_whitespace),
                    _ => !bytes.is_empty(),
                };
                let mut more = follows(&buf[next.min(buf.len())..]);
                while !more {
                    buf.clear();
                    if !fill(&mut buf, &mut eof)? {
                        break;
                    }
                    more = follows(&buf);
                }
                if !more {
                    return Ok(Replayed::Log {
                        format,
                        valid_end,
                        torn: true,
                    });
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "corrupt write-ahead-log record at byte {} of {} \
                         (valid records follow it, so this is not a torn tail); \
                         refusing to open rather than drop acknowledged writes",
                        base + pos as u64,
                        path.display()
                    ),
                )
                .into());
            }
        }
    }
}

/// Parse one JSON Lines record: a bare [`WalOp`] object or a JSON array of
/// ops. A blank line parses as zero ops; `None` means the bytes are not a
/// complete valid record.
#[must_use]
pub fn parse_json_line(line: &[u8]) -> Option<Vec<WalOp>> {
    let text = std::str::from_utf8(line).ok()?.trim();
    if text.is_empty() {
        return Some(Vec::new());
    }
    if text.starts_with('[') {
        serde_json::from_str::<Vec<WalOp>>(text).ok()
    } else {
        serde_json::from_str::<WalOp>(text).ok().map(|op| vec![op])
    }
}

fn decode_ops(format: WalFormat, payload: &[u8]) -> Option<Vec<WalOp>> {
    let value: Option<serde_json::Value> = match format {
        WalFormat::Json => serde_json::from_slice(payload).ok(),
        #[cfg(feature = "wal-cbor")]
        WalFormat::Cbor => ciborium::from_reader(payload).ok(),
        #[cfg(feature = "wal-msgpack")]
        WalFormat::MsgPack => rmp_serde::from_slice(payload).ok(),
        #[allow(unreachable_patterns)]
        _ => None,
    };
    serde_json::from_value(value?).ok()
}

fn encode_value(format: WalFormat, value: &serde_json::Value) -> Result<Vec<u8>> {
    match format {
        WalFormat::Json => Ok(serde_json::to_vec(value)?),
        #[cfg(feature = "wal-cbor")]
        WalFormat::Cbor => {
            let mut out = Vec::new();
            ciborium::into_writer(value, &mut out)
                .map_err(|e| CoreError::Backend(format!("CBOR encode: {e}")))?;
            Ok(out)
        }
        #[cfg(feature = "wal-msgpack")]
        WalFormat::MsgPack => rmp_serde::to_vec_named(value)
            .map_err(|e| CoreError::Backend(format!("MessagePack encode: {e}"))),
        #[allow(unreachable_patterns)]
        other => {
            let _ = value;
            Err(CoreError::Backend(format!(
                "WAL format {other} is not compiled in"
            )))
        }
    }
}

/// CRC-32 (IEEE 802.3, reflected), table-driven.
#[must_use]
pub fn crc32(bytes: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut c = i as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
                k += 1;
            }
            table[i] = c;
            i += 1;
        }
        table
    };
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc = TABLE[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(i: u64) -> WalOp {
        WalOp::DeleteEdge(i)
    }

    /// A log of `n` records in `format`, one op each.
    fn log(format: WalFormat, n: u64) -> Vec<u8> {
        let mut bytes = format.header();
        for i in 0..n {
            bytes.extend(format.encode_record(&[op(i)]).unwrap());
        }
        bytes
    }

    fn replay_all(bytes: &[u8], chunk: usize) -> Result<(Replayed, Vec<WalOp>)> {
        let mut ops = Vec::new();
        let r = replay(
            std::io::Cursor::new(bytes),
            chunk,
            std::path::Path::new("test.wal"),
            |o| ops.push(o),
        )?;
        Ok((r, ops))
    }

    fn formats() -> Vec<WalFormat> {
        [WalFormat::Json, WalFormat::Cbor, WalFormat::MsgPack]
            .into_iter()
            .filter(|f| f.compiled_in())
            .collect()
    }

    #[test]
    fn replay_reads_every_record_whatever_the_chunk() {
        for format in formats() {
            let bytes = log(format, 40);
            for chunk in [1, 3, 7, 64, REPLAY_CHUNK] {
                let (r, ops) = replay_all(&bytes, chunk).unwrap();
                assert_eq!(
                    ops,
                    (0..40).map(op).collect::<Vec<_>>(),
                    "{format} chunk {chunk}"
                );
                assert_eq!(
                    r,
                    Replayed::Log {
                        format,
                        valid_end: bytes.len() as u64,
                        torn: false
                    },
                    "{format} chunk {chunk}"
                );
            }
        }
    }

    #[test]
    fn replay_drops_a_torn_tail() {
        for format in formats() {
            let whole = log(format, 10);
            let nine = log(format, 9).len();
            // Cut inside the last record.
            let bytes = &whole[..whole.len() - 2];
            for chunk in [1, 5, 4096] {
                let (r, ops) = replay_all(bytes, chunk).unwrap();
                assert_eq!(ops.len(), 9, "{format} chunk {chunk}");
                assert_eq!(
                    r,
                    Replayed::Log {
                        format,
                        valid_end: nine as u64,
                        torn: true
                    },
                    "{format} chunk {chunk}"
                );
            }
        }
    }

    #[test]
    fn replay_refuses_a_corrupt_record_with_records_after_it() {
        for format in formats() {
            let mut bytes = log(format, 10);
            // Inside the fifth record's payload, past a binary frame's length
            // and checksum (a damaged length reads as a cut-off record).
            let frame = if format == WalFormat::Json {
                0
            } else {
                FRAME_LEN
            };
            let at = log(format, 4).len() + frame + 2;
            bytes[at] ^= 0x5a;
            for chunk in [1, 6, 4096] {
                let err = replay_all(&bytes, chunk).unwrap_err();
                assert!(
                    err.to_string().contains("corrupt"),
                    "{format} chunk {chunk}: {err}"
                );
            }
        }
    }

    #[test]
    fn replay_treats_trailing_whitespace_after_a_bad_json_line_as_torn() {
        let mut bytes = log(WalFormat::Json, 3);
        bytes.extend_from_slice(b"{\"DeleteEd");
        bytes.extend_from_slice(b"\n   \n");
        for chunk in [1, 2, 4096] {
            let (r, ops) = replay_all(&bytes, chunk).unwrap();
            assert_eq!(ops.len(), 3);
            assert!(matches!(r, Replayed::Log { torn: true, .. }), "{r:?}");
        }
    }

    #[test]
    fn replay_of_nothing_and_of_a_partial_header() {
        assert_eq!(replay_all(b"", 3).unwrap().0, Replayed::Empty);
        if WalFormat::Cbor.compiled_in() {
            assert_eq!(replay_all(b"DRVW", 3).unwrap().0, Replayed::TornHeader);
        }
    }

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn json_logs_have_no_header_and_are_detected_by_default() {
        assert!(WalFormat::Json.header().is_empty());
        assert_eq!(
            detect(b"{\"UpsertNode\":{}}\n").unwrap(),
            Detected::Log {
                format: WalFormat::Json,
                body_start: 0
            }
        );
        assert_eq!(
            detect(b"").unwrap(),
            Detected::Log {
                format: WalFormat::Json,
                body_start: 0
            }
        );
    }

    #[test]
    fn a_partial_header_is_a_torn_header() {
        assert_eq!(detect(b"DRV").unwrap(), Detected::TornHeader);
        assert_eq!(detect(b"DRVWAL\x01").unwrap(), Detected::TornHeader);
    }

    #[test]
    fn unknown_tags_and_versions_refuse() {
        assert!(detect(b"DRVWAL\x01X").is_err());
        assert!(detect(b"DRVWAL\x09C").is_err());
    }

    #[test]
    fn names_parse_and_unknown_ones_do_not() {
        assert_eq!(" JSON ".parse::<WalFormat>(), Ok(WalFormat::Json));
        assert_eq!(
            "yaml".parse::<WalFormat>(),
            Err(WalFormatError::Unknown("yaml".into()))
        );
    }
}
