//! PackStream — the binary serialisation format used by the Bolt
//! protocol.
//!
//! Every PackStream value is encoded as a one-byte *marker* followed
//! by length / payload bytes. The marker byte is enough to determine
//! the value's type and (for short variants) its length, so the
//! decoder is a single-pass state machine that never needs to look
//! ahead.
//!
//! ## Supported types
//!
//! - **Null** (`0xC0`)
//! - **Boolean** (`0xC2` false / `0xC3` true)
//! - **Integer** — five width-bucketed forms covering the full `i64`
//!   range (TINY, INT_8, INT_16, INT_32, INT_64)
//! - **Float** (`0xC1`) — 64-bit IEEE 754, big-endian
//! - **String** — TINY (0x80–0x8F) + STRING_8 / STRING_16 / STRING_32
//! - **Bytes** — BYTES_8 / BYTES_16 / BYTES_32
//! - **List** — TINY (0x90–0x9F) + LIST_8 / LIST_16 / LIST_32
//! - **Dictionary** — TINY (0xA0–0xAF) + DICT_8 / DICT_16 / DICT_32
//! - **Structure** — TINY (0xB0–0xBF) + STRUCT_8 / STRUCT_16
//!
//! ## Not in scope for this task
//!
//! Bolt v4 layers domain-specific structures (Node, Relationship,
//! Path, Date, Time, …) on top of PackStream by reserving structure
//! tag bytes (`0x4E`, `0x52`, `0x50`, …). The codec here exposes the
//! raw [`Value::Structure`] form; higher Bolt session layers
//! (task `00071`+) build typed wrappers on top.
//!
//! ## Implementation notes
//!
//! * The codec works on plain byte slices and `Vec<u8>` — no
//!   `Read` / `Write` traits, no allocations beyond the resulting
//!   `Value`. This keeps it usable from sync, async, FFI and WASM
//!   contexts without dragging in an I/O trait.
//! * Dictionary keys are sorted at the BTreeMap level, giving the
//!   encoder a deterministic byte layout (important for golden
//!   tests and for any future hash-of-encoded comparisons).

use std::collections::BTreeMap;

use super::error::{BoltError, BoltResult};

/// A decoded PackStream value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// PackStream `Null` (marker `0xC0`).
    Null,
    /// PackStream `Boolean` (`0xC2` false, `0xC3` true).
    Boolean(bool),
    /// PackStream `Integer`. Spans TINY (-16..=127) up through INT_64.
    Integer(i64),
    /// PackStream `Float` — IEEE 754 double, marker `0xC1`.
    Float(f64),
    /// PackStream `String`. UTF-8 byte length determines the marker
    /// bucket (TINY / STRING_8 / STRING_16 / STRING_32).
    String(String),
    /// PackStream `Bytes`. Always uses BYTES_8 / 16 / 32 — there is
    /// no tiny-bytes form in the v4 spec.
    Bytes(Vec<u8>),
    /// PackStream `List`. Items are heterogeneously typed.
    List(Vec<Value>),
    /// PackStream `Dictionary` — string-keyed map. A `BTreeMap` is
    /// used to give the encoder a deterministic key order, matching
    /// what most Bolt clients also produce.
    Dictionary(BTreeMap<String, Value>),
    /// PackStream `Structure` — a tagged tuple of fields. The Bolt
    /// session layer uses this to carry messages (HELLO, RUN, …) and
    /// graph types (Node, Relationship, …).
    Structure {
        /// Tag byte that identifies the structure (e.g. `0x01` for HELLO).
        tag: u8,
        /// Ordered field values.
        fields: Vec<Value>,
    },
}

// PackStream marker bytes (per Bolt v4 spec) -------------------------------
const M_NULL: u8 = 0xC0;
const M_FALSE: u8 = 0xC2;
const M_TRUE: u8 = 0xC3;
const M_FLOAT: u8 = 0xC1;
const M_INT_8: u8 = 0xC8;
const M_INT_16: u8 = 0xC9;
const M_INT_32: u8 = 0xCA;
const M_INT_64: u8 = 0xCB;
const M_BYTES_8: u8 = 0xCC;
const M_BYTES_16: u8 = 0xCD;
const M_BYTES_32: u8 = 0xCE;
const M_STRING_8: u8 = 0xD0;
const M_STRING_16: u8 = 0xD1;
const M_STRING_32: u8 = 0xD2;
const M_LIST_8: u8 = 0xD4;
const M_LIST_16: u8 = 0xD5;
const M_LIST_32: u8 = 0xD6;
const M_DICT_8: u8 = 0xD8;
const M_DICT_16: u8 = 0xD9;
const M_DICT_32: u8 = 0xDA;
const M_STRUCT_8: u8 = 0xDC;
const M_STRUCT_16: u8 = 0xDD;

// --- Encoder --------------------------------------------------------------

/// Encode a single [`Value`] into `out`, appending PackStream bytes.
///
/// # Errors
///
/// Encoding only fails if a sub-value cannot be encoded; for current
/// supported variants this never produces an error in practice, but
/// the `Result` return keeps the API ready for forward-compatible
/// additions (e.g. a `Value::Decimal` whose encoding could fail).
pub fn encode(value: &Value, out: &mut Vec<u8>) -> BoltResult<()> {
    match value {
        Value::Null => out.push(M_NULL),
        Value::Boolean(false) => out.push(M_FALSE),
        Value::Boolean(true) => out.push(M_TRUE),
        Value::Integer(i) => encode_integer(*i, out),
        Value::Float(f) => {
            out.push(M_FLOAT);
            out.extend_from_slice(&f.to_be_bytes());
        }
        Value::String(s) => encode_string(s, out),
        Value::Bytes(b) => encode_bytes(b, out),
        Value::List(items) => {
            encode_list_header(items.len(), out);
            for item in items {
                encode(item, out)?;
            }
        }
        Value::Dictionary(map) => {
            encode_dict_header(map.len(), out);
            for (k, v) in map {
                encode_string(k, out);
                encode(v, out)?;
            }
        }
        Value::Structure { tag, fields } => {
            encode_struct_header(fields.len(), out);
            out.push(*tag);
            for field in fields {
                encode(field, out)?;
            }
        }
    }
    Ok(())
}

fn encode_integer(i: i64, out: &mut Vec<u8>) {
    if (-16..=127).contains(&i) {
        out.push((i as i8) as u8);
    } else if (i8::MIN as i64..=i8::MAX as i64).contains(&i) {
        out.push(M_INT_8);
        out.push((i as i8) as u8);
    } else if (i16::MIN as i64..=i16::MAX as i64).contains(&i) {
        out.push(M_INT_16);
        out.extend_from_slice(&(i as i16).to_be_bytes());
    } else if (i32::MIN as i64..=i32::MAX as i64).contains(&i) {
        out.push(M_INT_32);
        out.extend_from_slice(&(i as i32).to_be_bytes());
    } else {
        out.push(M_INT_64);
        out.extend_from_slice(&i.to_be_bytes());
    }
}

fn encode_string(s: &str, out: &mut Vec<u8>) {
    let bytes = s.as_bytes();
    let len = bytes.len();
    if len <= 15 {
        out.push(0x80 | (len as u8));
    } else if len <= u8::MAX as usize {
        out.push(M_STRING_8);
        out.push(len as u8);
    } else if len <= u16::MAX as usize {
        out.push(M_STRING_16);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(M_STRING_32);
        out.extend_from_slice(&(len as u32).to_be_bytes());
    }
    out.extend_from_slice(bytes);
}

fn encode_bytes(b: &[u8], out: &mut Vec<u8>) {
    let len = b.len();
    if len <= u8::MAX as usize {
        out.push(M_BYTES_8);
        out.push(len as u8);
    } else if len <= u16::MAX as usize {
        out.push(M_BYTES_16);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(M_BYTES_32);
        out.extend_from_slice(&(len as u32).to_be_bytes());
    }
    out.extend_from_slice(b);
}

fn encode_list_header(len: usize, out: &mut Vec<u8>) {
    if len <= 15 {
        out.push(0x90 | (len as u8));
    } else if len <= u8::MAX as usize {
        out.push(M_LIST_8);
        out.push(len as u8);
    } else if len <= u16::MAX as usize {
        out.push(M_LIST_16);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(M_LIST_32);
        out.extend_from_slice(&(len as u32).to_be_bytes());
    }
}

fn encode_dict_header(len: usize, out: &mut Vec<u8>) {
    if len <= 15 {
        out.push(0xA0 | (len as u8));
    } else if len <= u8::MAX as usize {
        out.push(M_DICT_8);
        out.push(len as u8);
    } else if len <= u16::MAX as usize {
        out.push(M_DICT_16);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(M_DICT_32);
        out.extend_from_slice(&(len as u32).to_be_bytes());
    }
}

fn encode_struct_header(size: usize, out: &mut Vec<u8>) {
    if size <= 15 {
        out.push(0xB0 | (size as u8));
    } else if size <= u8::MAX as usize {
        out.push(M_STRUCT_8);
        out.push(size as u8);
    } else {
        // STRUCT_16 caps at u16::MAX; Bolt v4 does not define STRUCT_32.
        out.push(M_STRUCT_16);
        out.extend_from_slice(&(size as u16).to_be_bytes());
    }
}

// --- Decoder --------------------------------------------------------------

/// Decode a single [`Value`] from the front of `input`.
///
/// Returns the decoded value plus the unconsumed tail of the slice
/// so callers can drive a one-value-per-call loop without managing a
/// cursor themselves.
///
/// # Errors
///
/// * [`BoltError::Eof`] — the input ran out mid-value.
/// * [`BoltError::UnknownMarker`] — a marker byte not defined in the
///   Bolt v4 PackStream spec was encountered.
/// * [`BoltError::LengthOverflow`] — a 32-bit length prefix did not
///   fit into `usize` on the current target.
pub fn decode(input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let (marker, rest) = split_first(input)?;
    decode_with_marker(marker, rest)
}

fn decode_with_marker(marker: u8, input: &[u8]) -> BoltResult<(Value, &[u8])> {
    match marker {
        M_NULL => Ok((Value::Null, input)),
        M_TRUE => Ok((Value::Boolean(true), input)),
        M_FALSE => Ok((Value::Boolean(false), input)),
        M_FLOAT => decode_float(input),
        M_INT_8 => {
            let (b, rest) = split_first(input)?;
            Ok((Value::Integer((b as i8) as i64), rest))
        }
        M_INT_16 => {
            let (bytes, rest) = split_n::<2>(input)?;
            Ok((Value::Integer(i16::from_be_bytes(bytes) as i64), rest))
        }
        M_INT_32 => {
            let (bytes, rest) = split_n::<4>(input)?;
            Ok((Value::Integer(i32::from_be_bytes(bytes) as i64), rest))
        }
        M_INT_64 => {
            let (bytes, rest) = split_n::<8>(input)?;
            Ok((Value::Integer(i64::from_be_bytes(bytes)), rest))
        }
        M_BYTES_8 => decode_bytes_u8(input),
        M_BYTES_16 => decode_bytes_u16(input),
        M_BYTES_32 => decode_bytes_u32(input),
        M_STRING_8 => decode_string_u8(input),
        M_STRING_16 => decode_string_u16(input),
        M_STRING_32 => decode_string_u32(input),
        M_LIST_8 => decode_list_u8(input),
        M_LIST_16 => decode_list_u16(input),
        M_LIST_32 => decode_list_u32(input),
        M_DICT_8 => decode_dict_u8(input),
        M_DICT_16 => decode_dict_u16(input),
        M_DICT_32 => decode_dict_u32(input),
        M_STRUCT_8 => decode_struct_u8(input),
        M_STRUCT_16 => decode_struct_u16(input),
        _ => decode_tiny_or_int(marker, input),
    }
}

fn decode_tiny_or_int(marker: u8, input: &[u8]) -> BoltResult<(Value, &[u8])> {
    // Order matters: tiny-int range (-16..=127) overlaps with no other
    // tiny marker family, so check it first.
    let signed = marker as i8;
    if (-16..=127).contains(&signed) {
        return Ok((Value::Integer(signed as i64), input));
    }
    match marker & 0xF0 {
        0x80 => decode_tiny_string(marker, input),
        0x90 => decode_tiny_list(marker, input),
        0xA0 => decode_tiny_dict(marker, input),
        0xB0 => decode_tiny_struct(marker, input),
        _ => Err(BoltError::UnknownMarker(marker)),
    }
}

fn decode_float(input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let (bytes, rest) = split_n::<8>(input)?;
    Ok((Value::Float(f64::from_be_bytes(bytes)), rest))
}

fn decode_tiny_string(marker: u8, input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let len = (marker & 0x0F) as usize;
    decode_string_payload(len, input)
}

fn decode_string_u8(input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let (b, rest) = split_first(input)?;
    decode_string_payload(b as usize, rest)
}

fn decode_string_u16(input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let (bytes, rest) = split_n::<2>(input)?;
    decode_string_payload(u16::from_be_bytes(bytes) as usize, rest)
}

fn decode_string_u32(input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let (bytes, rest) = split_n::<4>(input)?;
    let len = u32::from_be_bytes(bytes);
    let len_usize: usize = len.try_into().map_err(|_| BoltError::LengthOverflow(len))?;
    decode_string_payload(len_usize, rest)
}

fn decode_string_payload(len: usize, input: &[u8]) -> BoltResult<(Value, &[u8])> {
    if input.len() < len {
        return Err(BoltError::Eof);
    }
    let (payload, rest) = input.split_at(len);
    let s = std::str::from_utf8(payload)
        .map_err(|e| BoltError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?
        .to_string();
    Ok((Value::String(s), rest))
}

fn decode_bytes_u8(input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let (b, rest) = split_first(input)?;
    decode_bytes_payload(b as usize, rest)
}

fn decode_bytes_u16(input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let (bytes, rest) = split_n::<2>(input)?;
    decode_bytes_payload(u16::from_be_bytes(bytes) as usize, rest)
}

fn decode_bytes_u32(input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let (bytes, rest) = split_n::<4>(input)?;
    let len = u32::from_be_bytes(bytes);
    let len_usize: usize = len.try_into().map_err(|_| BoltError::LengthOverflow(len))?;
    decode_bytes_payload(len_usize, rest)
}

fn decode_bytes_payload(len: usize, input: &[u8]) -> BoltResult<(Value, &[u8])> {
    if input.len() < len {
        return Err(BoltError::Eof);
    }
    let (payload, rest) = input.split_at(len);
    Ok((Value::Bytes(payload.to_vec()), rest))
}

fn decode_tiny_list(marker: u8, input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let len = (marker & 0x0F) as usize;
    decode_list_items(len, input)
}

fn decode_list_u8(input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let (b, rest) = split_first(input)?;
    decode_list_items(b as usize, rest)
}

fn decode_list_u16(input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let (bytes, rest) = split_n::<2>(input)?;
    decode_list_items(u16::from_be_bytes(bytes) as usize, rest)
}

fn decode_list_u32(input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let (bytes, rest) = split_n::<4>(input)?;
    let len = u32::from_be_bytes(bytes);
    let len_usize: usize = len.try_into().map_err(|_| BoltError::LengthOverflow(len))?;
    decode_list_items(len_usize, rest)
}

fn decode_list_items(len: usize, mut input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let mut items = Vec::with_capacity(len);
    for _ in 0..len {
        let (item, rest) = decode(input)?;
        items.push(item);
        input = rest;
    }
    Ok((Value::List(items), input))
}

fn decode_tiny_dict(marker: u8, input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let len = (marker & 0x0F) as usize;
    decode_dict_items(len, input)
}

fn decode_dict_u8(input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let (b, rest) = split_first(input)?;
    decode_dict_items(b as usize, rest)
}

fn decode_dict_u16(input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let (bytes, rest) = split_n::<2>(input)?;
    decode_dict_items(u16::from_be_bytes(bytes) as usize, rest)
}

fn decode_dict_u32(input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let (bytes, rest) = split_n::<4>(input)?;
    let len = u32::from_be_bytes(bytes);
    let len_usize: usize = len.try_into().map_err(|_| BoltError::LengthOverflow(len))?;
    decode_dict_items(len_usize, rest)
}

fn decode_dict_items(len: usize, mut input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let mut map = BTreeMap::new();
    for _ in 0..len {
        let (key_val, rest_after_key) = decode(input)?;
        let key = match key_val {
            Value::String(s) => s,
            other => {
                return Err(BoltError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("dictionary key is not a string: {other:?}"),
                )));
            }
        };
        let (val, rest_after_val) = decode(rest_after_key)?;
        map.insert(key, val);
        input = rest_after_val;
    }
    Ok((Value::Dictionary(map), input))
}

fn decode_tiny_struct(marker: u8, input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let size = (marker & 0x0F) as usize;
    decode_struct_fields(size, input)
}

fn decode_struct_u8(input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let (b, rest) = split_first(input)?;
    decode_struct_fields(b as usize, rest)
}

fn decode_struct_u16(input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let (bytes, rest) = split_n::<2>(input)?;
    decode_struct_fields(u16::from_be_bytes(bytes) as usize, rest)
}

fn decode_struct_fields(size: usize, input: &[u8]) -> BoltResult<(Value, &[u8])> {
    let (tag, mut rest) = split_first(input)?;
    let mut fields = Vec::with_capacity(size);
    for _ in 0..size {
        let (field, next) = decode(rest)?;
        fields.push(field);
        rest = next;
    }
    Ok((Value::Structure { tag, fields }, rest))
}

// --- Byte-slice helpers ---------------------------------------------------

fn split_first(input: &[u8]) -> BoltResult<(u8, &[u8])> {
    input
        .split_first()
        .map(|(b, rest)| (*b, rest))
        .ok_or(BoltError::Eof)
}

fn split_n<const N: usize>(input: &[u8]) -> BoltResult<([u8; N], &[u8])> {
    if input.len() < N {
        return Err(BoltError::Eof);
    }
    let (head, rest) = input.split_at(N);
    let mut out = [0u8; N];
    out.copy_from_slice(head);
    Ok((out, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiny_int_boundary_127_uses_one_byte() {
        let mut buf = Vec::new();
        encode(&Value::Integer(127), &mut buf).unwrap();
        assert_eq!(buf, vec![0x7F]);
    }

    #[test]
    fn tiny_int_boundary_minus_16_uses_one_byte() {
        let mut buf = Vec::new();
        encode(&Value::Integer(-16), &mut buf).unwrap();
        assert_eq!(buf, vec![0xF0]);
    }

    #[test]
    fn int_8_boundary_at_minus_17() {
        let mut buf = Vec::new();
        encode(&Value::Integer(-17), &mut buf).unwrap();
        assert_eq!(buf[0], M_INT_8);
    }

    #[test]
    fn int_16_boundary_at_128() {
        let mut buf = Vec::new();
        encode(&Value::Integer(128), &mut buf).unwrap();
        assert_eq!(buf[0], M_INT_16);
    }

    #[test]
    fn float_nan_roundtrips_bitwise() {
        // NAN != NAN in float comparisons but bit pattern must survive.
        let nan = f64::NAN;
        let mut buf = Vec::new();
        encode(&Value::Float(nan), &mut buf).unwrap();
        let (decoded, _) = decode(&buf).unwrap();
        match decoded {
            Value::Float(f) => assert_eq!(f.to_bits(), nan.to_bits()),
            other => panic!("expected Float, got {other:?}"),
        }
    }

    #[test]
    fn empty_dict_marker_is_a0() {
        let mut buf = Vec::new();
        encode(&Value::Dictionary(BTreeMap::new()), &mut buf).unwrap();
        assert_eq!(buf, vec![0xA0]);
    }

    #[test]
    fn struct_tag_byte_preserved_through_roundtrip() {
        let val = Value::Structure {
            tag: 0x4E, // Bolt Node tag
            fields: vec![Value::Integer(42)],
        };
        let mut buf = Vec::new();
        encode(&val, &mut buf).unwrap();
        let (decoded, _) = decode(&buf).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn unknown_marker_in_d3_range_is_rejected() {
        // 0xD3 sits between STRING_32 (0xD2) and LIST_8 (0xD4) and is
        // unassigned in the v4 spec.
        let err = decode(&[0xD3]).unwrap_err();
        assert!(matches!(err, BoltError::UnknownMarker(0xD3)));
    }

    #[test]
    fn unknown_marker_in_d7_range_is_rejected() {
        // 0xD7 sits between LIST_32 (0xD6) and DICT_8 (0xD8) and is
        // unassigned.
        let err = decode(&[0xD7]).unwrap_err();
        assert!(matches!(err, BoltError::UnknownMarker(0xD7)));
    }

    #[test]
    fn empty_string_is_a_single_byte_80() {
        let mut buf = Vec::new();
        encode(&Value::String(String::new()), &mut buf).unwrap();
        assert_eq!(buf, vec![0x80]);
    }

    #[test]
    fn empty_list_is_a_single_byte_90() {
        let mut buf = Vec::new();
        encode(&Value::List(Vec::new()), &mut buf).unwrap();
        assert_eq!(buf, vec![0x90]);
    }

    #[test]
    fn dict_keys_emerge_in_sorted_order() {
        let mut dict = BTreeMap::new();
        dict.insert("z".to_string(), Value::Integer(1));
        dict.insert("a".to_string(), Value::Integer(2));
        let mut buf = Vec::new();
        encode(&Value::Dictionary(dict), &mut buf).unwrap();
        // TINY_DICT marker 0xA2 + key 'a' (0x81 'a') + value 02 + 'z' + 01
        assert_eq!(buf, vec![0xA2, 0x81, b'a', 0x02, 0x81, b'z', 0x01]);
    }
}
