//! A target's `timeout`, enforced by the engine around the driver's `run`.
//!
//! The driver only moves the declaration onto the def
//! ([`TargetDef::timeout`](hplugin::driver::targetdef::TargetDef::timeout));
//! everything else is here, so a driver — a cdylib above all — cannot opt out
//! of it by ignoring it, or by ignoring cancellation:
//!
//! 1. The run gets a cancellation token of its own, which its [`RunStop`]
//!    (the request's token, and the abandonment of the execute it runs in) is
//!    forwarded into. A run without a timeout is handed the [`RunStop`]
//!    directly.
//! 2. When the timeout elapses, that token is cancelled, as timed out
//!    ([`Cancellable::timed_out`](hcore::hasync::Cancellable::timed_out)):
//!    the driver's own cancellation path runs (exec kills the process group),
//!    and the managed layer tells the timeout from a request cancel by it.
//! 3. The run then has [`GRACE`] to return. Whether it does or not, the
//!    target fails with [`TimedOutError`]; a run still going after the grace
//!    is dropped (abandoned) rather than waited on. Its process may still be
//!    running: dropping the future stops heph waiting, not the program. So an
//!    abandoned run's sandbox is not removed under it ([`is_abandoned`]): it
//!    is left on disk, its `sandbox` slot back at once, for the target's next
//!    run to reclaim. Credential material is the exception, removed anyway.
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
//! The clock covers the target's own process. A driver that
//! [arms the run clock](hplugin::driver::Driver::arms_run_clock) (`exec`,
//! `bash`) starts it right after spawning the process and stops it when the
//! process exits, so staging the inputs, preparing the runner and collecting
//! the outputs are not timed, any more than the queue for admission,
//! dependency resolution, or the engine's sandbox lease and credentials before
//! `run`. A driver that does not arm it (a cdylib's, whose run the clock does
//! not cross the seam into) is timed from the call to its `run` to its return.
//! `--shell` never gets one: it is a person at a prompt.
//!
//! Nothing here reaches a hash. A limit does not change what a run that
//! finishes within it produces, so changing it never rebuilds anything. That
//! holds only while no driver exposes the timeout to the run itself (argv,
//! env, a file): a run that can read its limit can produce something that
//! depends on it.

use hcore::hasync::{Cancellable, StdCancellationToken};
use hmodel::htaddr::Addr;
use hplugin::driver::{RunClockState, Timeout};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

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
                hcore::units::format_duration(grace)
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

/// `e` is a timeout whose run did not return within the grace and was
/// abandoned: its process may still be running, and writing to its sandbox.
pub fn is_abandoned(e: &anyhow::Error) -> bool {
    timed_out(e).is_some_and(|t| t.abandoned_after.is_some())
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

/// Resolves when the run has been on its clock for `timeout`; never, when the
/// run has not started its clock or has stopped it.
///
/// With no `clock` (a driver that does not arm one) the whole run is on it, so
/// this is `timeout` from now. With one, it waits for the driver to start it,
/// then gives the process `timeout`, unless the driver stops it first. A run
/// that dropped its clock running is still on it: only a stop ends it.
async fn deadline(clock: Option<watch::Receiver<RunClockState>>, timeout: Duration) {
    let Some(mut clock) = clock else {
        return tokio::time::sleep(timeout).await;
    };
    loop {
        // Copied out, so the channel's read guard is not held across an await.
        let state = *clock.borrow_and_update();
        match state {
            RunClockState::Running => break,
            RunClockState::Stopped => return std::future::pending().await,
            RunClockState::Idle => {}
        }
        if clock.changed().await.is_err() {
            // Dropped unstarted: the process never ran, so it cannot outlast
            // its timeout. The run is about to return anyway.
            return std::future::pending().await;
        }
    }
    let stopped = async {
        loop {
            let state = *clock.borrow_and_update();
            if state == RunClockState::Stopped {
                break;
            }
            if clock.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    };
    // Biased toward the stop: a process that exited in the same poll the
    // deadline came due finished within its limit.
    tokio::select! {
        biased;
        () = stopped => std::future::pending().await,
        () = tokio::time::sleep(timeout) => {}
    }
}

/// Aborts [`enforce`]'s watchdog task however `enforce` ends.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The token a run is handed: cancelled when the request's token is, or when
/// nobody wants the execute it runs in any more.
///
/// Fail-fast stops a running sibling by dropping its last caller, which
/// evicts its memo cell and aborts the cell's task. The abort lands at the
/// task's next yield, and a run that holds its thread instead never reaches
/// one while its process lives: exec's output park on macOS is a
/// `block_in_place`. The process then ran to its own end, and a failure it
/// reached after the stop was kept as the target's.
///
/// So the run's token is the pair: the request's token and the cell's
/// [`abandoned_token`](hcore::hmemoizer::abandoned_token), which the memoizer
/// cancels before it aborts the task. Whoever holds a clone (exec's process
/// wait, on a task of its own) hears either one directly, so the driver's
/// cancellation path runs (exec kills the process group) whether or not the
/// run ever yields, and however soon its future is dropped after the abort:
/// nothing has to be forwarded by a task that the same drop could abort.
///
/// Outside a memoized body there is nothing to be abandoned by, and the pair
/// is the request's token alone.
#[derive(Clone)]
pub(crate) struct RunStop {
    request: StdCancellationToken,
    abandoned: Option<StdCancellationToken>,
}

impl RunStop {
    /// Taken on the execute's own task, where its memo cell is visible.
    pub(crate) fn new(request: &StdCancellationToken) -> Self {
        Self {
            request: request.clone(),
            abandoned: hcore::hmemoizer::abandoned_token(),
        }
    }
}

impl Cancellable for RunStop {
    fn is_cancelled(&self) -> bool {
        self.request.is_cancelled() || self.abandoned.as_ref().is_some_and(|a| a.is_cancelled())
    }

    /// A stop is never a timeout: `enforce` times a run out on a token of
    /// its own. The request's token is never cancelled as timed out either;
    /// it is read rather than assumed.
    fn timed_out(&self) -> bool {
        self.request.timed_out()
    }

    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        match &self.abandoned {
            None => self.request.cancelled(),
            Some(abandoned) => Box::pin(async move {
                futures::future::select(self.request.cancelled(), abandoned.cancelled()).await;
            }),
        }
    }

    fn clone_arc(&self) -> Arc<dyn Cancellable + Send + Sync> {
        Arc::new(self.clone())
    }
}

/// Whether the run in progress on this task was told to stop rather than
/// left to finish: the request was cancelled, or the execute it runs in was
/// abandoned (see [`RunStop`]).
pub(crate) fn stop_requested(request: &StdCancellationToken) -> bool {
    request.is_cancelled() || hcore::hmemoizer::is_abandoned()
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
///
/// `clock` is the engine's end of the run's
/// [`RunClock`](hplugin::driver::RunClock) when the driver arms one: the
/// timeout then runs only while the driver says its process runs. `None`
/// times the whole of `run`.
pub(crate) async fn enforce<T, P>(
    run: impl Future<Output = anyhow::Result<T>>,
    run_token: &StdCancellationToken,
    parent: &P,
    addr: &Addr,
    timeout: Timeout,
    grace: Duration,
    clock: Option<watch::Receiver<RunClockState>>,
) -> anyhow::Result<T>
where
    P: Cancellable + Clone + Send + Sync + 'static,
{
    let mut guard = CancelOnDrop {
        token: run_token,
        armed: true,
    };
    // The deadline and the request's cancellation are also watched from a
    // task of their own. A run may hold its thread rather than return
    // `Pending` — exec's output tee on macOS parks in `block_in_place` while
    // the target's pipes are open — and then the `select!` below is not
    // polled until it lets go: without the watchdog a run was stopped only
    // once it closed its output, and one that returned in that same poll
    // was reported as its own result. Cancelling the token is what makes
    // such a run let go (exec's process wait, on a task of its own, kills
    // the process group). Aborted when `enforce` ends, so it never touches
    // the token of a run that finished. It waits on the same deadline as the
    // `select!`, on its own receiver of the run's clock: a clock that never
    // starts, or stops, never times the run out from here either.
    let _watchdog = AbortOnDrop(tokio::spawn(enclose::enclose!(
        (run_token => token, parent, clock => watched) async move {
            tokio::select! {
                biased;
                () = parent.cancelled() => token.cancel(),
                () = deadline(watched, timeout.duration()) => token.cancel_timed_out(),
            }
        }
    )));
    tokio::pin!(run);
    // Biased, the request's cancellation first: a request already cancelled
    // never gives the driver a poll with a live token. Then the run, so a run
    // that finished is reported as finished however late the timeout became
    // ready in the same poll — unless the watchdog had already stopped it.
    let stopped = tokio::select! {
        biased;
        () = parent.cancelled() => {
            run_token.cancel();
            let out = run.await;
            guard.armed = false;
            return out;
        }
        out = &mut run => {
            guard.armed = false;
            // The watchdog cancelled it at its timeout while it held its
            // thread: it returned because it was stopped. A request cancel
            // the watchdog forwarded is not a timeout, and is returned as is.
            if !run_token.timed_out() {
                return out;
            }
            Some(out)
        }
        () = deadline(clock, timeout.duration()) => None,
    };

    // Timed out, and the token says so: the managed layer keeps a timed-out
    // run's `failure_out` and nothing of a cancelled one's, and a cdylib's
    // token hears it across the seam (`StableCancel::cancel_timed_out`).
    run_token.cancel_timed_out();
    let returned = match stopped {
        Some(out) => Ok(out),
        None => tokio::time::timeout(grace, &mut run).await,
    };
    let (abandoned_after, artifacts) = match returned {
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
            None,
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
            None,
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
            None,
        )
        .await
        .expect_err("times out");
        assert!(token.is_cancelled(), "the driver was told to stop");
        assert!(token.timed_out(), "and told why: its timeout");
        assert!(!parent.is_cancelled(), "only this run, not the request");
        assert_eq!(started.elapsed(), Duration::from_secs(90), "no grace used");
        assert!(is_timed_out(&err), "{err:#}");
        // Its own cancellation error is discarded: a timeout is a failure.
        assert!(!hplugin::error::is_cancelled(&err), "{err:#}");
        assert_eq!(
            format!("{err}"),
            "//p:t timed out after 1m30s (its `timeout`)"
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
        let err = enforce(run, &token, &parent, &addr(), secs(1), GRACE, None)
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
        let err = enforce(run, &token, &parent, &addr(), secs(5), GRACE, None)
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
        let err = enforce(run, &token, &parent, &addr(), secs(10), GRACE, None)
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
            None,
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
            None,
        )
        .await
        .expect_err("cancelled");
        assert!(token.is_cancelled(), "forwarded to the run");
        assert!(!token.timed_out(), "a request cancel is not a timeout");
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
        let out = enforce(run, &token, &parent, &addr(), secs(60), GRACE, None).await;
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
            None,
        );
        // Started, then abandoned well before the deadline.
        let polled = tokio::time::timeout(Duration::from_secs(1), fut).await;
        assert!(polled.is_err(), "still running at 1s");
        assert!(held.is_cancelled(), "dropping enforce cancels the run");
        assert!(!parent.is_cancelled(), "only this run, not the request");
    }

    /// A run that holds its thread until its token is cancelled, as exec's
    /// output tee does on macOS: it parks in `block_in_place` on a condvar
    /// that re-checks a flag, and is never `Pending` while the target's pipes
    /// are open. `at_most` bounds it, so a test that regresses fails rather
    /// than hangs.
    fn parked(token: &StdCancellationToken, at_most: Duration) -> anyhow::Result<()> {
        let until = std::time::Instant::now() + at_most;
        tokio::task::block_in_place(|| {
            while !token.is_cancelled() && std::time::Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        if token.is_cancelled() {
            Err(hplugin::error::CancelledError.into())
        } else {
            Ok(())
        }
    }

    /// A run that never yields still has its token cancelled at its timeout,
    /// and fails as timed out: the clock does not wait for the run to give
    /// `enforce` a poll. Before, a macOS exec run was stopped only once its
    /// output pipes closed, and one that ran on past its timeout and then
    /// returned in a single poll was reported as its own result.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_run_that_holds_its_thread_is_stopped_at_its_timeout() {
        let parent = StdCancellationToken::new();
        let token = StdCancellationToken::new();
        let started = std::time::Instant::now();
        let err = enforce(
            async { parked(&token, Duration::from_secs(30)) },
            &token,
            &parent,
            &addr(),
            Timeout::from_millis(200).expect("timeout"),
            GRACE,
            None,
        )
        .await
        .expect_err("times out");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "stopped at its timeout, not when the run let go: {:?}",
            started.elapsed()
        );
        assert!(token.timed_out(), "told why: its timeout");
        assert!(is_timed_out(&err), "{err:#}");
        assert!(!is_abandoned(&err), "it returned once told: {err:#}");
        assert!(!hplugin::error::is_cancelled(&err), "{err:#}");
    }

    /// The request's cancellation reaches a timed run that never yields, as
    /// it reaches an untimed one (handed the request's token directly): it
    /// is not held until the run lets go of its thread.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_cancel_reaches_a_run_that_holds_its_thread() {
        let parent = StdCancellationToken::new();
        let token = StdCancellationToken::new();
        let canceller = parent.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            canceller.cancel();
        });
        let started = std::time::Instant::now();
        let err = enforce(
            async { parked(&token, Duration::from_secs(30)) },
            &token,
            &parent,
            &addr(),
            secs(3600),
            GRACE,
            None,
        )
        .await
        .expect_err("cancelled");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert!(token.is_cancelled() && !token.timed_out());
        assert!(!is_timed_out(&err), "a cancel, not a timeout: {err:#}");
        assert!(hplugin::error::is_cancelled(&err), "{err:#}");
    }

    /// The watchdog runs on the run's clock: a run that holds its thread
    /// after starting its clock is stopped at its timeout from the start, and
    /// one that holds it after stopping its clock is never timed out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_watchdog_runs_on_the_run_clock() {
        let parent = StdCancellationToken::new();
        let token = StdCancellationToken::new();
        let (clock, reader) = hplugin::driver::RunClock::new();
        let started = std::time::Instant::now();
        let err = enforce(
            async {
                clock.start();
                parked(&token, Duration::from_secs(30))
            },
            &token,
            &parent,
            &addr(),
            Timeout::from_millis(200).expect("timeout"),
            GRACE,
            Some(reader),
        )
        .await
        .expect_err("times out");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "stopped at its timeout, not when the run let go: {:?}",
            started.elapsed()
        );
        assert!(is_timed_out(&err), "{err:#}");

        let token = StdCancellationToken::new();
        let (clock, reader) = hplugin::driver::RunClock::new();
        let out = enforce(
            async {
                clock.start();
                clock.stop();
                parked(&token, Duration::from_millis(600))
            },
            &token,
            &parent,
            &addr(),
            Timeout::from_millis(200).expect("timeout"),
            GRACE,
            Some(reader),
        )
        .await;
        assert!(out.is_ok(), "a stopped clock never times out: {out:?}");
        assert!(!token.is_cancelled());
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
            None,
        )
        .await;
        assert!(out.is_ok(), "{out:?}");
    }

    /// A clocked run: `before` of staging, the process (which honours its
    /// token) for `work`, then `after` of collecting outputs.
    async fn clocked(
        clock: hplugin::driver::RunClock,
        token: &StdCancellationToken,
        before: Duration,
        work: Duration,
        after: Duration,
    ) -> anyhow::Result<()> {
        tokio::time::sleep(before).await;
        clock.start();
        let out = polite(token, work).await;
        clock.stop();
        out?;
        tokio::time::sleep(after).await;
        Ok(())
    }

    /// The clock covers the process alone: a run whose staging and output
    /// collection each take far longer than its timeout, but whose process
    /// is quick, finishes.
    #[tokio::test(start_paused = true)]
    async fn staging_and_collection_are_not_timed() {
        let parent = StdCancellationToken::new();
        let token = StdCancellationToken::new();
        let (clock, reader) = hplugin::driver::RunClock::new();
        let started = tokio::time::Instant::now();
        let out = enforce(
            clocked(
                clock,
                &token,
                Duration::from_secs(100),
                Duration::from_secs(5),
                Duration::from_secs(100),
            ),
            &token,
            &parent,
            &addr(),
            secs(10),
            GRACE,
            Some(reader),
        )
        .await;
        assert!(out.is_ok(), "{out:?}");
        assert_eq!(started.elapsed(), Duration::from_secs(205));
        assert!(
            !token.is_cancelled(),
            "a finished run's token is left alone"
        );
    }

    /// The process is held to its timeout from its spawn, not from `run`:
    /// 100s of staging, then a process that would take an hour, stops at
    /// 110s, timed out exactly as an unclocked run is.
    #[tokio::test(start_paused = true)]
    async fn a_process_past_its_timeout_is_timed_out_from_its_spawn() {
        let parent = StdCancellationToken::new();
        let token = StdCancellationToken::new();
        let (clock, reader) = hplugin::driver::RunClock::new();
        let started = tokio::time::Instant::now();
        let err = enforce(
            clocked(
                clock,
                &token,
                Duration::from_secs(100),
                Duration::from_secs(3600),
                Duration::ZERO,
            ),
            &token,
            &parent,
            &addr(),
            secs(10),
            GRACE,
            Some(reader),
        )
        .await
        .expect_err("times out");
        assert_eq!(started.elapsed(), Duration::from_secs(110));
        assert!(token.timed_out(), "told why: its timeout");
        assert!(is_timed_out(&err), "{err:#}");
        assert!(!hplugin::error::is_cancelled(&err), "{err:#}");
        assert_eq!(timed_out(&err).expect("timed out").abandoned_after, None);
    }

    /// A driver that started its clock and then dropped it without stopping
    /// it is still on the clock: only a stop ends it.
    #[tokio::test(start_paused = true)]
    async fn a_clock_dropped_while_running_keeps_running() {
        let parent = StdCancellationToken::new();
        let token = StdCancellationToken::new();
        let (clock, reader) = hplugin::driver::RunClock::new();
        let run = async {
            clock.start();
            drop(clock);
            polite(&token, Duration::from_secs(3600)).await
        };
        let started = tokio::time::Instant::now();
        let err = enforce(run, &token, &parent, &addr(), secs(10), GRACE, Some(reader))
            .await
            .expect_err("times out");
        assert_eq!(started.elapsed(), Duration::from_secs(10));
        assert!(is_timed_out(&err), "{err:#}");
    }

    type StopMemo = hcore::hmemoizer::Memoizer<String, Result<(), Arc<anyhow::Error>>>;

    /// Outside a memoized body a run's stop is its request's token alone.
    #[tokio::test]
    async fn outside_a_memoized_body_the_stop_is_the_requests_token() {
        let request = StdCancellationToken::new();
        let stop = RunStop::new(&request);
        assert!(stop.abandoned.is_none());
        let held = stop.clone_arc();
        assert!(!stop.is_cancelled() && !stop_requested(&request));
        request.cancel();
        assert!(stop.is_cancelled() && held.is_cancelled() && stop_requested(&request));
        assert!(!stop.timed_out());
    }

    /// The race the stop is built for: fail-fast drops the last caller of an
    /// execute whose driver handed its token to a task of its own (exec's
    /// process wait). However soon the body is dropped after the abort, that
    /// task's clone hears the stop, and the request's token is untouched.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_abandoned_execute_stops_a_clone_its_driver_handed_away() {
        let memo: StopMemo = hcore::hmemoizer::Memoizer::with_tag_task(
            "run-stop",
            tokio::runtime::Handle::current(),
        );
        let request = StdCancellationToken::new();
        let (tx, rx) = tokio::sync::oneshot::channel::<Arc<dyn Cancellable + Send + Sync>>();
        let caller = tokio::spawn(enclose::enclose!((request) async move {
            memo.once("k".to_string(), move || async move {
                tokio::task::yield_now().await;
                let stop = RunStop::new(&request);
                let _sent = tx.send(stop.clone_arc());
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await
        }));
        let held = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("in time")
            .expect("the body hands its token away");
        assert!(!held.is_cancelled());
        caller.abort();
        tokio::time::timeout(Duration::from_secs(5), held.cancelled())
            .await
            .expect("the handed-away token never heard the abandonment");
        assert!(!held.timed_out());
        assert!(!request.is_cancelled(), "only this execute stops");
    }

    /// On the abandoned execute's own task, the failure that follows its stop
    /// is a stop: `stop_requested` reads the abandonment while the body still
    /// holds its thread, before any abort could land.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_abandoned_execute_reads_as_stop_requested_on_its_own_task() {
        let memo: StopMemo = hcore::hmemoizer::Memoizer::with_tag_task(
            "run-stop-own",
            tokio::runtime::Handle::current(),
        );
        let request = StdCancellationToken::new();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (seen_tx, seen_rx) = std::sync::mpsc::channel::<bool>();
        let caller = tokio::spawn(enclose::enclose!((request) async move {
            memo.once("k".to_string(), move || async move {
                tokio::task::yield_now().await;
                let stop = RunStop::new(&request);
                let _sent = started_tx.send(());
                let start = std::time::Instant::now();
                while !stop.is_cancelled() && start.elapsed() < Duration::from_secs(10) {
                    std::thread::yield_now();
                }
                let _sent = seen_tx.send(stop_requested(&request));
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await
        }));
        tokio::time::timeout(Duration::from_secs(5), started_rx)
            .await
            .expect("in time")
            .expect("the body starts");
        caller.abort();
        let seen =
            tokio::task::spawn_blocking(move || seen_rx.recv_timeout(Duration::from_secs(5)))
                .await
                .expect("join")
                .expect("the body reports");
        assert!(seen, "an abandoned execute's failure reads as a failure");
        assert!(!request.is_cancelled());
    }

    /// Abandoned before its timeout, a timed run that holds its thread is
    /// stopped as a cancellation, not timed out: `enforce` forwards the stop
    /// as it forwards a request cancel.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_abandoned_timed_run_is_a_cancel_not_a_timeout() {
        let abandoned = StdCancellationToken::new();
        let stop = RunStop {
            request: StdCancellationToken::new(),
            abandoned: Some(abandoned.clone()),
        };
        let token = StdCancellationToken::new();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            abandoned.cancel();
        });
        let err = enforce(
            async { parked(&token, Duration::from_secs(30)) },
            &token,
            &stop,
            &addr(),
            secs(3600),
            GRACE,
            None,
        )
        .await
        .expect_err("stopped");
        assert!(token.is_cancelled() && !token.timed_out());
        assert!(!is_timed_out(&err), "a stop, not a timeout: {err:#}");
    }
}
