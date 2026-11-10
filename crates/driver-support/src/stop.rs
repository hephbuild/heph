//! Blocking jobs that stop when their run is cancelled or abandoned.
//!
//! A managed run stages its inputs and collects its outputs in
//! `hcore::blocking::run` jobs, and such a job runs to completion even when
//! the future awaiting it is dropped ("drop-detaches"). The engine drops a run
//! that outlives its timeout's grace, and drops executes on `--fail-fast` and
//! Ctrl-C; the sandbox path is per addr, so the next run of the same target
//! reuses it while a detached job could still be unpacking old inputs into it
//! or packing its outputs. [`run_blocking`] gives each job a stop it polls
//! between entries (and per chunk inside one), set by either signal:
//!
//! - the run's cancellation token — the engine cancels it at the timeout, and
//!   the job returns within one chunk, so the driver's run returns well inside
//!   the engine's grace;
//! - the job's caller being dropped — whatever dropped the run, the job stops
//!   instead of writing on.

use hcore::hasync::Cancellable;
use hplugin::error::CancelledError;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Sets its flag when the future that owns it is dropped — on completion too,
/// by which time the job has already returned and nobody reads it.
struct AbandonOnDrop(Arc<AtomicBool>);

impl Drop for AbandonOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// The error a job returns when it observes its stop: [`CancelledError`], so
/// the engine reads it as the cancellation it is.
pub fn cancelled() -> anyhow::Error {
    CancelledError.into()
}

/// Run `f` as an `hcore::blocking::run` job that can stop early.
///
/// `f` receives a `stop` to poll between units of work (an atomic load or two,
/// no allocation); it reads true once `ctoken` is cancelled or this future is
/// dropped. It is `Sync`, so a job that fans out (rayon) can poll it from
/// every worker. On reading true, `f` should return [`cancelled`]. An error `f`
/// returns *after* the stop is set — an hcore loop's own "stopped" error, or
/// an I/O error the cancellation caused — is wrapped in [`CancelledError`] so
/// callers classify it as a cancellation either way.
///
/// A token already cancelled returns at once, without queueing a job.
pub async fn run_blocking<R, F>(ctoken: &(dyn Cancellable + Send + Sync), f: F) -> anyhow::Result<R>
where
    R: Send + 'static,
    F: FnOnce(&(dyn Fn() -> bool + Sync)) -> anyhow::Result<R> + Send + 'static,
{
    if ctoken.is_cancelled() {
        return Err(cancelled());
    }
    let token = ctoken.clone_arc();
    let abandoned = Arc::new(AtomicBool::new(false));
    let _abandon = AbandonOnDrop(Arc::clone(&abandoned));
    hcore::blocking::run(move || {
        let stop = || abandoned.load(Ordering::Acquire) || token.is_cancelled();
        f(&stop).map_err(|e| {
            if stop() && !hplugin::error::is_cancelled(&e) {
                e.context(CancelledError)
            } else {
                e
            }
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;

    #[tokio::test]
    async fn an_uncancelled_job_runs_and_its_errors_are_its_own() {
        let ct = StdCancellationToken::new();
        let v = run_blocking(&ct, |stop| {
            assert!(!stop(), "nothing asked it to stop");
            Ok(7)
        })
        .await
        .expect("runs");
        assert_eq!(v, 7);

        let err = run_blocking(&ct, |_| -> anyhow::Result<()> { anyhow::bail!("boom") })
            .await
            .expect_err("fails");
        assert!(!hplugin::error::is_cancelled(&err), "{err:#}");
    }

    #[tokio::test]
    async fn a_cancelled_token_returns_without_running_the_job() {
        let ct = StdCancellationToken::new();
        ct.cancel();
        let ran = Arc::new(AtomicBool::new(false));
        let r = Arc::clone(&ran);
        let err = run_blocking(&ct, move |_| {
            r.store(true, Ordering::Release);
            Ok(())
        })
        .await
        .expect_err("cancelled");
        assert!(hplugin::error::is_cancelled(&err), "{err:#}");
        assert!(!ran.load(Ordering::Acquire), "the job never started");
    }

    /// An error the job raises once it has been stopped — the hcore loops'
    /// own message, or the I/O error a stop caused — reads as a cancellation.
    #[tokio::test]
    async fn an_error_after_the_stop_is_a_cancellation() {
        let ct = StdCancellationToken::new();
        let token = ct.clone();
        let err = run_blocking(&ct, move |stop| -> anyhow::Result<()> {
            token.cancel();
            assert!(stop(), "the token is polled");
            anyhow::bail!("unpack stopped")
        })
        .await
        .expect_err("cancelled");
        assert!(hplugin::error::is_cancelled(&err), "{err:#}");
        assert!(format!("{err:#}").contains("unpack stopped"), "{err:#}");
    }
}
