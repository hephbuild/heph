use crate::hasync::Cancellable;
use rustc_hash::FxHashMap;
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
    // Fx, not SipHash: the keys are our own sequential ids, never attacker
    // input, and the map is hit under a lock shared by the whole request.
    wakers: Mutex<FxHashMap<u64, Waker>>,
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
                wakers: Mutex::new(FxHashMap::default()),
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
    /// The waker last put in the map under `id`. Lets a poll with a waker that
    /// would wake the same task return without the request-wide lock: inside a
    /// `select!` this future is re-polled on every wake of its siblings (each
    /// chunk of a copy, each driver wake), and the lock is shared by every
    /// target of the request.
    registered: Option<Waker>,
}

impl Future for Cancelled {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.state.state.load(Ordering::SeqCst) != LIVE {
            return Poll::Ready(());
        }
        // Our entry is still in the map, holding a waker for this task, unless
        // `cancel` drained it. Either way this task is woken on cancel without
        // touching the map. What makes the drained case sound is `Waker::wake`'s
        // contract: each `wake()` is followed by a poll of the task, and that
        // wake happens-before the poll it causes. `cancel` stores the state
        // before it drains and wakes, so the store happens-before that next
        // poll, whose load above sees it and returns `Ready`.
        if this
            .registered
            .as_ref()
            .is_some_and(|w| w.will_wake(cx.waker()))
        {
            return Poll::Pending;
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
        let waker = cx.waker().clone();
        guard.insert(id, waker.clone());
        this.registered = Some(waker);
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
            registered: None,
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

    /// Counts its wakes, so a test can tell which of two wakers `cancel` used.
    struct CountingWaker(AtomicU64);

    impl std::task::Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counting_waker() -> (Arc<CountingWaker>, Waker) {
        let counter = Arc::new(CountingWaker(AtomicU64::new(0)));
        (Arc::clone(&counter), Waker::from(counter))
    }

    /// A re-poll with a waker that wakes the same task does not take the
    /// request-wide lock, and the waker registered by the first poll is still
    /// the one `cancel` wakes.
    #[test]
    fn a_repoll_with_the_same_waker_skips_the_lock() {
        let token = StdCancellationToken::new();
        let (counter, waker) = counting_waker();
        let mut fut = token.cancelled();
        assert!(
            fut.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );

        // Hold the lock while another thread re-polls: a poll that took it
        // would block until the timeout. The guard is dropped before any
        // assertion, so a failure unblocks the poller instead of hanging the
        // scope's join.
        std::thread::scope(|s| {
            let guard = token.inner.wakers.lock().unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            let (fut, waker) = (&mut fut, &waker);
            s.spawn(move || {
                let pending = fut
                    .as_mut()
                    .poll(&mut Context::from_waker(waker))
                    .is_pending();
                tx.send(pending).unwrap();
            });
            let polled = rx.recv_timeout(Duration::from_secs(5));
            let registered = guard.len();
            drop(guard);
            assert_eq!(
                polled,
                Ok(true),
                "a same-waker re-poll blocked on the waker lock"
            );
            assert_eq!(registered, 1);
        });

        token.cancel();
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    }

    /// A poll with a waker for a different task replaces the registered one:
    /// `cancel` wakes the new waker, not the stale one, and the map still
    /// holds one entry for the future.
    #[test]
    fn a_repoll_with_a_new_waker_replaces_the_registered_one() {
        let token = StdCancellationToken::new();
        let (old_counter, old) = counting_waker();
        let (new_counter, new) = counting_waker();
        let mut fut = token.cancelled();
        assert!(
            fut.as_mut()
                .poll(&mut Context::from_waker(&old))
                .is_pending()
        );
        assert!(
            fut.as_mut()
                .poll(&mut Context::from_waker(&new))
                .is_pending()
        );
        assert_eq!(token.inner.wakers.lock().unwrap().len(), 1);

        token.cancel();
        assert_eq!(old_counter.0.load(Ordering::SeqCst), 0, "stale waker woken");
        assert_eq!(
            new_counter.0.load(Ordering::SeqCst),
            1,
            "new waker not woken"
        );
        assert!(fut.as_mut().poll(&mut Context::from_waker(&new)).is_ready());
    }

    /// Cancelled while a waiter keeps re-polling with the same waker: the
    /// fast path must never park it past the cancel.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_waiter_repolled_by_its_siblings_still_sees_cancel() {
        for _ in 0..200 {
            let token = StdCancellationToken::new();
            let waiter = token.clone();
            let task = tokio::spawn(async move {
                // A sibling that wakes this task on every poll keeps the
                // `select!` re-polling `cancelled()` with the same waker.
                let busy = std::future::poll_fn(|cx| {
                    cx.waker().wake_by_ref();
                    Poll::<()>::Pending
                });
                tokio::select! {
                    () = waiter.cancelled() => {}
                    () = busy => {}
                }
            });
            tokio::task::yield_now().await;
            token.cancel();
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("waiter missed the cancel")
                .expect("waiter task panicked");
        }
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
