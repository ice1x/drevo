//! Bolt connection handshake.
//!
//! Every Bolt connection starts with the client sending exactly 20
//! bytes:
//!
//! * 4-byte magic preamble `0x60 0x60 0xB0 0x17`
//! * Four 4-byte version proposals, in preference order:
//!   `[0x00, range, minor, major]`. A proposal covers the minor versions
//!   `minor - range ..= minor` of `major` (ranges came with Bolt 4.3), so
//!   `00 04 04 05` offers Bolt 5.4 down to 5.0.
//!
//! The server replies with 4 bytes carrying the selected version
//! (`major.minor` in the same encoding) or `0x00 0x00 0x00 0x00` to
//! indicate no compatible version. Either way the connection stays
//! open; the no-match case is followed by the server closing the
//! socket.
//!
//! ## Negotiation
//!
//! [`ClientHandshake::select`] walks the proposals in the client's order
//! and picks, within the first proposal that covers any version drevo
//! speaks, the highest such version. drevo speaks Bolt 5.4 down to 5.0 and
//! 4.4 / 4.3 ([`SUPPORTED_VERSIONS`]). Other slots, such as the Bolt 5.7+
//! handshake manifest (`00 00 01 FF`), are skipped.

use super::error::{BoltError, BoltResult};

/// The 4-byte magic preamble every Bolt handshake starts with.
pub const MAGIC_PREAMBLE: [u8; 4] = [0x60, 0x60, 0xB0, 0x17];

/// Total length of the client handshake message in bytes.
pub const HANDSHAKE_LEN: usize = 20;

/// A negotiated Bolt protocol version: `major.minor`. Wire format is
/// `[0x00, 0x00, minor, major]` per the Bolt v4 spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BoltVersion {
    /// Major version number (e.g. `4` for the Bolt 4.x family).
    pub major: u8,
    /// Minor version number (e.g. `4` for Bolt 4.4).
    pub minor: u8,
}

impl BoltVersion {
    /// Sentinel value the server returns when no version proposal is
    /// supported. Wire form `00 00 00 00`.
    pub const NONE: BoltVersion = BoltVersion { major: 0, minor: 0 };

    /// Decode a 4-byte handshake version slot.
    pub fn from_be_bytes(bytes: [u8; 4]) -> Self {
        // bytes[0], bytes[1] reserved / range — ignored. bytes[2] is
        // the minor version, bytes[3] is the major.
        Self {
            minor: bytes[2],
            major: bytes[3],
        }
    }

    /// Encode this version back to its 4-byte wire form, suitable for
    /// the server response.
    pub fn to_be_bytes(self) -> [u8; 4] {
        [0x00, 0x00, self.minor, self.major]
    }

    /// `true` if both major and minor are zero — the sentinel slot.
    pub fn is_none(self) -> bool {
        self.major == 0 && self.minor == 0
    }
}

/// Parsed client handshake — the four version proposals in the order
/// they arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHandshake {
    /// The highest version of each proposal, in client preference order.
    pub versions: [BoltVersion; 4],
    /// How many minor versions below `versions[i]` proposal `i` also covers.
    pub ranges: [u8; 4],
}

impl ClientHandshake {
    /// The version to speak: within the first proposal (in the client's
    /// order) that covers a version drevo supports, the highest one.
    /// `None` when no proposal covers one.
    #[must_use]
    pub fn select(&self) -> Option<BoltVersion> {
        self.versions
            .iter()
            .zip(self.ranges)
            .filter(|(top, _)| !top.is_none())
            .find_map(|(top, range)| {
                SUPPORTED_VERSIONS.iter().copied().find(|v| {
                    v.major == top.major
                        && v.minor <= top.minor
                        && v.minor >= top.minor.saturating_sub(range)
                })
            })
    }
}

/// Decode a 20-byte Bolt handshake.
///
/// # Errors
///
/// * [`BoltError::InvalidHandshakeLength`] — `bytes.len() != 20`.
/// * [`BoltError::InvalidMagic`] — the first four bytes did not
///   match [`MAGIC_PREAMBLE`].
pub fn parse_client_handshake(bytes: &[u8]) -> BoltResult<ClientHandshake> {
    if bytes.len() != HANDSHAKE_LEN {
        return Err(BoltError::InvalidHandshakeLength(bytes.len()));
    }
    if bytes[..4] != MAGIC_PREAMBLE {
        return Err(BoltError::InvalidMagic);
    }
    let mut versions = [BoltVersion::NONE; 4];
    let mut ranges = [0u8; 4];
    for (i, slot) in versions.iter_mut().enumerate() {
        let off = 4 + i * 4;
        let chunk = [bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]];
        *slot = BoltVersion::from_be_bytes(chunk);
        ranges[i] = bytes[off + 1];
    }
    Ok(ClientHandshake { versions, ranges })
}

/// The set of Bolt versions drevo's wire protocol speaks, highest first.
pub const SUPPORTED_VERSIONS: &[BoltVersion] = &[
    BoltVersion { major: 5, minor: 4 },
    BoltVersion { major: 5, minor: 3 },
    BoltVersion { major: 5, minor: 2 },
    BoltVersion { major: 5, minor: 1 },
    BoltVersion { major: 5, minor: 0 },
    BoltVersion { major: 4, minor: 4 },
    BoltVersion { major: 4, minor: 3 },
];

/// Pick the first client-proposed version that drevo supports, matching
/// each proposal's exact version (no ranges; see [`ClientHandshake::select`]).
/// Returns `None` if no proposal is supported — the caller should
/// then write `0x00 0x00 0x00 0x00` back and close the connection.
pub fn select_version(proposals: &[BoltVersion; 4]) -> Option<BoltVersion> {
    for proposal in proposals {
        if proposal.is_none() {
            continue;
        }
        if SUPPORTED_VERSIONS.contains(proposal) {
            return Some(*proposal);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_preamble_value_is_pinned() {
        assert_eq!(MAGIC_PREAMBLE, [0x60, 0x60, 0xB0, 0x17]);
    }

    #[test]
    fn bolt_version_round_trip_via_bytes() {
        let v = BoltVersion { major: 4, minor: 4 };
        assert_eq!(BoltVersion::from_be_bytes(v.to_be_bytes()), v);
    }

    #[test]
    fn supported_versions_are_in_descending_order() {
        // A simple invariant check: preference order should match the
        // semver-descending order of the supported list.
        let mut sorted = SUPPORTED_VERSIONS.to_vec();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(sorted, SUPPORTED_VERSIONS);
    }

    #[test]
    fn parse_short_input_is_rejected() {
        let err = parse_client_handshake(&[0x60, 0x60, 0xB0, 0x17]).unwrap_err();
        assert!(matches!(err, BoltError::InvalidHandshakeLength(4)));
    }

    #[test]
    fn parse_bad_magic_is_rejected() {
        let mut bytes = vec![0x00; 20];
        let err = parse_client_handshake(&bytes).unwrap_err();
        assert!(matches!(err, BoltError::InvalidMagic));
        bytes[0] = 0x60;
        bytes[1] = 0x60;
        bytes[2] = 0xB0;
        bytes[3] = 0x18;
        let err = parse_client_handshake(&bytes).unwrap_err();
        assert!(matches!(err, BoltError::InvalidMagic));
    }

    #[test]
    fn select_skips_unsupported_slots() {
        let proposals = [
            BoltVersion { major: 6, minor: 0 },
            BoltVersion { major: 0, minor: 0 },
            BoltVersion { major: 4, minor: 4 },
            BoltVersion { major: 4, minor: 3 },
        ];
        assert_eq!(
            select_version(&proposals),
            Some(BoltVersion { major: 4, minor: 4 })
        );
    }

    fn handshake(slots: [[u8; 4]; 4]) -> ClientHandshake {
        let mut bytes = MAGIC_PREAMBLE.to_vec();
        for slot in slots {
            bytes.extend_from_slice(&slot);
        }
        parse_client_handshake(&bytes).unwrap()
    }

    #[test]
    fn a_range_selects_the_highest_supported_version_in_it() {
        let v = |major, minor| Some(BoltVersion { major, minor });
        // 5.8 down to 5.0: the highest drevo speaks is 5.4.
        let h = handshake([[0, 8, 8, 5], [0, 2, 4, 4], [0, 0, 1, 4], [0, 0, 0, 3]]);
        assert_eq!(h.ranges, [8, 2, 0, 0]);
        assert_eq!(h.select(), v(5, 4));
        // 5.2 down to 5.0.
        assert_eq!(
            handshake([[0, 2, 2, 5], [0; 4], [0; 4], [0; 4]]).select(),
            v(5, 2)
        );
        // 5.9 down to 5.6 covers nothing drevo speaks: the 4.x slot wins.
        let h = handshake([[0, 3, 9, 5], [0, 2, 4, 4], [0; 4], [0; 4]]);
        assert_eq!(h.select(), v(4, 4));
        // The 5.7+ handshake manifest slot is skipped.
        let h = handshake([[0, 0, 1, 0xFF], [0, 4, 4, 5], [0; 4], [0; 4]]);
        assert_eq!(h.select(), v(5, 4));
        // 4.4 down to 4.2: 4.4.
        assert_eq!(
            handshake([[0, 2, 4, 4], [0; 4], [0; 4], [0; 4]]).select(),
            v(4, 4)
        );
        // 4.2 only, and nothing at all.
        assert_eq!(
            handshake([[0, 0, 2, 4], [0; 4], [0; 4], [0; 4]]).select(),
            None
        );
        assert_eq!(handshake([[0; 4]; 4]).select(), None);
    }

    #[test]
    fn select_returns_none_when_only_unsupported_offered() {
        let proposals = [
            BoltVersion { major: 5, minor: 9 },
            BoltVersion { major: 6, minor: 0 },
            BoltVersion::NONE,
            BoltVersion::NONE,
        ];
        assert!(select_version(&proposals).is_none());
    }
}
