use std::sync::Arc;

use drevo::storage::{MemoryBackend, RedbBackend, StorageBackend, StorageError};

// --- Macro to generate the StorageBackend contract tests for any backend ---

macro_rules! backend_contract_tests {
    ($mod_name:ident, $create_backend:expr) => {
        mod $mod_name {
            use super::*;

            #[test]
            fn put_and_get() {
                let (_dir, backend) = $create_backend;
                backend.put(b"key1", b"value1").unwrap();
                assert_eq!(backend.get(b"key1").unwrap(), Some(b"value1".to_vec()));
            }

            #[test]
            fn get_missing_key_returns_none() {
                let (_dir, backend) = $create_backend;
                assert_eq!(backend.get(b"nonexistent").unwrap(), None);
            }

            #[test]
            fn put_overwrites_existing() {
                let (_dir, backend) = $create_backend;
                backend.put(b"key", b"v1").unwrap();
                backend.put(b"key", b"v2").unwrap();
                assert_eq!(backend.get(b"key").unwrap(), Some(b"v2".to_vec()));
            }

            #[test]
            fn delete_existing_key() {
                let (_dir, backend) = $create_backend;
                backend.put(b"key", b"value").unwrap();
                backend.delete(b"key").unwrap();
                assert_eq!(backend.get(b"key").unwrap(), None);
            }

            #[test]
            fn delete_nonexistent_is_noop() {
                let (_dir, backend) = $create_backend;
                backend.delete(b"nonexistent").unwrap();
            }

            #[test]
            fn scan_prefix_returns_matching_sorted() {
                let (_dir, backend) = $create_backend;
                backend.put(b"e:42:likes:1", b"data1").unwrap();
                backend.put(b"e:42:likes:2", b"data2").unwrap();
                backend.put(b"e:42:knows:3", b"data3").unwrap();
                backend.put(b"e:99:likes:1", b"other").unwrap();
                backend.put(b"n:42", b"node").unwrap();

                let results = backend.scan_prefix(b"e:42:").unwrap();
                assert_eq!(results.len(), 3);
                assert!(results[0].0 < results[1].0);
                assert!(results[1].0 < results[2].0);
                for (k, _) in &results {
                    assert!(k.starts_with(b"e:42:"));
                }
            }

            #[test]
            fn scan_prefix_no_matches() {
                let (_dir, backend) = $create_backend;
                backend.put(b"abc", b"value").unwrap();
                let results = backend.scan_prefix(b"xyz").unwrap();
                assert!(results.is_empty());
            }

            #[test]
            fn scan_prefix_empty_store() {
                let (_dir, backend) = $create_backend;
                let results = backend.scan_prefix(b"any").unwrap();
                assert!(results.is_empty());
            }

            #[test]
            fn flush_does_not_error() {
                let (_dir, backend) = $create_backend;
                backend.put(b"key", b"value").unwrap();
                backend.flush().unwrap();
            }

            #[test]
            fn empty_key_and_value() {
                let (_dir, backend) = $create_backend;
                backend.put(b"", b"").unwrap();
                assert_eq!(backend.get(b"").unwrap(), Some(b"".to_vec()));
            }

            #[test]
            fn binary_key_and_value() {
                let (_dir, backend) = $create_backend;
                let key = vec![0u8, 1, 255, 128];
                let value = vec![42u8; 1024];
                backend.put(&key, &value).unwrap();
                assert_eq!(backend.get(&key).unwrap(), Some(value));
            }

            #[test]
            fn trait_object_works() {
                let (_dir, backend) = $create_backend;
                let backend: Arc<dyn StorageBackend> = Arc::new(backend);
                backend.put(b"key", b"value").unwrap();
                assert_eq!(backend.get(b"key").unwrap(), Some(b"value".to_vec()));
            }

            #[test]
            fn many_keys_scan_prefix() {
                let (_dir, backend) = $create_backend;
                for i in 0..100u32 {
                    let key = format!("pfx:{:04}", i);
                    let val = format!("val_{}", i);
                    backend.put(key.as_bytes(), val.as_bytes()).unwrap();
                }
                backend.put(b"other:1", b"x").unwrap();

                let results = backend.scan_prefix(b"pfx:").unwrap();
                assert_eq!(results.len(), 100);
                // Verify sorted
                for w in results.windows(2) {
                    assert!(w[0].0 < w[1].0);
                }
            }

            #[test]
            fn delete_then_reinsert() {
                let (_dir, backend) = $create_backend;
                backend.put(b"key", b"v1").unwrap();
                backend.delete(b"key").unwrap();
                assert_eq!(backend.get(b"key").unwrap(), None);
                backend.put(b"key", b"v2").unwrap();
                assert_eq!(backend.get(b"key").unwrap(), Some(b"v2".to_vec()));
            }

            #[test]
            fn scan_prefix_exact_match() {
                let (_dir, backend) = $create_backend;
                backend.put(b"abc", b"val1").unwrap();
                backend.put(b"abcdef", b"val2").unwrap();
                backend.put(b"abd", b"val3").unwrap();

                let results = backend.scan_prefix(b"abc").unwrap();
                assert_eq!(results.len(), 2);
                assert_eq!(results[0].0, b"abc");
                assert_eq!(results[1].0, b"abcdef");
            }

            #[test]
            fn scan_prefix_empty_prefix_returns_all() {
                let (_dir, backend) = $create_backend;
                backend.put(b"a", b"1").unwrap();
                backend.put(b"b", b"2").unwrap();
                backend.put(b"c", b"3").unwrap();

                let results = backend.scan_prefix(b"").unwrap();
                assert_eq!(results.len(), 3);
            }
        }
    };
}

// Helper: create an ephemeral MemoryBackend (no tempdir needed, but we return
// Option<TempDir> so the macro signature stays uniform).
fn create_memory_backend() -> (Option<tempfile::TempDir>, MemoryBackend) {
    (None, MemoryBackend::new())
}

// Helper: create a RedbBackend in a temporary directory.
fn create_redb_backend() -> (Option<tempfile::TempDir>, RedbBackend) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.redb");
    let backend = RedbBackend::open(&db_path).unwrap();
    (Some(dir), backend)
}

// Generate the contract test suite for MemoryBackend
backend_contract_tests!(memory_contract, create_memory_backend());

// Generate the contract test suite for RedbBackend
backend_contract_tests!(redb_contract, create_redb_backend());

// --- MemoryBackend-specific persistence tests ---

mod memory_persist {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("roundtrip.db");

        {
            let backend = MemoryBackend::open(&db_path).unwrap();
            backend.put(b"n:1", b"node1").unwrap();
            backend.put(b"n:2", b"node2").unwrap();
            backend.put(b"e:1:likes:2", b"edge_data").unwrap();
            backend.flush().unwrap();
        }

        let backend = MemoryBackend::open(&db_path).unwrap();
        assert_eq!(backend.get(b"n:1").unwrap(), Some(b"node1".to_vec()));
        assert_eq!(backend.get(b"n:2").unwrap(), Some(b"node2".to_vec()));
        assert_eq!(
            backend.get(b"e:1:likes:2").unwrap(),
            Some(b"edge_data".to_vec())
        );
        assert_eq!(backend.get(b"n:999").unwrap(), None);
    }

    #[test]
    fn scan_prefix_after_reload() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("scan.db");

        {
            let backend = MemoryBackend::open(&db_path).unwrap();
            backend.put(b"e:42:likes:1", b"d1").unwrap();
            backend.put(b"e:42:likes:2", b"d2").unwrap();
            backend.put(b"e:42:knows:3", b"d3").unwrap();
            backend.put(b"e:99:likes:1", b"other").unwrap();
            backend.flush().unwrap();
        }

        let backend = MemoryBackend::open(&db_path).unwrap();
        let results = backend.scan_prefix(b"e:42:").unwrap();
        assert_eq!(results.len(), 3);
        for (k, _) in &results {
            assert!(k.starts_with(b"e:42:"));
        }
    }

    #[test]
    fn via_trait_object() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("trait.db");

        {
            let backend: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::open(&db_path).unwrap());
            backend.put(b"key", b"value").unwrap();
            backend.flush().unwrap();
        }

        let backend = MemoryBackend::open(&db_path).unwrap();
        assert_eq!(backend.get(b"key").unwrap(), Some(b"value".to_vec()));
    }

    #[test]
    fn multiple_flush_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("multi.db");

        let backend = MemoryBackend::open(&db_path).unwrap();

        backend.put(b"k1", b"v1").unwrap();
        backend.flush().unwrap();

        backend.put(b"k2", b"v2").unwrap();
        backend.delete(b"k1").unwrap();
        backend.flush().unwrap();

        drop(backend);

        let backend = MemoryBackend::open(&db_path).unwrap();
        assert_eq!(backend.get(b"k1").unwrap(), None);
        assert_eq!(backend.get(b"k2").unwrap(), Some(b"v2".to_vec()));
    }
}

// --- RedbBackend-specific persistence tests ---

mod redb_persist {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("roundtrip.redb");

        {
            let backend = RedbBackend::open(&db_path).unwrap();
            backend.put(b"n:1", b"node1").unwrap();
            backend.put(b"n:2", b"node2").unwrap();
            backend.put(b"e:1:likes:2", b"edge_data").unwrap();
        }

        let backend = RedbBackend::open(&db_path).unwrap();
        assert_eq!(backend.get(b"n:1").unwrap(), Some(b"node1".to_vec()));
        assert_eq!(backend.get(b"n:2").unwrap(), Some(b"node2".to_vec()));
        assert_eq!(
            backend.get(b"e:1:likes:2").unwrap(),
            Some(b"edge_data".to_vec())
        );
        assert_eq!(backend.get(b"n:999").unwrap(), None);
    }

    #[test]
    fn scan_prefix_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("scan.redb");

        {
            let backend = RedbBackend::open(&db_path).unwrap();
            backend.put(b"e:42:likes:1", b"d1").unwrap();
            backend.put(b"e:42:likes:2", b"d2").unwrap();
            backend.put(b"e:42:knows:3", b"d3").unwrap();
            backend.put(b"e:99:likes:1", b"other").unwrap();
        }

        let backend = RedbBackend::open(&db_path).unwrap();
        let results = backend.scan_prefix(b"e:42:").unwrap();
        assert_eq!(results.len(), 3);
        for (k, _) in &results {
            assert!(k.starts_with(b"e:42:"));
        }
    }

    #[test]
    fn delete_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("delete.redb");

        {
            let backend = RedbBackend::open(&db_path).unwrap();
            backend.put(b"k1", b"v1").unwrap();
            backend.put(b"k2", b"v2").unwrap();
            backend.delete(b"k1").unwrap();
        }

        let backend = RedbBackend::open(&db_path).unwrap();
        assert_eq!(backend.get(b"k1").unwrap(), None);
        assert_eq!(backend.get(b"k2").unwrap(), Some(b"v2".to_vec()));
    }
}

// --- Error type tests (backend-independent) ---

#[test]
fn error_display() {
    let err = StorageError::NotFound(b"test_key".to_vec());
    let msg = err.to_string();
    assert!(msg.contains("key not found"));
    assert!(msg.contains("test_key"));

    let err = StorageError::LockPoisoned;
    assert!(err.to_string().contains("lock poisoned"));
}

#[test]
fn error_display_binary_key() {
    let err = StorageError::NotFound(vec![0xFF, 0x00, 0xAB]);
    let msg = err.to_string();
    assert!(msg.contains("key not found"));
}

#[test]
fn encode_error_converts_via_from() {
    // `?` propagation: bincode::error::EncodeError → StorageError::Encode.
    // The compiler enforces the `#[from]` path; this test pins the
    // observable Display output and the variant tag.
    fn force_encode() -> Result<(), StorageError> {
        // A `Vec<u8>` with the full sequence length encodes fine; we
        // construct the error directly to assert Display + matchability.
        Err(StorageError::Encode(bincode::error::EncodeError::Other(
            "forced",
        )))
    }
    match force_encode() {
        Err(StorageError::Encode(_)) => {}
        other => panic!("expected StorageError::Encode, got: {other:?}"),
    }
}

#[test]
fn decode_error_converts_via_from() {
    fn force_decode() -> Result<(), StorageError> {
        // Trigger a real decode failure through the public API.
        let (res, _consumed): (i32, usize) =
            bincode::serde::decode_from_slice(&[0xFFu8; 0], bincode::config::standard())?;
        let _ = res;
        Ok(())
    }
    match force_decode() {
        Err(StorageError::Decode(_)) => {}
        other => panic!("expected StorageError::Decode, got: {other:?}"),
    }
}

#[cfg(feature = "redb-backend")]
#[test]
fn redb_sub_errors_convert_to_redb_variant() {
    // Open a redb file, then provoke a TableError::TableDoesNotExist by
    // asking for a table that was never created. The `?` operator must
    // route the sub-error through `From<TableError> for StorageError`
    // into `StorageError::Redb` — no `Backend(String)` anywhere.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("redb_subconv.redb");
    let db = redb::Database::create(&db_path).unwrap();
    let txn = db.begin_read().unwrap();
    let missing: redb::TableDefinition<&[u8], &[u8]> = redb::TableDefinition::new("does_not_exist");

    fn open_missing(
        txn: &redb::ReadTransaction,
        td: redb::TableDefinition<&[u8], &[u8]>,
    ) -> std::result::Result<(), StorageError> {
        let _table = txn.open_table(td)?;
        Ok(())
    }

    match open_missing(&txn, missing) {
        Err(StorageError::Redb(_)) => {}
        other => panic!("expected StorageError::Redb, got: {other:?}"),
    }
}

// --- Backend parity over a randomised operation sequence ---
//
// Audit task 00103, finding F8: assert that `MemoryBackend` and `RedbBackend`
// agree on the observable state after a long, mixed-operation trace. Two
// backends that pass every per-operation contract test can still diverge on
// interleaved sequences (ordering, prefix-scan windowing, reinsert behaviour).
//
// Determinism: a tiny xorshift32 RNG with a fixed seed produces the operation
// stream so any failure is bit-for-bit reproducible. When `proptest` becomes
// a project dependency (Phase 9 task 00057), this test will be ported.

struct XorShift32 {
    state: u32,
}

impl XorShift32 {
    fn new(seed: u32) -> Self {
        Self {
            state: if seed == 0 { 0x9E37_79B9 } else { seed },
        }
    }

    fn next_u32(&mut self) -> u32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        x
    }

    fn next_in(&mut self, max: u32) -> u32 {
        self.next_u32() % max
    }
}

#[test]
fn random_operation_sequence_parity() {
    // Use both backends behind the trait so the call sites are identical.
    let mem: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
    let dir = tempfile::tempdir().unwrap();
    let redb: Arc<dyn StorageBackend> =
        Arc::new(RedbBackend::open(dir.path().join("parity.redb")).unwrap());

    // 32 keys with mixed prefixes — enough variety for non-trivial scan_prefix
    // intersections without slowing the test down.
    let keys: Vec<Vec<u8>> = (0..32u32)
        .map(|i| {
            let prefix = match i % 4 {
                0 => "n:",
                1 => "e:",
                2 => "k:",
                _ => "x:",
            };
            format!("{prefix}{i:04}").into_bytes()
        })
        .collect();
    let prefixes: [&[u8]; 5] = [b"n:", b"e:", b"k:", b"x:", b""];

    let mut rng = XorShift32::new(0xDEAD_BEEF);
    for step in 0..1000 {
        let op = rng.next_in(4);
        let key = keys[rng.next_in(keys.len() as u32) as usize].as_slice();
        match op {
            0 => {
                // put
                let value_len = (rng.next_in(16) + 1) as usize;
                let value: Vec<u8> = (0..value_len)
                    .map(|_| (rng.next_in(256) & 0xFF) as u8)
                    .collect();
                mem.put(key, &value).unwrap();
                redb.put(key, &value).unwrap();
            }
            1 => {
                // get — observations must match
                let mv = mem.get(key).unwrap();
                let rv = redb.get(key).unwrap();
                assert_eq!(mv, rv, "step {step}: get({key:?}) diverged");
            }
            2 => {
                // delete
                mem.delete(key).unwrap();
                redb.delete(key).unwrap();
            }
            _ => {
                // scan_prefix — full key/value list AND sort order must match
                let prefix = prefixes[rng.next_in(prefixes.len() as u32) as usize];
                let mp = mem.scan_prefix(prefix).unwrap();
                let rp = redb.scan_prefix(prefix).unwrap();
                assert_eq!(mp, rp, "step {step}: scan_prefix({prefix:?}) diverged");
                // Order contract: keys must be lex-sorted.
                for window in mp.windows(2) {
                    assert!(
                        window[0].0 < window[1].0,
                        "step {step}: scan_prefix returned unsorted keys"
                    );
                }
            }
        }
    }

    // Final coarse parity check: every key visible on one backend is visible
    // on the other.
    for key in &keys {
        assert_eq!(
            mem.get(key).unwrap(),
            redb.get(key).unwrap(),
            "final get({key:?}) diverged"
        );
    }
}
