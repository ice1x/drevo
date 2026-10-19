//! Authenticated encryption of a store's files at rest.
//!
//! With a [`Key`], every record of the write-ahead log and every sidecar is
//! sealed with XChaCha20-Poly1305: a fresh random 24-byte nonce per seal, and
//! a 16-byte tag that makes any change to the bytes — or a different key —
//! fail to open instead of yielding garbage.
//!
//! ```
//! use drevo_core::encryption::{open, seal, Key};
//!
//! let key = Key::from_bytes([7; 32]);
//! let sealed = seal(&key, b"wal", b"hello").unwrap();
//! assert_ne!(&sealed[..], b"hello");
//! assert_eq!(open(&key, b"wal", &sealed).unwrap(), b"hello");
//! assert!(open(&Key::from_bytes([8; 32]), b"wal", &sealed).is_err());
//! ```
//!
//! The key is never stored with the data: the caller passes it in, for
//! example from a key file outside the data directory.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

/// Length of a sealed value's nonce prefix.
pub const NONCE_LEN: usize = 24;
/// Length of the authentication tag every sealed value carries.
pub const TAG_LEN: usize = 16;

/// A 256-bit encryption key. Its bytes are wiped when it is dropped and never
/// printed.
#[derive(Clone)]
pub struct Key([u8; 32]);

impl Key {
    /// The key made of `bytes`.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The key written as 64 hexadecimal digits (surrounding whitespace
    /// ignored), as in a key file.
    ///
    /// # Errors
    ///
    /// [`EncryptionError::BadKey`] for anything else.
    pub fn from_hex(text: &str) -> Result<Self, EncryptionError> {
        let text = text.trim();
        if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(EncryptionError::BadKey(
                "a key is 64 hexadecimal digits (32 bytes)".to_string(),
            ));
        }
        let mut bytes = [0u8; 32];
        for (i, byte) in bytes.iter_mut().enumerate() {
            let pair = &text[2 * i..2 * i + 2];
            *byte = u8::from_str_radix(pair, 16)
                .map_err(|_| EncryptionError::BadKey("not a hexadecimal key".to_string()))?;
        }
        Ok(Self(bytes))
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new((&self.0).into())
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(<redacted>)")
    }
}

/// Why sealing or opening failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EncryptionError {
    /// The bytes do not open with this key: a wrong key, or they were
    /// changed.
    #[error("cannot decrypt: wrong key, or the data was changed")]
    Authentication,
    /// The sealed value is shorter than a nonce and a tag.
    #[error("the encrypted value is truncated")]
    Truncated,
    /// The key is malformed.
    #[error("invalid encryption key: {0}")]
    BadKey(String),
    /// No randomness for a nonce.
    #[error("no randomness for a nonce: {0}")]
    Random(String),
}

/// Seal `plaintext` under `key`: `nonce · ciphertext · tag`. `context` is
/// authenticated but not stored — opening needs the same context, so a value
/// cannot be moved to another place (a sidecar into the log, say) unnoticed.
///
/// # Errors
///
/// [`EncryptionError::Random`] when the system has no randomness.
pub fn seal(key: &Key, context: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, EncryptionError> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|e| EncryptionError::Random(e.to_string()))?;
    let sealed = key
        .cipher()
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: context,
            },
        )
        .map_err(|_| EncryptionError::Authentication)?;
    let mut out = Vec::with_capacity(NONCE_LEN + sealed.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// Open a value [`seal`] produced with the same `key` and `context`.
///
/// # Errors
///
/// [`EncryptionError::Truncated`] or [`EncryptionError::Authentication`].
pub fn open(key: &Key, context: &[u8], sealed: &[u8]) -> Result<Vec<u8>, EncryptionError> {
    if sealed.len() < NONCE_LEN + TAG_LEN {
        return Err(EncryptionError::Truncated);
    }
    let (nonce, body) = sealed.split_at(NONCE_LEN);
    key.cipher()
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: body,
                aad: context,
            },
        )
        .map_err(|_| EncryptionError::Authentication)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_value_opens_only_with_its_key_and_context() {
        let key = Key::from_bytes([1; 32]);
        let sealed = seal(&key, b"origin.json", b"{\"id\":7}").unwrap();
        assert_eq!(sealed.len(), NONCE_LEN + 8 + TAG_LEN);
        assert_eq!(open(&key, b"origin.json", &sealed).unwrap(), b"{\"id\":7}");
        assert_eq!(
            open(&Key::from_bytes([2; 32]), b"origin.json", &sealed),
            Err(EncryptionError::Authentication)
        );
        assert_eq!(
            open(&key, b"tombstones.json", &sealed),
            Err(EncryptionError::Authentication)
        );
    }

    #[test]
    fn a_changed_byte_fails_to_open() {
        let key = Key::from_bytes([3; 32]);
        let mut sealed = seal(&key, b"", b"payload").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        assert_eq!(
            open(&key, b"", &sealed),
            Err(EncryptionError::Authentication)
        );
        assert_eq!(
            open(&key, b"", &sealed[..10]),
            Err(EncryptionError::Truncated)
        );
    }

    #[test]
    fn every_seal_uses_a_new_nonce() {
        let key = Key::from_bytes([4; 32]);
        let a = seal(&key, b"", b"same").unwrap();
        let b = seal(&key, b"", b"same").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn keys_parse_from_hex_and_never_print() {
        let hex = "00112233445566778899aabbccddeeff00112233445566778899AABBCCDDEEFF\n";
        let key = Key::from_hex(hex).unwrap();
        assert_eq!(key.0[1], 0x11);
        assert_eq!(key.0[31], 0xff);
        assert_eq!(format!("{key:?}"), "Key(<redacted>)");
        for bad in ["", "00", &"g".repeat(64), &"0".repeat(63)] {
            assert!(
                matches!(Key::from_hex(bad), Err(EncryptionError::BadKey(_))),
                "{bad}"
            );
        }
    }
}
