use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;

/// Unwrap `Arc<anyhow::Error>` back to `anyhow::Error`. If the Arc is uniquely owned, the
/// original error (with full type info for downcasting) is recovered. If multiple clones exist
/// (concurrent memoizer waiters), the error is reconstructed from its display string.
pub fn unwrap_arc_err(arc: Arc<anyhow::Error>) -> anyhow::Error {
    Arc::try_unwrap(arc).unwrap_or_else(|arc| anyhow::anyhow!("{:#}", arc))
}

pub struct Memoizer<K, V> {
    cache: Mutex<HashMap<K, Shared<BoxFuture<'static, V>>>>,
}

impl<K, V> Default for Memoizer<K, V>
where
    K: std::hash::Hash + Eq + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> Memoizer<K, V>
where
    K: std::hash::Hash + Eq + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub async fn process<F, Fut>(&self, key: K, f: F) -> V
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = V> + Send + 'static,
    {
        // Phase 1: fast path — check without creating the future.
        // Block ensures MutexGuard is dropped before any await.
        let existing = {
            let cache = self.cache.lock().expect("memoizer lock poisoned");
            cache.get(&key).cloned()
        };

        if let Some(shared) = existing {
            return shared.await;
        }

        // Phase 2: create candidate, then re-lock to insert.
        // Another caller may have inserted between phase 1 and phase 2;
        // entry().or_insert_with() handles that race correctly.
        let candidate = f().boxed().shared();
        let shared = {
            let mut cache = self.cache.lock().expect("memoizer lock poisoned");
            cache.entry(key).or_insert_with(|| candidate).clone()
        };

        shared.await
    }
}

impl<K, T> Memoizer<K, Result<T, Arc<anyhow::Error>>>
where
    K: std::hash::Hash + Eq + Send + Sync + 'static,
    T: Clone + Send + Sync + 'static,
{
    /// Compute-once memoizer for `anyhow::Result`-returning async closures.
    ///
    /// Wraps errors in `Arc` internally for shareability across concurrent waiters.
    /// Returns `Result<T, Arc<anyhow::Error>>` so callers can inspect the error
    /// (e.g. downcast_ref) before converting to `anyhow::Error` via `unwrap_arc_err`.
    pub async fn once<F, Fut>(&self, key: K, f: F) -> Result<T, Arc<anyhow::Error>>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<T>> + Send + 'static,
    {
        self.process(key, || async move { f().await.map_err(Arc::new) })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use enclose::enclose;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::{Duration, sleep};

    #[tokio::test]
    async fn test_memoizer() {
        let memo: Memoizer<String, Result<Arc<i32>, Arc<anyhow::Error>>> = Memoizer::new();
        let counter = Arc::new(AtomicUsize::new(0));

        let key = "test".to_string();

        let f1 = {
            let memo = &memo;
            enclose!((counter, key) async move {
                memo.once(key, move || async move {
                    sleep(Duration::from_millis(100)).await;
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(Arc::new(42))
                }).await
            })
        };

        let f2 = {
            let memo = &memo;
            enclose!((counter, key) async move {
                memo.once(key, move || async move {
                    sleep(Duration::from_millis(100)).await;
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(Arc::new(42))
                }).await
            })
        };

        let (res1, res2) = tokio::join!(f1, f2);

        assert_eq!(*res1.unwrap(), 42);
        assert_eq!(*res2.unwrap(), 42);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
}
