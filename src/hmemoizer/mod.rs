use std::collections::HashMap;
use std::sync::Arc;
use std::fmt;
use tokio::sync::Mutex;
use futures::future::{BoxFuture, Shared};
use futures::FutureExt;
use std::future::Future;

#[derive(Clone)]
pub struct WrappedError(pub Arc<anyhow::Error>);

impl fmt::Debug for WrappedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for WrappedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for WrappedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

impl From<anyhow::Error> for WrappedError {
    fn from(e: anyhow::Error) -> Self {
        Self(Arc::new(e))
    }
}

pub struct Memoizer<K, V> {
    cache: Mutex<HashMap<K, Shared<BoxFuture<'static, V>>>>,
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

    pub async fn process<Arg, F, Fut>(&self, key: K, arg: Arg, f: F) -> V
    where
        Arg: Send + 'static,
        F: FnOnce(Arg) -> Fut,
        Fut: Future<Output = V> + Send + 'static,
    {
        let mut cache = self.cache.lock().await;

        if let Some(shared) = cache.get(&key) {
            let shared = shared.clone();
            drop(cache);
            return shared.await;
        }

        let shared = f(arg).boxed().shared();
        cache.insert(key, shared.clone());
        drop(cache);
        shared.await
    }
}

impl<K, T, E> Memoizer<K, Result<T, E>>
where
    K: std::hash::Hash + Eq + Send + Sync + 'static,
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
{
    pub async fn process_result<Arg, F, Fut>(&self, key: K, arg: Arg, f: F) -> Result<T, E>
    where
        Arg: Send + 'static,
        F: FnOnce(Arg) -> Fut,
        Fut: Future<Output = Result<T, E>> + Send + 'static,
    {
        self.process(key, arg, f).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::{sleep, Duration};

    #[tokio::test]
    async fn test_memoizer() {
        let memo: Memoizer<String, Result<Arc<i32>, WrappedError>> = Memoizer::new();
        let counter = Arc::new(AtomicUsize::new(0));

        let key = "test".to_string();

        let f1 = {
            let counter = counter.clone();
            let memo = &memo;
            let key = key.clone();
            async move {
                memo.process_result(key, counter, |counter| async move {
                    sleep(Duration::from_millis(100)).await;
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(Arc::new(42))
                }).await
            }
        };

        let f2 = {
            let counter = counter.clone();
            let memo = &memo;
            let key = key.clone();
            async move {
                memo.process_result(key, counter, |counter| async move {
                    sleep(Duration::from_millis(100)).await;
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(Arc::new(42))
                }).await
            }
        };

        let (res1, res2) = tokio::join!(f1, f2);

        assert_eq!(*res1.unwrap(), 42);
        assert_eq!(*res2.unwrap(), 42);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
}
