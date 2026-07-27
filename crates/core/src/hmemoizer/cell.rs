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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Once, OnceLock};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

/// [`Cell::driver`] when no awaiter is currently on the hook for re-polling.
const NO_DRIVER: usize = usize::MAX;

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

    /// Take the waker at `key`, if one is registered. The awaiter re-registers on
    /// its next poll — same contract as the slab `Shared` uses.
    fn take(&mut self, key: usize) -> Option<Waker> {
        self.slots.get_mut(key).and_then(Option::take)
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
    /// Set when the driver has been woken and has not yet re-polled.
    ///
    /// Without it, a burst of inner wakes is indistinguishable from a dead
    /// driver: the first wake consumes the driver's waker slot, and every wake
    /// after it sees an empty slot. Treating that as "unreachable" and waking
    /// everyone would reintroduce the storm on exactly the bursty workload this
    /// module exists to fix; treating it as "already scheduled" and doing nothing
    /// would drop a real wake when the driver is genuinely gone. This latch
    /// distinguishes the two.
    driver_awake: AtomicBool,
}

impl<V> Cell<V> {
    pub(crate) fn new(fut: BoxFuture<'static, V>) -> Arc<Self> {
        Arc::new(Self {
            done: OnceLock::new(),
            slot: Mutex::new(Some(fut)),
            wakers: Mutex::new(Wakers::default()),
            driver: AtomicUsize::new(NO_DRIVER),
            driver_awake: AtomicBool::new(false),
        })
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
            wakers.take(driver)
        };

        // The optimization: an inner wake normally reaches exactly one task, not
        // all W of them. That is the whole point of the module.
        if let Some(waker) = targeted {
            // The driver now owes us a re-poll; further wakes until then are
            // redundant.
            cell.driver_awake.store(true, Ordering::Release);
            drop(wakers);
            waker.wake();
            return;
        }

        // The slot is empty. If the driver already owes us a re-poll, it will
        // observe this progress when it runs — polling the inner future re-reads
        // its state, so nothing is lost by staying quiet. This is the common case
        // during a burst, and waking everyone here would be the storm again.
        if driver != NO_DRIVER && cell.driver_awake.load(Ordering::Acquire) {
            return;
        }

        // No driver, or a driver that owes us nothing and has no waker — nobody
        // is going to poll this cell. Wake everyone and let one re-elect itself.
        // An extra wake costs one spurious poll; a missing one hangs the cell
        // forever, and at the tail of a build there is no ambient re-polling left
        // to rescue it.
        let all = wakers.take_all();
        drop(wakers);
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
    pub(crate) fn new(cell: Arc<Cell<V>>) -> Self {
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
        // Clear before polling, so a wake raised *during* the poll re-arms the
        // latch rather than being swallowed as "already notified".
        cell.driver_awake.store(false, Ordering::Release);

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
        (Cell::new(Box::pin(fut)), stashed, ready)
    }

    fn poll_with(waiter: &mut Await<u32>, waker: &Waker) -> Poll<u32> {
        Pin::new(waiter).poll(&mut Context::from_waker(waker))
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
            1,
            "the driver is woken once and re-registers on its next poll"
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

        // First wake consumes the driver's slot and arms the latch.
        inner.wake_by_ref();
        assert_eq!(count(&dc), 1);
        assert_eq!((count(&w1c), count(&w2c)), (0, 0));

        // Still latched: the driver owes a re-poll, so further wakes stay quiet
        // rather than fanning out. This is the burst case, and waking everyone
        // here would be the storm again.
        inner.wake_by_ref();
        assert_eq!(
            (count(&w1c), count(&w2c)),
            (0, 0),
            "a wake fanned out while the driver was already scheduled"
        );

        // Now simulate the driver being unreachable: it owes nothing and has no
        // waker registered. The wake must reach the others rather than vanish.
        cell.driver_awake.store(false, Ordering::SeqCst);
        inner.wake_by_ref();
        assert_eq!(
            (count(&w1c), count(&w2c)),
            (1, 1),
            "a wake with an unreachable driver was dropped; every awaiter is stranded"
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
        if was_driver {
            // We owed the cell a re-poll and will never make it.
            self.cell.driver_awake.store(false, Ordering::Release);
        }

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
