use crate::engine::error::MultiError;
use std::future::Future;

/// In-flight cap for the discovery walks (`Engine::query`,
/// `EngineProviderExecutor::query`, `states_under`).
///
/// Sized to [`hcore::blocking`]'s concurrency limit (`2 * cores`), because
/// those run slots are what the discovery work actually lands on: a package's
/// `list` is a whole-package
/// Starlark evaluation submitted there. This is an *orchestration* bound — the
/// real admission control lives further down (`PKG_EVAL_SLOTS` caps concurrent
/// package evaluations at the core count, in async-land, before queueing) — so
/// its only job is to keep the set of live per-package futures, and the memory
/// they pin, proportional to the machine rather than to the package count.
///
/// **The bound is per walk, and walks nest.** `Engine::query` buffers K
/// packages; a package's `list` may call back through `ListRequest::executor`
/// into `states_under` (plugin-go does, per module root) or
/// `EngineProviderExecutor::query`, each opening its own `buffered(K)`. Live
/// orchestration state is therefore K x depth, not K — on a 16-core machine the
/// reachable two-level case is ~1024 concurrent probe futures rather than 32.
/// What that does *not* multiply is the work: `PKG_EVAL_SLOTS` is a single
/// global semaphore, so concurrent package evaluations stay capped at the core
/// count no matter how deep the nesting. The cost of depth is pinned memory and
/// scheduler pressure, not CPU oversubscription. Bounding the product would
/// need this to become a process-wide semaphore rather than a per-stream cap.
///
/// Deliberately the same expression the engine's other stream-consuming walks
/// use (`labels.rs`, `revdeps.rs`, `deppath.rs`).
pub fn discovery_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .saturating_mul(2)
}

/// `try_join_all` equivalent that honors `fail_fast`.
///
/// - `fail_fast = true`: identical to `futures::future::try_join_all` —
///   short-circuits on the first `Err` and drops the rest of the in-flight
///   futures (current behavior at every fanout site).
/// - `fail_fast = false`: drives every future to completion, then if any
///   failed returns `Err(MultiError(Vec<anyhow::Error>))` carrying every
///   error. Used when the caller wants to see all the failures rather than
///   only the first.
///
/// Hot-path note: keep the `true` branch unchanged — `feedback_engine_fanout_try_join_all`
/// memory pins this primitive after a measured regression with `FuturesUnordered`.
pub async fn join_all_failable<T, F>(
    futs: impl IntoIterator<Item = F>,
    fail_fast: bool,
) -> anyhow::Result<Vec<T>>
where
    F: Future<Output = anyhow::Result<T>>,
{
    if fail_fast {
        return futures::future::try_join_all(futs).await;
    }
    let results = futures::future::join_all(futs).await;
    let mut ok = Vec::with_capacity(results.len());
    let mut errs = Vec::new();
    for r in results {
        match r {
            Ok(v) => ok.push(v),
            Err(e) => errs.push(e),
        }
    }
    if errs.is_empty() {
        Ok(ok)
    } else if errs.len() == 1 {
        // A sole failure surfaces directly: wrapping it renders as
        // "1 errors:\n  [0] …" mid-chain, and every downstream `MultiError`
        // consumer treats a singleton exactly like the bare error anyway.
        Err(errs.pop().expect("len checked"))
    } else {
        Err(MultiError(errs).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::BoxFuture;

    fn boxed<T: Send + 'static>(r: anyhow::Result<T>) -> BoxFuture<'static, anyhow::Result<T>> {
        Box::pin(async move { r })
    }

    #[tokio::test]
    async fn fail_fast_true_short_circuits_first_err() {
        let futs: Vec<BoxFuture<'static, anyhow::Result<i32>>> = vec![
            boxed(Ok(1)),
            boxed(Err(anyhow::anyhow!("boom"))),
            boxed(Ok(3)),
        ];
        let err = join_all_failable(futs, true).await.unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn fail_fast_false_aggregates_all_errs() {
        let futs: Vec<BoxFuture<'static, anyhow::Result<i32>>> = vec![
            boxed(Ok(1)),
            boxed(Err(anyhow::anyhow!("first"))),
            boxed(Err(anyhow::anyhow!("second"))),
        ];
        let err = join_all_failable(futs, false).await.unwrap_err();
        let multi = err
            .downcast_ref::<MultiError>()
            .expect("expected MultiError");
        assert_eq!(multi.0.len(), 2);
        let rendered = format!("{multi}");
        assert!(rendered.contains("first"), "got: {rendered}");
        assert!(rendered.contains("second"), "got: {rendered}");
    }

    /// A sole failure comes back bare — not wrapped in a `MultiError` whose
    /// Display ("1 errors:") is noise mid-chain for the most common case.
    #[tokio::test]
    async fn fail_fast_false_returns_a_sole_error_unwrapped() {
        let futs: Vec<BoxFuture<'static, anyhow::Result<i32>>> =
            vec![boxed(Ok(1)), boxed(Err(anyhow::anyhow!("only")))];
        let err = join_all_failable(futs, false).await.unwrap_err();
        assert!(err.downcast_ref::<MultiError>().is_none());
        assert!(err.to_string().contains("only"));
    }

    #[tokio::test]
    async fn fail_fast_false_all_ok_returns_results() {
        let futs: Vec<BoxFuture<'static, anyhow::Result<i32>>> =
            vec![boxed(Ok(1)), boxed(Ok(2)), boxed(Ok(3))];
        let v = join_all_failable(futs, false).await.unwrap();
        assert_eq!(v, vec![1, 2, 3]);
    }
}
