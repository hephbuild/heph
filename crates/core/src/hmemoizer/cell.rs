//! A single-flight cell: one shared future, many awaiters, **completion-only
//! broadcast**.
//!
//! This replaces `futures::future::Shared` inside [`Memoizer`](super::Memoizer).
//! `Shared` is otherwise exactly the right shape — it re-elects a driver on every
//! poll, polls the inner future with a cell-owned waker, and keeps that future
//! alive in the cell rather than on any awaiter's stack — and all three of those
//! properties are preserved here deliberately. See "What is kept, and why" below
//! before changing any of them.
//!
//! The one thing `Shared` gets wrong for this workload is *who it wakes*. Its
//! `Notifier::wake_by_ref` drains the waker slab and `take()`s **every** slot on
//! **every** inner wake, so each awaiter is forced to re-poll merely to
//! re-register. In heph that is quadratic: a base library with W reverse-deps has
//! W awaiters on its `mem_result` cell, and every one of that library's own D
//! dependencies completing wakes all W, each of which re-descends ~27 levels of
//! nested cells to get back to where it was. Six saturated worker threads and a
//! build that makes no progress, which is the bug this module exists to remove.
//!
//! Here an inner wake goes to the **driver alone** — the awaiter that last polled
//! the inner future, identified by its slab key in [`Cell::driver`]. Everyone else
//! is woken exactly once, when the value is ready.
//!
//! ## A wake is never discarded
//!
//! The driver is woken through a *clone* of its registration, never by taking it.
//! An earlier version took the waker out of the slab and, on the next wake,
//! found the slot empty; a `driver_awake` latch then told it the driver "already
//! owes a re-poll", and the wake was dropped on the reasoning that re-polling the
//! inner future re-reads its state anyway.
//!
//! That reasoning holds for a future whose readiness can be re-read — a `oneshot`
//! still holding its value — and is false for one that takes a wake as a
//! **one-shot handoff**. `tokio::sync::Semaphore` is the latter: on release it
//! *assigns* a permit to the first queued waiter and wakes it. Swallow that wake
//! and the permit is stranded inside an `Acquire` future nobody will poll again.
//! A wedged build showed exactly that — every worker permit gone, 78 targets
//! queued on `execute`'s semaphore, and not one thread doing anything.
//!
//! So the slab keeps the driver's waker until it re-registers or goes away, and a
//! wake with a live driver is always delivered. The property that matters is
//! unchanged: one task is woken, not W. Waking a task that is already scheduled
//! is a cheap no-op.
//!
//! ## What is kept, and why
//!
//! - **Election is per-poll, never "first awaiter wins".** Drivership is held only
//!   for the duration of one poll — it is literally the `slot` mutex — so an
//!   awaiter that is dropped between polls leaves a cell any other awaiter can
//!   pick up. A cell whose driver is assigned once and owned thereafter would
//!   strand every awaiter the moment that task dies, and dying is routine here:
//!   `try_join_all` with fail-fast drops every sibling on the first error, and
//!   Ctrl-C drops them wholesale.
//! - **The inner future is polled with a cell-owned waker**, never the driver's
//!   `cx.waker()`. Sub-futures stash the waker they are handed — a semaphore
//!   acquire parks it in its wait list — so handing out the driver's waker means
//!   a later wake lands on a task that no longer exists, and nothing ever polls
//!   the cell again.
//! - **The future lives in the cell.** It survives every awaiter coming and going.
//!
//! ## What is fixed beyond the wake set
//!
//! - **Abdication wakes everyone.** When the driver is dropped it clears
//!   [`Cell::driver`] and wakes all remaining awaiters, so one of them re-elects
//!   itself. Waking just one is not enough: that one may itself be mid-drop, and
//!   the wake is then lost with nobody left to re-poll.
//! - **Completion drops the future immediately.** The value moves into
//!   [`Cell::done`] and the boxed future is dropped while the lock is still held.
//!   `result_addr_impl` is `#[async_recursion]` and its state machine is large
//!   (the `large_futures` lint is on for this reason); retaining one per cell for
//!   the life of a request would cost more memory than the wake fix saves.
//! - **A completed cell is read with no lock at all** — `done` is a `OnceLock`, so
//!   the warm-hit path is an acquire load and a clone. That path dominates a
//!   cached build.

use futures::future::BoxFuture;
use futures::task::{ArcWake, waker_ref};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Once, OnceLock};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

/// [`Cell::driver`] when no awaiter is currently on the hook for re-polling.
const NO_DRIVER: usize = usize::MAX;

/// Wakes that reached an incomplete cell and found nobody — no driver and not a
/// single registered waker — so the fallback broadcast woke zero tasks.
///
/// With cancel-on-abandonment this is *almost* always benign: a straggler wake
/// racing the eviction window (interest already zero, future being taken and
/// dropped) lands here, and so does a leaf completing just as its last awaiter
/// is torn down. Those are bounded by the number of cancellations. What this
/// counter exists for is the pathological shape it replaced: an abandoned cell
/// whose future was *retained*, being re-woken every 250ms by the blocking
/// backstop with every wake evaporating — ~1000 ticks over 269s in the
/// production wedge. A count that keeps climbing while a build makes no
/// progress is that regression announcing itself; it is rendered in
/// `render_full_report`, so it lands in the `SIGQUIT` dump and the stall
/// watchdog file. Not a `debug_assert!`: the benign cases above are reachable
/// in correct executions.
static VOID_WAKES: AtomicU64 = AtomicU64::new(0);

/// Monotone count of wakes delivered to an incomplete cell that reached no one.
pub(crate) fn void_wakes() -> u64 {
    VOID_WAKES.load(Ordering::Relaxed)
}

/// Waker set, keyed by a stable index each awaiter holds for its lifetime.
///
/// A `Vec` with a free list rather than a `HashMap`: this allocates one slot per
/// awaiter instead of a hash entry, and the engine holds on the order of a
/// million cells. `crates/core/src/hasync/cancellable_std.rs` teaches the right
/// *lessons* here — never a single `Option<Waker>`, always remove on drop — but
/// its `HashMap<u64, Waker>` is the wrong container at this scale.
#[derive(Default)]
struct Wakers {
    slots: Vec<Option<Waker>>,
    free: Vec<usize>,
}

impl Wakers {
    /// Record `waker` under `key`, allocating a key on first registration.
    ///
    /// Re-registration **updates in place**. Pushing a new slot per poll would
    /// grow the set without bound across the many re-polls a busy cell sees.
    fn register(&mut self, key: &mut Option<usize>, waker: &Waker) {
        match *key {
            Some(k) => {
                if let Some(slot) = self.slots.get_mut(k) {
                    // `will_wake` spares a clone on the overwhelmingly common
                    // re-poll-by-the-same-task path.
                    match slot {
                        Some(existing) if existing.will_wake(waker) => {}
                        _ => *slot = Some(waker.clone()),
                    }
                }
            }
            None => {
                // A key popped off the free list always indexes an existing
                // slot; `get_mut` keeps that an invariant rather than a panic.
                let reused = self
                    .free
                    .pop()
                    .filter(|k| self.slots.get(*k).is_some())
                    .inspect(|k| {
                        if let Some(slot) = self.slots.get_mut(*k) {
                            *slot = Some(waker.clone());
                        }
                    });
                *key = Some(reused.unwrap_or_else(|| {
                    self.slots.push(Some(waker.clone()));
                    self.slots.len() - 1
                }));
            }
        }
    }

    /// Clone `key`'s waker, leaving it registered.
    ///
    /// The driver is woken through this rather than [`take`](Self::take): its
    /// registration has to survive the wake, so that a *second* wake arriving
    /// before it re-polls still has somewhere to land. See
    /// [`Cell::wake_by_ref`].
    fn peek(&self, key: usize) -> Option<Waker> {
        self.slots.get(key).and_then(Option::clone)
    }

    /// Drop `key`'s slot and return it to the free list. Called once, from the
    /// awaiter's `Drop`, so a long-lived popular cell doesn't accumulate wakers
    /// for awaiters that have gone away.
    fn remove(&mut self, key: usize) {
        if let Some(slot) = self.slots.get_mut(key) {
            *slot = None;
            self.free.push(key);
        }
    }

    fn take_all(&mut self) -> Vec<Waker> {
        self.slots.iter_mut().filter_map(Option::take).collect()
    }
}

/// The shared state behind one memoized key.
pub(crate) struct Cell<V> {
    /// The value, once computed. A `OnceLock` so the warm-hit path never takes a
    /// lock — this is the path a fully-cached build spends its time on.
    done: OnceLock<V>,
    /// The in-flight future. `None` once it has completed (dropped eagerly) or
    /// been taken. The mutex **is** the drivership election: whoever wins
    /// `try_lock` drives this poll, and only for this poll.
    slot: Mutex<Option<BoxFuture<'static, V>>>,
    wakers: Mutex<Wakers>,
    /// Slab key of the awaiter that last polled the inner future, or
    /// [`NO_DRIVER`]. An inner wake is routed here and nowhere else.
    driver: AtomicUsize,
    /// How many callers still want this value.
    ///
    /// Incremented by [`Memoizer::process`](super::Memoizer::process) under the
    /// cache lock as it hands the cell out, decremented when that caller is
    /// done or gone. Zero means the computation is abandoned and can be
    /// cancelled.
    ///
    /// It cannot be inferred from `Arc::strong_count`: the cell **is** its own
    /// waker (`waker_ref(cell)` in [`Await::poll`]), so every clone of that
    /// waker is a strong clone of the cell. A parked `oneshot`, a queued
    /// semaphore acquire, a `hcore::blocking` backstop registration, and every
    /// child cell's waker slab all hold one — which is to say every computation
    /// that has actually parked, which is every computation that matters here.
    /// Counting Arcs would see those as interested callers and never cancel
    /// anything.
    interest: AtomicUsize,
}

impl<V> Cell<V> {
    pub(crate) fn new(fut: BoxFuture<'static, V>) -> Arc<Self> {
        Arc::new(Self {
            done: OnceLock::new(),
            slot: Mutex::new(Some(fut)),
            wakers: Mutex::new(Wakers::default()),
            driver: AtomicUsize::new(NO_DRIVER),
            interest: AtomicUsize::new(0),
        })
    }

    /// Register a caller that wants this value. Called under the memoizer's
    /// cache lock — a cancellation deciding under that same lock therefore
    /// either sees this interest and stands down, or has already evicted the
    /// entry, in which case the joiner never found this cell to begin with.
    pub(crate) fn acquire_interest(&self) {
        self.interest.fetch_add(1, Ordering::AcqRel);
    }

    /// Drop a caller's interest, returning how many remain.
    ///
    /// `AcqRel` is load-bearing: a frame that *completed* the cell stores the
    /// value (`done.set`, a release store) strictly before its own release here.
    /// All `fetch_sub`s on one atomic form a release sequence, so a guard whose
    /// decrement observes `remaining == 0` synchronizes-with every earlier
    /// release — and its subsequent `is_done()` (an acquire load) is then
    /// guaranteed to see that completion. That is what makes "remaining == 0 and
    /// not done" mean *abandoned*, never *completed-but-not-yet-visible*.
    pub(crate) fn release_interest(&self) -> usize {
        let prev = self.interest.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev > 0, "interest release without a matching acquire");
        prev - 1
    }

    /// Callers currently interested. Re-read under the cache lock before
    /// cancelling, so a joiner that arrived after the decrement is seen.
    pub(crate) fn interest(&self) -> usize {
        self.interest.load(Ordering::Acquire)
    }

    /// The completed value, if there is one. No locking.
    pub(crate) fn peek(&self) -> Option<&V> {
        self.done.get()
    }

    /// The lock guarding the waker set.
    ///
    /// Poisoning is ignored: every critical section only moves `Waker`s and
    /// indices around, so a panicking waker leaves the set structurally intact,
    /// and refusing to hand it back would strand every awaiter. Same stance as
    /// `hasync::cancellable_std`.
    fn wakers(&self) -> MutexGuard<'_, Wakers> {
        self.wakers.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wake every registered awaiter and clear the set.
    fn wake_all(&self) {
        let all = self.wakers().take_all();
        for waker in all {
            waker.wake();
        }
    }

    /// Whether the value has been published.
    pub(crate) fn is_done(&self) -> bool {
        self.done.get().is_some()
    }

    /// Take the in-flight future out of the cell, so the caller can drop it.
    ///
    /// For a cell nobody awaits any more. Retaining the future is deliberate
    /// while awaiters come and go — one dropped between polls must be
    /// replaceable by another — but when the last one goes for good there is no
    /// successor, and what is left is a future nobody will ever poll again.
    ///
    /// It is not inert. It holds everything it captured, and keeps its place in
    /// every queue it was waiting on: a worker permit acquired and never
    /// released, a `oneshot` whose result nobody reads, a backstop registration
    /// re-woken every 250ms into a graph that discards the wake. Twelve of those
    /// held every permit in the pool while the build sat idle.
    ///
    /// Returned rather than dropped here so the caller drops it with no lock
    /// held — the drop cascades through a large state machine and runs arbitrary
    /// user destructors.
    pub(crate) fn take_future(&self) -> Option<BoxFuture<'static, V>> {
        if self.done.get().is_some() {
            return None;
        }
        // A blocking `lock`, deliberately, even though a *poll* can never hold
        // this slot here: interest reaching zero means no live `Await`, and no
        // `Await` means nobody is polling.
        //
        // The contender is the other canceller. `cancel_abandoned` documents the
        // two-cancellations interleaving — both frames observe their own zero
        // crossing, both reach here, and both call this with no lock held
        // (mandated, so the cache lock is never held across `slot`). They race
        // on the mutex rather than serializing on the cache lock.
        //
        // An earlier version used `try_lock` and asserted the `WouldBlock` arm
        // unreachable. It is reachable by exactly that path, and the assert
        // fires inside `Drop` glue — which aborts outright if the drop is itself
        // unwinding. Blocking is safe: the only holder is another `take_future`,
        // which owns the guard for a single `Option::take` and acquires nothing
        // else, so there is no cycle to deadlock on. The loser then correctly
        // gets `None`, which is the whole point — exactly one canceller drops
        // the future.
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.take()
    }

    /// How many awaiters are attached to this cell, or `None` if the waker set
    /// was locked when sampled.
    ///
    /// Counts *allocated slots*, not slots currently holding a `Waker`. The two
    /// differ exactly when it matters: waking an awaiter `take`s its waker and
    /// leaves the slot empty until that awaiter re-polls and re-registers. So a
    /// live-waker count reads zero for the population this diagnostic exists to
    /// find — tasks that were woken, never re-polled, and are therefore parked
    /// forever. A slot is returned to the free list only by [`Await::drop`], so
    /// `slots - free` is the number of awaiters that still exist.
    ///
    /// `try_lock`, never `lock`: this is read by the stall watchdog and the
    /// `SIGQUIT` dump, and a diagnostic that can block is a diagnostic that can
    /// hang the process it exists to explain. A missed sample costs one cell in
    /// one report; the next fire re-reads it.
    pub(crate) fn waiters(&self) -> Option<usize> {
        let guard = match self.wakers.try_lock() {
            Ok(g) => g,
            // Same stance as `wakers()`: a poisoned set is structurally intact.
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        Some(guard.slots.len().saturating_sub(guard.free.len()))
    }

    /// Whether an awaiter is currently elected to re-poll the inner future.
    ///
    /// `false` together with a non-zero [`waiters`](Self::waiters) is the
    /// signature of a lost wake-up: tasks are parked on this cell and nobody is
    /// on the hook to poll it. Transiently normal (abdication clears the driver
    /// and wakes everyone to re-elect); a standing condition on a build that has
    /// made no progress for a minute is not.
    pub(crate) fn has_driver(&self) -> bool {
        self.driver.load(Ordering::Relaxed) != NO_DRIVER
    }
}

/// The inner future is polled with this, so a wake from deep inside it lands on
/// the cell rather than on whichever task happened to be driving.
impl<V: Send + Sync + 'static> ArcWake for Cell<V> {
    fn wake_by_ref(cell: &Arc<Self>) {
        // Everything below runs under the waker lock, `driver` included.
        // `Await::drop` clears `driver` and then edits the set while holding this
        // lock, so a load taken outside it can name a driver that is already gone.
        let mut wakers = cell.wakers();
        let driver = cell.driver.load(Ordering::Acquire);
        let targeted = if driver == NO_DRIVER {
            None
        } else {
            // Cloned, never taken: the registration must outlive the wake, so a
            // second wake arriving before the driver re-polls still finds it.
            wakers.peek(driver)
        };

        // The optimization: an inner wake normally reaches exactly one task, not
        // all W of them. That is the whole point of the module.
        if let Some(waker) = targeted {
            drop(wakers);
            waker.wake();
            return;
        }

        // No driver, or a driver with no waker registered — nobody is going to
        // poll this cell. Wake everyone and let one re-elect itself.
        // An extra wake costs one spurious poll; a missing one hangs the cell
        // forever, and at the tail of a build there is no ambient re-polling left
        // to rescue it.
        let all = wakers.take_all();
        drop(wakers);
        if all.is_empty() && !cell.is_done() {
            // The wake reached nobody at all. See [`VOID_WAKES`] for why this is
            // counted rather than asserted.
            VOID_WAKES.fetch_add(1, Ordering::Relaxed);
        }
        for waker in all {
            waker.wake();
        }
    }
}

/// One awaiter's handle on a [`Cell`]. Cloning the `Arc` is how a second caller
/// joins the same single-flight computation.
pub(crate) struct Await<V> {
    cell: Arc<Cell<V>>,
    /// Our slot in the cell's waker set, allocated on first `Pending` poll.
    key: Option<usize>,
}

impl<V> Await<V> {
    /// Contract: the caller holds a registered interest
    /// ([`Cell::acquire_interest`]) for this cell, and releases it only after
    /// this `Await` is dropped. `Memoizer::process` is the single construction
    /// site and enforces the pairing with an RAII guard; the assertion exists so
    /// any future second construction site that forgets fails its first test
    /// run instead of re-introducing a permit leak (interest at zero cancels the
    /// computation out from under the awaiter).
    pub(crate) fn new(cell: Arc<Cell<V>>) -> Self {
        debug_assert!(
            cell.interest() > 0,
            "Await created on a cell with no registered interest — \
             cancel-on-abandonment would tear this computation down mid-await"
        );
        Self { cell, key: None }
    }
}

impl<V: Clone + Send + Sync + 'static> Future for Await<V> {
    type Output = V;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<V> {
        let this = self.get_mut();
        let cell = &this.cell;

        // Warm hit: an acquire load, no lock, no waker traffic.
        if let Some(v) = cell.peek() {
            return Poll::Ready(v.clone());
        }

        // Register before attempting to drive, and re-check completion while
        // holding the waker lock. Without that re-check a value landing between
        // our `peek` above and our registration would wake a set we are not yet
        // in, and we would park forever. This is the same store-then-lock,
        // re-check-under-the-lock ordering `hasync::cancellable_std` documents.
        {
            let mut wakers = cell.wakers();
            if let Some(v) = cell.peek() {
                return Poll::Ready(v.clone());
            }
            wakers.register(&mut this.key, cx.waker());
        }

        // Elect ourselves by taking the future. Failure means another task is
        // mid-poll; we are registered, so its completion (or its abdication)
        // will reach us.
        let Ok(mut slot) = cell.slot.try_lock() else {
            return Poll::Pending;
        };

        // It may have completed while we were registering.
        if let Some(v) = cell.peek() {
            return Poll::Ready(v.clone());
        }
        let Some(fut) = slot.as_mut() else {
            // No future and no value: only reachable if a driver unwound out of
            // the poll below. Stay parked rather than spin; `Memoizer::once`
            // converts panics to values before they get here.
            return Poll::Pending;
        };

        // Safe to unwrap-free: `register` above always leaves `key` set.
        if let Some(key) = this.key {
            cell.driver.store(key, Ordering::Release);
        }

        let waker = waker_ref(cell);
        let mut cx = Context::from_waker(&waker);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => {
                // Publish, then drop the future while still holding the lock, so
                // no awaiter can observe a completed cell that is still carrying
                // its (large) state machine.
                let stored = cell.done.get_or_init(|| v.clone());
                let out = stored.clone();
                *slot = None;
                drop(slot);
                cell.driver.store(NO_DRIVER, Ordering::Release);
                cell.wake_all();
                Poll::Ready(out)
            }
            // We stay recorded as driver: the inner future kept our cell-owned
            // waker, and its next wake routes back here.
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    /// A waker that only counts, so a test can assert *how many* wakes a given
    /// awaiter received rather than merely that it eventually resolved.
    struct Counting(AtomicUsize);

    impl std::task::Wake for Counting {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counting() -> (Arc<Counting>, Waker) {
        let arc = Arc::new(Counting(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&arc));
        (arc, waker)
    }

    fn count(c: &Arc<Counting>) -> usize {
        c.0.load(Ordering::SeqCst)
    }

    /// Inner future that stashes whichever waker it is polled with and stays
    /// `Pending` until `ready` is set — standing in for the semaphore acquire
    /// the real stacks were parked on.
    struct Stash {
        stashed: Arc<Mutex<Option<Waker>>>,
        ready: Arc<AtomicBool>,
        value: u32,
    }

    impl Future for Stash {
        type Output = u32;
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u32> {
            if self.ready.load(Ordering::SeqCst) {
                return Poll::Ready(self.value);
            }
            *self.stashed.lock().unwrap_or_else(|e| e.into_inner()) = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    fn stash_cell(value: u32) -> (Arc<Cell<u32>>, Arc<Mutex<Option<Waker>>>, Arc<AtomicBool>) {
        let stashed = Arc::new(Mutex::new(None));
        let ready = Arc::new(AtomicBool::new(false));
        let fut = Stash {
            stashed: Arc::clone(&stashed),
            ready: Arc::clone(&ready),
            value,
        };
        let cell = Cell::new(Box::pin(fut));
        // These tests drive `Await`s by hand, standing in for `process` frames
        // that would each hold an interest. One registration satisfies the
        // `Await::new` contract for the whole test.
        cell.acquire_interest();
        (cell, stashed, ready)
    }

    fn poll_with(waiter: &mut Await<u32>, waker: &Waker) -> Poll<u32> {
        Pin::new(waiter).poll(&mut Context::from_waker(waker))
    }

    /// The shape a lost wake-up leaves behind, which is what the inventory reads.
    ///
    /// A driven cell has waiters *and* a driver. When the driver goes away
    /// without the cell completing, the driver is cleared — and if the wake it
    /// broadcasts on the way out never lands, what remains is waiters parked on
    /// a cell nobody is on the hook to poll. That is the state a wedged build
    /// sits in, and until it was observable the only evidence was 250 threads
    /// idle in `futex_wait`.
    #[test]
    fn a_cell_reports_waiters_and_whether_anyone_will_poll_it() {
        let (cell, _stashed, ready) = stash_cell(3);

        let (_pc, pw) = counting();
        let mut parked = Await::new(Arc::clone(&cell));
        assert!(poll_with(&mut parked, &pw).is_pending());

        let (_dc, dw) = counting();
        let mut driver = Await::new(Arc::clone(&cell));
        assert!(poll_with(&mut driver, &dw).is_pending());

        assert_eq!(cell.waiters(), Some(2));
        assert!(cell.has_driver(), "the last poller is on the hook");
        assert!(!cell.is_done());

        // The driver goes away mid-flight — a fail-fast sibling drop or Ctrl-C.
        // Abdication wakes the rest, which `take`s their wakers; the awaiters
        // themselves are still attached and still parked, and that is what must
        // be reported. Counting live wakers here would read 0 and hide the very
        // population this exists to find.
        drop(driver);
        assert!(
            !cell.has_driver(),
            "abdication must clear the driver so another awaiter can re-elect"
        );
        assert_eq!(
            cell.waiters(),
            Some(1),
            "the departing awaiter releases its slot; the parked one is still attached"
        );

        // Completion is what takes a cell out of the inventory.
        ready.store(true, Ordering::SeqCst);
        assert!(poll_with(&mut parked, &pw).is_ready());
        assert!(cell.is_done());
    }

    /// **The reason this module exists.** A wake from inside the shared future
    /// must reach the driver and nobody else.
    ///
    /// `futures::Shared` drains and `take()`s *every* registered slot on *every*
    /// inner wake, so each of W awaiters is forced to re-poll — and in the engine
    /// each of those re-polls re-descends ~27 levels of nested cells. That is the
    /// quadratic blow-up that pegged six workers with the build making no
    /// progress. This test is red under that behavior: the parked awaiters would
    /// show a non-zero count.
    #[test]
    fn inner_wake_reaches_only_the_driver() {
        let (cell, stashed, ready) = stash_cell(7);

        // Poll the parked awaiters first, the driver last — election is per-poll,
        // so the last one to poll the inner future is the one on the hook.
        let mut parked: Vec<(Await<u32>, Arc<Counting>, Waker)> = (0..8)
            .map(|_| {
                let (c, w) = counting();
                (Await::new(Arc::clone(&cell)), c, w)
            })
            .collect();
        for (waiter, _, waker) in &mut parked {
            assert!(poll_with(waiter, waker).is_pending());
        }
        let (dc, dw) = counting();
        let mut driver = Await::new(Arc::clone(&cell));
        assert!(poll_with(&mut driver, &dw).is_pending());

        let inner = stashed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .expect("inner future must have stashed the cell's waker");

        // Progress inside the shared future, over and over.
        for _ in 0..100 {
            inner.wake_by_ref();
        }

        for (i, (_, c, _)) in parked.iter().enumerate() {
            assert_eq!(
                count(c),
                0,
                "parked awaiter {i} was woken by inner progress; this is the wake storm"
            );
        }
        assert_eq!(
            count(&dc),
            100,
            "every wake must reach the driver — one it never re-polls for is a \
             one-shot handoff lost forever"
        );

        // Completion is the one event that must reach everybody.
        ready.store(true, Ordering::SeqCst);
        assert!(poll_with(&mut driver, &dw).is_ready());
        for (i, (_, c, _)) in parked.iter().enumerate() {
            assert_eq!(
                count(c),
                1,
                "parked awaiter {i} must be woken on completion"
            );
        }
    }

    /// An inner wake with no reachable driver must wake everyone.
    ///
    /// A recorded driver whose waker slot is empty is ambiguous — it either
    /// already got woken and has not re-registered yet, or it is gone. Targeting
    /// it and finding nothing used to drop the wake on the floor, which strands
    /// every remaining awaiter: with completion-only wakes there is no ambient
    /// re-polling left to rescue them, and at the tail of a build that is a hang.
    #[test]
    fn inner_wake_falls_back_to_everyone_when_the_driver_slot_is_empty() {
        let (cell, stashed, _ready) = stash_cell(9);

        let (w1c, w1w) = counting();
        let mut w1 = Await::new(Arc::clone(&cell));
        assert!(poll_with(&mut w1, &w1w).is_pending());
        let (w2c, w2w) = counting();
        let mut w2 = Await::new(Arc::clone(&cell));
        assert!(poll_with(&mut w2, &w2w).is_pending());

        let (dc, dw) = counting();
        let mut driver = Await::new(Arc::clone(&cell));
        assert!(poll_with(&mut driver, &dw).is_pending());

        let inner = stashed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .expect("inner future must have stashed the cell's waker");

        // Every wake reaches the driver, and only the driver — a burst does not
        // fan out to the other awaiters, which is what this module is for.
        for expected in 1..=3 {
            inner.wake_by_ref();
            assert_eq!(
                count(&dc),
                expected,
                "every wake must reach the driver, not just the first"
            );
            assert_eq!(
                (count(&w1c), count(&w2c)),
                (0, 0),
                "a wake fanned out while the driver was reachable"
            );
        }

        // Now the cell has awaiters but nobody on the hook — the state left
        // behind when a driver goes away between polls. Poked directly because
        // any real poll would elect its poller straight back into drivership.
        cell.driver.store(NO_DRIVER, Ordering::SeqCst);
        let (w1_before, w2_before) = (count(&w1c), count(&w2c));
        inner.wake_by_ref();
        assert_eq!(
            (count(&w1c), count(&w2c)),
            (w1_before + 1, w2_before + 1),
            "a wake with no driver was dropped; every awaiter is stranded"
        );
    }

    /// A wake that reaches no one on an *incomplete* cell is counted; the same
    /// wake on a completed cell is routine and is not.
    ///
    /// The counter is the tripwire for the production wedge's signature — the
    /// blocking backstop re-waking an abandoned cell every 250ms with every
    /// wake evaporating. It must not fire for the post-completion straggler,
    /// or every build would end with a pile of false positives in the dump.
    #[test]
    fn a_wake_that_reaches_nobody_is_counted_only_while_incomplete() {
        let (cell, stashed, ready) = stash_cell(5);

        let (_dc, dw) = counting();
        let mut driver = Await::new(Arc::clone(&cell));
        assert!(poll_with(&mut driver, &dw).is_pending());
        let inner = stashed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .expect("inner future must have stashed the cell's waker");

        // The last awaiter goes away; the leaf then completes and wakes. This is
        // the cancellation-race straggler: with the future still in the slot and
        // nobody registered, the wake reaches no one.
        drop(driver);
        let before = void_wakes();
        inner.wake_by_ref();
        assert!(
            void_wakes() > before,
            "a wake into an incomplete cell with no receivers must be counted"
        );

        // Complete the cell; a straggler wake after completion reaches no one
        // *by design* and must not count.
        ready.store(true, Ordering::SeqCst);
        let (_lc, lw) = counting();
        let mut late = Await::new(Arc::clone(&cell));
        assert!(poll_with(&mut late, &lw).is_ready());
        let before = void_wakes();
        inner.wake_by_ref();
        assert_eq!(
            void_wakes(),
            before,
            "a post-completion straggler wake is routine, not a lost wake"
        );
    }

    /// **The regression this module's clone-don't-take exists for.**
    ///
    /// A second wake arriving before the driver has re-polled must still be
    /// delivered. The old code took the driver's waker out of the slab on the
    /// first wake and, finding the slot empty on the second, discarded it —
    /// sound only if re-polling can re-read the readiness. `Semaphore::acquire`
    /// cannot: tokio *assigns* the permit to the queued waiter and wakes it once.
    /// Swallowing that wake strands the permit inside a future nobody polls
    /// again, which is how a build ended up with every worker permit gone, 78
    /// targets queued on the semaphore, and every thread idle.
    #[test]
    fn a_second_wake_before_the_driver_re_polls_is_still_delivered() {
        let (cell, stashed, _ready) = stash_cell(11);

        let (dc, dw) = counting();
        let mut driver = Await::new(Arc::clone(&cell));
        assert!(poll_with(&mut driver, &dw).is_pending());

        let inner = stashed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .expect("inner future must have stashed the cell's waker");

        // Two wakes, no re-poll in between: the driver is woken, and before it
        // gets a chance to run the inner future hands off again.
        inner.wake_by_ref();
        inner.wake_by_ref();

        assert_eq!(
            count(&dc),
            2,
            "the second wake was swallowed — a one-shot handoff is lost forever"
        );
    }

    /// A dropped driver must hand the cell back, not strand it.
    ///
    /// This is the routine path, not an exotic one: `try_join_all` with fail-fast
    /// drops every sibling on the first error, and Ctrl-C drops them wholesale.
    /// A design where drivership is assigned once and owned would leave the
    /// remaining awaiters — and any *later* arrival at the same key — parked
    /// forever.
    #[test]
    fn dropped_driver_hands_the_cell_to_the_others() {
        let (cell, stashed, ready) = stash_cell(11);

        let (bc, bw) = counting();
        let mut b = Await::new(Arc::clone(&cell));
        assert!(poll_with(&mut b, &bw).is_pending());

        let (dc, dw) = counting();
        let mut driver = Await::new(Arc::clone(&cell));
        assert!(poll_with(&mut driver, &dw).is_pending());
        assert_ne!(cell.driver.load(Ordering::SeqCst), NO_DRIVER);

        drop(driver);
        assert_eq!(
            cell.driver.load(Ordering::SeqCst),
            NO_DRIVER,
            "abdication must clear the driver slot"
        );
        assert_eq!(
            count(&bc),
            1,
            "the remaining awaiter must be woken so it can re-elect itself"
        );
        assert_eq!(count(&dc), 0);

        // B re-polls, takes over the *same* future, and completes it.
        ready.store(true, Ordering::SeqCst);
        assert_eq!(poll_with(&mut b, &bw), Poll::Ready(11));
        drop(stashed);
    }

    /// A late arrival at a cell whose driver already went away must still be able
    /// to drive it.
    #[test]
    fn a_new_awaiter_can_drive_an_abandoned_cell() {
        let (cell, _stashed, ready) = stash_cell(3);

        let (_dc, dw) = counting();
        let mut driver = Await::new(Arc::clone(&cell));
        assert!(poll_with(&mut driver, &dw).is_pending());
        drop(driver);

        ready.store(true, Ordering::SeqCst);
        let (_lc, lw) = counting();
        let mut latecomer = Await::new(Arc::clone(&cell));
        assert_eq!(poll_with(&mut latecomer, &lw), Poll::Ready(3));
    }
    /// Completion must drop the shared future immediately.
    ///
    /// `result_addr_impl` is `#[async_recursion]` and its state machine is large
    /// — the `large_futures` lint is on precisely because of this. Holding one
    /// per completed cell for the life of a request would cost far more memory
    /// than the wake fix saves.
    #[test]
    fn completion_drops_the_inner_future() {
        struct Flag(Arc<AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let flag = Flag(Arc::clone(&dropped));
        let cell = Cell::new(Box::pin(async move {
            let _held = flag;
            5u32
        }));
        cell.acquire_interest();

        let (_c, w) = counting();
        let mut waiter = Await::new(Arc::clone(&cell));
        assert_eq!(poll_with(&mut waiter, &w), Poll::Ready(5));

        assert!(
            dropped.load(Ordering::SeqCst),
            "the shared future must be dropped at completion, not retained beside the value"
        );
        // Still resolvable afterwards, from the stored value.
        assert_eq!(cell.peek().copied(), Some(5));
    }

    /// Re-polling the same awaiter updates its slot instead of adding one.
    /// A busy cell is re-polled constantly; growing the set per poll would leak
    /// for the life of the request.
    #[test]
    fn re_registration_does_not_grow_the_waker_set() {
        let (cell, _stashed, _ready) = stash_cell(1);
        let (_c, w) = counting();
        let mut waiter = Await::new(Arc::clone(&cell));
        for _ in 0..1000 {
            assert!(poll_with(&mut waiter, &w).is_pending());
        }
        assert_eq!(cell.wakers().slots.len(), 1);
    }

    /// A dropped awaiter releases its slot, so a long-lived popular cell doesn't
    /// accumulate wakers for tasks that are gone.
    #[test]
    fn dropped_awaiter_is_unregistered_and_its_slot_reused() {
        let (cell, _stashed, _ready) = stash_cell(1);
        let (_c, w) = counting();

        let mut first = Await::new(Arc::clone(&cell));
        assert!(poll_with(&mut first, &w).is_pending());
        assert_eq!(cell.wakers().slots.iter().flatten().count(), 1);
        drop(first);
        assert_eq!(cell.wakers().slots.iter().flatten().count(), 0);

        let mut second = Await::new(Arc::clone(&cell));
        assert!(poll_with(&mut second, &w).is_pending());
        assert_eq!(
            cell.wakers().slots.len(),
            1,
            "the freed slot must be reused rather than appended to"
        );
    }

    /// An awaiter that arrives while the value is being published must observe
    /// it rather than park on a waker set that has already been drained.
    ///
    /// This is the classic lost-wakeup, and it is why registration re-checks
    /// completion while holding the waker lock.
    #[test]
    fn awaiter_racing_completion_never_parks_forever() {
        for _ in 0..2_000 {
            let ready = Arc::new(AtomicBool::new(true));
            let stashed = Arc::new(Mutex::new(None));
            let cell = Cell::new(Box::pin(Stash {
                stashed,
                ready,
                value: 42,
            }));
            cell.acquire_interest();

            let completer = Arc::clone(&cell);
            let handle = std::thread::spawn(move || {
                let (_c, w) = counting();
                let mut a = Await::new(completer);
                poll_with(&mut a, &w)
            });

            let (_c, w) = counting();
            let mut b = Await::new(Arc::clone(&cell));
            let mine = poll_with(&mut b, &w);
            let theirs = handle.join().expect("completer thread");

            // Whoever lost the election is either Ready (value already stored) or
            // Pending-with-a-registered-waker; the latter must have been woken by
            // the winner, so a re-poll resolves it.
            for outcome in [mine, theirs] {
                if outcome.is_pending() {
                    assert_eq!(poll_with(&mut b, &w), Poll::Ready(42));
                }
            }
        }
    }
}

/// How often a waiter of [`timeout_without_reactor`] is re-woken so it can
/// observe its own deadline.
const STALL_TICK: Duration = Duration::from_millis(250);

/// Waiters to re-wake on the next tick, drained each time.
static STALL_PENDING: Mutex<Vec<Waker>> = Mutex::new(Vec::new());
static STALL_THREAD: Once = Once::new();

fn stall_pending() -> MutexGuard<'static, Vec<Waker>> {
    STALL_PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Re-wake `waker` within [`STALL_TICK`], starting the ticker on first use — so a
/// process that never enables the stall check never pays for the thread.
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

/// `tokio::time::timeout`, without touching the reactor.
///
/// The memoizer is statically linked into cdylib plugins, and a plugin's
/// `provider_get` / `list` / `probe` / `parse` are awaited **inline by host
/// workers** — only `driver_run_stream` hops to the plugin's own runtime. So the
/// plugin's tokio holds no runtime context there, a timer call panics with "there
/// is no reactor running", and that panic aborts the process at the `extern "C"`
/// seam. This function is reached only when `HEPH_MEMOIZER_STALL_SECS` is set,
/// i.e. exactly when someone is already debugging a hang — the tool would abort
/// on the workload it exists for. Same class of bug as PR #180/#182.
///
/// So: a plain `Instant` deadline checked on each poll, plus a plain-thread
/// ticker to guarantee those polls happen even when the inner future never wakes
/// (which is the stalled case, and the whole point). Nothing here is a timer.
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

impl<V> Drop for Await<V> {
    fn drop(&mut self) {
        let Some(key) = self.key else {
            return;
        };
        let was_driver = self
            .cell
            .driver
            .compare_exchange(key, NO_DRIVER, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();

        let mut wakers = self.cell.wakers();
        wakers.remove(key);

        // We were the one the inner future would have woken, and we are going
        // away. Hand the cell back to the others — otherwise the next inner wake
        // has nobody to reach and every remaining awaiter parks forever. This is
        // the cancellation path: fail-fast sibling drops and Ctrl-C both land
        // here.
        if was_driver && self.cell.done.get().is_none() {
            let all = wakers.take_all();
            drop(wakers);
            for waker in all {
                waker.wake();
            }
        }
    }
}
