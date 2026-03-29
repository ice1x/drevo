pub mod backend;
pub mod error;
pub mod memory;

pub use backend::StorageBackend;
pub use error::{Result, StorageError};
pub use memory::MemoryBackend;
