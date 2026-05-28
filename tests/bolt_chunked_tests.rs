//! Integration tests for Bolt chunked message framing — Phase 11
//! task `00070`.
//!
//! On the wire every Bolt message is split into one or more *chunks*,
//! each prefixed by a 16-bit big-endian length. The end of a logical
//! message is signalled by a zero-length chunk (the two bytes `0x00
//! 0x00`). The same chunked stream carries handshake-completion HELLO,
//! RUN, PULL, etc. — so the codec must work on any reader / writer.

#![cfg(not(target_arch = "wasm32"))]

use drevo::bolt::chunked::{read_message, write_message, MAX_CHUNK_SIZE};
use std::io::Cursor;

#[test]
fn write_then_read_short_message_roundtrips() {
    let payload = vec![0xB1, 0x01, 0x91, 0x01];
    let mut buf = Vec::new();
    write_message(&payload, &mut buf).unwrap();

    let mut cursor = Cursor::new(&buf);
    let decoded = read_message(&mut cursor).unwrap();
    assert_eq!(decoded, payload);
}

#[test]
fn short_message_is_a_single_chunk_with_zero_terminator() {
    let payload = vec![0xAA, 0xBB, 0xCC];
    let mut buf = Vec::new();
    write_message(&payload, &mut buf).unwrap();

    // 2-byte length prefix + 3-byte payload + 2-byte zero terminator = 7
    assert_eq!(buf.len(), 7);
    assert_eq!(u16::from_be_bytes([buf[0], buf[1]]), 3);
    assert_eq!(&buf[2..5], &payload[..]);
    assert_eq!(&buf[5..7], &[0x00, 0x00]);
}

#[test]
fn empty_message_writes_only_the_zero_terminator() {
    let mut buf = Vec::new();
    write_message(&[], &mut buf).unwrap();
    assert_eq!(buf, vec![0x00, 0x00]);

    let mut cursor = Cursor::new(&buf);
    let decoded = read_message(&mut cursor).unwrap();
    assert!(decoded.is_empty());
}

#[test]
fn message_exactly_max_chunk_size_uses_one_chunk() {
    let payload = vec![0xCD; MAX_CHUNK_SIZE];
    let mut buf = Vec::new();
    write_message(&payload, &mut buf).unwrap();

    // 2-byte prefix + MAX bytes + 2-byte zero terminator
    assert_eq!(buf.len(), 2 + MAX_CHUNK_SIZE + 2);
    assert_eq!(
        u16::from_be_bytes([buf[0], buf[1]]) as usize,
        MAX_CHUNK_SIZE
    );

    let mut cursor = Cursor::new(&buf);
    let decoded = read_message(&mut cursor).unwrap();
    assert_eq!(decoded.len(), MAX_CHUNK_SIZE);
    assert_eq!(decoded, payload);
}

#[test]
fn message_larger_than_max_chunk_size_splits_across_multiple_chunks() {
    // Just over one chunk → expect 2 chunks (full + remainder) + terminator.
    let payload_len = MAX_CHUNK_SIZE + 1234;
    let payload: Vec<u8> = (0..payload_len).map(|i| (i % 251) as u8).collect();

    let mut buf = Vec::new();
    write_message(&payload, &mut buf).unwrap();

    // First chunk: full MAX size
    let first_len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
    assert_eq!(first_len, MAX_CHUNK_SIZE);
    let second_off = 2 + MAX_CHUNK_SIZE;
    let second_len = u16::from_be_bytes([buf[second_off], buf[second_off + 1]]) as usize;
    assert_eq!(second_len, payload_len - MAX_CHUNK_SIZE);

    let mut cursor = Cursor::new(&buf);
    let decoded = read_message(&mut cursor).unwrap();
    assert_eq!(decoded, payload);
}

#[test]
fn read_handles_multiple_chunks_then_terminator() {
    // Manually construct: two chunks of 3 bytes each, then 0x00 0x00.
    let stream = vec![
        0x00, 0x03, 0x01, 0x02, 0x03, // chunk 1
        0x00, 0x03, 0x04, 0x05, 0x06, // chunk 2
        0x00, 0x00, // end of message
    ];
    let mut cursor = Cursor::new(&stream);
    let decoded = read_message(&mut cursor).unwrap();
    assert_eq!(decoded, vec![1, 2, 3, 4, 5, 6]);
}

#[test]
fn read_truncated_stream_returns_error() {
    // length says 4 but only 2 bytes follow.
    let stream = vec![0x00, 0x04, 0xAA, 0xBB];
    let mut cursor = Cursor::new(&stream);
    let err = read_message(&mut cursor).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.to_lowercase().contains("eof") || msg.to_lowercase().contains("unexpected"));
}

#[test]
fn read_two_messages_back_to_back() {
    // Two complete messages in one stream.
    let mut stream = Vec::new();
    write_message(&[0xAA], &mut stream).unwrap();
    write_message(&[0xBB, 0xCC], &mut stream).unwrap();

    let mut cursor = Cursor::new(&stream);
    let m1 = read_message(&mut cursor).unwrap();
    let m2 = read_message(&mut cursor).unwrap();
    assert_eq!(m1, vec![0xAA]);
    assert_eq!(m2, vec![0xBB, 0xCC]);
}

#[test]
fn read_returns_error_when_stream_ends_before_first_length() {
    let stream: Vec<u8> = Vec::new();
    let mut cursor = Cursor::new(&stream);
    let err = read_message(&mut cursor).unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("eof"));
}
