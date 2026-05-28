//! Integration tests for the PackStream codec — Phase 11 task `00070`.
//!
//! PackStream is the binary serialisation format used by Bolt. Every value
//! is encoded as a one-byte *marker* followed by length / payload bytes.
//! These tests pin the exact byte sequences from the Bolt v4 spec so any
//! future refactor that breaks wire compatibility fails loudly.

#![cfg(not(target_arch = "wasm32"))]

use drevo::bolt::packstream::{decode, encode, Value};
use std::collections::BTreeMap;

fn roundtrip(value: Value) -> Vec<u8> {
    let mut buf = Vec::new();
    encode(&value, &mut buf).expect("encode");
    let (decoded, rest) = decode(&buf).expect("decode");
    assert_eq!(decoded, value);
    assert!(rest.is_empty(), "trailing bytes after decode: {rest:?}");
    buf
}

#[test]
fn null_roundtrips_to_marker_c0() {
    let bytes = roundtrip(Value::Null);
    assert_eq!(bytes, vec![0xC0]);
}

#[test]
fn boolean_true_roundtrips_to_marker_c3() {
    let bytes = roundtrip(Value::Boolean(true));
    assert_eq!(bytes, vec![0xC3]);
}

#[test]
fn boolean_false_roundtrips_to_marker_c2() {
    let bytes = roundtrip(Value::Boolean(false));
    assert_eq!(bytes, vec![0xC2]);
}

#[test]
fn tiny_int_zero_encodes_as_single_byte() {
    let bytes = roundtrip(Value::Integer(0));
    assert_eq!(bytes, vec![0x00]);
}

#[test]
fn tiny_int_max_positive_127_encodes_as_single_byte() {
    let bytes = roundtrip(Value::Integer(127));
    assert_eq!(bytes, vec![0x7F]);
}

#[test]
fn tiny_int_minus_one_encodes_as_single_byte_ff() {
    let bytes = roundtrip(Value::Integer(-1));
    assert_eq!(bytes, vec![0xFF]);
}

#[test]
fn tiny_int_minus_16_encodes_as_single_byte_f0() {
    let bytes = roundtrip(Value::Integer(-16));
    assert_eq!(bytes, vec![0xF0]);
}

#[test]
fn int_8_negative_17_encodes_with_c8_marker() {
    let bytes = roundtrip(Value::Integer(-17));
    assert_eq!(bytes, vec![0xC8, 0xEF]);
}

#[test]
fn int_8_negative_128_encodes_with_c8_marker() {
    let bytes = roundtrip(Value::Integer(-128));
    assert_eq!(bytes, vec![0xC8, 0x80]);
}

#[test]
fn int_16_128_encodes_with_c9_marker() {
    let bytes = roundtrip(Value::Integer(128));
    assert_eq!(bytes, vec![0xC9, 0x00, 0x80]);
}

#[test]
fn int_16_negative_32768_encodes_with_c9_marker() {
    let bytes = roundtrip(Value::Integer(-32_768));
    assert_eq!(bytes, vec![0xC9, 0x80, 0x00]);
}

#[test]
fn int_32_encodes_with_ca_marker() {
    let bytes = roundtrip(Value::Integer(32_768));
    assert_eq!(bytes, vec![0xCA, 0x00, 0x00, 0x80, 0x00]);
}

#[test]
fn int_64_encodes_with_cb_marker() {
    let bytes = roundtrip(Value::Integer(2_147_483_648));
    assert_eq!(
        bytes,
        vec![0xCB, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00]
    );
}

#[test]
fn int_64_min_encodes_with_cb_marker() {
    let bytes = roundtrip(Value::Integer(i64::MIN));
    assert_eq!(
        bytes,
        vec![0xCB, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
    );
}

#[test]
fn float_zero_encodes_with_c1_marker_and_eight_zero_bytes() {
    let bytes = roundtrip(Value::Float(0.0));
    assert_eq!(
        bytes,
        vec![0xC1, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
    );
}

#[test]
fn float_one_encodes_with_c1_marker() {
    // 1.0 in IEEE 754 big-endian is 0x3FF0_0000_0000_0000
    let bytes = roundtrip(Value::Float(1.0));
    assert_eq!(
        bytes,
        vec![0xC1, 0x3F, 0xF0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
    );
}

#[test]
fn float_pi_roundtrips_bitwise() {
    let pi = std::f64::consts::PI;
    let mut buf = Vec::new();
    encode(&Value::Float(pi), &mut buf).unwrap();
    let (decoded, _) = decode(&buf).unwrap();
    match decoded {
        Value::Float(f) => assert_eq!(f.to_bits(), pi.to_bits()),
        other => panic!("expected Float, got {other:?}"),
    }
}

#[test]
fn empty_string_encodes_as_single_byte_80() {
    let bytes = roundtrip(Value::String(String::new()));
    assert_eq!(bytes, vec![0x80]);
}

#[test]
fn tiny_string_abc_encodes_with_83_marker() {
    let bytes = roundtrip(Value::String("abc".to_string()));
    assert_eq!(bytes, vec![0x83, b'a', b'b', b'c']);
}

#[test]
fn tiny_string_max_length_15_encodes_with_8f_marker() {
    let s = "x".repeat(15);
    let mut buf = Vec::new();
    encode(&Value::String(s.clone()), &mut buf).unwrap();
    assert_eq!(buf[0], 0x8F);
    assert_eq!(buf.len(), 16);
    let (decoded, _) = decode(&buf).unwrap();
    assert_eq!(decoded, Value::String(s));
}

#[test]
fn string_8_for_length_16_to_255() {
    let s = "y".repeat(100);
    let mut buf = Vec::new();
    encode(&Value::String(s.clone()), &mut buf).unwrap();
    assert_eq!(buf[0], 0xD0);
    assert_eq!(buf[1], 100);
    assert_eq!(buf.len(), 2 + 100);
    let (decoded, _) = decode(&buf).unwrap();
    assert_eq!(decoded, Value::String(s));
}

#[test]
fn string_16_for_length_256_to_65535() {
    let s = "z".repeat(300);
    let mut buf = Vec::new();
    encode(&Value::String(s.clone()), &mut buf).unwrap();
    assert_eq!(buf[0], 0xD1);
    assert_eq!(u16::from_be_bytes([buf[1], buf[2]]), 300);
    assert_eq!(buf.len(), 3 + 300);
    let (decoded, _) = decode(&buf).unwrap();
    assert_eq!(decoded, Value::String(s));
}

#[test]
fn string_32_for_length_above_65535() {
    let s = "a".repeat(70_000);
    let mut buf = Vec::new();
    encode(&Value::String(s.clone()), &mut buf).unwrap();
    assert_eq!(buf[0], 0xD2);
    let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);
    assert_eq!(len as usize, s.len());
    let (decoded, _) = decode(&buf).unwrap();
    assert_eq!(decoded, Value::String(s));
}

#[test]
fn unicode_string_roundtrips_byte_length_not_char_length() {
    // 3 chars but 9 bytes in UTF-8.
    let s = "日本語".to_string();
    assert_eq!(s.len(), 9);
    let mut buf = Vec::new();
    encode(&Value::String(s.clone()), &mut buf).unwrap();
    assert_eq!(buf[0], 0x89);
    let (decoded, _) = decode(&buf).unwrap();
    assert_eq!(decoded, Value::String(s));
}

#[test]
fn empty_bytes_encodes_with_cc_zero_marker() {
    let bytes = roundtrip(Value::Bytes(Vec::new()));
    assert_eq!(bytes, vec![0xCC, 0x00]);
}

#[test]
fn bytes_8_for_length_below_256() {
    let payload = vec![1, 2, 3, 4, 5];
    let mut buf = Vec::new();
    encode(&Value::Bytes(payload.clone()), &mut buf).unwrap();
    assert_eq!(buf[0], 0xCC);
    assert_eq!(buf[1], 5);
    assert_eq!(&buf[2..], &payload[..]);
}

#[test]
fn bytes_16_for_length_256_to_65535() {
    let payload = vec![0xAA; 300];
    let mut buf = Vec::new();
    encode(&Value::Bytes(payload.clone()), &mut buf).unwrap();
    assert_eq!(buf[0], 0xCD);
    assert_eq!(u16::from_be_bytes([buf[1], buf[2]]), 300);
    assert_eq!(&buf[3..], &payload[..]);
}

#[test]
fn bytes_32_for_length_above_65535() {
    let payload = vec![0xBB; 70_000];
    let mut buf = Vec::new();
    encode(&Value::Bytes(payload.clone()), &mut buf).unwrap();
    assert_eq!(buf[0], 0xCE);
    let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);
    assert_eq!(len as usize, payload.len());
    let (decoded, _) = decode(&buf).unwrap();
    assert_eq!(decoded, Value::Bytes(payload));
}

#[test]
fn empty_list_encodes_as_single_byte_90() {
    let bytes = roundtrip(Value::List(Vec::new()));
    assert_eq!(bytes, vec![0x90]);
}

#[test]
fn tiny_list_three_ints_encodes_correctly() {
    let bytes = roundtrip(Value::List(vec![
        Value::Integer(1),
        Value::Integer(2),
        Value::Integer(3),
    ]));
    assert_eq!(bytes, vec![0x93, 0x01, 0x02, 0x03]);
}

#[test]
fn list_8_for_length_16_to_255() {
    let items: Vec<Value> = (0..30).map(Value::Integer).collect();
    let mut buf = Vec::new();
    encode(&Value::List(items.clone()), &mut buf).unwrap();
    assert_eq!(buf[0], 0xD4);
    assert_eq!(buf[1], 30);
    let (decoded, _) = decode(&buf).unwrap();
    assert_eq!(decoded, Value::List(items));
}

#[test]
fn list_16_for_length_256_to_65535() {
    let items: Vec<Value> = (0..300).map(|n| Value::Integer(n % 100)).collect();
    let mut buf = Vec::new();
    encode(&Value::List(items.clone()), &mut buf).unwrap();
    assert_eq!(buf[0], 0xD5);
    assert_eq!(u16::from_be_bytes([buf[1], buf[2]]), 300);
    let (decoded, _) = decode(&buf).unwrap();
    assert_eq!(decoded, Value::List(items));
}

#[test]
fn nested_list_roundtrips() {
    let inner = Value::List(vec![Value::Integer(1), Value::Integer(2)]);
    let outer = Value::List(vec![inner.clone(), Value::String("hi".into()), inner]);
    let _bytes = roundtrip(outer);
}

#[test]
fn empty_dict_encodes_as_single_byte_a0() {
    let bytes = roundtrip(Value::Dictionary(BTreeMap::new()));
    assert_eq!(bytes, vec![0xA0]);
}

#[test]
fn tiny_dict_one_entry_encodes_correctly() {
    let mut dict = BTreeMap::new();
    dict.insert("a".to_string(), Value::Integer(1));
    let bytes = roundtrip(Value::Dictionary(dict));
    assert_eq!(bytes, vec![0xA1, 0x81, b'a', 0x01]);
}

#[test]
fn dict_8_for_size_16_to_255() {
    let mut dict = BTreeMap::new();
    for i in 0..20 {
        dict.insert(format!("k{i:02}"), Value::Integer(i));
    }
    let mut buf = Vec::new();
    encode(&Value::Dictionary(dict.clone()), &mut buf).unwrap();
    assert_eq!(buf[0], 0xD8);
    assert_eq!(buf[1], 20);
    let (decoded, _) = decode(&buf).unwrap();
    assert_eq!(decoded, Value::Dictionary(dict));
}

#[test]
fn empty_struct_encodes_as_b0_plus_tag() {
    let bytes = roundtrip(Value::Structure {
        tag: 0x0E,
        fields: Vec::new(),
    });
    assert_eq!(bytes, vec![0xB0, 0x0E]);
}

#[test]
fn tiny_struct_three_fields_encodes_correctly() {
    let bytes = roundtrip(Value::Structure {
        tag: 0x01, // HELLO
        fields: vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)],
    });
    assert_eq!(bytes, vec![0xB3, 0x01, 0x01, 0x02, 0x03]);
}

#[test]
fn struct_8_for_size_16_to_255() {
    let fields: Vec<Value> = (0..20).map(Value::Integer).collect();
    let mut buf = Vec::new();
    encode(
        &Value::Structure {
            tag: 0x70,
            fields: fields.clone(),
        },
        &mut buf,
    )
    .unwrap();
    assert_eq!(buf[0], 0xDC);
    assert_eq!(buf[1], 20);
    assert_eq!(buf[2], 0x70);
    let (decoded, _) = decode(&buf).unwrap();
    assert_eq!(decoded, Value::Structure { tag: 0x70, fields });
}

#[test]
fn struct_16_for_size_256_or_more() {
    let fields: Vec<Value> = (0..300).map(|n| Value::Integer(n % 100)).collect();
    let mut buf = Vec::new();
    encode(
        &Value::Structure {
            tag: 0x71,
            fields: fields.clone(),
        },
        &mut buf,
    )
    .unwrap();
    assert_eq!(buf[0], 0xDD);
    assert_eq!(u16::from_be_bytes([buf[1], buf[2]]), 300);
    assert_eq!(buf[3], 0x71);
    let (decoded, _) = decode(&buf).unwrap();
    assert_eq!(decoded, Value::Structure { tag: 0x71, fields });
}

#[test]
fn decoding_empty_input_returns_unexpected_eof() {
    let err = decode(&[]).unwrap_err();
    assert!(format!("{err:?}").contains("Eof") || format!("{err}").contains("eof"));
}

#[test]
fn decoding_truncated_string_returns_eof() {
    // 0x83 says "tiny string of 3 bytes" but only one byte follows.
    let err = decode(&[0x83, b'a']).unwrap_err();
    assert!(format!("{err:?}").contains("Eof") || format!("{err}").contains("eof"));
}

#[test]
fn decoding_unknown_marker_returns_error() {
    // 0xC4 is reserved / unused in PackStream.
    let err = decode(&[0xC4]).unwrap_err();
    assert!(format!("{err:?}").contains("UnknownMarker") || format!("{err}").contains("marker"));
}

#[test]
fn decode_returns_trailing_bytes() {
    // Encode 0x01 then add trailing 0xAA 0xBB.
    let input = vec![0x01, 0xAA, 0xBB];
    let (value, rest) = decode(&input).unwrap();
    assert_eq!(value, Value::Integer(1));
    assert_eq!(rest, &[0xAA, 0xBB][..]);
}

#[test]
fn hello_struct_resembles_bolt_initialization_message() {
    // The Bolt HELLO message is a Structure tag=0x01 with one field — a
    // Dictionary carrying client metadata.
    let mut meta = BTreeMap::new();
    meta.insert(
        "user_agent".to_string(),
        Value::String("drevo/0.1".to_string()),
    );
    let hello = Value::Structure {
        tag: 0x01,
        fields: vec![Value::Dictionary(meta)],
    };
    let _bytes = roundtrip(hello);
}
