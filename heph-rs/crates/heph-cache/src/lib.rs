//! Caching system for Heph build artifacts
//!
//! This module provides content-addressable storage (CAS) for build artifacts,
//! cache key computation, and cache management with statistics.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use thiserror::Error;

pub mod lru;

#[derive(Error, Debug)]
pub enum CacheError {
    #[error("Cache key not found: {0}")]
    NotFound(String),

    #[error("Cache is full")]
    CacheFull,

    #[error("Invalid cache key: {0}")]
    InvalidKey(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("KV store error: {0}")]
    KvError(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CacheError>;

/// Content-addressable key (SHA-256 hash)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CacheKey(String);

impl CacheKey {
    /// Create a new cache key from a string
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// Compute cache key from input data
    pub fn from_bytes(data: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(data);
        let hash = hasher.finalize();
        Self(hex::encode(hash))
    }

    /// Compute cache key from multiple inputs
    pub fn from_inputs(inputs: &[&[u8]]) -> Self {
        let mut hasher = Sha256::new();
        for input in inputs {
            hasher.update(input);
        }
        let hash = hasher.finalize();
        Self(hex::encode(hash))
    }

    /// Get the key as a string
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Validate that the key is a valid SHA-256 hash
    pub fn validate(&self) -> Result<()> {
        if self.0.len() != 64 {
            return Err(CacheError::InvalidKey(format!(
                "Invalid length: expected 64, got {}",
                self.0.len()
            )));
        }

        if !self.0.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(CacheError::InvalidKey(
                "Key contains non-hexadecimal characters".to_string(),
            ));
        }

        Ok(())
    }
}

impl std::fmt::Display for CacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Cache entry with metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// The cached data
    pub data: Vec<u8>,
    /// Size in bytes
    pub size: usize,
    /// Timestamp when cached
    pub timestamp: u64,
    /// Optional metadata
    pub metadata: HashMap<String, String>,
}

impl CacheEntry {
    /// Create a new cache entry
    pub fn new(data: Vec<u8>) -> Self {
        let size = data.len();
        Self {
            data,
            size,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            metadata: HashMap::new(),
        }
    }

    /// Create with metadata
    pub fn with_metadata(data: Vec<u8>, metadata: HashMap<String, String>) -> Self {
        let mut entry = Self::new(data);
        entry.metadata = metadata;
        entry
    }
}

/// Cache statistics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheStats {
    /// Total cache hits
    pub hits: u64,
    /// Total cache misses
    pub misses: u64,
    /// Total items in cache
    pub entries: u64,
    /// Total size in bytes
    pub total_size: u64,
    /// Number of evictions
    pub evictions: u64,
}

impl CacheStats {
    /// Calculate hit rate (0.0 to 1.0)
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }

    /// Record a cache hit
    pub fn record_hit(&mut self) {
        self.hits += 1;
    }

    /// Record a cache miss
    pub fn record_miss(&mut self) {
        self.misses += 1;
    }

    /// Record an eviction
    pub fn record_eviction(&mut self) {
        self.evictions += 1;
        if self.entries > 0 {
            self.entries -= 1;
        }
    }
}

/// Local cache implementation using content-addressable storage
pub struct LocalCache {
    /// KV store backend
    store: Box<dyn heph_kv::KvStore>,
    /// Cache statistics
    stats: Arc<Mutex<CacheStats>>,
    /// Cache directory
    cache_dir: PathBuf,
}

impl LocalCache {
    /// Create a new local cache
    pub fn new(cache_dir: impl AsRef<Path>) -> Result<Self> {
        let cache_dir = cache_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&cache_dir)?;

        let db_path = cache_dir.join("cache.db");
        let store = Box::new(
            heph_kv::sqlite::SqliteKvStore::open(&db_path)
                .map_err(|e: heph_kv::KvError| CacheError::KvError(e.to_string()))?,
        );

        Ok(Self {
            store,
            stats: Arc::new(Mutex::new(CacheStats::default())),
            cache_dir,
        })
    }

    /// Get an entry from the cache
    pub fn get(&self, key: &CacheKey) -> Result<Option<CacheEntry>> {
        key.validate()?;

        match self.store.get(key.as_str().as_bytes()) {
            Ok(Some(data)) => {
                let entry: CacheEntry = serde_json::from_slice(&data)?;
                self.stats.lock().unwrap().record_hit();
                Ok(Some(entry))
            }
            Ok(None) => {
                self.stats.lock().unwrap().record_miss();
                Ok(None)
            }
            Err(e) => Err(CacheError::KvError(e.to_string())),
        }
    }

    /// Set an entry in the cache
    pub fn set(&self, key: &CacheKey, entry: CacheEntry) -> Result<()> {
        key.validate()?;

        let data = serde_json::to_vec(&entry)?;
        self.store
            .set(key.as_str().as_bytes(), &data)
            .map_err(|e| CacheError::KvError(e.to_string()))?;

        let mut stats = self.stats.lock().unwrap();
        stats.entries += 1;
        stats.total_size += entry.size as u64;

        Ok(())
    }

    /// Check if a key exists in the cache
    pub fn exists(&self, key: &CacheKey) -> Result<bool> {
        key.validate()?;

        self.store
            .exists(key.as_str().as_bytes())
            .map_err(|e| CacheError::KvError(e.to_string()))
    }

    /// Delete an entry from the cache
    pub fn delete(&self, key: &CacheKey) -> Result<()> {
        key.validate()?;

        // Get entry to update stats
        if let Some(entry) = self.get(key)? {
            self.store
                .delete(key.as_str().as_bytes())
                .map_err(|e| CacheError::KvError(e.to_string()))?;

            let mut stats = self.stats.lock().unwrap();
            if stats.entries > 0 {
                stats.entries -= 1;
            }
            if stats.total_size >= entry.size as u64 {
                stats.total_size -= entry.size as u64;
            }
        }

        Ok(())
    }

    /// Get cache statistics
    pub fn stats(&self) -> CacheStats {
        self.stats.lock().unwrap().clone()
    }

    /// Clear all cache entries
    pub fn clear(&self) -> Result<()> {
        // Note: This is a simplified implementation
        // A real implementation would iterate and delete all keys
        let mut stats = self.stats.lock().unwrap();
        stats.entries = 0;
        stats.total_size = 0;
        Ok(())
    }

    /// Get cache directory path
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_cache_key_from_bytes() {
        let data = b"hello world";
        let key = CacheKey::from_bytes(data);

        // SHA-256 hash should be 64 hex characters
        assert_eq!(key.as_str().len(), 64);
        assert!(key.validate().is_ok());

        // Same input should produce same key
        let key2 = CacheKey::from_bytes(data);
        assert_eq!(key, key2);
    }

    #[test]
    fn test_cache_key_from_inputs() {
        let inputs = vec![b"hello".as_slice(), b"world".as_slice()];
        let key = CacheKey::from_inputs(&inputs);

        assert_eq!(key.as_str().len(), 64);
        assert!(key.validate().is_ok());
    }

    #[test]
    fn test_cache_key_validation() {
        // Valid key
        let valid = CacheKey::new("a".repeat(64));
        assert!(valid.validate().is_ok());

        // Invalid length
        let invalid_len = CacheKey::new("abc");
        assert!(invalid_len.validate().is_err());

        // Invalid characters
        let invalid_chars = CacheKey::new("z".repeat(64));
        assert!(invalid_chars.validate().is_err());
    }

    #[test]
    fn test_cache_entry_creation() {
        let data = b"test data".to_vec();
        let entry = CacheEntry::new(data.clone());

        assert_eq!(entry.data, data);
        assert_eq!(entry.size, 9);
        assert!(entry.timestamp > 0);
        assert!(entry.metadata.is_empty());
    }

    #[test]
    fn test_cache_stats() {
        let mut stats = CacheStats::default();

        stats.record_hit();
        stats.record_hit();
        stats.record_miss();

        assert_eq!(stats.hits, 2);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hit_rate(), 2.0 / 3.0);
    }

    #[test]
    fn test_local_cache_set_get() {
        let temp_dir = TempDir::new().unwrap();
        let cache = LocalCache::new(temp_dir.path()).unwrap();

        let key = CacheKey::from_bytes(b"test");
        let data = b"cached data".to_vec();
        let entry = CacheEntry::new(data.clone());

        // Set entry
        cache.set(&key, entry.clone()).unwrap();

        // Get entry
        let retrieved = cache.get(&key).unwrap().unwrap();
        assert_eq!(retrieved.data, data);
        assert_eq!(retrieved.size, 11);
    }

    #[test]
    fn test_local_cache_miss() {
        let temp_dir = TempDir::new().unwrap();
        let cache = LocalCache::new(temp_dir.path()).unwrap();

        let key = CacheKey::from_bytes(b"nonexistent");
        let result = cache.get(&key).unwrap();

        assert!(result.is_none());

        let stats = cache.stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 0);
    }

    #[test]
    fn test_local_cache_exists() {
        let temp_dir = TempDir::new().unwrap();
        let cache = LocalCache::new(temp_dir.path()).unwrap();

        let key = CacheKey::from_bytes(b"test");
        let entry = CacheEntry::new(b"data".to_vec());

        assert!(!cache.exists(&key).unwrap());

        cache.set(&key, entry).unwrap();

        assert!(cache.exists(&key).unwrap());
    }

    #[test]
    fn test_local_cache_delete() {
        let temp_dir = TempDir::new().unwrap();
        let cache = LocalCache::new(temp_dir.path()).unwrap();

        let key = CacheKey::from_bytes(b"test");
        let entry = CacheEntry::new(b"data".to_vec());

        cache.set(&key, entry).unwrap();
        assert!(cache.exists(&key).unwrap());

        cache.delete(&key).unwrap();
        assert!(!cache.exists(&key).unwrap());
    }

    #[test]
    fn test_local_cache_stats() {
        let temp_dir = TempDir::new().unwrap();
        let cache = LocalCache::new(temp_dir.path()).unwrap();

        let key1 = CacheKey::from_bytes(b"test1");
        let key2 = CacheKey::from_bytes(b"test2");
        let entry = CacheEntry::new(b"data".to_vec());

        cache.set(&key1, entry.clone()).unwrap();
        cache.set(&key2, entry).unwrap();

        // Trigger hit and miss
        let _ = cache.get(&key1).unwrap();
        let _ = cache.get(&CacheKey::from_bytes(b"nonexistent")).unwrap();

        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.entries, 2);
        assert!(stats.total_size > 0);
    }
}
