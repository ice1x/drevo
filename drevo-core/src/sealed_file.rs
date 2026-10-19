//! Sidecar files of an encrypted store.
//!
//! A store whose log is encrypted keeps its sidecars encrypted too. A
//! whole-file sidecar (`origin.json`, `tombstones.json`, `sequences.json`)
//! is written as `DRVSEAL1` followed by its sealed bytes. The stamp log,
//! which is appended to, is `DRVSEAL1` followed by records framed as a
//! little-endian `u32` length and the sealed bytes of one or more lines.
//! Every kind of sidecar is sealed under its own context, so one cannot be
//! swapped for another.

use std::path::Path;

use crate::wal_format::SealKey;

/// The first bytes of an encrypted sidecar.
pub(crate) const MAGIC: &[u8; 8] = b"DRVSEAL1";

/// The context `origin.json` is sealed under.
pub(crate) const ORIGIN: &[u8] = b"drevo origin";
/// The context `tombstones.json` is sealed under.
pub(crate) const TOMBSTONES: &[u8] = b"drevo tombstones";
/// The context of the stamp log's records.
pub(crate) const STAMPS: &[u8] = b"drevo stamps";
/// The context `sequences.json` is sealed under.
pub(crate) const SEQUENCES: &[u8] = b"drevo sequences";

fn invalid(path: &Path, what: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("{}: {what}", path.display()),
    )
}

#[cfg(feature = "encryption")]
fn seal(key: &SealKey, context: &[u8], bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    crate::encryption::seal(key, context, bytes).map_err(std::io::Error::other)
}

#[cfg(not(feature = "encryption"))]
fn seal(key: &SealKey, _context: &[u8], _bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    match *key {}
}

#[cfg(feature = "encryption")]
fn open(key: &SealKey, context: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
    crate::encryption::open(key, context, sealed).ok()
}

#[cfg(not(feature = "encryption"))]
fn open(key: &SealKey, _context: &[u8], _sealed: &[u8]) -> Option<Vec<u8>> {
    match *key {}
}

/// `bytes` as stored: unchanged without a key, sealed after [`MAGIC`] with
/// one.
///
/// # Errors
/// When no nonce can be drawn.
pub(crate) fn encode(
    key: Option<&SealKey>,
    context: &[u8],
    bytes: &[u8],
) -> std::io::Result<Vec<u8>> {
    let Some(key) = key else {
        return Ok(bytes.to_vec());
    };
    let mut out = MAGIC.to_vec();
    out.extend(seal(key, context, bytes)?);
    Ok(out)
}

/// Read the whole sidecar at `path`; `None` when there is none.
///
/// # Errors
/// An encrypted sidecar without a key, a plain one with a key, one that does
/// not decrypt, or a read failure.
pub(crate) fn read(
    path: &Path,
    key: Option<&SealKey>,
    context: &[u8],
) -> std::io::Result<Option<Vec<u8>>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let sealed = bytes.starts_with(MAGIC);
    match key {
        None if sealed => Err(invalid(
            path,
            "the file is encrypted; open the store with its key",
        )),
        None => Ok(Some(bytes)),
        Some(_) if !sealed => Err(invalid(path, "the file is not encrypted")),
        Some(key) => open(key, context, &bytes[MAGIC.len()..])
            .map(Some)
            .ok_or_else(|| invalid(path, "the file does not decrypt with this key")),
    }
}

/// Write `bytes` to `path` atomically (temp file, then rename), sealed when
/// there is a key.
///
/// # Errors
/// A write or rename failure, or no nonce.
pub(crate) fn write(
    path: &Path,
    key: Option<&SealKey>,
    context: &[u8],
    bytes: &[u8],
) -> std::io::Result<()> {
    let data = encode(key, context, bytes)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    std::fs::write(&tmp, &data)?;
    std::fs::rename(&tmp, path)
}

/// One record of an encrypted append-only sidecar: a length and the sealed
/// `bytes`.
///
/// # Errors
/// When no nonce can be drawn.
pub(crate) fn record(key: &SealKey, context: &[u8], bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    let sealed = seal(key, context, bytes)?;
    let len = u32::try_from(sealed.len()).map_err(std::io::Error::other)?;
    let mut out = Vec::with_capacity(4 + sealed.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend(sealed);
    Ok(out)
}

/// The plaintext of every record of an encrypted append-only sidecar, in
/// order, up to the first incomplete or undecryptable one (a torn tail).
///
/// # Errors
/// A file that is not an encrypted sidecar.
pub(crate) fn records(
    path: &Path,
    bytes: &[u8],
    key: &SealKey,
    context: &[u8],
) -> std::io::Result<Vec<Vec<u8>>> {
    let Some(mut rest) = bytes.strip_prefix(MAGIC.as_slice()) else {
        return Err(invalid(path, "the file is not encrypted"));
    };
    let mut out = Vec::new();
    while rest.len() >= 4 {
        let len = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        let Some(sealed) = rest.get(4..4 + len) else {
            break;
        };
        let Some(plain) = open(key, context, sealed) else {
            break;
        };
        out.push(plain);
        rest = &rest[4 + len..];
    }
    Ok(out)
}

#[cfg(all(test, feature = "encryption"))]
mod tests {
    use super::*;
    use crate::encryption::Key;

    fn dir() -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "drevo_sealed_file_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn whole_files_round_trip_and_hide_their_content() {
        let d = dir();
        let path = d.join("tombstones.json");
        let key = Key::from_bytes([3; 32]);
        write(&path, Some(&key), TOMBSTONES, b"[secret]").unwrap();
        let raw = std::fs::read(&path).unwrap();
        assert!(raw.starts_with(MAGIC));
        assert!(!raw.windows(6).any(|w| w == b"secret"));
        assert_eq!(
            read(&path, Some(&key), TOMBSTONES).unwrap().unwrap(),
            b"[secret]"
        );
        assert!(read(&path, None, TOMBSTONES).is_err());
        assert!(read(&path, Some(&Key::from_bytes([4; 32])), TOMBSTONES).is_err());
        assert!(
            read(&path, Some(&key), ORIGIN).is_err(),
            "a sidecar cannot pass for another"
        );
        assert_eq!(read(&d.join("missing"), Some(&key), ORIGIN).unwrap(), None);
        write(&path, None, TOMBSTONES, b"[plain]").unwrap();
        assert!(read(&path, Some(&key), TOMBSTONES).is_err());
        assert_eq!(read(&path, None, TOMBSTONES).unwrap().unwrap(), b"[plain]");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn append_only_records_stop_at_a_torn_tail() {
        let key = Key::from_bytes([5; 32]);
        let mut bytes = MAGIC.to_vec();
        for line in [b"one".as_slice(), b"two", b"three"] {
            bytes.extend(record(&key, STAMPS, line).unwrap());
        }
        let p = Path::new("stamps.jsonl");
        let all = records(p, &bytes, &key, STAMPS).unwrap();
        assert_eq!(
            all,
            vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
        );
        let torn = records(p, &bytes[..bytes.len() - 5], &key, STAMPS).unwrap();
        assert_eq!(torn.len(), 2);
        assert!(records(p, b"[plain]", &key, STAMPS).is_err());
    }
}
