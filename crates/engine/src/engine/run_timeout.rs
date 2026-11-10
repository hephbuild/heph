//! A target's `timeout`, enforced by the engine around the driver's `run`.
//!
//! The driver only moves the declaration onto the def
//! ([`TargetDef::timeout`](hplugin::driver::targetdef::TargetDef::timeout));
//! everything else is here, so a driver — a cdylib above all — cannot opt out
//! of it by ignoring it, or by ignoring cancellation:
//!
//! 1. The run gets a cancellation token of its own, which the request's token
//!    is forwarded into. A run without a timeout is handed the request's token
//!    directly and pays nothing.
//! 2. When the timeout elapses, that token is cancelled: the driver's own
//!    cancellation path runs (exec kills the process group).
//! 3. The run then has [`GRACE`] to return. Whether it does or not, the
//!    target fails with [`TimedOutError`]; a run still going after the grace
//!    is dropped (abandoned) rather than waited on.
//!
//! A timed-out run is a **failure**, not a cancellation: it is not
//! [`is_cancelled`](hplugin::error::is_cancelled), so its log is kept for the
//! failure diagnostic, and nothing is cached. What the driver returned after
//! being cancelled — usually its own cancellation error — is discarded: it
//! describes the cancellation, not why the target failed.
//!
//! The clock covers the driver's `run` only — not the queue for admission, not
//! dependency resolution, not the sandbox preparation before it. `--shell`
//! never gets one: it is a person at a prompt.
//!
//! Nothing here reaches a hash. A limit does not change what a run that
//! finishes within it produces, so changing it never rebuilds anything.

use hcore::hasync::{Cancellable as _, StdCancellationToken};
use hmodel::htaddr::Addr;
use hplugin::driver::Timeout;
use std::fmt;
use std::future::Future;
use std::time::Duration;

/// How long a run past its timeout has, once cancelled, to return before the
/// engine stops waiting for it.
///
/// Long enough for a driver that honours cancellation to kill a process tree
/// and collect it; short enough that one that does not cannot hold a build
/// hostage.
pub const GRACE: Duration = Duration::from_secs(10);

/// A run outlasted its target's `timeout`.
#[derive(Debug, Clone)]
pub struct TimedOutError {
    pub addr: Addr,
    pub timeout: Timeout,
    /// `Some(grace)` when the run did not return within `grace` of being
    /// cancelled, and was dropped instead.
    pub abandoned_after: Option<Duration>,
}

impl fmt::Display for TimedOutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} timed out after {} (its `timeout`)",
            self.addr.format(),
            self.timeout
        )?;
        if let Some(grace) = self.abandoned_after {
            write!(
                f,
                "; its driver did not stop within {} of being cancelled, and was abandoned",
                hcore::units::human_duration(grace)
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for TimedOutError {}

/// True if `e` (anywhere in its source chain) is a [`TimedOutError`].
pub fn is_timed_out(e: &anyhow::Error) -> bool {
    hcore::hmemoizer::downcast_chain_ref::<TimedOutError>(e).is_some()
}

/// Await `run`, which was started with `run_token`, holding it to `timeout`.
///
/// `parent` (the request's token) cancelling is forwarded into `run_token`,
/// and the run's own result — a cancellation, normally — is returned as is:
/// a request that was cancelled did not time out. Past the timeout,
/// `run_token` is cancelled and the run gets `grace` to return before it is
/// dropped; either way the result is [`TimedOutError`].
pub(crate) async fn enforce<T>(
    run: impl Future<Output = anyhow::Result<T>>,
    run_token: &StdCancellationToken,
    parent: &StdCancellationToken,
    addr: &Addr,
    timeout: Timeout,
    grace: Duration,
) -> anyhow::Result<T> {
    tokio::pin!(run);
    // Biased: a run that finished is reported as finished, however late the
    // other two branches became ready in the same poll.
    tokio::select! {
        biased;
        out = &mut run => return out,
        () = parent.cancelled() => {
            run_token.cancel();
            return run.await;
        }
        () = tokio::time::sleep(timeout.duration()) => {}
    }

    run_token.cancel();
    let abandoned_after = match tokio::time::timeout(grace, &mut run).await {
        Ok(_discarded) => None,
        Err(_elapsed) => {
            tracing::warn!(
                addr = %addr.format(),
                %timeout,
                grace_ms = u64::try_from(grace.as_millis()).unwrap_or(u64::MAX),
                "timed-out run did not stop after cancellation; abandoning it"
            );
            Some(grace)
        }
    };
    Err(TimedOutError {
        addr: addr.clone(),
        timeout,
        abandoned_after,
    }
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn secs(n: u64) -> Timeout {
        Timeout::new(Duration::from_secs(n)).expect("timeout")
    }

    fn addr() -> Addr {
        hmodel::htaddr::parse_addr("//p:t").expect("addr")
    }

    /// A run that honours its token: returns a cancellation error once it is
    /// cancelled, or `Ok(())` after `work`.
    async fn polite(token: &StdCancellationToken, work: Duration) -> anyhow::Result<()> {
        tokio::select! {
            () = token.cancelled() => Err(hplugin::error::CancelledError.into()),
            () = tokio::time::sleep(work) => Ok(()),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_within_its_timeout_returns_its_own_result() {
        let parent = StdCancellationToken::new();
        let token = StdCancellationToken::new();
        let out = enforce(
            polite(&token, Duration::from_secs(5)),
            &token,
            &parent,
            &addr(),
            secs(10),
            GRACE,
        )
        .await;
        assert!(out.is_ok(), "{out:?}");
        assert!(!token.is_cancelled());

        // A failure inside the limit is the run's failure, not a timeout.
        let token = StdCancellationToken::new();
        let err = enforce(
            async { Err::<(), _>(anyhow::anyhow!("boom")) },
            &token,
            &parent,
            &addr(),
            secs(10),
            GRACE,
        )
        .await
        .expect_err("fails");
        assert!(!is_timed_out(&err), "{err:#}");
        assert_eq!(format!("{err}"), "boom");
    }

    #[tokio::test(start_paused = true)]
    async fn past_its_timeout_a_run_is_cancelled_and_fails_as_timed_out() {
        let parent = StdCancellationToken::new();
        let token = StdCancellationToken::new();
        let started = tokio::time::Instant::now();
        let err = enforce(
            polite(&token, Duration::from_secs(3600)),
            &token,
            &parent,
            &addr(),
            secs(90),
            GRACE,
        )
        .await
        .expect_err("times out");
        assert!(token.is_cancelled(), "the driver was told to stop");
        assert!(!parent.is_cancelled(), "only this run, not the request");
        assert_eq!(started.elapsed(), Duration::from_secs(90), "no grace used");
        assert!(is_timed_out(&err), "{err:#}");
        // Its own cancellation error is discarded: a timeout is a failure.
        assert!(!hplugin::error::is_cancelled(&err), "{err:#}");
        assert_eq!(
            format!("{err}"),
            "//p:t timed out after 90s (its `timeout`)"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_that_ignores_cancellation_is_abandoned_after_the_grace() {
        let parent = StdCancellationToken::new();
        let token = StdCancellationToken::new();
        let dropped = Arc::new(AtomicBool::new(false));
        struct SetOnDrop(Arc<AtomicBool>);
        impl Drop for SetOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let guard = SetOnDrop(Arc::clone(&dropped));
        let stubborn = async move {
            let _guard = guard;
            std::future::pending::<anyhow::Result<()>>().await
        };
        let started = tokio::time::Instant::now();
        let err = enforce(
            stubborn,
            &token,
            &parent,
            &addr(),
            secs(60),
            Duration::from_secs(10),
        )
        .await
        .expect_err("times out");
        assert_eq!(started.elapsed(), Duration::from_secs(70));
        assert!(dropped.load(Ordering::SeqCst), "the run was dropped");
        let te = hcore::hmemoizer::downcast_chain_ref::<TimedOutError>(&err).expect("timed out");
        assert_eq!(te.abandoned_after, Some(Duration::from_secs(10)));
        assert!(format!("{err}").contains("was abandoned"), "{err}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_request_is_a_cancellation_not_a_timeout() {
        let parent = StdCancellationToken::new();
        let token = StdCancellationToken::new();
        let cancel = parent.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            cancel.cancel();
        });
        let err = enforce(
            polite(&token, Duration::from_secs(3600)),
            &token,
            &parent,
            &addr(),
            secs(60),
            GRACE,
        )
        .await
        .expect_err("cancelled");
        assert!(token.is_cancelled(), "forwarded to the run");
        assert!(hplugin::error::is_cancelled(&err), "{err:#}");
        assert!(!is_timed_out(&err), "{err:#}");
    }
}
