//! Key-value storage abstraction

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use parking_lot::RwLock;
use thiserror::Error;

pub mod sqlite;

#[derive(Error, Debug)]
pub enum KvError {
    #[error("Key not found: {0}")]
    NotFound(String),

    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("Database error: {0}")]
    Database(String),
}

pub type Result<T> = std::result::Result<T, KvError>;

/// Key-value storage trait
pub trait KvStore: Send + Sync {
    /// Get value by key
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// Set key-value pair
    fn set(&self, key: &[u8], value: &[u8]) -> Result<()>;

    /// Delete key
    fn delete(&self, key: &[u8]) -> Result<()>;

    /// Check if key exists
    fn exists(&self, key: &[u8]) -> Result<bool> {
        Ok(self.get(key)?.is_some())
    }
}

/// Batch operations trait
pub trait KvBatch {
    /// Write batch atomically
    fn write_batch(&self, ops: Vec<BatchOp>) -> Result<()>;
}

#[derive(Debug, Clone)]
pub enum BatchOp {
    Set { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
}

/// In-memory key-value store (for testing)
#[derive(Clone)]
pub struct MemoryKvStore {
    data: Arc<RwLock<HashMap<Vec<u8>, Vec<u8>>>>,
}

impl MemoryKvStore {
    pub fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for MemoryKvStore {
    fn default() -> Self {
        Self::new()
    }
}

impl KvStore for MemoryKvStore {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let data = self.data.read();
        Ok(data.get(key).cloned())
    }

    fn set(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let mut data = self.data.write();
        data.insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn delete(&self, key: &[u8]) -> Result<()> {
        let mut data = self.data.write();
        data.remove(key);
        Ok(())
    }
}
