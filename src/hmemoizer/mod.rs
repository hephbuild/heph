use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct Memoizer<K, V> {
    cache: Mutex<HashMap<K, V>>,
}

impl<K, V> Memoizer<K, V>
where
    K: std::hash::Hash + Eq + Clone,
    V: Clone,
{
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn get_or_init<F>(&self, key: K, f: F) -> V
    where
        F: FnOnce() -> V,
    {
        let mut cache = self.cache.lock().unwrap();

        // If the key exists, return it. Otherwise, execute f() and insert.
        cache.entry(key).or_insert_with(f).clone()
    }
}
