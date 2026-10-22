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
/// The zstd level of a `cbor-zstd` record. On a knowledge graph's log, level
/// 19 was 2% smaller than level 3 at twelve times the CPU.
#[cfg(feature = "wal-zstd")]
const ZSTD_LEVEL: i32 = 3;
/// The largest record a zstd frame header may announce: a damaged header
/// must not make a read allocate without bound.
#[cfg(feature = "wal-zstd")]
const ZSTD_MAX_RECORD: usize = 1 << 30;

#[cfg(feature = "wal-zstd")]
thread_local! {
    // One context per thread: creating one for every record made a log of
    // small records twice as slow to read as its uncompressed form.
    static ZSTD_COMPRESSOR: std::cell::RefCell<Option<zstd::bulk::Compressor<'static>>> =
        const { std::cell::RefCell::new(None) };
    static ZSTD_DECOMPRESSOR: std::cell::RefCell<Option<zstd::bulk::Decompressor<'static>>> =
        const { std::cell::RefCell::new(None) };
}

/// `bytes` as one zstd frame, which records its content size.
#[cfg(feature = "wal-zstd")]
fn zstd_compress(bytes: &[u8]) -> Result<Vec<u8>> {
    let backend = |e: std::io::Error| CoreError::Backend(format!("zstd encode: {e}"));
    ZSTD_COMPRESSOR.with(|cell| {
        let mut slot = cell.borrow_mut();
        let compressor = match slot.as_mut() {
            Some(c) => c,
            None => slot.insert(zstd::bulk::Compressor::new(ZSTD_LEVEL).map_err(backend)?),
        };
        compressor.compress(bytes).map_err(backend)
    })
}

/// The content of one zstd frame; `None` when `bytes` are not one.
#[cfg(feature = "wal-zstd")]
fn zstd_decompress(bytes: &[u8]) -> Option<Vec<u8>> {
    let size = zstd::zstd_safe::get_frame_content_size(bytes).ok()??;
    let size = usize::try_from(size)
        .ok()
        .filter(|s| *s <= ZSTD_MAX_RECORD)?;
    ZSTD_DECOMPRESSOR.with(|cell| {
        let mut slot = cell.borrow_mut();
        let decompressor = match slot.as_mut() {
            Some(d) => d,
            None => slot.insert(zstd::bulk::Decompressor::new().ok()?),
        };
        decompressor.decompress(bytes, size).ok()
    })
}

/// The header version of an encrypted log: the tag names the encoding of
/// the records inside the encryption (JSON included), every record is
/// sealed, and the first one is the key check.
const SEALED_VERSION: u8 = 2;
/// Authenticated context of every sealed record, so a sealed sidecar can
/// never pass for a log record.
#[cfg_attr(
    any(not(feature = "encryption"), target_arch = "wasm32"),
    allow(dead_code)
)]
const RECORD_CONTEXT: &[u8] = b"drevo wal record";
/// Context and text of the key-check record that opens an encrypted log: a
/// wrong key fails on it before any record is read.
#[cfg_attr(
    any(not(feature = "encryption"), target_arch = "wasm32"),
    allow(dead_code)
)]
const CHECK_CONTEXT: &[u8] = b"drevo wal key check";
#[cfg_attr(
    any(not(feature = "encryption"), target_arch = "wasm32"),
    allow(dead_code)
)]
const CHECK_TEXT: &[u8] = b"drevo";

/// The key an encrypted log is sealed with ([`crate::encryption::Key`]).
#[cfg(feature = "encryption")]
pub type SealKey = crate::encryption::Key;

/// The key an encrypted log is sealed with. Without the `encryption`
/// feature there is none, so no log is ever sealed.
#[cfg(not(feature = "encryption"))]
#[derive(Debug)]
pub enum SealKey {}

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
    /// Framed CBOR records, each compressed with zstd, behind the `wal-zstd`
    /// feature.
    CborZstd,
    /// Framed CBOR records whose built-in field names (`UpsertNode`, `id`,
    /// `kind`, …) are written as small integers, behind the `wal-cbor`
    /// feature. Property names stay strings.
    CborCompact,
    /// [`WalFormat::CborCompact`] records, each compressed with zstd, behind
    /// the `wal-zstd` feature.
    CborCompactZstd,
}

/// Why a WAL format name was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WalFormatError {
    /// Not a known format name.
    #[error(
        "unknown WAL format `{0}` (known: json, cbor, msgpack, cbor-zstd, cbor-compact, cbor-compact-zstd)"
    )]
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
            WalFormat::CborZstd => "cbor-zstd",
            WalFormat::CborCompact => "cbor-compact",
            WalFormat::CborCompactZstd => "cbor-compact-zstd",
        }
    }

    /// Whether this build can read and write the format.
    #[must_use]
    pub fn compiled_in(self) -> bool {
        match self {
            WalFormat::Json => true,
            WalFormat::Cbor => cfg!(feature = "wal-cbor"),
            WalFormat::MsgPack => cfg!(feature = "wal-msgpack"),
            WalFormat::CborZstd => cfg!(feature = "wal-zstd"),
            WalFormat::CborCompact => cfg!(feature = "wal-cbor"),
            WalFormat::CborCompactZstd => cfg!(feature = "wal-zstd"),
        }
    }

    fn tag(self) -> u8 {
        match self {
            WalFormat::Json => b'J',
            WalFormat::Cbor => b'C',
            WalFormat::MsgPack => b'M',
            WalFormat::CborZstd => b'Z',
            WalFormat::CborCompact => b'K',
            WalFormat::CborCompactZstd => b'X',
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
        let payload = encode_ops(self, ops)?;
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
        let format = [
            WalFormat::Json,
            WalFormat::Cbor,
            WalFormat::MsgPack,
            WalFormat::CborZstd,
            WalFormat::CborCompact,
            WalFormat::CborCompactZstd,
        ]
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
        /// The log's format (for an encrypted log, the encoding inside the
        /// encryption).
        format: WalFormat,
        /// Where the first record starts (after the header, if any).
        body_start: usize,
        /// Whether the records are encrypted.
        sealed: bool,
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
            sealed: false,
        });
    }
    if &bytes[..MAGIC.len()] != MAGIC {
        return Ok(Detected::Log {
            format: WalFormat::Json,
            body_start: 0,
            sealed: false,
        });
    }
    let (version, tag) = (bytes[MAGIC.len()], bytes[MAGIC.len() + 1]);
    let sealed = match version {
        HEADER_VERSION => false,
        SEALED_VERSION => true,
        _ => return Err(invalid(format!("unsupported WAL header version {version}"))),
    };
    let format = match tag {
        b'J' if sealed => WalFormat::Json,
        b'C' => WalFormat::Cbor,
        b'M' => WalFormat::MsgPack,
        b'Z' => WalFormat::CborZstd,
        b'K' => WalFormat::CborCompact,
        b'X' => WalFormat::CborCompactZstd,
        _ => return Err(invalid(format!("unknown WAL format tag {tag:#04x}"))),
    };
    if !format.compiled_in() {
        return Err(invalid(format!(
            "this write-ahead log is in {format} format, which this build was compiled without"
        )));
    }
    Ok(Detected::Log {
        format,
        body_start: HEADER_LEN,
        sealed,
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
    /// An encrypted record whose checksum holds but which does not decrypt:
    /// a wrong key or a changed log — never a torn tail.
    Undecryptable {
        /// Byte offset just past the record.
        next: usize,
    },
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

/// A record payload framed as `length · CRC-32 · payload`.
#[cfg_attr(
    any(not(feature = "encryption"), target_arch = "wasm32"),
    allow(dead_code)
)]
fn frame(payload: &[u8]) -> Result<Vec<u8>> {
    let len = u32::try_from(payload.len())
        .map_err(|_| CoreError::Backend("WAL record over 4 GiB".into()))?;
    let mut out = Vec::with_capacity(FRAME_LEN + payload.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&crc32(payload).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// The frame at `pos`: `Ok((payload, next))`, or the [`Record`] saying why
/// there is none.
#[cfg_attr(
    any(not(feature = "encryption"), target_arch = "wasm32"),
    allow(dead_code)
)]
fn read_frame(bytes: &[u8], pos: usize) -> std::result::Result<(&[u8], usize), Record> {
    let rest = &bytes[pos..];
    if rest.len() < FRAME_LEN {
        return Err(Record::Incomplete);
    }
    let len = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
    let crc = u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]);
    let Some(end) = FRAME_LEN.checked_add(len).filter(|e| *e <= rest.len()) else {
        return Err(Record::Incomplete);
    };
    let payload = &rest[FRAME_LEN..end];
    if crc32(payload) != crc {
        return Err(Record::Invalid { next: pos + end });
    }
    Ok((payload, pos + end))
}

/// The encoding of `ops` in `format`, before framing or encryption.
#[cfg_attr(
    any(not(feature = "encryption"), target_arch = "wasm32"),
    allow(dead_code)
)]
fn record_payload(format: WalFormat, ops: &[WalOp]) -> Result<Vec<u8>> {
    if format == WalFormat::Json {
        return Ok(serde_json::to_vec(ops)?);
    }
    encode_ops(format, ops)
}

/// The payload of a binary record, before framing or encryption. The plain
/// CBOR and MessagePack forms are written straight from the ops; the compact
/// forms go through a `serde_json::Value` tree, which their key transform
/// works on.
fn encode_ops(format: WalFormat, ops: &[WalOp]) -> Result<Vec<u8>> {
    match format {
        #[cfg(feature = "wal-cbor")]
        WalFormat::Cbor => {
            let mut out = Vec::new();
            ciborium::into_writer(&wire::borrow(ops), &mut out)
                .map_err(|e| CoreError::Backend(format!("CBOR encode: {e}")))?;
            Ok(out)
        }
        #[cfg(feature = "wal-zstd")]
        WalFormat::CborZstd => zstd_compress(&encode_ops(WalFormat::Cbor, ops)?),
        #[cfg(feature = "wal-msgpack")]
        WalFormat::MsgPack => rmp_serde::to_vec_named(&wire::borrow(ops))
            .map_err(|e| CoreError::Backend(format!("MessagePack encode: {e}"))),
        _ => {
            // Human-readable, so property maps keep their map form.
            let value = serde_json::to_value(ops)?;
            encode_value(format, &value)
        }
    }
}

#[cfg(feature = "encryption")]
#[cfg_attr(
    any(not(feature = "encryption"), target_arch = "wasm32"),
    allow(dead_code)
)]
fn seal_with(key: &SealKey, context: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    crate::encryption::seal(key, context, plaintext).map_err(|e| CoreError::Backend(e.to_string()))
}

#[cfg(not(feature = "encryption"))]
#[cfg_attr(
    any(not(feature = "encryption"), target_arch = "wasm32"),
    allow(dead_code)
)]
fn seal_with(key: &SealKey, _context: &[u8], _plaintext: &[u8]) -> Result<Vec<u8>> {
    match *key {}
}

#[cfg(feature = "encryption")]
#[cfg_attr(
    any(not(feature = "encryption"), target_arch = "wasm32"),
    allow(dead_code)
)]
fn open_with(key: &SealKey, context: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
    crate::encryption::open(key, context, sealed).ok()
}

#[cfg(not(feature = "encryption"))]
#[cfg_attr(
    any(not(feature = "encryption"), target_arch = "wasm32"),
    allow(dead_code)
)]
fn open_with(key: &SealKey, _context: &[u8], _sealed: &[u8]) -> Option<Vec<u8>> {
    match *key {}
}

/// The bytes a new log in `format` starts with: its header, and with `key`
/// the encrypted header followed by the key-check record.
///
/// # Errors
/// [`CoreError::Backend`] when no nonce can be drawn.
#[cfg_attr(
    any(not(feature = "encryption"), target_arch = "wasm32"),
    allow(dead_code)
)]
pub(crate) fn log_header(format: WalFormat, key: Option<&SealKey>) -> Result<Vec<u8>> {
    let Some(key) = key else {
        return Ok(format.header());
    };
    let mut header = MAGIC.to_vec();
    header.push(SEALED_VERSION);
    header.push(format.tag());
    header.extend(frame(&seal_with(key, CHECK_CONTEXT, CHECK_TEXT)?)?);
    Ok(header)
}

/// Encode one record holding `ops` — sealed with `key` when there is one.
///
/// # Errors
/// As [`WalFormat::encode_record`], and [`CoreError::Backend`] when no
/// nonce can be drawn.
#[cfg_attr(
    any(not(feature = "encryption"), target_arch = "wasm32"),
    allow(dead_code)
)]
pub(crate) fn encode_record_with(
    format: WalFormat,
    key: Option<&SealKey>,
    ops: &[WalOp],
) -> Result<Vec<u8>> {
    match key {
        None => format.encode_record(ops),
        Some(_) if ops.is_empty() => Ok(Vec::new()),
        Some(key) => frame(&seal_with(
            key,
            RECORD_CONTEXT,
            &record_payload(format, ops)?,
        )?),
    }
}

/// Read the record at `pos` of an encrypted `format` log.
#[must_use]
#[cfg_attr(
    any(not(feature = "encryption"), target_arch = "wasm32"),
    allow(dead_code)
)]
pub(crate) fn read_sealed_record(
    format: WalFormat,
    key: &SealKey,
    bytes: &[u8],
    pos: usize,
) -> Record {
    let (sealed, next) = match read_frame(bytes, pos) {
        Ok(found) => found,
        Err(record) => return record,
    };
    let Some(plain) = open_with(key, RECORD_CONTEXT, sealed) else {
        return Record::Undecryptable { next };
    };
    // JSON straight into ops, as a plain log's lines are read.
    let ops = match format {
        WalFormat::Json => serde_json::from_slice::<Vec<WalOp>>(&plain).ok(),
        _ => decode_ops(format, &plain),
    };
    match ops {
        Some(ops) => Record::Ops { ops, next },
        None => Record::Invalid { next },
    }
}

/// Read the record at `pos` of a log: sealed with `key` when `sealed`.
#[cfg_attr(
    any(not(feature = "encryption"), target_arch = "wasm32"),
    allow(dead_code)
)]
fn read_any(format: WalFormat, sealed: Option<&SealKey>, bytes: &[u8], pos: usize) -> Record {
    match sealed {
        Some(key) => read_sealed_record(format, key, bytes, pos),
        None => read_record(format, bytes, pos),
    }
}

/// Decode a record [`encode_record_with`] produced (a single record).
///
/// # Errors
/// [`CoreError::Io`] when the bytes are not one valid record.
#[cfg_attr(
    any(not(feature = "encryption"), target_arch = "wasm32"),
    allow(dead_code)
)]
pub(crate) fn decode_record_with(
    format: WalFormat,
    key: Option<&SealKey>,
    bytes: &[u8],
) -> Result<Vec<WalOp>> {
    match read_any(format, key, bytes, 0) {
        Record::Ops { ops, .. } => Ok(ops),
        _ => Err(invalid(
            "a queued write-ahead-log record does not decode".to_string(),
        )),
    }
}

/// What [`replay`] found in a log.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Replayed {
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
        /// Whether the log is encrypted.
        sealed: bool,
    },
}

/// The bytes [`replay`] reads at a time.
#[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn replay<R: std::io::Read>(
    mut reader: R,
    chunk: usize,
    path: &std::path::Path,
    key: Option<&SealKey>,
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
    let (format, body_start, sealed) = match detect(&buf)? {
        Detected::TornHeader => return Ok(Replayed::TornHeader),
        Detected::Log { .. } if buf.is_empty() => return Ok(Replayed::Empty),
        Detected::Log {
            format,
            body_start,
            sealed,
        } => (format, body_start, sealed),
    };
    let refuse = |message: String| -> CoreError {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: {message}", path.display()),
        )
        .into()
    };
    // The key a sealed log is read with; a sealed log needs one, and a key
    // never opens a log that is not sealed.
    let seal: Option<&SealKey> = match (sealed, key) {
        (true, Some(key)) => Some(key),
        (true, None) => {
            return Err(refuse(
                "the write-ahead log is encrypted; open it with its encryption key".to_string(),
            ))
        }
        (false, Some(_)) => {
            return Err(refuse(
                "the write-ahead log is not encrypted; encrypt the store first, or open it \
                 without a key"
                    .to_string(),
            ))
        }
        (false, None) => None,
    };
    let mut pos = body_start;
    if let Some(key) = seal {
        // The key check: a crash while the log was being created leaves it
        // incomplete (nothing was ever acknowledged); a wrong key fails it.
        while buf.len() < pos + FRAME_LEN && fill(&mut buf, &mut eof)? {}
        loop {
            match read_frame(&buf, pos) {
                Err(Record::Incomplete) if !eof => {
                    fill(&mut buf, &mut eof)?;
                }
                Err(Record::Incomplete) => return Ok(Replayed::TornHeader),
                Err(_) => return Err(refuse("the encryption key check is damaged".to_string())),
                Ok((check, next)) => {
                    if open_with(key, CHECK_CONTEXT, check).as_deref() != Some(CHECK_TEXT) {
                        return Err(refuse(
                            "wrong encryption key for this write-ahead log".to_string(),
                        ));
                    }
                    pos = next;
                    break;
                }
            }
        }
    }
    let mut valid_end = pos as u64;
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
                    sealed,
                });
            }
            continue;
        }
        // A JSON record ends at a newline: without one in the window, the
        // record may continue in the next chunk.
        if seal.is_none() && format == WalFormat::Json && !eof && !buf[pos..].contains(&b'\n') {
            buf.drain(..pos);
            base += pos as u64;
            pos = 0;
            fill(&mut buf, &mut eof)?;
            continue;
        }
        match read_any(format, seal, &buf, pos) {
            Record::Ops { ops, next } => {
                for op in ops {
                    apply(op);
                }
                valid_end = base + next as u64;
                pos = next;
            }
            Record::Undecryptable { .. } => {
                return Err(refuse(format!(
                    "the encrypted record at byte {} does not decrypt: the key is wrong or \
                     the log was changed",
                    base + pos as u64
                )))
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
                    sealed,
                })
            }
            Record::Invalid { next } => {
                // A torn tail only if nothing follows: look at the rest of the
                // window, then at the rest of the log.
                let follows = |bytes: &[u8]| match format {
                    WalFormat::Json if seal.is_none() => !bytes.iter().all(u8::is_ascii_whitespace),
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
                        sealed,
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
        // Straight into the ops: no `serde_json::Value` tree in between.
        #[cfg(feature = "wal-cbor")]
        WalFormat::Cbor => return ciborium::from_reader(payload).ok().map(wire::own),
        #[cfg(feature = "wal-msgpack")]
        WalFormat::MsgPack => return rmp_serde::from_slice(payload).ok().map(wire::own),
        #[cfg(feature = "wal-zstd")]
        WalFormat::CborZstd => {
            return zstd_decompress(payload)
                .and_then(|cbor| ciborium::from_reader(cbor.as_slice()).ok())
                .map(wire::own)
        }
        #[cfg(feature = "wal-cbor")]
        WalFormat::CborCompact => compact::decode(payload),
        #[cfg(feature = "wal-zstd")]
        WalFormat::CborCompactZstd => {
            zstd_decompress(payload).and_then(|cbor| compact::decode(&cbor))
        }
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
        #[cfg(feature = "wal-zstd")]
        WalFormat::CborZstd => zstd_compress(&encode_value(WalFormat::Cbor, value)?),
        #[cfg(feature = "wal-cbor")]
        WalFormat::CborCompact => compact::encode(value),
        #[cfg(feature = "wal-zstd")]
        WalFormat::CborCompactZstd => zstd_compress(&compact::encode(value)?),
        #[allow(unreachable_patterns)]
        other => {
            let _ = value;
            Err(CoreError::Backend(format!(
                "WAL format {other} is not compiled in"
            )))
        }
    }
}

/// The shape of a record for the binary formats, read and written without
/// the `serde_json::Value` tree the generic path builds: the same field
/// names and variants as [`WalOp`], with a property map that is a map
/// under every serializer. ([`crate::model::Properties`] writes JSON text
/// under a serializer that is not human-readable, for bincode; CBOR and
/// MessagePack report themselves as such, so reading a `WalOp` straight
/// from them would take that branch.)
#[cfg(any(feature = "wal-cbor", feature = "wal-msgpack"))]
mod wire {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Serialize};

    use crate::model::{Edge, Node, Properties};
    use crate::native::WalOp;

    /// A record's ops, borrowed for writing.
    pub(super) fn borrow(ops: &[WalOp]) -> Vec<OpRef<'_>> {
        ops.iter().map(OpRef::from).collect()
    }

    /// A record's ops, read.
    pub(super) fn own(ops: Vec<Op>) -> Vec<WalOp> {
        ops.into_iter().map(WalOp::from).collect()
    }

    #[derive(Serialize)]
    pub(super) enum OpRef<'a> {
        UpsertNode(NodeRef<'a>),
        DeleteNode(u64),
        UpsertEdge(EdgeRef<'a>),
        DeleteEdge(u64),
        SetEmbedding(u64, &'a [f32]),
        DeleteEmbedding(u64),
        Prepare {
            gid: &'a str,
            prepared_at_ms: i64,
            ops: Vec<OpRef<'a>>,
        },
        CommitPrepared {
            gid: &'a str,
        },
        RollbackPrepared {
            gid: &'a str,
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            heuristic: bool,
        },
    }

    impl<'a> From<&'a WalOp> for OpRef<'a> {
        fn from(op: &'a WalOp) -> Self {
            match op {
                WalOp::UpsertNode(n) => OpRef::UpsertNode(NodeRef {
                    id: n.id,
                    uuid: &n.uuid,
                    kind: &n.kind,
                    title: &n.title,
                    body: &n.body,
                    body_html: &n.body_html,
                    created_at: n.created_at,
                    updated_at: n.updated_at,
                    properties: PropertiesRef(&n.properties),
                }),
                WalOp::DeleteNode(id) => OpRef::DeleteNode(*id),
                WalOp::UpsertEdge(e) => OpRef::UpsertEdge(EdgeRef {
                    id: e.id,
                    uuid: &e.uuid,
                    from_id: e.from_id,
                    to_id: e.to_id,
                    kind: &e.kind,
                    weight: e.weight,
                    created_at: e.created_at,
                    properties: PropertiesRef(&e.properties),
                }),
                WalOp::DeleteEdge(id) => OpRef::DeleteEdge(*id),
                WalOp::SetEmbedding(id, v) => OpRef::SetEmbedding(*id, v),
                WalOp::DeleteEmbedding(id) => OpRef::DeleteEmbedding(*id),
                WalOp::Prepare {
                    gid,
                    prepared_at_ms,
                    ops,
                } => OpRef::Prepare {
                    gid,
                    prepared_at_ms: *prepared_at_ms,
                    ops: borrow(ops),
                },
                WalOp::CommitPrepared { gid } => OpRef::CommitPrepared { gid },
                WalOp::RollbackPrepared { gid, heuristic } => OpRef::RollbackPrepared {
                    gid,
                    heuristic: *heuristic,
                },
            }
        }
    }

    #[derive(Serialize)]
    pub(super) struct NodeRef<'a> {
        id: u64,
        uuid: &'a [u8; 16],
        kind: &'a str,
        title: &'a str,
        body: &'a str,
        body_html: &'a str,
        created_at: i64,
        updated_at: i64,
        properties: PropertiesRef<'a>,
    }

    #[derive(Serialize)]
    pub(super) struct EdgeRef<'a> {
        id: u64,
        uuid: &'a [u8; 16],
        from_id: u64,
        to_id: u64,
        kind: &'a str,
        weight: f32,
        created_at: i64,
        properties: PropertiesRef<'a>,
    }

    /// A property map written as a map, whatever the serializer says of
    /// itself.
    pub(super) struct PropertiesRef<'a>(&'a Properties);

    impl Serialize for PropertiesRef<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.collect_map(self.0.iter())
        }
    }

    #[derive(Deserialize)]
    pub(super) enum Op {
        UpsertNode(NodeWire),
        DeleteNode(u64),
        UpsertEdge(EdgeWire),
        DeleteEdge(u64),
        SetEmbedding(u64, Vec<f32>),
        DeleteEmbedding(u64),
        Prepare {
            gid: String,
            prepared_at_ms: i64,
            ops: Vec<Op>,
        },
        CommitPrepared {
            gid: String,
        },
        RollbackPrepared {
            gid: String,
            #[serde(default)]
            heuristic: bool,
        },
    }

    impl From<Op> for WalOp {
        fn from(op: Op) -> Self {
            match op {
                Op::UpsertNode(n) => WalOp::UpsertNode(Node {
                    id: n.id,
                    uuid: n.uuid,
                    kind: n.kind,
                    title: n.title,
                    body: n.body,
                    body_html: n.body_html,
                    created_at: n.created_at,
                    updated_at: n.updated_at,
                    properties: Properties::from(n.properties),
                }),
                Op::DeleteNode(id) => WalOp::DeleteNode(id),
                Op::UpsertEdge(e) => WalOp::UpsertEdge(Edge {
                    id: e.id,
                    uuid: e.uuid,
                    from_id: e.from_id,
                    to_id: e.to_id,
                    kind: e.kind,
                    weight: e.weight,
                    created_at: e.created_at,
                    properties: Properties::from(e.properties),
                }),
                Op::DeleteEdge(id) => WalOp::DeleteEdge(id),
                Op::SetEmbedding(id, v) => WalOp::SetEmbedding(id, v),
                Op::DeleteEmbedding(id) => WalOp::DeleteEmbedding(id),
                Op::Prepare {
                    gid,
                    prepared_at_ms,
                    ops,
                } => WalOp::Prepare {
                    gid,
                    prepared_at_ms,
                    ops: own(ops),
                },
                Op::CommitPrepared { gid } => WalOp::CommitPrepared { gid },
                Op::RollbackPrepared { gid, heuristic } => {
                    WalOp::RollbackPrepared { gid, heuristic }
                }
            }
        }
    }

    #[derive(Deserialize)]
    pub(super) struct NodeWire {
        id: u64,
        uuid: [u8; 16],
        kind: String,
        title: String,
        body: String,
        body_html: String,
        created_at: i64,
        updated_at: i64,
        properties: BTreeMap<String, serde_json::Value>,
    }

    #[derive(Deserialize)]
    pub(super) struct EdgeWire {
        id: u64,
        uuid: [u8; 16],
        from_id: u64,
        to_id: u64,
        kind: String,
        weight: f32,
        created_at: i64,
        properties: BTreeMap<String, serde_json::Value>,
    }
}

/// The `cbor-compact` record encoding: CBOR in which the built-in field
/// names of a record — the [`WalOp`] variants and the [`Node`] / [`Edge`]
/// fields — are small integer map keys, and property names stay text. A
/// road-network node of three numeric properties is mostly field names as
/// plain CBOR; here it is a third smaller.
///
/// The transform follows the shape of a record rather than renaming every
/// key, so a property named `id` or `kind` is left alone; a field name this
/// build does not know is kept as text, so a record from a newer build still
/// decodes.
///
/// [`Node`]: crate::model::Node
/// [`Edge`]: crate::model::Edge
#[cfg(feature = "wal-cbor")]
mod compact {
    use ciborium::Value as Cbor;
    use serde_json::Value as Json;

    use crate::error::{CoreError, Result};

    /// The field names a compact record abbreviates, in code order. Append
    /// only: a code is part of the on-disk format.
    const FIELD_NAMES: [&str; 25] = [
        "UpsertNode",
        "DeleteNode",
        "UpsertEdge",
        "DeleteEdge",
        "SetEmbedding",
        "DeleteEmbedding",
        "Prepare",
        "CommitPrepared",
        "RollbackPrepared",
        "id",
        "uuid",
        "kind",
        "title",
        "body",
        "body_html",
        "created_at",
        "updated_at",
        "properties",
        "from_id",
        "to_id",
        "weight",
        "gid",
        "prepared_at_ms",
        "ops",
        "heuristic",
    ];

    fn key(name: &str) -> Cbor {
        match FIELD_NAMES.iter().position(|n| *n == name) {
            Some(code) => Cbor::Integer(code.into()),
            None => Cbor::Text(name.to_string()),
        }
    }

    fn name(key: Cbor) -> Option<String> {
        match key {
            Cbor::Integer(code) => {
                let code: usize = code.try_into().ok()?;
                FIELD_NAMES.get(code).map(|n| (*n).to_string())
            }
            Cbor::Text(name) => Some(name),
            _ => None,
        }
    }

    /// The ops of a record, with their field names abbreviated.
    pub(super) fn encode(record: &Json) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        ciborium::into_writer(&ops(record), &mut out)
            .map_err(|e| CoreError::Backend(format!("CBOR encode: {e}")))?;
        Ok(out)
    }

    /// The ops of a compact record; `None` when the bytes are not one.
    pub(super) fn decode(payload: &[u8]) -> Option<Json> {
        let value: Cbor = ciborium::from_reader(payload).ok()?;
        unops(value)
    }

    fn ops(value: &Json) -> Cbor {
        match value {
            Json::Array(items) => Cbor::Array(items.iter().map(op).collect()),
            other => plain(other),
        }
    }

    /// One op: `{Variant: payload}`.
    fn op(value: &Json) -> Cbor {
        let Json::Object(map) = value else {
            return plain(value);
        };
        Cbor::Map(
            map.iter()
                .map(|(variant, payload)| (key(variant), op_payload(variant, payload)))
                .collect(),
        )
    }

    fn op_payload(variant: &str, payload: &Json) -> Cbor {
        match (variant, payload) {
            (
                "UpsertNode" | "UpsertEdge" | "Prepare" | "CommitPrepared" | "RollbackPrepared",
                Json::Object(fields),
            ) => Cbor::Map(
                fields
                    .iter()
                    .map(|(field, v)| {
                        let v = match field.as_str() {
                            "ops" => ops(v),
                            _ => plain(v),
                        };
                        (key(field), v)
                    })
                    .collect(),
            ),
            _ => plain(payload),
        }
    }

    /// A value as it is, text keys included.
    fn plain(value: &Json) -> Cbor {
        match value {
            Json::Null => Cbor::Null,
            Json::Bool(b) => Cbor::Bool(*b),
            Json::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Cbor::Integer(i.into())
                } else if let Some(u) = n.as_u64() {
                    Cbor::Integer(u.into())
                } else {
                    Cbor::Float(n.as_f64().unwrap_or(f64::NAN))
                }
            }
            Json::String(s) => Cbor::Text(s.clone()),
            Json::Array(items) => Cbor::Array(items.iter().map(plain).collect()),
            Json::Object(map) => Cbor::Map(
                map.iter()
                    .map(|(k, v)| (Cbor::Text(k.clone()), plain(v)))
                    .collect(),
            ),
        }
    }

    fn unops(value: Cbor) -> Option<Json> {
        match value {
            Cbor::Array(items) => Some(Json::Array(
                items.into_iter().map(unop).collect::<Option<_>>()?,
            )),
            other => unplain(other),
        }
    }

    fn unop(value: Cbor) -> Option<Json> {
        let Cbor::Map(entries) = value else {
            return unplain(value);
        };
        let mut map = serde_json::Map::with_capacity(entries.len());
        for (k, payload) in entries {
            let variant = name(k)?;
            let payload = unop_payload(&variant, payload)?;
            map.insert(variant, payload);
        }
        Some(Json::Object(map))
    }

    fn unop_payload(variant: &str, payload: Cbor) -> Option<Json> {
        match (variant, payload) {
            (
                "UpsertNode" | "UpsertEdge" | "Prepare" | "CommitPrepared" | "RollbackPrepared",
                Cbor::Map(entries),
            ) => {
                let mut map = serde_json::Map::with_capacity(entries.len());
                for (k, v) in entries {
                    let field = name(k)?;
                    let v = match field.as_str() {
                        "ops" => unops(v)?,
                        _ => unplain(v)?,
                    };
                    map.insert(field, v);
                }
                Some(Json::Object(map))
            }
            (_, payload) => unplain(payload),
        }
    }

    fn unplain(value: Cbor) -> Option<Json> {
        Some(match value {
            Cbor::Null => Json::Null,
            Cbor::Bool(b) => Json::Bool(b),
            Cbor::Integer(i) => {
                if let Ok(i) = i64::try_from(i) {
                    Json::from(i)
                } else {
                    Json::from(u64::try_from(i).ok()?)
                }
            }
            Cbor::Float(f) => Json::Number(serde_json::Number::from_f64(f)?),
            Cbor::Text(s) => Json::String(s),
            Cbor::Array(items) => {
                Json::Array(items.into_iter().map(unplain).collect::<Option<_>>()?)
            }
            Cbor::Map(entries) => {
                let mut map = serde_json::Map::with_capacity(entries.len());
                for (k, v) in entries {
                    let Cbor::Text(k) = k else {
                        return None;
                    };
                    map.insert(k, unplain(v)?);
                }
                Json::Object(map)
            }
            _ => return None,
        })
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
            None,
            |o| ops.push(o),
        )?;
        Ok((r, ops))
    }

    fn formats() -> Vec<WalFormat> {
        [
            WalFormat::Json,
            WalFormat::Cbor,
            WalFormat::MsgPack,
            WalFormat::CborZstd,
            WalFormat::CborCompact,
            WalFormat::CborCompactZstd,
        ]
        .into_iter()
        .filter(|f| f.compiled_in())
        .collect()
    }

    /// One of every op, with the field names a compact record abbreviates
    /// and property names that look like them.
    #[cfg(feature = "wal-cbor")]
    fn every_op() -> Vec<WalOp> {
        use crate::model::{Edge, Node, Properties};
        let props = Properties::from(std::collections::HashMap::from([
            ("id".to_string(), serde_json::json!("not the node id")),
            (
                "properties".to_string(),
                serde_json::json!({"kind": [1, 2.5]}),
            ),
            ("lat".to_string(), serde_json::json!(42.6668746)),
        ]));
        let node = Node {
            id: 7,
            uuid: [1; 16],
            kind: "Junction".into(),
            title: "t".into(),
            body: "b".into(),
            body_html: String::new(),
            created_at: 1_792_353_672_521,
            updated_at: 1_792_353_672_522,
            properties: props.clone(),
        };
        let edge = Edge {
            id: 9,
            uuid: [2; 16],
            from_id: 7,
            to_id: 8,
            kind: "ROAD".into(),
            weight: 0.5,
            created_at: 1_792_353_672_523,
            properties: props,
        };
        vec![
            WalOp::UpsertNode(node.clone()),
            WalOp::DeleteNode(7),
            WalOp::UpsertEdge(edge.clone()),
            WalOp::DeleteEdge(9),
            WalOp::SetEmbedding(7, vec![0.25, -1.0e-3, 3.0]),
            WalOp::DeleteEmbedding(7),
            WalOp::Prepare {
                gid: "gid-1".into(),
                prepared_at_ms: 5,
                ops: vec![WalOp::UpsertNode(node), WalOp::UpsertEdge(edge)],
            },
            WalOp::CommitPrepared {
                gid: "gid-1".into(),
            },
            WalOp::RollbackPrepared {
                gid: "gid-2".into(),
                heuristic: true,
            },
            WalOp::RollbackPrepared {
                gid: "gid-3".into(),
                heuristic: false,
            },
        ]
    }

    /// Records of `every_op()` as drevo 0.0.59 wrote them.
    #[cfg(all(feature = "wal-cbor", feature = "wal-msgpack"))]
    const FIXTURE_0_0_59: [(WalFormat, &str); 2] = [
        (WalFormat::Cbor, "86030000a0c3ccab8aa16a5570736572744e6f6465a964626f6479616269626f64795f68746d6c606a637265617465645f61741b000001a1509a9d4962696407646b696e64684a756e6374696f6e6a70726f70657274696573a36269646f6e6f7420746865206e6f6465206964636c6174fb4045555c259ac43c6a70726f70657274696573a1646b696e648201f94100657469746c6561746a757064617465645f61741b000001a1509a9d4a64757569649001010101010101010101010101010101a16a44656c6574654e6f646507a16a55707365727445646765a86a637265617465645f61741b000001a1509a9d4b6766726f6d5f69640762696409646b696e6464524f41446a70726f70657274696573a36269646f6e6f7420746865206e6f6465206964636c6174fb4045555c259ac43c6a70726f70657274696573a1646b696e648201f9410065746f5f6964086475756964900202020202020202020202020202020266776569676874f93800a16a44656c6574654564676509a16c536574456d62656464696e67820783f93400faba83126ff94200a16f44656c657465456d62656464696e6707a16750726570617265a363676964656769642d31636f707382a16a5570736572744e6f6465a964626f6479616269626f64795f68746d6c606a637265617465645f61741b000001a1509a9d4962696407646b696e64684a756e6374696f6e6a70726f70657274696573a36269646f6e6f7420746865206e6f6465206964636c6174fb4045555c259ac43c6a70726f70657274696573a1646b696e648201f94100657469746c6561746a757064617465645f61741b000001a1509a9d4a64757569649001010101010101010101010101010101a16a55707365727445646765a86a637265617465645f61741b000001a1509a9d4b6766726f6d5f69640762696409646b696e6464524f41446a70726f70657274696573a36269646f6e6f7420746865206e6f6465206964636c6174fb4045555c259ac43c6a70726f70657274696573a1646b696e648201f9410065746f5f6964086475756964900202020202020202020202020202020266776569676874f938006e70726570617265645f61745f6d7305a16e436f6d6d69745072657061726564a163676964656769642d31a170526f6c6c6261636b5072657061726564a263676964656769642d3269686575726973746963f5a170526f6c6c6261636b5072657061726564a163676964656769642d33"),
        (WalFormat::MsgPack, "c20300000b00a9c69a81aa5570736572744e6f646589a4626f6479a162a9626f64795f68746d6ca0aa637265617465645f6174cf000001a1509a9d49a2696407a46b696e64a84a756e6374696f6eaa70726f7065727469657383a26964af6e6f7420746865206e6f6465206964a36c6174cb4045555c259ac43caa70726f7065727469657381a46b696e649201cb4004000000000000a57469746c65a174aa757064617465645f6174cf000001a1509a9d4aa475756964dc00100101010101010101010101010101010181aa44656c6574654e6f64650781aa5570736572744564676588aa637265617465645f6174cf000001a1509a9d4ba766726f6d5f696407a2696409a46b696e64a4524f4144aa70726f7065727469657383a26964af6e6f7420746865206e6f6465206964a36c6174cb4045555c259ac43caa70726f7065727469657381a46b696e649201cb4004000000000000a5746f5f696408a475756964dc001002020202020202020202020202020202a6776569676874cb3fe000000000000081aa44656c657465456467650981ac536574456d62656464696e67920793cb3fd0000000000000cbbf50624de0000000cb400800000000000081af44656c657465456d62656464696e670781a75072657061726583a3676964a56769642d31a36f70739281aa5570736572744e6f646589a4626f6479a162a9626f64795f68746d6ca0aa637265617465645f6174cf000001a1509a9d49a2696407a46b696e64a84a756e6374696f6eaa70726f7065727469657383a26964af6e6f7420746865206e6f6465206964a36c6174cb4045555c259ac43caa70726f7065727469657381a46b696e649201cb4004000000000000a57469746c65a174aa757064617465645f6174cf000001a1509a9d4aa475756964dc00100101010101010101010101010101010181aa5570736572744564676588aa637265617465645f6174cf000001a1509a9d4ba766726f6d5f696407a2696409a46b696e64a4524f4144aa70726f7065727469657383a26964af6e6f7420746865206e6f6465206964a36c6174cb4045555c259ac43caa70726f7065727469657381a46b696e649201cb4004000000000000a5746f5f696408a475756964dc001002020202020202020202020202020202a6776569676874cb3fe0000000000000ae70726570617265645f61745f6d730581ae436f6d6d6974507265706172656481a3676964a56769642d3181b0526f6c6c6261636b507265706172656482a3676964a56769642d32a9686575726973746963c381b0526f6c6c6261636b507265706172656481a3676964a56769642d33"),
    ];

    #[cfg(all(feature = "wal-cbor", feature = "wal-msgpack"))]
    fn unhex(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    #[cfg(all(feature = "wal-cbor", feature = "wal-msgpack"))]
    #[test]
    fn records_written_by_0_0_59_still_read() {
        for (format, hex) in FIXTURE_0_0_59 {
            let bytes = unhex(hex);
            match read_record(format, &bytes, 0) {
                Record::Ops { ops, next } => {
                    assert_eq!(ops, every_op(), "{format}");
                    assert_eq!(next, bytes.len());
                }
                other => panic!("{format}: {other:?}"),
            }
        }
    }

    /// What a build before the direct path does with a record: through a
    /// `serde_json::Value` tree. A record written now must read that way
    /// too, so older servers open a newer log.
    #[cfg(all(feature = "wal-cbor", feature = "wal-msgpack"))]
    #[test]
    fn records_written_now_read_through_the_value_tree() {
        let ops = every_op();
        let cbor = WalFormat::Cbor.encode_record(&ops).unwrap();
        let value: serde_json::Value = ciborium::from_reader(&cbor[FRAME_LEN..]).unwrap();
        assert_eq!(serde_json::from_value::<Vec<WalOp>>(value).unwrap(), ops);
        let msgpack = WalFormat::MsgPack.encode_record(&ops).unwrap();
        let value: serde_json::Value = rmp_serde::from_slice(&msgpack[FRAME_LEN..]).unwrap();
        assert_eq!(serde_json::from_value::<Vec<WalOp>>(value).unwrap(), ops);
    }

    #[cfg(feature = "wal-cbor")]
    #[test]
    fn every_op_survives_every_binary_record() {
        for format in formats() {
            if format == WalFormat::Json {
                continue;
            }
            let ops = every_op();
            let record = format.encode_record(&ops).unwrap();
            match read_record(format, &record, 0) {
                Record::Ops { ops: back, next } => {
                    assert_eq!(back, ops, "{format}");
                    assert_eq!(next, record.len(), "{format}");
                }
                other => panic!("{format}: {other:?}"),
            }
        }
    }

    #[cfg(feature = "wal-cbor")]
    #[test]
    fn every_op_survives_a_compact_record() {
        for format in [WalFormat::CborCompact, WalFormat::CborCompactZstd] {
            if !format.compiled_in() {
                continue;
            }
            let ops = every_op();
            let record = format.encode_record(&ops).unwrap();
            match read_record(format, &record, 0) {
                Record::Ops { ops: back, next } => {
                    assert_eq!(back, ops, "{format}");
                    assert_eq!(next, record.len(), "{format}");
                }
                other => panic!("{format}: {other:?}"),
            }
            // Smaller than the same record as plain CBOR: the field names
            // are gone.
            let cbor = WalFormat::Cbor.encode_record(&ops).unwrap();
            assert!(
                record.len() < cbor.len(),
                "{format}: {} vs {}",
                record.len(),
                cbor.len()
            );
        }
    }

    #[cfg(feature = "wal-cbor")]
    #[test]
    fn a_compact_record_holds_no_built_in_field_name_as_text() {
        let record = WalFormat::CborCompact.encode_record(&every_op()).unwrap();
        for name in [
            "UpsertNode",
            "UpsertEdge",
            "created_at",
            "body_html",
            "prepared_at_ms",
            "from_id",
        ] {
            assert!(
                !record.windows(name.len()).any(|w| w == name.as_bytes()),
                "{name} written as text"
            );
        }
        // Property names stay as they are.
        assert!(record.windows(3).any(|w| w == b"lat"));
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
                        torn: false,
                        sealed: false
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
                        torn: true,
                        sealed: false
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

    #[cfg(feature = "encryption")]
    mod sealed {
        use super::*;
        use crate::encryption::Key;

        fn key() -> Key {
            Key::from_bytes([9; 32])
        }

        fn sealed_log(format: WalFormat, n: u64) -> Vec<u8> {
            let k = key();
            let mut bytes = log_header(format, Some(&k)).unwrap();
            for i in 0..n {
                bytes.extend(encode_record_with(format, Some(&k), &[op(i)]).unwrap());
            }
            bytes
        }

        fn replay_sealed(
            bytes: &[u8],
            chunk: usize,
            k: Option<&Key>,
        ) -> Result<(Replayed, Vec<WalOp>)> {
            let mut ops = Vec::new();
            let r = replay(
                std::io::Cursor::new(bytes),
                chunk,
                std::path::Path::new("test.wal"),
                k,
                |o| ops.push(o),
            )?;
            Ok((r, ops))
        }

        #[test]
        fn sealed_logs_round_trip_whatever_the_chunk() {
            for format in formats() {
                let bytes = sealed_log(format, 25);
                for chunk in [1, 5, 64, REPLAY_CHUNK] {
                    let (r, ops) = replay_sealed(&bytes, chunk, Some(&key())).unwrap();
                    assert_eq!(ops, (0..25).map(op).collect::<Vec<_>>(), "{format} {chunk}");
                    assert_eq!(
                        r,
                        Replayed::Log {
                            format,
                            valid_end: bytes.len() as u64,
                            torn: false,
                            sealed: true
                        }
                    );
                }
            }
        }

        #[test]
        fn plaintext_never_appears_in_a_sealed_log() {
            let k = key();
            let node = WalOp::DeleteNode(424_242);
            let mut bytes = log_header(WalFormat::Json, Some(&k)).unwrap();
            bytes.extend(encode_record_with(WalFormat::Json, Some(&k), &[node]).unwrap());
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains("DeleteNode"), "{text}");
            assert!(!text.contains("424242"), "{text}");
        }

        #[test]
        fn a_wrong_or_missing_key_is_refused_never_read_as_torn() {
            let bytes = sealed_log(WalFormat::Json, 3);
            let wrong = replay_sealed(&bytes, 4096, Some(&Key::from_bytes([1; 32]))).unwrap_err();
            assert!(
                wrong.to_string().contains("wrong encryption key"),
                "{wrong}"
            );
            let missing = replay_sealed(&bytes, 4096, None).unwrap_err();
            assert!(missing.to_string().contains("encrypted"), "{missing}");
            let plain = log(WalFormat::Json, 3);
            let keyed = replay_sealed(&plain, 4096, Some(&key())).unwrap_err();
            assert!(keyed.to_string().contains("not encrypted"), "{keyed}");
        }

        #[test]
        fn a_changed_record_is_refused() {
            let mut bytes = sealed_log(WalFormat::Json, 6);
            // Inside the third record's sealed payload, past its frame header;
            // fix the checksum so the change reaches decryption.
            let start = sealed_log(WalFormat::Json, 2).len();
            let len = u32::from_le_bytes(bytes[start..start + 4].try_into().unwrap()) as usize;
            bytes[start + FRAME_LEN + 30] ^= 1;
            let crc = crc32(&bytes[start + FRAME_LEN..start + FRAME_LEN + len]);
            bytes[start + 4..start + 8].copy_from_slice(&crc.to_le_bytes());
            let err = replay_sealed(&bytes, 4096, Some(&key())).unwrap_err();
            assert!(err.to_string().contains("does not decrypt"), "{err}");
        }

        #[test]
        fn a_torn_tail_and_a_torn_key_check_recover() {
            let whole = sealed_log(WalFormat::Cbor, 5);
            let four = sealed_log(WalFormat::Cbor, 4).len();
            if WalFormat::Cbor.compiled_in() {
                let (r, ops) = replay_sealed(&whole[..whole.len() - 3], 7, Some(&key())).unwrap();
                assert_eq!(ops.len(), 4);
                assert_eq!(
                    r,
                    Replayed::Log {
                        format: WalFormat::Cbor,
                        valid_end: four as u64,
                        torn: true,
                        sealed: true
                    }
                );
            }
            let header = log_header(WalFormat::Json, Some(&key())).unwrap();
            let (r, _) = replay_sealed(&header[..HEADER_LEN + 10], 3, Some(&key())).unwrap();
            assert_eq!(r, Replayed::TornHeader);
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
                body_start: 0,
                sealed: false
            }
        );
        assert_eq!(
            detect(b"").unwrap(),
            Detected::Log {
                format: WalFormat::Json,
                body_start: 0,
                sealed: false
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
        assert!(detect(b"DRVWAL\x01?").is_err());
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
