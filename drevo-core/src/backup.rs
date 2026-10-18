//! Online physical backups of a durable store, and restoring them.
//!
//! A backup is a directory holding the write-ahead log, its sidecars and a
//! `manifest.json` with each file's size and SHA-256. It is taken at a commit
//! boundary while the store keeps serving: writers pause only while the log's
//! length and the small sidecar tables are read.
//!
//! The log only grows between compactions, so an **incremental** backup holds
//! just the log bytes written since the previous backup (`native.wal.tail`).
//! It continues the previous backup only when the log still starts with the
//! bytes that backup saw; after a compaction it is taken in full, and its
//! manifest says why.
//!
//! ```no_run
//! use drevo_core::backup::{restore, BackupOptions};
//! use drevo_core::native::NativeGraph;
//!
//! let graph = NativeGraph::open_durable("data/native.wal")?;
//! let monday = graph.backup_to("backups/monday".as_ref(), &BackupOptions::default())?;
//! let tuesday = graph.backup_to(
//!     "backups/tuesday".as_ref(),
//!     &BackupOptions { since: Some(monday), ..BackupOptions::default() },
//! )?;
//! println!("{} log bytes since Monday", tuesday.wal_end - tuesday.wal_start);
//!
//! // Later, into a new data directory:
//! restore(&["backups/monday", "backups/tuesday"], "restored/native.wal".as_ref())?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::io::{Read, Seek, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::native::NativeGraph;

/// The manifest's file name inside a backup directory.
pub const MANIFEST_FILE: &str = "manifest.json";

/// The manifest format this version writes and reads.
pub const MANIFEST_FORMAT: u32 = 1;

/// The whole log, in a full backup.
const WAL_FILE: &str = "native.wal";
/// The log bytes after the previous backup, in an incremental one.
const WAL_TAIL_FILE: &str = "native.wal.tail";
/// The sidecars the engine keeps next to its log.
const ORIGIN_FILE: &str = "origin.json";
const TOMBSTONES_FILE: &str = "tombstones.json";
const STAMPS_FILE: &str = "stamps.jsonl";
const SEQUENCES_FILE: &str = "sequences.json";

/// Names an extra file may not take.
const RESERVED: &[&str] = &[
    MANIFEST_FILE,
    WAL_FILE,
    WAL_TAIL_FILE,
    ORIGIN_FILE,
    TOMBSTONES_FILE,
    STAMPS_FILE,
    SEQUENCES_FILE,
    "native.wal.lock",
];

/// Why a backup or restore failed.
#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    /// The store lives in memory only: there is no log to copy.
    #[error("an in-memory store has no log to back up")]
    NotDurable,
    /// The backup directory is not empty, or the store to restore exists.
    #[error("`{0}` already exists; choose a new location")]
    TargetExists(String),
    /// An extra file's name is empty, has a path in it, or is one of the
    /// backup's own files.
    #[error("`{0}` cannot be the name of an extra backup file")]
    InvalidFileName(String),
    /// A backup's manifest is missing or unreadable.
    #[error("no readable backup manifest in `{dir}`: {message}")]
    Manifest {
        /// The backup directory.
        dir: String,
        /// What is wrong with it.
        message: String,
    },
    /// A file does not match its manifest entry.
    #[error("`{file}` is damaged: expected {expected}, found {found}")]
    Corrupt {
        /// The damaged file.
        file: String,
        /// Its size and checksum in the manifest.
        expected: String,
        /// What is on disk.
        found: String,
    },
    /// The backups do not form a chain: a full backup followed by
    /// incrementals, each continuing the one before.
    #[error("the backups do not form a chain: {0}")]
    BrokenChain(String),
    /// Reading the store failed.
    #[error(transparent)]
    Core(#[from] crate::error::CoreError),
    /// A filesystem error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A full backup, or the log bytes since another backup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackupKind {
    /// The whole log.
    Full,
    /// The log bytes after the previous backup.
    Incremental,
}

/// One file of a backup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupFile {
    /// Its name inside the backup directory.
    pub name: String,
    /// Its size in bytes.
    pub bytes: u64,
    /// Its SHA-256, in lowercase hex.
    pub sha256: String,
}

/// What a backup holds: `manifest.json` in the backup directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupManifest {
    /// The manifest format ([`MANIFEST_FORMAT`]).
    pub format: u32,
    /// Full, or incremental.
    pub kind: BackupKind,
    /// When it was taken, in Unix milliseconds.
    pub created_at_ms: i64,
    /// Where in the log this backup's bytes start: 0 for a full backup, the
    /// previous backup's `wal_end` for an incremental one.
    pub wal_start: u64,
    /// The log's length when the backup was taken.
    pub wal_end: u64,
    /// The SHA-256 of the log's first `wal_end` bytes: what an incremental
    /// backup taken later checks that it continues.
    pub wal_sha256: String,
    /// For an incremental backup, the `wal_sha256` of the backup it
    /// continues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha256: Option<String>,
    /// The change-feed head when the backup was taken.
    pub change_head: u64,
    /// The files, with their sizes and checksums.
    pub files: Vec<BackupFile>,
    /// Why an incremental backup that was asked for was taken in full.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_because: Option<String>,
}

impl BackupManifest {
    /// Read the manifest of the backup in `dir`.
    ///
    /// # Errors
    /// [`BackupError::Manifest`] when it is missing, unparseable or written
    /// in a newer format.
    pub fn read(dir: &Path) -> Result<Self, BackupError> {
        let fail = |message: String| BackupError::Manifest {
            dir: dir.display().to_string(),
            message,
        };
        let bytes = std::fs::read(dir.join(MANIFEST_FILE)).map_err(|e| fail(e.to_string()))?;
        let manifest: Self = serde_json::from_slice(&bytes).map_err(|e| fail(e.to_string()))?;
        if manifest.format != MANIFEST_FORMAT {
            return Err(fail(format!(
                "manifest format {} is not {MANIFEST_FORMAT}; restore with the drevo that wrote it",
                manifest.format
            )));
        }
        Ok(manifest)
    }
}

/// How to take a backup.
#[derive(Debug, Clone, Default)]
pub struct BackupOptions {
    /// Take only the log bytes written since this backup. The backup is
    /// taken in full instead when the log no longer continues it.
    pub since: Option<BackupManifest>,
    /// More files to keep with the backup, by name, restored next to the
    /// log: the sidecars of the layers above the engine.
    pub extra_files: Vec<(String, Vec<u8>)>,
}

/// What [`restore`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    /// The restored log's length.
    pub wal_bytes: u64,
    /// The change-feed head of the last backup in the chain.
    pub change_head: u64,
    /// How many backups were replayed.
    pub backups: usize,
    /// The files written next to the log.
    pub files: Vec<String>,
}

const CHUNK: usize = 1 << 16;

fn hex(digest: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Feed the next `len` bytes of `from` to `hash`, and to `to` when given.
fn pipe(
    from: &mut impl Read,
    len: u64,
    hash: &mut Sha256,
    mut to: Option<&mut dyn Write>,
) -> std::io::Result<()> {
    let mut left = len;
    let mut buf = vec![0u8; CHUNK];
    while left > 0 {
        let want = usize::try_from(left.min(CHUNK as u64)).unwrap_or(CHUNK);
        from.read_exact(&mut buf[..want])?;
        hash.update(&buf[..want]);
        if let Some(to) = to.as_deref_mut() {
            to.write_all(&buf[..want])?;
        }
        left -= want as u64;
    }
    Ok(())
}

/// Write `bytes` to `dir/name`, durably, and describe it.
fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<BackupFile> {
    let mut f = std::fs::File::create(dir.join(name))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(BackupFile {
        name: name.to_string(),
        bytes: bytes.len() as u64,
        sha256: hex(&Sha256::digest(bytes)),
    })
}

/// The size and SHA-256 of the file at `path`.
fn describe(path: &Path) -> std::io::Result<(u64, String)> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let mut hash = Sha256::new();
    pipe(&mut f, len, &mut hash, None)?;
    Ok((len, hex(&hash.finalize())))
}

fn check_extra_name(name: &str) -> Result<(), BackupError> {
    let plain = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && !RESERVED.contains(&name);
    if plain {
        Ok(())
    } else {
        Err(BackupError::InvalidFileName(name.to_string()))
    }
}

/// Create `dir` for a backup; it must not exist, or be empty.
fn fresh_dir(dir: &Path) -> Result<(), BackupError> {
    if let Ok(mut entries) = std::fs::read_dir(dir) {
        if entries.next().is_some() {
            return Err(BackupError::TargetExists(dir.display().to_string()));
        }
    }
    std::fs::create_dir_all(dir)?;
    Ok(())
}

impl NativeGraph {
    /// Back up this durable store into `dir`, a new or empty directory,
    /// while it keeps serving. With [`BackupOptions::since`], only the log
    /// bytes written since that backup are copied, unless the log was
    /// compacted in between. See the [module docs](crate::backup).
    ///
    /// # Errors
    /// [`BackupError::NotDurable`] for an in-memory store,
    /// [`BackupError::TargetExists`] when `dir` has files,
    /// [`BackupError::InvalidFileName`] for a bad extra file name, and I/O
    /// errors.
    pub fn backup_to(
        &self,
        dir: &Path,
        options: &BackupOptions,
    ) -> Result<BackupManifest, BackupError> {
        for (name, _) in &options.extra_files {
            check_extra_name(name)?;
        }
        let snapshot = self.log_snapshot()?.ok_or(BackupError::NotDurable)?;
        fresh_dir(dir)?;
        let mut log = snapshot.file;
        let len = snapshot.len;

        // Does the log still start with what the previous backup saw?
        let mut hash = Sha256::new();
        let mut continued = None;
        let mut full_because = None;
        if let Some(since) = &options.since {
            if since.wal_end > len {
                full_because = Some(format!(
                    "the log was compacted since the previous backup (it is {len} bytes, \
                     the previous backup ended at {})",
                    since.wal_end
                ));
            } else {
                pipe(&mut log, since.wal_end, &mut hash, None)?;
                let prefix = hex(&hash.clone().finalize());
                if prefix == since.wal_sha256 {
                    continued = Some(since);
                } else {
                    full_because = Some(
                        "the log was compacted since the previous backup (it no longer \
                         starts with the bytes that backup saw)"
                            .to_string(),
                    );
                    hash = Sha256::new();
                    log.rewind()?;
                }
            }
        }

        let (kind, wal_start, wal_name) = match continued {
            Some(since) => (BackupKind::Incremental, since.wal_end, WAL_TAIL_FILE),
            None => (BackupKind::Full, 0, WAL_FILE),
        };
        let mut files = Vec::new();
        {
            let mut out = std::fs::File::create(dir.join(wal_name))?;
            let mut piece = Sha256::new();
            let mut both = TeeWriter {
                file: &mut out,
                hash: &mut piece,
            };
            pipe(&mut log, len - wal_start, &mut hash, Some(&mut both))?;
            out.sync_all()?;
            files.push(BackupFile {
                name: wal_name.to_string(),
                bytes: len - wal_start,
                sha256: hex(&piece.finalize()),
            });
        }
        files.push(write_file(dir, ORIGIN_FILE, &snapshot.origin)?);
        files.push(write_file(dir, TOMBSTONES_FILE, &snapshot.tombstones)?);
        files.push(write_file(dir, STAMPS_FILE, &snapshot.stamps)?);
        files.push(write_file(dir, SEQUENCES_FILE, &snapshot.sequences)?);
        for (name, bytes) in &options.extra_files {
            files.push(write_file(dir, name, bytes)?);
        }

        let manifest = BackupManifest {
            format: MANIFEST_FORMAT,
            kind,
            created_at_ms: crate::model::now_ms(),
            wal_start,
            wal_end: len,
            wal_sha256: hex(&hash.finalize()),
            base_sha256: continued.map(|since| since.wal_sha256.clone()),
            change_head: snapshot.change_head,
            files,
            full_because,
        };
        // The manifest last, so a backup without one is visibly unfinished.
        let json = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| BackupError::Io(std::io::Error::other(e)))?;
        let tmp = dir.join("manifest.json.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&json)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, dir.join(MANIFEST_FILE))?;
        Ok(manifest)
    }
}

/// Writes to a file and a hash at once.
struct TeeWriter<'a> {
    file: &'a mut std::fs::File,
    hash: &'a mut Sha256,
}

impl Write for TeeWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.file.write(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// Check every file of the backup in `dir` against `manifest`.
fn verify(dir: &Path, manifest: &BackupManifest) -> Result<(), BackupError> {
    for file in &manifest.files {
        let path = dir.join(&file.name);
        let expected = format!("{} bytes, sha256 {}", file.bytes, file.sha256);
        match describe(&path) {
            Ok((bytes, sha256)) if bytes == file.bytes && sha256 == file.sha256 => {}
            Ok((bytes, sha256)) => {
                return Err(BackupError::Corrupt {
                    file: path.display().to_string(),
                    expected,
                    found: format!("{bytes} bytes, sha256 {sha256}"),
                });
            }
            Err(e) => {
                return Err(BackupError::Corrupt {
                    file: path.display().to_string(),
                    expected,
                    found: e.to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Restore the backups in `chain` — a full backup, then any incrementals in
/// the order they were taken — into a new store whose log is `wal_path`.
/// Every file is checked against its manifest and the chain against itself
/// before anything is written; the sidecars come from the last backup.
///
/// # Errors
/// [`BackupError::BrokenChain`], [`BackupError::Corrupt`],
/// [`BackupError::Manifest`], [`BackupError::TargetExists`] when a file of
/// the new store is already there, and I/O errors.
pub fn restore<P: AsRef<Path>>(chain: &[P], wal_path: &Path) -> Result<RestoreReport, BackupError> {
    let Some((first, rest)) = chain.split_first() else {
        return Err(BackupError::BrokenChain("no backup given".into()));
    };
    let mut manifests = vec![(first.as_ref(), BackupManifest::read(first.as_ref())?)];
    for dir in rest {
        manifests.push((dir.as_ref(), BackupManifest::read(dir.as_ref())?));
    }

    // The chain: one full backup, then incrementals each continuing the last.
    if manifests[0].1.kind != BackupKind::Full {
        return Err(BackupError::BrokenChain(format!(
            "`{}` is incremental; start from a full backup",
            manifests[0].0.display()
        )));
    }
    for pair in manifests.windows(2) {
        let (prev_dir, prev) = &pair[0];
        let (dir, next) = &pair[1];
        let continues = next.kind == BackupKind::Incremental
            && next.wal_start == prev.wal_end
            && next.base_sha256.as_deref() == Some(prev.wal_sha256.as_str());
        if !continues {
            return Err(BackupError::BrokenChain(format!(
                "`{}` does not continue `{}`",
                dir.display(),
                prev_dir.display()
            )));
        }
    }
    for (dir, manifest) in &manifests {
        verify(dir, manifest)?;
    }

    // Nothing of the new store may exist yet.
    let (last_dir, last) = &manifests[manifests.len() - 1];
    let sidecars: Vec<&BackupFile> = last
        .files
        .iter()
        .filter(|f| f.name != WAL_FILE && f.name != WAL_TAIL_FILE)
        .collect();
    let target = |name: &str| wal_path.with_file_name(name);
    for path in
        std::iter::once(wal_path.to_path_buf()).chain(sidecars.iter().map(|f| target(&f.name)))
    {
        if path.exists() {
            return Err(BackupError::TargetExists(path.display().to_string()));
        }
    }
    if let Some(parent) = wal_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // The log: the full backup's, then each tail, checked as a whole.
    let tmp = wal_path.with_extension("wal.restoring");
    let mut hash = Sha256::new();
    let written = (|| -> Result<u64, BackupError> {
        let mut out = std::fs::File::create(&tmp)?;
        let mut total = 0;
        for (dir, manifest) in &manifests {
            let name = match manifest.kind {
                BackupKind::Full => WAL_FILE,
                BackupKind::Incremental => WAL_TAIL_FILE,
            };
            let mut piece = std::fs::File::open(dir.join(name))?;
            let len = manifest.wal_end - manifest.wal_start;
            pipe(&mut piece, len, &mut hash, Some(&mut out))?;
            total += len;
        }
        out.sync_all()?;
        Ok(total)
    })();
    let written = match written {
        Ok(n) => n,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    let sha256 = hex(&hash.finalize());
    if sha256 != last.wal_sha256 || written != last.wal_end {
        let _ = std::fs::remove_file(&tmp);
        return Err(BackupError::BrokenChain(format!(
            "the assembled log ({written} bytes, sha256 {sha256}) is not the one `{}` recorded",
            last_dir.display()
        )));
    }

    let mut files = Vec::new();
    for file in &sidecars {
        std::fs::copy(last_dir.join(&file.name), target(&file.name))?;
        files.push(file.name.clone());
    }
    std::fs::rename(&tmp, wal_path)?;
    Ok(RestoreReport {
        wal_bytes: written,
        change_head: last.change_head,
        backups: manifests.len(),
        files,
    })
}
