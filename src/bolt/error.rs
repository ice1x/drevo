//! Bolt-specific error type.
//!
//! Wire-protocol errors live in their own enum (rather than inside
//! [`crate::error::DrevoError`]) because the Bolt module is layered
//! *above* the database — it negotiates protocol versions, frames
//! messages, and serialises [`crate::bolt::packstream::Value`] before
//! ever touching [`crate::db::Drevo`]. Bubbling these failures through
//! `DrevoError` would force unrelated layers (FFI, WASM, HTTP) to
//! match against variants they can never produce.
//!
//! Conversion into [`crate::error::DrevoError`] happens at the
//! session layer once Bolt sessions land (task `00071`), where a
//! `BoltError` becomes part of a Bolt `FAILURE` response.

use std::io;

/// Errors raised by the Bolt wire-protocol layer.
#[derive(Debug, thiserror::Error)]
pub enum BoltError {
    /// The client handshake did not start with `0x60 0x60 0xB0 0x17`.
    #[error("invalid Bolt magic preamble")]
    InvalidMagic,

    /// The handshake input was shorter than the required 20 bytes
    /// (magic + four 4-byte version proposals).
    #[error("invalid handshake length: expected 20 bytes, got {0}")]
    InvalidHandshakeLength(usize),

    /// A PackStream marker byte that does not appear in the Bolt v4
    /// spec was encountered during decoding.
    #[error("unknown PackStream marker: 0x{0:02X}")]
    UnknownMarker(u8),

    /// The byte stream ended before the value being decoded was
    /// complete (e.g. a TINY_STRING declared 5 bytes but only 2 were
    /// available).
    #[error("unexpected EOF in Bolt input")]
    Eof,

    /// A length prefix on the wire is too large for the addressable
    /// payload size. Currently raised when a STRING_32 / LIST_32 /
    /// DICT_32 length exceeds `usize::MAX`, which can only happen on
    /// 16-bit targets — but the check is cheap and documents intent.
    #[error("length prefix overflows addressable size: {0}")]
    LengthOverflow(u32),

    /// A chunked-framing length prefix declared more bytes than the
    /// 65 535 maximum chunk size. Defends against a buggy peer that
    /// mis-formats the 2-byte length.
    #[error("chunk length {0} exceeds maximum of 65535")]
    ChunkTooLarge(u32),

    /// Underlying I/O error from the socket / reader / writer.
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    /// TLS layer failure raised by `crate::bolt::tls` (Phase 11 task
    /// `00073`) — bad PEM, missing private key, mismatched cert/key,
    /// or a `tokio_rustls::TlsAcceptor::accept` handshake error.
    ///
    /// Carries a `String` rather than wrapping the underlying
    /// `rustls::Error` because the `rustls` dependency is optional
    /// (feature `bolt-tls`) — keeping the message-only payload here
    /// means the `BoltError` enum compiles unchanged whether or not
    /// the feature is enabled, and the rest of the Bolt module never
    /// has to `cfg`-gate its error-matching arms.
    #[error("tls error: {0}")]
    Tls(String),
}

/// Convenience `Result` alias used throughout the Bolt module.
pub type BoltResult<T> = Result<T, BoltError>;
