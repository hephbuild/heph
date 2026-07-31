//! Task-backed memoizer cell: the computation is a spawned tokio task, not a
//! future cooperatively polled by its awaiters.
//!
//! The poll-based cell (`cell.rs`) exists for two constraints that no longer
//! hold: plugin-linked code had no runtime context (fixed by spawn-at-the-seam
//! — every ABI entry point body now runs on its side's runtime), and
//! cancellation-by-abandonment (a future nobody polls) broke `futures::Shared`
//! and fair-semaphore semantics. Here the computation is always polled by the
//! runtime, and cancellation is an explicit `JoinHandle::abort`, so the driver
//! election, the hand-rolled waker slab, and the stall-ticker insurance all
//! have nothing left to insure.
//!
//! State machine: a cell is spawned `Running` and only ever moves to `Done`.
//! There is no `Idle` and no reuse: cancellation *evicts* the cell from the
//! map (the `cancel_abandoned` protocol carried over from the poll cell, with
//! `abort()` in place of take-future-and-drop), and a later caller builds a
//! fresh cell. A task publishes only into its own cell, so a publish that
//! races eviction lands in an unreachable cell and is harmless.
//!
//! ## The arbiter lock
//!
//! The poll cell proves "interest == 0 ∧ !done ⇒ abandoned" through the
//! completer's own interest release. That proof's premise dies here — the
//! completer is the task, and it holds no interest. Instead, publish and the
//! cancel decision serialize on the *cache* lock: publish sets `done` (and
//! drops the task handle) inside a cache-lock section, and cancellation
//! re-reads `interest` and `is_done` under that same lock before evicting.
//! No fence argument, no reasoning from the atomic alone.
//!
//! ## No two bodies for one key, ever
//!
//! `abort()` is a request — the task dies when the runtime next processes it,
//! and its destructors run on a runtime worker, not on the canceller's stack.
//! A successor spawned in that window must not overlap the predecessor, so the
//! canceller parks the aborted `JoinHandle` in a per-key grave and the
//! successor's task awaits it before running its own body. If the successor is
//! itself aborted *while still awaiting the grave*, its drop re-parks the
//! not-yet-dead predecessor handle (`ReGrave`) — it never started its body, so
//! its own handle is the wrong thing for generation N+2 to wait on. The grave
//! insert happens *before* the abort is issued so the re-park can never race
//! the canceller's own insert.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use futures::FutureExt as _;
use rustc_hash::FxHashMap;

/// One memoized computation. `Running` while the spawned task lives; `Done`
/// once `done` is set (at which point `task` is empty — a completed cell pins
/// no dead task harness).
pub(crate) struct TaskCell<V> {
    done: OnceLock<V>,
    /// Terminal failure that is not a value: the task was dropped without
    /// publishing (a panicking body outside `once()`'s guard, or its runtime
    /// shut down). Waiters surface it loudly instead of parking forever.
    poison: OnceLock<&'static str>,
    /// Wakes waiters on publish/poison. Waiters use the register-then-recheck
    /// discipline (see `wait_done`) — `notify_waiters` leaves nothing behind
    /// for a waiter that registers later, so the recheck is what's load-bearing.
    notify: tokio::sync::Notify,
    /// Callers currently interested. Fast-path counter only: the abandonment
    /// *decision* is always re-taken under the cache lock (module docs).
    interest: AtomicUsize,
    /// The running task. Taken on publish (`Done` holds no handle) and on
    /// cancellation (moved to the grave).
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// When the computation was spawned — diagnostic age in the SIGQUIT dump.
    created: Instant,
    /// How many aborted predecessors this key has had. Rendered in the dump so
    /// abort/rejoin thrash is visible instead of read as "mysteriously slow".
    restarts: u32,
}

impl<V> TaskCell<V> {
    pub(crate) fn peek(&self) -> Option<&V> {
        self.done.get()
    }

    pub(crate) fn is_done(&self) -> bool {
        self.done.get().is_some()
    }

    pub(crate) fn acquire_interest(&self) {
        // Always taken under the cache lock (both `process` arms), so Relaxed
        // suffices — the lock orders it against every decision that reads it.
        self.interest.fetch_add(1, Ordering::Relaxed);
    }

    /// Returns the remaining count. AcqRel so the "exactly one guard observes
    /// each zero crossing" property holds; the *decision* still re-reads under
    /// the cache lock.
    pub(crate) fn release_interest(&self) -> usize {
        let prev = self.interest.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev > 0, "interest release without a matching acquire");
        prev - 1
    }

    pub(crate) fn interest(&self) -> usize {
        self.interest.load(Ordering::Acquire)
    }

    fn task_slot(&self) -> MutexGuard<'_, Option<tokio::task::JoinHandle<()>>> {
        // Poisoning ignored for the same reason as the poll cell's waker set:
        // the critical sections only move a handle around, so the slot is
        // structurally intact, and refusing it would strand the protocol.
        self.task.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the spawned task is still live (spawned, not finished, not yet
    /// cancelled away). `false` on an incomplete cell is the stranded signal:
    /// the task died without publishing.
    pub(crate) fn task_live(&self) -> bool {
        self.task
            .try_lock()
            .map(|slot| slot.as_ref().is_some_and(|h| !h.is_finished()))
            // Slot briefly held (spawn/publish/cancel in progress) — the task
            // is being worked on, which is the opposite of stranded.
            .unwrap_or(true)
    }

    pub(crate) fn age(&self) -> std::time::Duration {
        self.created.elapsed()
    }

    pub(crate) fn restarts(&self) -> u32 {
        self.restarts
    }
}

/// An aborted predecessor for a key: its handle (awaited by the successor
/// before the successor's body runs) and its accumulated restart count.
struct Grave {
    handle: tokio::task::JoinHandle<()>,
    restarts: u32,
}

/// Task-backed implementation behind `Memoizer`. Same map + interest protocol
/// as the poll implementation; the computation lifecycle is the module docs'
/// state machine.
pub(crate) struct TaskInner<K, V> {
    cache: Arc<Mutex<FxHashMap<K, Arc<TaskCell<V>>>>>,
    /// Aborted-but-possibly-still-dying predecessors, per key. Entries are
    /// consumed by the next caller for the key; unconsumed entries die with
    /// the memoizer (bounded by cancelled keys per request).
    graves: Arc<Mutex<FxHashMap<K, Grave>>>,
    tag: &'static str,
    /// Captured at construction — the runtime every cold cell spawns on. Never
    /// discovered via `Handle::current()` at spawn time: whichever runtime the
    /// first caller happened to be on winning silently is exactly the
    /// environment assumption the plugin rules ban.
    handle: tokio::runtime::Handle,
}

impl<K, V> TaskInner<K, V>
where
    K: std::hash::Hash + Eq + Send + Sync + 'static + std::fmt::Debug + Clone,
    V: Clone + Send + Sync + 'static,
{
    pub(crate) fn new(tag: &'static str, handle: tokio::runtime::Handle) -> Self {
        let cache = Arc::new(Mutex::new(FxHashMap::default()));
        super::register_source(Box::new(TaskSource {
            tag,
            cache: Arc::downgrade(&cache),
        }));
        Self {
            cache,
            graves: Arc::new(Mutex::new(FxHashMap::default())),
            tag,
            handle,
        }
    }

    pub(crate) fn tag(&self) -> &'static str {
        self.tag
    }

    /// Remove `key` iff its cell is completed and its value satisfies `pred`.
    /// An in-flight cell is never touched — same contract as the poll path's
    /// cycle-error eviction.
    pub(crate) fn evict_if(&self, key: &K, pred: impl FnOnce(&V) -> bool) {
        let mut cache = self.cache_lock();
        if cache
            .get(key)
            .is_some_and(|cell| cell.peek().is_some_and(pred))
        {
            cache.remove(key);
        }
    }

    fn cache_lock(&self) -> MutexGuard<'_, FxHashMap<K, Arc<TaskCell<V>>>> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn peek(&self, key: &K) -> Option<V> {
        self.cache_lock().get(key).and_then(|c| c.peek().cloned())
    }

    pub(crate) async fn process<F, Fut>(&self, key: K, f: F) -> V
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = V> + Send + 'static,
    {
        let cell = {
            let mut cache = self.cache_lock();
            match cache.entry(key.clone()) {
                std::collections::hash_map::Entry::Occupied(e) => {
                    if let Some(v) = e.get().peek() {
                        return v.clone();
                    }
                    // Under the lock, so a cancellation racing us either sees
                    // this interest and stands down, or already evicted the
                    // entry and we never find it — same rule as the poll cell.
                    e.get().acquire_interest();
                    Arc::clone(e.get())
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    let grave = self
                        .graves
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&key);
                    let restarts = grave.as_ref().map_or(0, |g| g.restarts + 1);
                    let cell = Arc::new(TaskCell {
                        done: OnceLock::new(),
                        poison: OnceLock::new(),
                        notify: tokio::sync::Notify::new(),
                        interest: AtomicUsize::new(0),
                        task: Mutex::new(None),
                        created: Instant::now(),
                        restarts,
                    });
                    cell.acquire_interest();
                    // Lazy async blocks: `f()` only builds the state machine,
                    // so constructing it under the lock is free (and spares
                    // insert-race losers a wasted allocation — not that a
                    // vacant entry has racers under this lock).
                    let fut = f();
                    let body = BodyTask {
                        cell: Arc::clone(&cell),
                        cache: Arc::clone(&self.cache),
                        graves: Arc::clone(&self.graves),
                        key: key.clone(),
                        grave,
                    };
                    // Spawn while still holding the cache lock: publish also
                    // takes that lock, so the task cannot publish before its
                    // handle is stored below — a `Done` cell never ends up
                    // holding a live handle.
                    //
                    // The spawn itself is guarded: on a shut-down runtime,
                    // tokio either panics the spawn or drops the task without
                    // ever polling it. Both degrade to a poisoned cell (the
                    // drop path via `BodyTask::drop`), so a joiner gets a loud
                    // failure — never an eternal park, and never an unwind out
                    // of this frame into a cdylib seam.
                    let spawned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        super::spawn_on_with_cycle_ctx(&self.handle, body.run(fut))
                    }));
                    match spawned {
                        Ok(task) => {
                            *cell.task_slot() = Some(task);
                        }
                        Err(_) => {
                            let _already_poisoned = cell
                                .poison
                                .set("memoized task could not spawn (runtime shut down)");
                        }
                    }
                    e.insert(Arc::clone(&cell));
                    cell
                }
            }
        };

        // Cancel the computation if we turn out to be its last awaiter —
        // declared before the await so it drops after the wait is gone.
        let mut abandon = TaskAbandonGuard {
            inner: self,
            key: &key,
            cell: Arc::clone(&cell),
            armed: true,
        };
        let out = super::await_with_stall_check(wait_done(&cell, self.tag), &key, self.tag).await;
        abandon.armed = false;
        out
    }

    /// Evict-and-abort, unless somebody wants the cell after all. The decision
    /// re-checks under the cache lock (the arbiter — see module docs); the
    /// abort itself happens with no lock held, matching the poll cell's
    /// "never hold the cache lock across teardown" discipline.
    fn cancel_abandoned(&self, key: &K, cell: &Arc<TaskCell<V>>) {
        {
            let mut cache = self.cache_lock();
            if cell.interest() != 0 || cell.is_done() {
                return;
            }
            // Evict before aborting, so a later caller builds a fresh cell
            // rather than joining one that can never complete. `ptr_eq`-guarded:
            // a fresh cell under the same key is never evicted by a stale
            // cancellation.
            if cache.get(key).is_some_and(|c| Arc::ptr_eq(c, cell)) {
                cache.remove(key);
            }
        }

        // Idempotent across two zero-crossings on the same cell: the loser
        // finds the slot empty.
        let Some(task) = cell.task_slot().take() else {
            return;
        };
        tracing::debug!(
            tag = self.tag,
            key = ?key,
            restarts = cell.restarts,
            "memoized computation abandoned; aborting its task"
        );
        // Grave BEFORE abort: once the abort is issued the task can be dropped
        // at any instant, and `ReGrave` (its drop path) may re-park a
        // predecessor under this key — that insert must find ours already
        // present, never overwrite-race it.
        let abort = task.abort_handle();
        self.graves
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                key.clone(),
                Grave {
                    handle: task,
                    restarts: cell.restarts,
                },
            );
        abort.abort();
    }
}

/// The spawned computation: await the predecessor's grave (no two bodies for
/// one key overlap — module docs), run the body, publish. Publishing is
/// infallible from the waiters' perspective: if this future is dropped without
/// publishing (panic in the body outside `once()`'s guard, runtime shutdown),
/// the drop poisons the cell so waiters fail loudly instead of parking
/// forever.
struct BodyTask<K, V>
where
    K: std::hash::Hash + Eq + Clone,
{
    cell: Arc<TaskCell<V>>,
    cache: Arc<Mutex<FxHashMap<K, Arc<TaskCell<V>>>>>,
    graves: Arc<Mutex<FxHashMap<K, Grave>>>,
    key: K,
    grave: Option<Grave>,
}

impl<K, V> BodyTask<K, V>
where
    K: std::hash::Hash + Eq + Send + Sync + 'static + Clone,
    V: Clone + Send + Sync + 'static,
{
    async fn run<Fut: Future<Output = V>>(mut self, fut: Fut) {
        if let Some(g) = self.grave.as_mut() {
            // `&mut JoinHandle` is a future (JoinHandle is Unpin), so an abort
            // landing mid-await leaves the handle in place for `Drop` to
            // re-park. A JoinError here is the expected `is_cancelled` — the
            // predecessor was aborted, that's why it's in a grave.
            let _cancelled = (&mut g.handle).await;
        }
        // Predecessor observed dead: it can no longer touch anything. Only now
        // is the body allowed to run.
        self.grave = None;
        let v = fut.await;
        self.publish(v);
    }

    fn publish(self, v: V) {
        // Under the cache lock — the arbiter between publish and the cancel
        // decision (module docs). Also drops the task's own handle: a `Done`
        // cell holds no dead task harness. (Dropping one's own JoinHandle is a
        // detach, which is exactly right — the task is finishing.)
        {
            let _arbiter = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            let _first_publish = self.cell.done.set(v);
            *self.cell.task_slot() = None;
        }
        self.cell.notify.notify_waiters();
        // `self` drops disarmed: `done` is set, so `Drop` won't poison.
        // The consumed grave (if any) is gone — nothing to re-park.
    }
}

impl<K, V> Drop for BodyTask<K, V>
where
    K: std::hash::Hash + Eq + Clone,
{
    fn drop(&mut self) {
        // Aborted while still awaiting the predecessor's grave: this task
        // never ran its body, so its own handle is the wrong thing for the
        // next generation to serialize on — re-park the predecessor's.
        // (The canceller's own grave insert always precedes the abort, so
        // this overwrite replaces *this* task's handle with the still-dying
        // predecessor's — the one that might still have body state.)
        if let Some(g) = self.grave.take() {
            self.graves
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(self.key.clone(), g);
        }
        if self.cell.done.get().is_some() {
            return;
        }
        // Dropped without publishing. For an aborted cell this is ordinary
        // (evicted, zero interest — the notify wakes nobody). For a live cell
        // it means the body panicked (only reachable outside `once()`'s
        // guard) or the runtime shut down mid-flight: poison so waiters fail
        // loudly instead of parking forever.
        let _already_poisoned = self.cell.poison.set(
            "memoized task dropped without publishing (body panicked or its runtime shut down)",
        );
        self.cell.notify.notify_waiters();
    }
}

/// Await a cell's publication. Register-then-recheck: the `Notified` is
/// registered (`enable`) *before* the `done`/`poison` check, so a publish
/// landing between the check and the await is observed — `notify_waiters`
/// stores nothing for late registrants, making this ordering load-bearing
/// (same discipline as the engine's `WorkerPool` acquire loop).
#[expect(
    clippy::panic,
    reason = "a poisoned cell has no value and never will; the panic is typed \
              (PoisonPanic) so once() converts it to a memoized Err, and a raw \
              process() joiner fails loudly instead of parking forever"
)]
async fn wait_done<V: Clone>(cell: &TaskCell<V>, tag: &'static str) -> V {
    loop {
        let notified = cell.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(v) = cell.done.get() {
            return v.clone();
        }
        if let Some(msg) = cell.poison.get() {
            // Loud on purpose: a poisoned cell has no value and never will.
            // `once()` catches this and memoizes it as an `Err`; a raw
            // `process()` caller surfaces it as a panic in the joiner. A typed
            // payload, so `catch_poison` converts exactly this panic and
            // resumes every other one (the debug stall panic must stay loud).
            std::panic::panic_any(PoisonPanic {
                tag,
                msg: (*msg).to_string(),
            });
        }
        notified.await;
    }
}

/// Panic payload for a poisoned cell — typed so [`catch_poison`] can tell it
/// apart from panics that must propagate.
struct PoisonPanic {
    tag: &'static str,
    msg: String,
}

/// `once()`-side wrapper: a poisoned task cell panics its joiners (see
/// [`wait_done`]); for the `Result`-typed `once` surface that panic is caught
/// here and memoized-shaped as an `Err`, so a shut-down runtime or an
/// unguarded panic degrades to a failed target instead of unwinding into a
/// cdylib seam (where an unwind is an abort).
pub(crate) async fn catch_poison<T, Fut>(fut: Fut) -> Result<T, Arc<anyhow::Error>>
where
    Fut: Future<Output = Result<T, Arc<anyhow::Error>>>,
{
    match std::panic::AssertUnwindSafe(fut).catch_unwind().await {
        Ok(r) => r,
        Err(panic) => match panic.downcast::<PoisonPanic>() {
            Ok(poison) => Err(Arc::new(anyhow::anyhow!(
                "[memoizer:{}] {}",
                poison.tag,
                poison.msg
            ))),
            // Anything else (the debug-only stall panic, a bug) stays loud.
            Err(other) => std::panic::resume_unwind(other),
        },
    }
}

/// Last-awaiter cancellation for the task cell — the poll cell's
/// `AbandonGuard`, with the decision delegated to [`TaskInner::cancel_abandoned`].
struct TaskAbandonGuard<'a, K, V>
where
    K: std::hash::Hash + Eq + Send + Sync + 'static + std::fmt::Debug + Clone,
    V: Clone + Send + Sync + 'static,
{
    inner: &'a TaskInner<K, V>,
    key: &'a K,
    cell: Arc<TaskCell<V>>,
    armed: bool,
}

impl<K, V> Drop for TaskAbandonGuard<'_, K, V>
where
    K: std::hash::Hash + Eq + Send + Sync + 'static + std::fmt::Debug + Clone,
    V: Clone + Send + Sync + 'static,
{
    fn drop(&mut self) {
        let remaining = self.cell.release_interest();
        if !self.armed || remaining != 0 || self.cell.is_done() {
            return;
        }
        self.inner.cancel_abandoned(self.key, &self.cell);
    }
}

/// Inventory source for task-cell maps. The existing `StuckCell` semantics
/// carry over exactly: `has_driver` maps to "the task is live", so
/// `is_stranded` (waiters, no driver) becomes "the task died without
/// publishing while joiners wait" — the signal that used to require the
/// driver-election bookkeeping now falls out of `JoinHandle::is_finished`.
struct TaskSource<K, V> {
    tag: &'static str,
    cache: std::sync::Weak<Mutex<FxHashMap<K, Arc<TaskCell<V>>>>>,
}

impl<K, V> super::CellSource for TaskSource<K, V>
where
    K: std::fmt::Debug + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    fn collect(&self, out: &mut Vec<super::StuckCell>) -> bool {
        let Some(cache) = self.cache.upgrade() else {
            return false;
        };
        // `try_lock`: never block a diagnostic dump on the process being dumped.
        let map = match cache.try_lock() {
            Ok(m) => m,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return true,
        };
        for (key, cell) in map.iter() {
            if cell.is_done() {
                continue;
            }
            out.push(super::StuckCell {
                tag: self.tag,
                key: format!("{key:?}"),
                waiters: Some(cell.interest()),
                has_driver: cell.task_live(),
                restarts: cell.restarts(),
                age: Some(cell.age()),
            });
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;

    fn inner<K, V>(tag: &'static str) -> TaskInner<K, V>
    where
        K: std::hash::Hash + Eq + Send + Sync + 'static + std::fmt::Debug + Clone,
        V: Clone + Send + Sync + 'static,
    {
        TaskInner::new(tag, tokio::runtime::Handle::current())
    }

    const TIMEOUT: Duration = Duration::from_secs(5);

    async fn within<T>(fut: impl Future<Output = T>) -> T {
        tokio::time::timeout(TIMEOUT, fut)
            .await
            .expect("test future must complete within the timeout")
    }

    /// The core no-overlap guarantee: an aborted body's destructors complete
    /// before the successor's body starts. The body holds an RAII occupancy
    /// guard; the successor asserts the section is empty on entry. Without the
    /// grave await this fails whenever the runtime processes the abort after
    /// the successor spawns.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn no_two_bodies_for_one_key_ever_overlap() {
        struct Occupancy(Arc<AtomicUsize>);
        impl Occupancy {
            fn enter(counter: &Arc<AtomicUsize>) -> Self {
                let prev = counter.fetch_add(1, Ordering::SeqCst);
                assert_eq!(prev, 0, "two bodies for one key are executing concurrently");
                Self(Arc::clone(counter))
            }
        }
        impl Drop for Occupancy {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }

        let m: Arc<TaskInner<String, u32>> = Arc::new(inner("overlap-test"));
        let occupancy = Arc::new(AtomicUsize::new(0));

        for round in 0..50u32 {
            // Fresh key per round: a completed successor stays memoized, so
            // reusing one key would make every later round a warm hit.
            let key = format!("k{round}");
            // First joiner: body enters the section and parks forever. Dropping
            // the joiner is the last-interest abort.
            let first = {
                let occupancy = Arc::clone(&occupancy);
                let m = Arc::clone(&m);
                let key = key.clone();
                let mut fut = Box::pin(async move {
                    m.process(key, move || async move {
                        let _occ = Occupancy::enter(&occupancy);
                        futures::future::pending::<()>().await;
                        0u32
                    })
                    .await
                });
                // Poll once so the cell exists and the task is spawned.
                assert!(futures::poll!(&mut fut).is_pending());
                fut
            };
            // Give the spawned body a chance to actually enter the section.
            tokio::task::yield_now().await;
            drop(first);

            // Immediate re-join: the successor body asserts sole occupancy.
            let occupancy2 = Arc::clone(&occupancy);
            let v = within(m.process(key, move || async move {
                let _occ = Occupancy::enter(&occupancy2);
                round
            }))
            .await;
            assert_eq!(v, round);
        }
    }

    /// A stale cancellation must never evict a completed cell — the value
    /// survives and no recompute happens. (The publish/cancel arbitration is
    /// the cache lock; this drives the loser's path by hand.)
    #[tokio::test]
    async fn a_stale_cancellation_never_evicts_a_completed_cell() {
        let m: TaskInner<String, u32> = inner("stale-vs-done-test");
        let runs = Arc::new(AtomicU32::new(0));

        let v = within(m.process("k".to_string(), {
            let runs = Arc::clone(&runs);
            move || async move {
                runs.fetch_add(1, Ordering::SeqCst);
                5
            }
        }))
        .await;
        assert_eq!(v, 5);

        let cell = m
            .cache_lock()
            .get("k")
            .cloned()
            .expect("a completed cell is retained as the memoized answer");
        m.cancel_abandoned(&"k".to_string(), &cell);

        let v = within(m.process("k".to_string(), {
            let runs = Arc::clone(&runs);
            move || async move {
                runs.fetch_add(1, Ordering::SeqCst);
                7
            }
        }))
        .await;
        assert_eq!(v, 5, "the memoized value must survive a stale cancellation");
        assert_eq!(runs.load(Ordering::SeqCst), 1, "no recompute");
    }

    /// The `ptr_eq` eviction guard: a stale cancellation of an old cell never
    /// evicts the fresh cell a later caller re-created under the same key.
    #[tokio::test]
    async fn a_stale_cancellation_never_evicts_a_recreated_cell() {
        let m: TaskInner<String, u32> = inner("stale-vs-fresh-test");

        let mut first = Box::pin(m.process("k".to_string(), || async {
            futures::future::pending::<u32>().await
        }));
        assert!(futures::poll!(&mut first).is_pending());
        let old_cell = m
            .cache_lock()
            .get("k")
            .cloned()
            .expect("in-flight cell is in the map");
        drop(first); // last-interest abort + evict

        // Idempotent against the already-evicted cell.
        m.cancel_abandoned(&"k".to_string(), &old_cell);

        let gate = Arc::new(tokio::sync::Notify::new());
        let mut second = Box::pin(m.process("k".to_string(), {
            let gate = Arc::clone(&gate);
            move || async move {
                gate.notified().await;
                3
            }
        }));
        assert!(futures::poll!(&mut second).is_pending());

        // The stale cancellation must not touch the fresh cell.
        m.cancel_abandoned(&"k".to_string(), &old_cell);

        // `notify_one`, not `notify_waiters`: the body runs in a spawned task
        // that may not have registered yet — `notify_one` leaves a permit for
        // it, where `notify_waiters` would be lost and the body would park.
        gate.notify_one();
        let v = within(&mut second).await;
        assert_eq!(v, 3);
    }

    /// A `Done` cell holds no `JoinHandle` — a completed computation must not
    /// pin a dead task harness for the life of the request.
    #[tokio::test]
    async fn a_done_cell_holds_no_join_handle() {
        let m: TaskInner<String, u32> = inner("done-handle-test");
        let v = within(m.process("k".to_string(), || async { 9 })).await;
        assert_eq!(v, 9);
        let cell = m.cache_lock().get("k").cloned().expect("completed cell");
        assert!(
            cell.task_slot().is_none(),
            "a Done cell must not retain its JoinHandle"
        );
    }

    /// Register-then-recheck: a joiner racing the publish must never park
    /// forever. Loop hard enough that both interleavings actually occur.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn joiner_racing_completion_never_parks_forever() {
        let m: Arc<TaskInner<u32, u32>> = Arc::new(inner("race-test"));
        for i in 0..2000u32 {
            let a = {
                let m = Arc::clone(&m);
                tokio::spawn(async move { m.process(i, move || async move { i }).await })
            };
            let b = {
                let m = Arc::clone(&m);
                tokio::spawn(async move { m.process(i, move || async move { i }).await })
            };
            let (a, b) = within(async { tokio::join!(a, b) }).await;
            assert_eq!(a.expect("joiner a"), i);
            assert_eq!(b.expect("joiner b"), i);
        }
    }

    /// Cross-runtime: a joiner on runtime A awaits a cell whose task runs on
    /// runtime B; publish crosses runtimes. Abort from A of a task on B is
    /// clean, and a successor completes.
    #[tokio::test]
    async fn cross_runtime_join_publish_and_abort() {
        let rt_b = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build runtime B");
        let m: Arc<TaskInner<String, u32>> =
            Arc::new(TaskInner::new("cross-rt-test", rt_b.handle().clone()));

        // Publish crosses from B to this (A) runtime's joiner.
        let v = within(m.process("k1".to_string(), || async { 11 })).await;
        assert_eq!(v, 11);

        // Abort from A of a task running on B, then a successor completes.
        let mut hung = Box::pin(m.process("k2".to_string(), || async {
            futures::future::pending::<u32>().await
        }));
        assert!(futures::poll!(&mut hung).is_pending());
        drop(hung);
        let v = within(m.process("k2".to_string(), || async { 12 })).await;
        assert_eq!(v, 12);

        // A runtime cannot be dropped from async context.
        tokio::task::spawn_blocking(move || drop(rt_b))
            .await
            .expect("drop runtime B");
    }

    /// A cold join against a shut-down runtime fails loudly — poisoned cell,
    /// not an eternal park. (`once()` additionally converts the loud failure
    /// into a memoized `Err`; that conversion is asserted separately below.)
    #[tokio::test]
    async fn spawn_on_a_shut_down_runtime_is_loud_not_a_hang() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("build runtime");
        let m: Arc<TaskInner<String, u32>> =
            Arc::new(TaskInner::new("dead-rt-test", rt.handle().clone()));
        // A runtime cannot be dropped from async context.
        tokio::task::spawn_blocking(move || drop(rt))
            .await
            .expect("shut down the runtime");

        let joined = within(
            std::panic::AssertUnwindSafe(m.process("k".to_string(), || async { 1 })).catch_unwind(),
        )
        .await;
        let panic = joined.expect_err("a dead runtime must not produce a value");
        assert!(
            panic.downcast_ref::<PoisonPanic>().is_some(),
            "the failure must be the typed poison panic"
        );
    }

    /// `catch_poison` converts exactly the poison panic into an `Err` and
    /// resumes everything else.
    #[tokio::test]
    async fn catch_poison_converts_poison_and_resumes_other_panics() {
        let converted = catch_poison::<u32, _>(async {
            std::panic::panic_any(PoisonPanic {
                tag: "t",
                msg: "boom".to_string(),
            })
        })
        .await;
        let err = converted.expect_err("poison must convert to Err");
        assert!(err.to_string().contains("boom"));

        let resumed =
            std::panic::AssertUnwindSafe(catch_poison::<u32, _>(async { panic!("not poison") }))
                .catch_unwind()
                .await;
        assert!(resumed.is_err(), "a non-poison panic must be resumed");
    }

    /// A body that panics in raw `process` (no `once` guard) poisons the cell:
    /// the joiner fails loudly instead of parking forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panicking_body_poisons_the_cell_instead_of_stranding_joiners() {
        let m: Arc<TaskInner<String, u32>> = Arc::new(inner("panic-test"));
        let joined = within(
            std::panic::AssertUnwindSafe(m.process("k".to_string(), || async {
                panic!("body panic");
            }))
            .catch_unwind(),
        )
        .await;
        let panic = joined.expect_err("a panicking body must not produce a value");
        assert!(panic.downcast_ref::<PoisonPanic>().is_some());
    }

    /// Restart accounting: each abort→rejoin generation increments the
    /// successor cell's counter, and the inventory carries it (with an age)
    /// so thrash is visible in the dump.
    #[tokio::test]
    async fn restarts_are_counted_and_reported() {
        let m: TaskInner<String, u32> = inner("restart-count-test");

        for expected in 0..3u32 {
            let mut hung = Box::pin(m.process("k".to_string(), || async {
                futures::future::pending::<u32>().await
            }));
            assert!(futures::poll!(&mut hung).is_pending());
            let cell = m.cache_lock().get("k").cloned().expect("in-flight cell");
            assert_eq!(cell.restarts(), expected);
            drop(hung);
        }

        // The next generation appears in the inventory with its restart count.
        let mut hung = Box::pin(m.process("k".to_string(), || async {
            futures::future::pending::<u32>().await
        }));
        assert!(futures::poll!(&mut hung).is_pending());
        let stuck = super::super::inventory();
        let mine = stuck
            .iter()
            .find(|c| c.tag == "restart-count-test")
            .expect("in-flight task cell must appear in the inventory");
        assert_eq!(mine.restarts, 3);
        assert!(mine.age.is_some(), "task cells report their age");
        assert_eq!(mine.waiters, Some(1));
        assert!(mine.has_driver, "a live task is the driver");
        drop(hung);
    }
}
