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
