//! LRU (Least Recently Used) cache eviction policy

use crate::{CacheEntry, CacheError, CacheKey, Result};
use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

/// LRU cache with automatic eviction
pub struct LruLocalCache {
    /// LRU cache
    cache: Arc<Mutex<LruCache<String, CacheEntry>>>,
    /// Maximum capacity (number of entries)
    capacity: usize,
    /// Total evictions
    evictions: Arc<Mutex<u64>>,
}

impl LruLocalCache {
    /// Create a new LRU cache with specified capacity
    pub fn new(capacity: usize) -> Result<Self> {
        let capacity_nz = NonZeroUsize::new(capacity)
            .ok_or_else(|| CacheError::InvalidKey("Capacity must be > 0".to_string()))?;

        Ok(Self {
            cache: Arc::new(Mutex::new(LruCache::new(capacity_nz))),
            capacity,
            evictions: Arc::new(Mutex::new(0)),
        })
    }

    /// Get an entry from the cache
    pub fn get(&self, key: &CacheKey) -> Result<Option<CacheEntry>> {
        key.validate()?;

        let mut cache = self.cache.lock().unwrap();
        Ok(cache.get(key.as_str()).cloned())
    }

    /// Set an entry in the cache (may trigger eviction)
    pub fn set(&self, key: &CacheKey, entry: CacheEntry) -> Result<()> {
        key.validate()?;

        let mut cache = self.cache.lock().unwrap();

        // Check if we'll evict an item
        if cache.len() >= self.capacity && !cache.contains(key.as_str()) {
            let mut evictions = self.evictions.lock().unwrap();
            *evictions += 1;
        }

        cache.put(key.as_str().to_string(), entry);

        Ok(())
    }

    /// Check if a key exists in the cache
    pub fn exists(&self, key: &CacheKey) -> Result<bool> {
        key.validate()?;

        let cache = self.cache.lock().unwrap();
        Ok(cache.contains(key.as_str()))
    }

    /// Remove an entry from the cache
    pub fn remove(&self, key: &CacheKey) -> Result<Option<CacheEntry>> {
        key.validate()?;

        let mut cache = self.cache.lock().unwrap();
        Ok(cache.pop(key.as_str()))
    }

    /// Get the current number of entries
    pub fn len(&self) -> usize {
        let cache = self.cache.lock().unwrap();
        cache.len()
    }

    /// Check if the cache is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get the capacity
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Get total evictions
    pub fn evictions(&self) -> u64 {
        *self.evictions.lock().unwrap()
    }

    /// Clear all entries
    pub fn clear(&self) {
        let mut cache = self.cache.lock().unwrap();
        cache.clear();
    }

    /// Get all keys (for testing)
    pub fn keys(&self) -> Vec<String> {
        let cache = self.cache.lock().unwrap();
        cache.iter().map(|(k, _)| k.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lru_cache_basic() {
        let cache = LruLocalCache::new(2).unwrap();

        let key1 = CacheKey::from_bytes(b"key1");
        let key2 = CacheKey::from_bytes(b"key2");
        let entry1 = CacheEntry::new(b"data1".to_vec());
        let entry2 = CacheEntry::new(b"data2".to_vec());

        cache.set(&key1, entry1.clone()).unwrap();
        cache.set(&key2, entry2.clone()).unwrap();

        assert_eq!(cache.len(), 2);
        assert!(cache.exists(&key1).unwrap());
        assert!(cache.exists(&key2).unwrap());
    }

    #[test]
    fn test_lru_eviction() {
        let cache = LruLocalCache::new(2).unwrap();

        let key1 = CacheKey::from_bytes(b"key1");
        let key2 = CacheKey::from_bytes(b"key2");
        let key3 = CacheKey::from_bytes(b"key3");
        let entry = CacheEntry::new(b"data".to_vec());

        cache.set(&key1, entry.clone()).unwrap();
        cache.set(&key2, entry.clone()).unwrap();
        cache.set(&key3, entry).unwrap(); // This should evict key1

        assert_eq!(cache.len(), 2);
        assert!(!cache.exists(&key1).unwrap()); // key1 was evicted
        assert!(cache.exists(&key2).unwrap());
        assert!(cache.exists(&key3).unwrap());
        assert_eq!(cache.evictions(), 1);
    }

    #[test]
    fn test_lru_access_updates_order() {
        let cache = LruLocalCache::new(2).unwrap();

        let key1 = CacheKey::from_bytes(b"key1");
        let key2 = CacheKey::from_bytes(b"key2");
        let key3 = CacheKey::from_bytes(b"key3");
        let entry = CacheEntry::new(b"data".to_vec());

        cache.set(&key1, entry.clone()).unwrap();
        cache.set(&key2, entry.clone()).unwrap();

        // Access key1 to make it recently used
        let _ = cache.get(&key1).unwrap();

        // Add key3, which should evict key2 (not key1)
        cache.set(&key3, entry).unwrap();

        assert!(cache.exists(&key1).unwrap()); // key1 is still there
        assert!(!cache.exists(&key2).unwrap()); // key2 was evicted
        assert!(cache.exists(&key3).unwrap());
    }

    #[test]
    fn test_lru_remove() {
        let cache = LruLocalCache::new(2).unwrap();

        let key = CacheKey::from_bytes(b"key");
        let entry = CacheEntry::new(b"data".to_vec());

        cache.set(&key, entry).unwrap();
        assert_eq!(cache.len(), 1);

        let removed = cache.remove(&key).unwrap();
        assert!(removed.is_some());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_lru_clear() {
        let cache = LruLocalCache::new(10).unwrap();

        for i in 0..5 {
            let key = CacheKey::from_bytes(format!("key{}", i).as_bytes());
            let entry = CacheEntry::new(b"data".to_vec());
            cache.set(&key, entry).unwrap();
        }

        assert_eq!(cache.len(), 5);

        cache.clear();
        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());
    }

    #[test]
    fn test_lru_zero_capacity() {
        let result = LruLocalCache::new(0);
        assert!(result.is_err());
    }
}
