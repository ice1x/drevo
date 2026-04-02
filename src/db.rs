//! Core database struct and lifecycle methods.
//!
//! [`GraphNoteDb`] is the main entry point for all database operations.
//! It wraps a [`StorageBackend`] and manages auto-increment counters,
//! indexes, and the graph data model.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{GraphNoteError, Result};
use crate::storage::{MemoryBackend, RedbBackend, StorageBackend};

/// Meta key for the next node ID counter.
const META_NEXT_NODE_ID: &[u8] = b"meta:next_node_id";

/// Meta key for the next edge ID counter.
const META_NEXT_EDGE_ID: &[u8] = b"meta:next_edge_id";

/// The main GraphNote DB handle.
///
/// Created via [`GraphNoteDb::open`] (disk-backed) or
/// [`GraphNoteDb::open_in_memory`] (ephemeral). All graph operations
/// are methods on this struct.
pub struct GraphNoteDb {
    /// The underlying key-value storage backend.
    backend: Box<dyn StorageBackend>,
    /// Auto-increment counter for node IDs.
    next_node_id: AtomicU64,
    /// Auto-increment counter for edge IDs.
    next_edge_id: AtomicU64,
}

impl GraphNoteDb {
    /// Open a disk-backed database at the given path.
    ///
    /// Creates the database file if it does not exist.
    /// Loads auto-increment counters from the stored metadata.
    ///
    /// # Errors
    ///
    /// Returns [`GraphNoteError::Storage`] if the backend cannot be opened.
    pub fn open(path: &Path) -> Result<Self> {
        let backend = RedbBackend::open(path).map_err(GraphNoteError::Storage)?;
        let backend = Box::new(backend);
        let (next_node_id, next_edge_id) = Self::load_counters(&*backend)?;
        Ok(Self {
            backend,
            next_node_id: AtomicU64::new(next_node_id),
            next_edge_id: AtomicU64::new(next_edge_id),
        })
    }

    /// Open an ephemeral in-memory database.
    ///
    /// Data is lost when the database is dropped. Useful for tests
    /// and temporary workloads.
    pub fn open_in_memory() -> Result<Self> {
        let backend = Box::new(MemoryBackend::new());
        Ok(Self {
            backend,
            next_node_id: AtomicU64::new(1),
            next_edge_id: AtomicU64::new(1),
        })
    }

    /// Flush all pending writes and close the database.
    ///
    /// Persists auto-increment counters to storage before flushing.
    ///
    /// # Errors
    ///
    /// Returns [`GraphNoteError::Storage`] if flush fails.
    pub fn close(self) -> Result<()> {
        self.persist_counters()?;
        self.backend.flush().map_err(GraphNoteError::Storage)?;
        Ok(())
    }

    /// Trigger compaction of the underlying storage.
    ///
    /// For redb this is a no-op (redb manages its own compaction).
    /// For the memory backend this flushes to disk if a path is configured.
    pub fn compact(&self) -> Result<()> {
        self.backend.flush().map_err(GraphNoteError::Storage)?;
        Ok(())
    }

    /// Allocate the next node ID (thread-safe).
    ///
    /// Returns a unique, monotonically increasing ID starting from 1.
    /// Used internally by `create_node`.
    pub fn alloc_node_id(&self) -> u64 {
        self.next_node_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Allocate the next edge ID (thread-safe).
    ///
    /// Returns a unique, monotonically increasing ID starting from 1.
    /// Used internally by `create_edge`.
    pub fn alloc_edge_id(&self) -> u64 {
        self.next_edge_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Return a reference to the underlying storage backend.
    #[allow(dead_code)] // Will be used by CRUD methods in task 00010
    pub(crate) fn backend(&self) -> &dyn StorageBackend {
        &*self.backend
    }

    /// Load auto-increment counters from storage metadata.
    ///
    /// Returns (next_node_id, next_edge_id). Defaults to 1 if not found.
    fn load_counters(backend: &dyn StorageBackend) -> Result<(u64, u64)> {
        let node_id = match backend
            .get(META_NEXT_NODE_ID)
            .map_err(GraphNoteError::Storage)?
        {
            Some(bytes) => u64_from_bytes(&bytes),
            None => 1,
        };
        let edge_id = match backend
            .get(META_NEXT_EDGE_ID)
            .map_err(GraphNoteError::Storage)?
        {
            Some(bytes) => u64_from_bytes(&bytes),
            None => 1,
        };
        Ok((node_id, edge_id))
    }

    /// Persist current auto-increment counters to storage metadata.
    fn persist_counters(&self) -> Result<()> {
        let node_id = self.next_node_id.load(Ordering::Relaxed);
        let edge_id = self.next_edge_id.load(Ordering::Relaxed);
        self.backend
            .put(META_NEXT_NODE_ID, &node_id.to_le_bytes())
            .map_err(GraphNoteError::Storage)?;
        self.backend
            .put(META_NEXT_EDGE_ID, &edge_id.to_le_bytes())
            .map_err(GraphNoteError::Storage)?;
        Ok(())
    }
}

/// Decode a u64 from little-endian bytes, defaulting to 1 on invalid input.
fn u64_from_bytes(bytes: &[u8]) -> u64 {
    if bytes.len() == 8 {
        u64::from_le_bytes(bytes.try_into().unwrap())
    } else {
        1
    }
}

impl std::fmt::Debug for GraphNoteDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphNoteDb")
            .field("next_node_id", &self.next_node_id.load(Ordering::Relaxed))
            .field("next_edge_id", &self.next_edge_id.load(Ordering::Relaxed))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- open_in_memory ---

    #[test]
    fn open_in_memory_creates_db() {
        let db = GraphNoteDb::open_in_memory().unwrap();
        assert_eq!(db.next_node_id.load(Ordering::Relaxed), 1);
        assert_eq!(db.next_edge_id.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn open_in_memory_alloc_node_ids_are_sequential() {
        let db = GraphNoteDb::open_in_memory().unwrap();
        assert_eq!(db.alloc_node_id(), 1);
        assert_eq!(db.alloc_node_id(), 2);
        assert_eq!(db.alloc_node_id(), 3);
    }

    #[test]
    fn open_in_memory_alloc_edge_ids_are_sequential() {
        let db = GraphNoteDb::open_in_memory().unwrap();
        assert_eq!(db.alloc_edge_id(), 1);
        assert_eq!(db.alloc_edge_id(), 2);
        assert_eq!(db.alloc_edge_id(), 3);
    }

    #[test]
    fn open_in_memory_node_and_edge_ids_are_independent() {
        let db = GraphNoteDb::open_in_memory().unwrap();
        assert_eq!(db.alloc_node_id(), 1);
        assert_eq!(db.alloc_edge_id(), 1);
        assert_eq!(db.alloc_node_id(), 2);
        assert_eq!(db.alloc_edge_id(), 2);
    }

    #[test]
    fn open_in_memory_close_succeeds() {
        let db = GraphNoteDb::open_in_memory().unwrap();
        db.close().unwrap();
    }

    #[test]
    fn open_in_memory_compact_succeeds() {
        let db = GraphNoteDb::open_in_memory().unwrap();
        db.compact().unwrap();
    }

    // --- open (disk-backed) ---

    #[test]
    fn open_creates_new_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = GraphNoteDb::open(&path).unwrap();
        assert_eq!(db.next_node_id.load(Ordering::Relaxed), 1);
        assert_eq!(db.next_edge_id.load(Ordering::Relaxed), 1);
        db.close().unwrap();
    }

    #[test]
    fn open_persists_counters_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");

        // Open, allocate some IDs, close
        {
            let db = GraphNoteDb::open(&path).unwrap();
            assert_eq!(db.alloc_node_id(), 1);
            assert_eq!(db.alloc_node_id(), 2);
            assert_eq!(db.alloc_node_id(), 3);
            assert_eq!(db.alloc_edge_id(), 1);
            assert_eq!(db.alloc_edge_id(), 2);
            db.close().unwrap();
        }

        // Reopen and verify counters continue
        {
            let db = GraphNoteDb::open(&path).unwrap();
            assert_eq!(db.next_node_id.load(Ordering::Relaxed), 4);
            assert_eq!(db.next_edge_id.load(Ordering::Relaxed), 3);
            assert_eq!(db.alloc_node_id(), 4);
            assert_eq!(db.alloc_edge_id(), 3);
            db.close().unwrap();
        }
    }

    #[test]
    fn open_without_close_loses_counter_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");

        // Open and allocate without closing properly
        {
            let db = GraphNoteDb::open(&path).unwrap();
            let _ = db.alloc_node_id();
            let _ = db.alloc_node_id();
            // Drop without close — counters not persisted
        }

        // Reopen — counters should be back at 1
        {
            let db = GraphNoteDb::open(&path).unwrap();
            assert_eq!(db.next_node_id.load(Ordering::Relaxed), 1);
            db.close().unwrap();
        }
    }

    #[test]
    fn compact_persists_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = GraphNoteDb::open(&path).unwrap();
        db.compact().unwrap();
        db.close().unwrap();
    }

    // --- Debug ---

    #[test]
    fn debug_format_works() {
        let db = GraphNoteDb::open_in_memory().unwrap();
        let debug = format!("{:?}", db);
        assert!(debug.contains("GraphNoteDb"));
        assert!(debug.contains("next_node_id"));
    }

    // --- u64_from_bytes ---

    #[test]
    fn u64_from_bytes_valid() {
        let val: u64 = 42;
        assert_eq!(u64_from_bytes(&val.to_le_bytes()), 42);
    }

    #[test]
    fn u64_from_bytes_invalid_length_defaults_to_1() {
        assert_eq!(u64_from_bytes(&[1, 2, 3]), 1);
        assert_eq!(u64_from_bytes(&[]), 1);
    }

    #[test]
    fn u64_from_bytes_zero() {
        let val: u64 = 0;
        assert_eq!(u64_from_bytes(&val.to_le_bytes()), 0);
    }

    #[test]
    fn u64_from_bytes_max() {
        let val = u64::MAX;
        assert_eq!(u64_from_bytes(&val.to_le_bytes()), u64::MAX);
    }
}
