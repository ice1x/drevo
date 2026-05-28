//! Integration tests for the Bolt handshake — Phase 11 task `00070`.
//!
//! When a client opens a Bolt connection it sends 20 bytes: 4 magic
//! preamble bytes (`0x60 0x60 0xB0 0x17`) followed by four 4-byte
//! version proposals. The server replies with 4 bytes carrying the
//! version it selected, or `0x00 0x00 0x00 0x00` if no proposal is
//! supported.

#![cfg(not(target_arch = "wasm32"))]

use drevo::bolt::handshake::{parse_client_handshake, select_version, BoltVersion, MAGIC_PREAMBLE};

fn raw_handshake(v1: [u8; 4], v2: [u8; 4], v3: [u8; 4], v4: [u8; 4]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(20);
    bytes.extend_from_slice(&MAGIC_PREAMBLE);
    bytes.extend_from_slice(&v1);
    bytes.extend_from_slice(&v2);
    bytes.extend_from_slice(&v3);
    bytes.extend_from_slice(&v4);
    bytes
}

#[test]
fn magic_preamble_is_the_documented_value() {
    assert_eq!(MAGIC_PREAMBLE, [0x60, 0x60, 0xB0, 0x17]);
}

#[test]
fn parse_extracts_four_versions_after_magic() {
    // Per the Bolt v4 spec each version slot is [0x00, 0x00, minor, major].
    let bytes = raw_handshake(
        [0x00, 0x00, 0x04, 0x04],
        [0x00, 0x00, 0x03, 0x04],
        [0x00, 0x00, 0x00, 0x00],
        [0x00, 0x00, 0x00, 0x00],
    );
    let parsed = parse_client_handshake(&bytes).unwrap();
    assert_eq!(parsed.versions.len(), 4);
    assert_eq!(parsed.versions[0], BoltVersion { major: 4, minor: 4 });
    assert_eq!(parsed.versions[1], BoltVersion { major: 4, minor: 3 });
    assert_eq!(parsed.versions[2], BoltVersion { major: 0, minor: 0 });
    assert_eq!(parsed.versions[3], BoltVersion { major: 0, minor: 0 });
}

#[test]
fn parse_rejects_wrong_magic_preamble() {
    let mut bytes = vec![0xDE, 0xAD, 0xBE, 0xEF];
    bytes.extend_from_slice(&[0; 16]);
    let err = parse_client_handshake(&bytes).unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("magic"));
}

#[test]
fn parse_rejects_truncated_handshake() {
    let bytes = vec![0x60, 0x60, 0xB0, 0x17, 0x00];
    let err = parse_client_handshake(&bytes).unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("length"));
}

#[test]
fn select_picks_first_supported_version_in_proposal_order() {
    // Bolt 5.0 is unsupported by drevo today; Bolt 4.4 must win.
    let bytes = raw_handshake(
        [0x00, 0x00, 0x00, 0x05],
        [0x00, 0x00, 0x04, 0x04],
        [0x00, 0x00, 0x00, 0x00],
        [0x00, 0x00, 0x00, 0x00],
    );
    let parsed = parse_client_handshake(&bytes).unwrap();
    let selected = select_version(&parsed.versions);
    assert_eq!(selected, Some(BoltVersion { major: 4, minor: 4 }));
}

#[test]
fn select_returns_none_when_no_proposal_is_supported() {
    let bytes = raw_handshake(
        [0x00, 0x00, 0x00, 0x05],
        [0x00, 0x00, 0x00, 0x06],
        [0x00, 0x00, 0x00, 0x00],
        [0x00, 0x00, 0x00, 0x00],
    );
    let parsed = parse_client_handshake(&bytes).unwrap();
    assert_eq!(select_version(&parsed.versions), None);
}

#[test]
fn bolt_version_encodes_to_four_bytes_big_endian() {
    let bytes = BoltVersion { major: 4, minor: 4 }.to_be_bytes();
    assert_eq!(bytes, [0x00, 0x00, 0x04, 0x04]);

    let bytes_43 = BoltVersion { major: 4, minor: 3 }.to_be_bytes();
    assert_eq!(bytes_43, [0x00, 0x00, 0x03, 0x04]);

    let bytes_zero = BoltVersion { major: 0, minor: 0 }.to_be_bytes();
    assert_eq!(bytes_zero, [0x00, 0x00, 0x00, 0x00]);
}

#[test]
fn supported_versions_include_bolt_4_4_and_4_3() {
    // Smoke-test the supported list. Reflect the actual support matrix
    // the listener will negotiate against.
    let bytes_44 = raw_handshake(
        [0x00, 0x00, 0x04, 0x04],
        [0x00, 0x00, 0x00, 0x00],
        [0x00, 0x00, 0x00, 0x00],
        [0x00, 0x00, 0x00, 0x00],
    );
    let parsed = parse_client_handshake(&bytes_44).unwrap();
    assert_eq!(
        select_version(&parsed.versions),
        Some(BoltVersion { major: 4, minor: 4 })
    );

    let bytes_43 = raw_handshake(
        [0x00, 0x00, 0x03, 0x04],
        [0x00, 0x00, 0x00, 0x00],
        [0x00, 0x00, 0x00, 0x00],
        [0x00, 0x00, 0x00, 0x00],
    );
    let parsed = parse_client_handshake(&bytes_43).unwrap();
    assert_eq!(
        select_version(&parsed.versions),
        Some(BoltVersion { major: 4, minor: 3 })
    );
}

#[test]
fn bolt_version_v4_4_is_greater_than_v4_3_via_ord() {
    let v44 = BoltVersion { major: 4, minor: 4 };
    let v43 = BoltVersion { major: 4, minor: 3 };
    assert!(v44 > v43);
}
