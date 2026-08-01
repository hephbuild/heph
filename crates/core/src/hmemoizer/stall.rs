//! `tokio::time::timeout`, without touching the reactor — the opt-in stall
//! detector's clock (`HEPH_MEMOIZER_STALL_SECS`).
//!
//! Why not `tokio::time`: the joiner running this check may sit on a runtime
//! whose reactor is not this one's (a cell is joinable across the two runtimes
//! of the plugin seam), and the historical in-process harnesses drive futures
//! with no reactor at all. A debug tool that can abort on the workload it
//! exists to diagnose (the PR #180/#182 class) is worse than a 250 ms ticker
//! thread that only ever starts when someone is already debugging a hang.
//!
//! A plain `Instant` deadline checked on each poll, plus a plain-thread ticker
//! to guarantee those polls happen even when the inner future never wakes —
//! which is the stalled case, and the whole point. Nothing here is a timer.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, MutexGuard, Once};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

/// How often a waiter of [`timeout_without_reactor`] is re-woken so it can
/// observe its own deadline.
const STALL_TICK: Duration = Duration::from_millis(250);

/// Waiters to re-wake on the next tick, drained each time.
static STALL_PENDING: Mutex<Vec<Waker>> = Mutex::new(Vec::new());
static STALL_THREAD: Once = Once::new();

fn stall_pending() -> MutexGuard<'static, Vec<Waker>> {
    STALL_PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Re-wake `waker` within [`STALL_TICK`], starting the ticker on first use — so
/// a process that never enables the stall check never pays for the thread.
fn stall_backstop(waker: Waker) {
    STALL_THREAD.call_once(|| {
        thread::Builder::new()
            .name("heph-memoizer-stall".to_string())
            .spawn(|| {
                loop {
                    thread::sleep(STALL_TICK);
                    for waker in std::mem::take(&mut *stall_pending()) {
                        waker.wake();
                    }
                }
            })
            // Same stance as `hcore::blocking`: a process that cannot spawn its
            // diagnostic thread has nothing to fall back to.
            .expect("spawn heph memoizer stall thread");
    });
    stall_pending().push(waker);
}

/// `Err(())` means the deadline passed.
pub(crate) fn timeout_without_reactor<F: Future + Unpin>(
    limit: Duration,
    inner: F,
) -> TimeoutWithoutReactor<F> {
    TimeoutWithoutReactor {
        inner,
        deadline: Instant::now() + limit,
    }
}

pub(crate) struct TimeoutWithoutReactor<F> {
    inner: F,
    deadline: Instant,
}

impl<F: Future + Unpin> Future for TimeoutWithoutReactor<F> {
    type Output = Result<F::Output, ()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Poll::Ready(v) = Pin::new(&mut this.inner).poll(cx) {
            return Poll::Ready(Ok(v));
        }
        if Instant::now() >= this.deadline {
            return Poll::Ready(Err(()));
        }
        stall_backstop(cx.waker().clone());
        Poll::Pending
    }
}
