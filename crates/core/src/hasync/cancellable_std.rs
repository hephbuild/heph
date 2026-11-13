use crate::hasync::Cancellable;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::task::{Context, Poll, Waker};

/// A manual, std-only implementation of a cancellation signal.
///
/// The token fans out to *every* outstanding [`cancelled`](Cancellable::cancelled)
/// awaiter — not just the most recent one. Each `Cancelled` future registers
/// its own waker under a unique id, so a single `cancel()` wakes all in-flight
/// waiters at once. This is what makes `cancel_all_requests` propagate to every
/// concurrently-executing target instead of only the last one to poll.
#[derive(Clone)]
pub struct StdCancellationToken {
    inner: Arc<SharedState>,
}

/// [`SharedState::state`]: not cancelled.
const LIVE: u8 = 0;
/// [`SharedState::state`]: cancelled ([`StdCancellationToken::cancel`]).
const CANCELLED: u8 = 1;
/// [`SharedState::state`]: cancelled because its run timed out
/// ([`StdCancellationToken::cancel_timed_out`]).
const TIMED_OUT: u8 = 2;

struct SharedState {
    /// [`LIVE`], [`CANCELLED`] or [`TIMED_OUT`]. One atomic, so the reason is
    /// published with the cancel itself: whoever sees the token cancelled sees
    /// why. It leaves [`LIVE`] once, so the first cancel's reason sticks.
    state: AtomicU8,
    // One waker per outstanding `Cancelled` future, keyed by a unique id so
    // concurrent waiters don't clobber each other. A single `Option<Waker>`
    // slot would only ever wake the last registered waiter — the bug this
    // map fixes.
    wakers: Mutex<HashMap<u64, Waker>>,
    next_id: AtomicU64,
}

impl Default for StdCancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl StdCancellationToken {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(SharedState {
                state: AtomicU8::new(LIVE),
                wakers: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(0),
            }),
        }
    }

    pub fn cancel(&self) {
        self.cancel_with(CANCELLED);
    }

    /// Cancel because the run this token was handed to outlasted its
    /// `timeout`: [`timed_out`](Cancellable::timed_out) then reads `true`,
    /// unless the token was already cancelled.
    pub fn cancel_timed_out(&self) {
        self.cancel_with(TIMED_OUT);
    }

    fn cancel_with(&self, reason: u8) {
        // Set the state BEFORE taking the waker lock. `Cancelled::poll`
        // re-checks it while holding the same lock, so any waiter that
        // registers concurrently with `cancel` either sees it (→ Ready) or is
        // already in the map and gets woken below. No lost wakeups. Only from
        // LIVE: an earlier cancel's reason stands.
        let _already =
            self.inner
                .state
                .compare_exchange(LIVE, reason, Ordering::SeqCst, Ordering::SeqCst);

        // Wake every outstanding waiter. The mutex is never poisoned: the only
        // critical sections move `Waker`s around, no panics possible.
        let wakers: Vec<Waker> = {
            let Ok(mut guard) = self.inner.wakers.lock() else {
                return;
            };
            guard.drain().map(|(_, w)| w).collect()
        };
        for w in wakers {
            w.wake();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.state.load(Ordering::SeqCst) != LIVE
    }

    pub fn timed_out(&self) -> bool {
        self.inner.state.load(Ordering::SeqCst) == TIMED_OUT
    }

    /// Token identity: `true` iff `other` is a clone of `self` (they share the
    /// same underlying state). Lets a registry keyed by a reusable id confirm an
    /// entry is *this* token before acting on it.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

/// Future returned by [`StdCancellationToken::cancelled`]. Resolves once the
/// token is cancelled. Registers its waker under a unique id on first poll and
/// removes it on drop so the shared map can't grow without bound across many
/// short-lived waiters.
struct Cancelled {
    state: Arc<SharedState>,
    id: Option<u64>,
}

impl Future for Cancelled {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.state.state.load(Ordering::SeqCst) != LIVE {
            return Poll::Ready(());
        }

        // The mutex is never poisoned (see `cancel`).
        #[expect(
            clippy::unwrap_used,
            reason = "mutex cannot be poisoned; lock only moves Wakers"
        )]
        let mut guard = this.state.wakers.lock().unwrap();
        // Re-check under the lock: if `cancel` ran after our load above, it has
        // already set the flag (it stores before locking), so we observe it
        // here and avoid registering a waker that would never be drained.
        if this.state.state.load(Ordering::SeqCst) != LIVE {
            return Poll::Ready(());
        }
        let id = *this
            .id
            .get_or_insert_with(|| this.state.next_id.fetch_add(1, Ordering::Relaxed));
        guard.insert(id, cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for Cancelled {
    fn drop(&mut self) {
        if let Some(id) = self.id
            && let Ok(mut guard) = self.state.wakers.lock()
        {
            guard.remove(&id);
        }
    }
}

// Integration with our Trait from before
impl Cancellable for StdCancellationToken {
    fn is_cancelled(&self) -> bool {
        self.is_cancelled()
    }

    fn timed_out(&self) -> bool {
        self.timed_out()
    }

    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(Cancelled {
            state: Arc::clone(&self.inner),
            id: None,
        })
    }

    fn clone_arc(&self) -> Arc<dyn Cancellable + Send + Sync> {
        Arc::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A single `cancel()` must wake *every* concurrent waiter, not just the
    /// last one to register. This is the regression that left in-flight
    /// targets parked on ctrl-c until they exited on their own.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancel_wakes_all_concurrent_waiters() {
        let token = StdCancellationToken::new();

        let mut handles = Vec::new();
        for _ in 0..16 {
            let t = token.clone();
            handles.push(tokio::spawn(async move {
                t.cancelled().await;
            }));
        }

        // Give every waiter a chance to register its waker.
        tokio::time::sleep(Duration::from_millis(20)).await;
        token.cancel();

        // Every waiter must resolve promptly — if only the last registered
        // waker were stored, all but one would hang here.
        let all = tokio::time::timeout(Duration::from_secs(2), async {
            for h in handles {
                h.await.expect("waiter task panicked");
            }
        })
        .await;
        assert!(all.is_ok(), "not all waiters woke on cancel");
    }

    #[tokio::test]
    async fn cancelled_resolves_immediately_when_already_cancelled() {
        let token = StdCancellationToken::new();
        token.cancel();
        // Must not hang.
        tokio::time::timeout(Duration::from_secs(1), token.cancelled())
            .await
            .expect("already-cancelled token must resolve immediately");
    }

    /// The first cancel's reason sticks, on every clone; a timed-out token is
    /// cancelled, wakes its waiters, and stays timed out when cancelled again.
    #[tokio::test]
    async fn the_first_cancel_decides_whether_it_timed_out() {
        let timed = StdCancellationToken::new();
        let waiter = timed.clone();
        assert!(!timed.timed_out());
        timed.cancel_timed_out();
        tokio::time::timeout(Duration::from_secs(1), Cancellable::cancelled(&waiter))
            .await
            .expect("a timed-out token wakes its waiters");
        timed.cancel();
        assert!(waiter.is_cancelled() && waiter.timed_out());
        assert!(Cancellable::timed_out(&*timed.clone_arc()));

        let plain = StdCancellationToken::new();
        plain.cancel();
        plain.cancel_timed_out();
        assert!(plain.is_cancelled() && !plain.timed_out());
    }

    #[test]
    fn dropped_waiter_is_unregistered() {
        let token = StdCancellationToken::new();
        {
            let fut = token.cancelled();
            // Poll once to register a waker, then drop without completing.
            let mut fut = Box::pin(fut);
            let waker = futures::task::noop_waker();
            let mut cx = Context::from_waker(&waker);
            assert!(fut.as_mut().poll(&mut cx).is_pending());
            assert_eq!(token.inner.wakers.lock().unwrap().len(), 1);
        }
        assert_eq!(
            token.inner.wakers.lock().unwrap().len(),
            0,
            "dropping a waiter must remove its waker from the shared map"
        );
    }
}
