use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

/// Returned when `Memoizer::once` detects same-task self-recursion on the same key —
/// the in-flight future would await itself, deadlocking the runtime. Callers should
/// catch this via [`downcast_chain_ref`] and treat it as a dependency cycle (e.g.
/// `EngineProviderExecutor::query` skips the offending addr).
#[derive(Debug, Clone)]
pub struct MemoizerCycleError {
    pub tag: &'static str,
    pub key: String,
}

impl fmt::Display for MemoizerCycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "memoizer self-recursion: tag={} key={}",
            self.tag, self.key
        )
    }
}

impl std::error::Error for MemoizerCycleError {}

tokio::task_local! {
    /// Per-call-chain set of (tag, key) currently being computed. Scoped via
    /// `tokio::task_local::scope` so sibling futures in `try_join_all` don't see
    /// each other's keys — only the *recursive* descendants of a given `once`
    /// inherit the parent's set. Re-entry on a key already in scope = cycle.
    static IN_FLIGHT: HashSet<(&'static str, String)>;
}

fn check_recursion(entry: &(&'static str, String)) -> bool {
    IN_FLIGHT.try_with(|s| s.contains(entry)).unwrap_or(false)
}

fn scope_with_entry(entry: (&'static str, String)) -> HashSet<(&'static str, String)> {
    let mut new_set = IN_FLIGHT.try_with(|s| s.clone()).unwrap_or_default();
    new_set.insert(entry);
    new_set
}

/// Transparent wrapper that lets multiple memoizer waiters share an anyhow::Error
/// without losing access to typed downcasting. Display delegates to the inner
/// error; `source()` exposes the inner anyhow::Error so `anyhow::Error::chain()`
/// walks into it, and concrete types can be retrieved with [`downcast_chain_ref`].
#[derive(Debug)]
struct SharedAnyhow(Arc<anyhow::Error>);

impl fmt::Display for SharedAnyhow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Always render the inner anyhow::Error with its full chain. anyhow's
        // chain formatter walks `Error::source()` and writes each link via `{}`
        // (non-alternate), which would otherwise stop at the top of the inner
        // error and drop nested causes. We have no source(), so this is the
        // only place the inner chain gets rendered.
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for SharedAnyhow {
    // Intentionally no source(): exposing the inner anyhow::Error as a source
    // would make standard chain formatters (including anyhow::Error's alternate
    // formatter) print every cause twice. Use [`downcast_chain_ref`] to retrieve
    // concrete error types from a memoizer-returned error.
}

/// Unwrap `Arc<anyhow::Error>` back to `anyhow::Error`.
///
/// If the Arc is uniquely owned, the original error (with full type info for
/// top-level downcasting) is recovered. Otherwise, the error is wrapped in a
/// transparent [`SharedAnyhow`] adapter that exposes the inner error via
/// `Error::source()`. Top-level `downcast_ref` won't find concrete types in the
/// shared case — use [`downcast_chain_ref`] to inspect the chain.
pub fn unwrap_arc_err(arc: Arc<anyhow::Error>) -> anyhow::Error {
    Arc::try_unwrap(arc).unwrap_or_else(|arc| anyhow::Error::new(SharedAnyhow(arc)))
}

/// Recover a typed error reference from an `anyhow::Error` even if the error
/// has been routed through a memoizer (and therefore through [`unwrap_arc_err`]).
///
/// Use this instead of `e.downcast_ref::<T>()` when the error may have come
/// from a [`Memoizer::once`] result; otherwise the top-level type is the
/// internal `SharedAnyhow` wrapper and a direct downcast would fail.
pub fn downcast_chain_ref<T: std::error::Error + Send + Sync + 'static>(
    e: &anyhow::Error,
) -> Option<&T> {
    let mut cur = e;
    loop {
        if let Some(t) = cur.downcast_ref::<T>() {
            return Some(t);
        }
        match cur.downcast_ref::<SharedAnyhow>() {
            Some(shared) => cur = &shared.0,
            None => return None,
        }
    }
}

pub struct Memoizer<K, V> {
    cache: Mutex<HashMap<K, Shared<BoxFuture<'static, V>>>>,
    /// Tag used in stall warnings to identify which memoizer is stuck.
    tag: &'static str,
}

impl<K, V> Default for Memoizer<K, V>
where
    K: std::hash::Hash + Eq + Send + Sync + 'static + fmt::Debug + Clone,
    V: Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

/// If a memoizer await takes longer than this, we panic with the key info to
/// surface a likely deadlock instead of hanging forever. Off by default — set
/// `RHEPH_MEMOIZER_STALL_SECS=<seconds>` to enable when debugging.
fn stall_threshold() -> Option<Duration> {
    let secs: u64 = std::env::var("RHEPH_MEMOIZER_STALL_SECS")
        .ok()
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if secs == 0 {
        None
    } else {
        Some(Duration::from_secs(secs))
    }
}

impl<K, V> Memoizer<K, V>
where
    K: std::hash::Hash + Eq + Send + Sync + 'static + fmt::Debug + Clone,
    V: Clone + Send + Sync + 'static,
{
    pub fn new() -> Self {
        Self::with_tag("memoizer")
    }

    pub fn with_tag(tag: &'static str) -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            tag,
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
            return await_with_stall_check(shared, &key, self.tag).await;
        }

        // Phase 2: create candidate, then re-lock to insert.
        // Another caller may have inserted between phase 1 and phase 2;
        // entry().or_insert_with() handles that race correctly.
        let candidate = f().boxed().shared();
        let shared = {
            let mut cache = self.cache.lock().expect("memoizer lock poisoned");
            cache
                .entry(key.clone())
                .or_insert_with(|| candidate)
                .clone()
        };

        await_with_stall_check(shared, &key, self.tag).await
    }
}

async fn await_with_stall_check<V, K>(
    shared: Shared<BoxFuture<'static, V>>,
    key: &K,
    tag: &'static str,
) -> V
where
    V: Clone,
    K: fmt::Debug,
{
    let Some(threshold) = stall_threshold() else {
        return shared.await;
    };
    match tokio::time::timeout(threshold, shared).await {
        Ok(v) => v,
        Err(_) => {
            // Debug-only: opt-in via RHEPH_MEMOIZER_STALL_SECS. Panic surfaces a
            // suspected deadlock (silent self-recursion past the cycle detector)
            // with the offending key so it can be diagnosed.
            #[expect(
                clippy::panic,
                reason = "debug-only stall detector; opt-in via env var"
            )]
            {
                panic!(
                    "[memoizer:{tag}] STALLED for {:?} on key={key:?} — likely self-recursion on same key. \
                     Unset RHEPH_MEMOIZER_STALL_SECS to disable this check.",
                    threshold
                );
            }
        }
    }
}

impl<K, T> Memoizer<K, Result<T, Arc<anyhow::Error>>>
where
    K: std::hash::Hash + Eq + Send + Sync + 'static + fmt::Debug + Clone,
    T: Clone + Send + Sync + 'static,
{
    /// Compute-once memoizer for `anyhow::Result`-returning async closures.
    ///
    /// Wraps errors in `Arc` internally for shareability across concurrent waiters.
    /// Returns `Result<T, Arc<anyhow::Error>>` so callers can inspect the error
    /// (e.g. downcast_ref) before converting to `anyhow::Error` via `unwrap_arc_err`.
    ///
    /// Same-task re-entry on the same key returns [`MemoizerCycleError`] instead of
    /// awaiting the in-flight shared future (which would deadlock). Callers can detect
    /// this via [`downcast_chain_ref::<MemoizerCycleError>`] and treat as a cycle.
    ///
    /// Cycle errors (direct or transitively bubbled up from an inner memoizer call)
    /// are NOT cached — they are context-dependent (only valid for the current call
    /// chain). The cache entry is evicted before returning so a future, non-cyclic
    /// caller can compute the real result.
    pub async fn once<F, Fut>(&self, key: K, f: F) -> Result<T, Arc<anyhow::Error>>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<T>> + Send + 'static,
    {
        let key_str = format!("{:?}", key);
        let entry = (self.tag, key_str);

        if check_recursion(&entry) {
            return Err(Arc::new(anyhow::Error::new(MemoizerCycleError {
                tag: entry.0,
                key: entry.1,
            })));
        }

        let new_scope = scope_with_entry(entry);
        let key_for_evict = key.clone();
        let result = IN_FLIGHT
            .scope(
                new_scope,
                self.process(key, || async move { f().await.map_err(Arc::new) }),
            )
            .await;
        if let Err(arc) = &result
            && downcast_chain_ref::<MemoizerCycleError>(arc).is_some()
        {
            let mut cache = self.cache.lock().expect("memoizer lock poisoned");
            cache.remove(&key_for_evict);
        }
        result
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

    #[derive(Debug, Clone)]
    struct Marker(&'static str);

    impl std::fmt::Display for Marker {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "marker({})", self.0)
        }
    }

    impl std::error::Error for Marker {}

    #[test]
    fn unwrap_arc_err_preserves_type_when_unique() {
        let arc = Arc::new(anyhow::Error::new(Marker("a")));
        let err = unwrap_arc_err(arc);
        assert!(err.downcast_ref::<Marker>().is_some());
    }

    #[test]
    fn unwrap_arc_err_shared_arc_preserves_type_via_chain() {
        let arc = Arc::new(anyhow::Error::new(Marker("b")));
        let _keepalive = Arc::clone(&arc); // force shared, blocks try_unwrap
        let err = unwrap_arc_err(arc);
        // Top-level downcast does not see Marker (wrapped in SharedAnyhow).
        assert!(err.downcast_ref::<Marker>().is_none());
        // Chain-walking downcast does find it.
        assert!(downcast_chain_ref::<Marker>(&err).is_some());
    }

    #[test]
    fn downcast_chain_ref_walks_nested_shared_anyhow() {
        // Simulate two stacked memoizers: inner returns CycleError, outer
        // wraps it via unwrap_arc_err, then is itself memoized + unwrapped.
        let inner_arc = Arc::new(anyhow::Error::new(Marker("nested")));
        let _inner_keep = Arc::clone(&inner_arc);
        let outer_err = unwrap_arc_err(inner_arc);
        let outer_arc = Arc::new(outer_err);
        let _outer_keep = Arc::clone(&outer_arc);
        let err = unwrap_arc_err(outer_arc);

        // Two nested SharedAnyhow layers — single-level peek would miss it.
        assert!(err.downcast_ref::<Marker>().is_none());
        assert!(err.downcast_ref::<SharedAnyhow>().is_some());
        assert!(downcast_chain_ref::<Marker>(&err).is_some());
    }

    #[test]
    fn shared_anyhow_display_matches_inner() {
        let arc = Arc::new(anyhow::Error::new(Marker("c")));
        let _keepalive = Arc::clone(&arc);
        let err = unwrap_arc_err(arc);
        assert_eq!(format!("{err:#}"), "marker(c)");
    }
}
