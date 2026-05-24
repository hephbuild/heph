use crate::engine::error::MultiError;
use std::future::Future;

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

    #[tokio::test]
    async fn fail_fast_false_all_ok_returns_results() {
        let futs: Vec<BoxFuture<'static, anyhow::Result<i32>>> =
            vec![boxed(Ok(1)), boxed(Ok(2)), boxed(Ok(3))];
        let v = join_all_failable(futs, false).await.unwrap();
        assert_eq!(v, vec![1, 2, 3]);
    }
}
