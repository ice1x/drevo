//! Sequences: named counters that hand out unique, increasing numbers
//! outside transaction control, for human-readable ids such as `BUG-1042` or
//! invoice numbers.
//!
//! ```
//! use drevo_core::sequence::Sequences;
//!
//! let seqs = Sequences::in_memory();
//! seqs.create("bug", 1000, 1).unwrap();
//! assert_eq!(seqs.next("bug").unwrap(), 1000);
//! assert_eq!(seqs.next("bug").unwrap(), 1001);
//! assert_eq!(seqs.current("bug"), Some(1001));
//! ```
//!
//! A value, once handed out, is never handed out again — not after a rolled
//! back transaction (the value is simply skipped) and not after a restart.
//! A durable store keeps the sequences in `sequences.json` next to its log
//! and reserves values in blocks: each time a block is used up, the end of
//! the next one is written to the file before any of its values is returned.
//! A restart resumes after the reserved block, so it may skip unused values
//! but never repeats one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// How many values a durable sequence reserves per write of its file.
pub const BLOCK: i64 = 1000;

/// Why a sequence operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SequenceError {
    /// A sequence of that name already exists.
    #[error("a sequence named `{0}` already exists")]
    AlreadyExists(String),
    /// No sequence of that name exists.
    #[error("no sequence named `{0}`")]
    NotFound(String),
    /// The name is empty, or the step is zero.
    #[error("{0}")]
    Invalid(String),
    /// The next value would not fit in a 64-bit integer.
    #[error("sequence `{0}` is exhausted")]
    Exhausted(String),
    /// The store is read-only.
    #[error("the database is read-only")]
    ReadOnly,
    /// Writing `sequences.json` failed; no value was handed out.
    #[error("cannot persist sequences: {0}")]
    Io(String),
}

/// What one sequence is and where it stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SequenceInfo {
    /// The first value.
    pub start: i64,
    /// The difference between consecutive values; negative counts down.
    pub step: i64,
    /// The last value handed out, `None` before the first.
    pub current: Option<i64>,
}

/// The on-disk and in-memory state of one sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct State {
    start: i64,
    step: i64,
    /// The last value handed out.
    #[serde(default)]
    current: Option<i64>,
    /// The last value the file allows to be handed out without writing it
    /// again (durable stores only).
    #[serde(default)]
    reserved: Option<i64>,
}

impl State {
    fn following(&self) -> Option<i64> {
        match self.current {
            None => Some(self.start),
            Some(c) => c.checked_add(self.step),
        }
    }

    /// Whether `value` is past the reserved block.
    fn beyond_reservation(&self, value: i64) -> bool {
        match self.reserved {
            None => true,
            Some(r) if self.step > 0 => value > r,
            Some(r) => value < r,
        }
    }
}

/// The sequences of one store.
#[derive(Debug, Default)]
pub struct Sequences {
    state: Mutex<BTreeMap<String, State>>,
    /// `sequences.json` of a durable store.
    file: Option<PathBuf>,
    read_only: std::sync::atomic::AtomicBool,
    /// The key an encrypted store seals the file with.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    key: Option<std::sync::Arc<crate::wal_format::SealKey>>,
}

impl Sequences {
    /// Sequences kept in memory only.
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            state: Mutex::new(BTreeMap::new()),
            file: None,
            read_only: std::sync::atomic::AtomicBool::new(false),
            key: None,
        }
    }

    /// The sequences persisted in `file`, which need not exist yet. A
    /// sequence resumes after its reserved block, so a value handed out
    /// before a crash is never handed out again.
    ///
    /// # Errors
    ///
    /// [`SequenceError::Io`] when the file exists but cannot be read or
    /// parsed: starting from nothing could reissue values.
    pub fn open(file: &Path) -> Result<Self, SequenceError> {
        Self::load(file, None)
    }

    /// [`open`](Self::open) for an encrypted store: the file is sealed with
    /// `key` ([`crate::encryption`]).
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn open_sealed(
        file: &Path,
        key: Option<std::sync::Arc<crate::wal_format::SealKey>>,
    ) -> Result<Self, SequenceError> {
        Self::load(file, key)
    }

    fn load(
        file: &Path,
        key: Option<std::sync::Arc<crate::wal_format::SealKey>>,
    ) -> Result<Self, SequenceError> {
        let io = |e: std::io::Error| SequenceError::Io(format!("{}: {e}", file.display()));
        #[cfg(not(target_arch = "wasm32"))]
        let bytes = crate::sealed_file::read(file, key.as_deref(), crate::sealed_file::SEQUENCES)
            .map_err(io)?;
        #[cfg(target_arch = "wasm32")]
        let bytes = match std::fs::read(file) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(io(e)),
        };
        let mut state: BTreeMap<String, State> = match bytes {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| SequenceError::Io(format!("{}: {e}", file.display())))?,
            None => BTreeMap::new(),
        };
        for s in state.values_mut() {
            // Anything up to the reservation may have been handed out.
            if s.reserved.is_some() {
                s.current = s.reserved;
            }
        }
        Ok(Self {
            state: Mutex::new(state),
            file: Some(file.to_path_buf()),
            read_only: std::sync::atomic::AtomicBool::new(false),
            key,
        })
    }

    /// Refuse (`true`) or allow changes and new values.
    pub fn set_read_only(&self, on: bool) {
        self.read_only
            .store(on, std::sync::atomic::Ordering::SeqCst);
    }

    fn writable(&self) -> Result<(), SequenceError> {
        if self.read_only.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(SequenceError::ReadOnly);
        }
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, State>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Write `state` to the file, atomically and durably.
    fn persist(&self, state: &BTreeMap<String, State>) -> Result<(), SequenceError> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        let io = |e: std::io::Error| SequenceError::Io(e.to_string());
        let json = serde_json::to_vec(state).map_err(|e| SequenceError::Io(e.to_string()))?;
        let bytes = self.stored(&json).map_err(io)?;
        let tmp = file.with_extension("json.tmp");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp).map_err(io)?;
            f.write_all(&bytes).map_err(io)?;
            f.sync_all().map_err(io)?;
        }
        std::fs::rename(&tmp, file).map_err(io)
    }

    /// Create sequence `name` counting from `start` by `step`.
    ///
    /// # Errors
    ///
    /// [`SequenceError::AlreadyExists`], [`SequenceError::Invalid`] for an
    /// empty name or a zero step, [`SequenceError::ReadOnly`] or
    /// [`SequenceError::Io`].
    pub fn create(&self, name: &str, start: i64, step: i64) -> Result<(), SequenceError> {
        self.writable()?;
        if name.trim().is_empty() {
            return Err(SequenceError::Invalid("a sequence needs a name".into()));
        }
        if step == 0 {
            return Err(SequenceError::Invalid(format!(
                "the step of sequence `{name}` cannot be 0"
            )));
        }
        let mut state = self.lock();
        if state.contains_key(name) {
            return Err(SequenceError::AlreadyExists(name.to_string()));
        }
        let mut next = state.clone();
        next.insert(
            name.to_string(),
            State {
                start,
                step,
                current: None,
                reserved: None,
            },
        );
        self.persist(&next)?;
        *state = next;
        Ok(())
    }

    /// Drop sequence `name`; `false` when there is none.
    ///
    /// # Errors
    ///
    /// [`SequenceError::ReadOnly`] or [`SequenceError::Io`].
    pub fn drop_sequence(&self, name: &str) -> Result<bool, SequenceError> {
        self.writable()?;
        let mut state = self.lock();
        if !state.contains_key(name) {
            return Ok(false);
        }
        let mut next = state.clone();
        next.remove(name);
        self.persist(&next)?;
        *state = next;
        Ok(true)
    }

    /// Hand out the next value of sequence `name`.
    ///
    /// # Errors
    ///
    /// [`SequenceError::NotFound`], [`SequenceError::Exhausted`],
    /// [`SequenceError::ReadOnly`], or [`SequenceError::Io`] when the next
    /// block cannot be reserved (no value is handed out then).
    pub fn next(&self, name: &str) -> Result<i64, SequenceError> {
        self.writable()?;
        let mut state = self.lock();
        let s = *state
            .get(name)
            .ok_or_else(|| SequenceError::NotFound(name.to_string()))?;
        let value = s
            .following()
            .ok_or_else(|| SequenceError::Exhausted(name.to_string()))?;
        if self.file.is_some() && s.beyond_reservation(value) {
            let span = s.step.saturating_mul(BLOCK - 1);
            let reserved = value.saturating_add(span);
            let mut next = state.clone();
            if let Some(entry) = next.get_mut(name) {
                entry.reserved = Some(reserved);
            }
            self.persist(&next)?;
            *state = next;
        }
        if let Some(entry) = state.get_mut(name) {
            entry.current = Some(value);
        }
        Ok(value)
    }

    /// The last value sequence `name` handed out; `None` when there is no
    /// such sequence or it has handed out nothing yet.
    #[must_use]
    pub fn current(&self, name: &str) -> Option<i64> {
        self.lock().get(name).and_then(|s| s.current)
    }

    /// Sequence `name`, if it exists.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<SequenceInfo> {
        self.lock().get(name).map(|s| SequenceInfo {
            start: s.start,
            step: s.step,
            current: s.current,
        })
    }

    /// Every sequence, by name.
    #[must_use]
    pub fn list(&self) -> Vec<(String, SequenceInfo)> {
        self.lock()
            .iter()
            .map(|(name, s)| {
                (
                    name.clone(),
                    SequenceInfo {
                        start: s.start,
                        step: s.step,
                        current: s.current,
                    },
                )
            })
            .collect()
    }

    /// Write the current state to the file sealed with `key` (in the clear
    /// for `None`), for re-encrypting a store that is about to be closed.
    #[cfg(all(not(target_arch = "wasm32"), feature = "encryption"))]
    pub(crate) fn rewrite_sealed(
        &self,
        key: Option<std::sync::Arc<crate::wal_format::SealKey>>,
    ) -> Result<(), SequenceError> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        let json =
            serde_json::to_vec(&*self.lock()).map_err(|e| SequenceError::Io(e.to_string()))?;
        crate::sealed_file::write(file, key.as_deref(), crate::sealed_file::SEQUENCES, &json)
            .map_err(|e| SequenceError::Io(e.to_string()))
    }

    /// `json` as the file stores it: sealed for an encrypted store.
    fn stored(&self, json: &[u8]) -> std::io::Result<Vec<u8>> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            crate::sealed_file::encode(self.key.as_deref(), crate::sealed_file::SEQUENCES, json)
        }
        #[cfg(target_arch = "wasm32")]
        {
            Ok(json.to_vec())
        }
    }

    /// The bytes of `sequences.json` as it stands, for a backup.
    #[must_use]
    pub fn snapshot_bytes(&self) -> Vec<u8> {
        let json = serde_json::to_vec(&*self.lock()).unwrap_or_default();
        self.stored(&json).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_file(tag: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "drevo_seq_{tag}_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("sequences.json")
    }

    #[test]
    fn values_start_at_start_and_follow_the_step() {
        let s = Sequences::in_memory();
        s.create("down", 10, -5).unwrap();
        assert_eq!(s.current("down"), None);
        assert_eq!(
            (0..3).map(|_| s.next("down").unwrap()).collect::<Vec<_>>(),
            vec![10, 5, 0]
        );
        assert_eq!(s.current("down"), Some(0));
    }

    #[test]
    fn create_drop_and_errors() {
        let s = Sequences::in_memory();
        s.create("bug", 1, 1).unwrap();
        assert_eq!(
            s.create("bug", 1, 1),
            Err(SequenceError::AlreadyExists("bug".into()))
        );
        assert!(matches!(s.create("", 1, 1), Err(SequenceError::Invalid(_))));
        assert!(matches!(
            s.create("z", 1, 0),
            Err(SequenceError::Invalid(_))
        ));
        assert_eq!(s.next("nope"), Err(SequenceError::NotFound("nope".into())));
        assert!(s.drop_sequence("bug").unwrap());
        assert!(!s.drop_sequence("bug").unwrap());
        assert!(s.get("bug").is_none());
    }

    #[test]
    fn an_exhausted_sequence_says_so() {
        let s = Sequences::in_memory();
        s.create("top", i64::MAX, 1).unwrap();
        assert_eq!(s.next("top").unwrap(), i64::MAX);
        assert_eq!(s.next("top"), Err(SequenceError::Exhausted("top".into())));
    }

    #[test]
    fn a_read_only_store_hands_out_nothing() {
        let s = Sequences::in_memory();
        s.create("bug", 1, 1).unwrap();
        s.set_read_only(true);
        assert_eq!(s.next("bug"), Err(SequenceError::ReadOnly));
        assert_eq!(s.create("x", 1, 1), Err(SequenceError::ReadOnly));
        assert_eq!(s.drop_sequence("bug"), Err(SequenceError::ReadOnly));
        s.set_read_only(false);
        assert_eq!(s.next("bug").unwrap(), 1);
    }

    #[test]
    fn concurrent_callers_get_distinct_values() {
        let file = tmp_file("threads");
        let s = std::sync::Arc::new(Sequences::open(&file).unwrap());
        s.create("bug", 1, 1).unwrap();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let s = std::sync::Arc::clone(&s);
                std::thread::spawn(move || {
                    let mut got = Vec::with_capacity(10_000);
                    for _ in 0..10_000 {
                        got.push(s.next("bug").unwrap());
                    }
                    got
                })
            })
            .collect();
        let mut all = Vec::new();
        for h in handles {
            let got = h.join().unwrap();
            assert!(got.windows(2).all(|w| w[0] < w[1]), "increasing per caller");
            all.extend(got);
        }
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 80_000);
        assert_eq!(all.first(), Some(&1));
        assert_eq!(all.last(), Some(&80_000));
    }

    #[test]
    fn a_restart_never_reissues_a_value() {
        let file = tmp_file("restart");
        let last = {
            let s = Sequences::open(&file).unwrap();
            s.create("invoice", 100, 1).unwrap();
            let mut last = 0;
            for _ in 0..1_500 {
                last = s.next("invoice").unwrap();
            }
            last
            // Dropped without any shutdown step, as in a crash.
        };
        assert_eq!(last, 1_599);
        let s = Sequences::open(&file).unwrap();
        let after = s.next("invoice").unwrap();
        assert!(after > last, "{after} must be past {last}");
        // The rest of the reserved block is skipped: a gap, not a repeat.
        assert_eq!(after, 100 + 2 * BLOCK);
    }

    #[test]
    fn an_unused_sequence_restarts_at_its_start() {
        let file = tmp_file("unused");
        Sequences::open(&file).unwrap().create("x", 7, 3).unwrap();
        let s = Sequences::open(&file).unwrap();
        assert_eq!(
            s.get("x").map(|i| (i.start, i.step, i.current)),
            Some((7, 3, None))
        );
        assert_eq!(s.next("x").unwrap(), 7);
    }

    #[test]
    fn a_damaged_file_is_an_error_not_a_reset() {
        let file = tmp_file("damaged");
        std::fs::write(&file, b"{not json").unwrap();
        assert!(matches!(Sequences::open(&file), Err(SequenceError::Io(_))));
    }

    #[test]
    fn list_is_by_name() {
        let s = Sequences::in_memory();
        s.create("b", 1, 1).unwrap();
        s.create("a", 5, 2).unwrap();
        s.next("a").unwrap();
        let names: Vec<(String, Option<i64>)> =
            s.list().into_iter().map(|(n, i)| (n, i.current)).collect();
        assert_eq!(names, vec![("a".into(), Some(5)), ("b".into(), None)]);
    }
}
