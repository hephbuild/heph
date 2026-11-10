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
//!    is dropped (abandoned) rather than waited on. Its process may still be
//!    running: dropping the future stops heph waiting, not the program.
//! 4. However [`enforce`] ends — a result, a timeout, or being dropped
//!    unfinished (fail-fast aborting the target's memo cell) — a run that has
//!    not returned has its token cancelled. A driver that handed the token to
//!    a task of its own (exec's process wait) is never left holding a live
//!    token nothing can reach.
//!
//! A timed-out run is a **failure**, not a cancellation: it is not
//! [`is_cancelled`](hplugin::error::is_cancelled), so its log is kept for the
//! failure diagnostic, and nothing is cached. What the driver returned after
//! being cancelled — usually its own cancellation error — is discarded as the
//! *reason*, since it describes the cancellation rather than why the target
//! failed; the failure artifacts it carried (exec's log) are kept on the
//! [`TimedOutError`], so the failure box shows what the run printed.
//!
//! The clock starts when the driver's `run` is called — not the queue for
//! admission, not dependency resolution, not the engine's sandbox lease and
//! credentials before it. For a managed driver (`exec`, `bash`) `run` is the
//! whole managed layer, so the clock **does** cover staging the inputs into
//! the sandbox and collecting the outputs, not just the command. `--shell`
//! never gets one: it is a person at a prompt.
//!
//! Nothing here reaches a hash. A limit does not change what a run that
//! finishes within it produces, so changing it never rebuilds anything. That
//! holds only while no driver exposes the timeout to the run itself (argv,
//! env, a file): a run that can read its limit can produce something that
//! depends on it.

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
#[derive(Debug)]
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
                "; the run did not stop within {} of being cancelled and was abandoned, so its \
                 process may still be running",
                hcore::units::human_duration(grace)
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for TimedOutError {}

/// True if `e` (anywhere in its source chain) is a [`TimedOutError`].
pub fn is_timed_out(e: &anyhow::Error) -> bool {
    timed_out(e).is_some()
}

/// The [`TimedOutError`] anywhere in `e`'s source chain.
pub fn timed_out(e: &anyhow::Error) -> Option<&TimedOutError> {
    hcore::hmemoizer::downcast_chain_ref::<TimedOutError>(e)
}

/// Cancels the run's token when dropped, unless the run returned first.
///
/// What makes the run token a *child* of [`enforce`] structurally: a caller
/// that drops `enforce` mid-run (fail-fast aborting the memo cell, a
/// `try_join_all` sibling failing) cancels it here, rather than leaving a
/// driver's detached task holding a token no one will ever cancel.
struct CancelOnDrop<'a> {
    token: &'a StdCancellationToken,
    armed: bool,
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
    }
}

/// Await `run`, which was started with `run_token`, holding it to `timeout`.
///
/// `parent` (the request's token) cancelling is forwarded into `run_token`,
/// and the run's own result — a cancellation, normally — is returned as is:
/// a request that was cancelled did not time out. Past the timeout,
/// `run_token` is cancelled and the run gets `grace` to return before it is
/// dropped; either way the result is [`TimedOutError`], carrying the failure
/// artifacts of whatever the run returned. Dropped before it finishes,
/// `run_token` is cancelled.
pub(crate) async fn enforce<T>(
    run: impl Future<Output = anyhow::Result<T>>,
    run_token: &StdCancellationToken,
    parent: &StdCancellationToken,
    addr: &Addr,
    timeout: Timeout,
    grace: Duration,
) -> anyhow::Result<T> {
    let mut guard = CancelOnDrop {
        token: run_token,
        armed: true,
    };
    tokio::pin!(run);
    // Biased, the request's cancellation first: a request already cancelled
    // never gives the driver a poll with a live token. Then the run, so a run
    // that finished is reported as finished however late the timeout became
    // ready in the same poll.
    tokio::select! {
        biased;
        () = parent.cancelled() => {
            run_token.cancel();
            let out = run.await;
            guard.armed = false;
            return out;
        }
        out = &mut run => {
            guard.armed = false;
            return out;
        }
        () = tokio::time::sleep(timeout.duration()) => {}
    }

    run_token.cancel();
    let (abandoned_after, artifacts) = match tokio::time::timeout(grace, &mut run).await {
        // Even an `Ok` is a timeout: the run returned only because it was
        // stopped, and whatever it produced is not what a finished run would.
        Ok(Ok(_discarded)) => (None, Vec::new()),
        Ok(Err(discarded)) => (
            None,
            hplugin::error::failure_artifacts(&discarded)
                .map(|a| a.0.clone())
                .unwrap_or_default(),
        ),
        Err(_elapsed) => {
            tracing::warn!(
                addr = %addr.format(),
                %timeout,
                grace_ms = u64::try_from(grace.as_millis()).unwrap_or(u64::MAX),
                "timed-out run did not stop after cancellation; abandoning it"
            );
            (Some(grace), Vec::new())
        }
    };
    let err = anyhow::Error::new(TimedOutError {
        addr: addr.clone(),
        timeout,
        abandoned_after,
    });
    Err(hplugin::error::with_failure_artifacts(err, artifacts))
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

    fn log_artifact(text: &str) -> hplugin::driver::outputartifact::OutputArtifact {
        use hplugin::driver::outputartifact::{Content, ContentRaw, OutputArtifact, Type};
        OutputArtifact {
            group: String::new(),
            name: "log.txt".to_string(),
            r#type: Type::Log,
            content: Content::Raw(ContentRaw {
                data: text.as_bytes().to_vec(),
                path: "log.txt".to_string(),
                x: false,
            }),
            hashout: String::new(),
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
        assert!(
            !token.is_cancelled(),
            "a finished run's token is left alone"
        );

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

    /// The log the run attached to its cancellation error rides on the
    /// timeout, so the failure slot keeps what it printed — and that does not
    /// make the timeout a cancellation.
    #[tokio::test(start_paused = true)]
    async fn the_discarded_results_failure_artifacts_ride_on_the_timeout() {
        let parent = StdCancellationToken::new();
        let token = StdCancellationToken::new();
        let run = async {
            token.cancelled().await;
            Err::<(), _>(hplugin::error::with_failure_artifacts(
                hplugin::error::CancelledError.into(),
                vec![log_artifact("started\n")],
            ))
        };
        let err = enforce(run, &token, &parent, &addr(), secs(1), GRACE)
            .await
            .expect_err("times out");
        assert!(is_timed_out(&err), "{err:#}");
        assert!(!hplugin::error::is_cancelled(&err), "{err:#}");
        let log = hplugin::error::failure_log(&err).expect("the run's log is kept");
        assert_eq!(log.name, "log.txt");
    }

    /// A run that comes back `Ok` once it is cancelled still timed out: it
    /// returned because it was stopped, not because it finished.
    #[tokio::test(start_paused = true)]
    async fn a_run_that_returns_ok_once_cancelled_is_still_timed_out() {
        let parent = StdCancellationToken::new();
        let token = StdCancellationToken::new();
        let run = async {
            token.cancelled().await;
            anyhow::Ok(())
        };
        let err = enforce(run, &token, &parent, &addr(), secs(5), GRACE)
            .await
            .expect_err("times out");
        let te = timed_out(&err).expect("timed out");
        assert_eq!(te.abandoned_after, None);
    }

    /// The request being cancelled while the run is in its grace changes
    /// nothing: the timeout came first, and it is what the target reports.
    #[tokio::test(start_paused = true)]
    async fn a_request_cancelled_during_the_grace_stays_timed_out() {
        let parent = StdCancellationToken::new();
        let token = StdCancellationToken::new();
        let cancel = parent.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(12)).await;
            cancel.cancel();
        });
        // Ignores its token, so it is still in the grace when the request is
        // cancelled 2s into it.
        let run = async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            anyhow::Ok(())
        };
        let err = enforce(run, &token, &parent, &addr(), secs(10), GRACE)
            .await
            .expect_err("times out");
        assert!(parent.is_cancelled());
        assert!(is_timed_out(&err), "{err:#}");
        assert!(!hplugin::error::is_cancelled(&err), "{err:#}");
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
        let te = timed_out(&err).expect("timed out");
        assert_eq!(te.abandoned_after, Some(Duration::from_secs(10)));
        assert_eq!(
            format!("{err}"),
            "//p:t timed out after 1m (its `timeout`); the run did not stop within 10s of being \
             cancelled and was abandoned, so its process may still be running"
        );
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

    /// A request cancelled before the run is first polled: the driver's very
    /// first poll already sees its token cancelled, so it never starts work
    /// the request no longer wants.
    #[tokio::test(start_paused = true)]
    async fn an_already_cancelled_request_never_polls_the_run_with_a_live_token() {
        let parent = StdCancellationToken::new();
        parent.cancel();
        let token = StdCancellationToken::new();
        let live_on_first_poll = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&live_on_first_poll);
        let run = async {
            seen.store(!token.is_cancelled(), Ordering::SeqCst);
            anyhow::Ok(())
        };
        let out = enforce(run, &token, &parent, &addr(), secs(60), GRACE).await;
        assert!(out.is_ok(), "{out:?}");
        assert!(
            !live_on_first_poll.load(Ordering::SeqCst),
            "the run was polled with a live token"
        );
    }

    /// B1: whoever drops `enforce` before the run returns — fail-fast
    /// aborting the memo cell — cancels the run's token, so a driver task
    /// holding a clone of it (exec's process wait) is not left running.
    #[tokio::test(start_paused = true)]
    async fn dropping_enforce_mid_run_cancels_the_runs_token() {
        let parent = StdCancellationToken::new();
        let token = StdCancellationToken::new();
        let held = token.clone();
        let addr = addr();
        let fut = enforce(
            std::future::pending::<anyhow::Result<()>>(),
            &token,
            &parent,
            &addr,
            secs(60),
            GRACE,
        );
        // Started, then abandoned well before the deadline.
        let polled = tokio::time::timeout(Duration::from_secs(1), fut).await;
        assert!(polled.is_err(), "still running at 1s");
        assert!(held.is_cancelled(), "dropping enforce cancels the run");
        assert!(!parent.is_cancelled(), "only this run, not the request");
    }

    /// The longest timeout the wire can carry does not overflow the deadline
    /// arithmetic; a quick run returns normally.
    #[tokio::test(start_paused = true)]
    async fn the_longest_timeout_does_not_overflow() {
        let parent = StdCancellationToken::new();
        let token = StdCancellationToken::new();
        let longest = Timeout::from_millis(u64::MAX).expect("max");
        let out = enforce(
            polite(&token, Duration::from_millis(5)),
            &token,
            &parent,
            &addr(),
            longest,
            GRACE,
        )
        .await;
        assert!(out.is_ok(), "{out:?}");
    }
}
