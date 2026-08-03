//! Task-backed memoizer cell: the computation is a spawned tokio task, not a
//! future cooperatively polled by its awaiters.
//!
//! The poll-based cell it replaced existed for two constraints that no longer
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
//! The arbiter is **sharded by key hash** (see [`Maps::cells`]) — it is a
//! per-key arbiter that happened to be implemented as one lock, and every
//! operation on the map is confined to a single key. Read "the cache lock"
//! throughout this module as "the shard owning this key"; publish and the
//! cancel decision for one key still take the very same lock as each other,
//! which is the whole of what the argument above needs.
//!
//! ## No two bodies for one key, ever
//!
//! `abort()` is a request — the task dies when the runtime next processes it,
//! and its destructors run on a runtime worker, not on the canceller's stack.
//! A successor spawned in that window must not overlap the predecessor, so the
//! canceller parks the aborted `JoinHandle` in a per-key grave and the
//! successor's task awaits it before running its own body. Three orderings
//! make this airtight rather than merely likely:
//!
//! * The eviction, the handle-take, and the grave park are **one cache-lock
//!   critical section** — a caller that finds the map vacant is guaranteed to
//!   also find the grave (park-after-unlock would let it spawn grave-less).
//! * A successor aborted *while still awaiting the grave* never started its
//!   body, so its own handle is the wrong thing for generation N+2 to wait
//!   on: its drop **re-parks** the not-yet-dead predecessor handle
//!   (`ReGrave`), with the canceller's own insert ordered before the abort so
//!   the re-park can never race it.
//! * The successor **drains the grave in a loop** — a handle it awaited
//!   resolves only after that task's drop ran, and any re-park that drop
//!   performed is therefore visible to the re-check. Without the loop,
//!   generation N+2 could take the dead N+1 handle before N+1's drop re-parks
//!   the still-live N, and overlap it.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use futures::FutureExt as _;
use rustc_hash::FxHashMap;

/// One memoized computation. `Running` while the spawned task lives; `Done`
/// once `done` is set (at which point `task` is empty — a completed cell pins
/// no dead task harness).
pub(crate) struct TaskCell<V> {
    /// The terminal state: `Ok` is the published value; `Err` is poison — the
    /// task was dropped without publishing (a panicking body outside
    /// `once()`'s guard, or its runtime shut down), and waiters surface it
    /// loudly instead of parking forever. One `OnceLock`, not two: a cell has
    /// exactly one terminal state, and the merged slot is a footprint win at
    /// hundreds of thousands of cells.
    outcome: OnceLock<Result<V, &'static str>>,
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
    /// Set while [`TaskInner::process`] is driving this cell's body *inline*,
    /// before any task exists for it (see the inline-first path there).
    ///
    /// Only [`Self::task_live`] reads it, and only so that window does not read
    /// as "stranded": during it the cell is genuinely being driven, just by the
    /// caller's own stack rather than by a spawned task.
    inline: AtomicBool,
}

impl<V> TaskCell<V> {
    pub(crate) fn peek(&self) -> Option<&V> {
        self.outcome.get().and_then(|o| o.as_ref().ok())
    }

    /// A published *value*. Poison is deliberately not "done": a poisoned
    /// cell may be evicted by its unwinding joiner so the next caller
    /// recomputes.
    pub(crate) fn is_done(&self) -> bool {
        matches!(self.outcome.get(), Some(Ok(_)))
    }

    fn poison_msg(&self) -> Option<&'static str> {
        match self.outcome.get() {
            Some(Err(msg)) => Some(msg),
            _ => None,
        }
    }

    pub(crate) fn acquire_interest(&self) {
        // Always taken under the cache lock (both `process` arms), so Relaxed
        // suffices — the lock orders it against every decision that reads it.
        self.interest.fetch_add(1, Ordering::Relaxed);
    }

    /// Returns the remaining count. "Exactly one guard observes each zero
    /// crossing" comes from RMW atomicity (any ordering would do); the AcqRel
    /// is kept only so the guard's unlocked pre-check is a sensible hint. The
    /// *decision* is always re-taken under the cache lock — nothing correct
    /// rests on this atomic's ordering.
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
        // The inline window has no task by construction, but the body *is*
        // being polled — on the caller's stack. Reporting it as stranded would
        // put a false "no driver" line in every SIGQUIT dump taken during one.
        if self.inline.load(Ordering::Acquire) {
            return true;
        }
        match self.task.try_lock() {
            Ok(slot) => slot.as_ref().is_some_and(|h| !h.is_finished()),
            // Same stance as `task_slot` / `TaskSource::collect`: a poisoned
            // slot is structurally intact — read through it.
            Err(std::sync::TryLockError::Poisoned(e)) => {
                e.into_inner().as_ref().is_some_and(|h| !h.is_finished())
            }
            // Slot briefly held (spawn/publish/cancel in progress) — the task
            // is being worked on, which is the opposite of stranded.
            Err(std::sync::TryLockError::WouldBlock) => true,
        }
    }

    pub(crate) fn age(&self) -> std::time::Duration {
        self.created.elapsed()
    }

    pub(crate) fn restarts(&self) -> u32 {
        self.restarts
    }
}

/// If less than this much stack remains when a body is polled inline, grow.
/// Matches `engine::grow_stack`'s red zone — sized for the ~100 KiB frames a
/// memoized descent puts on the stack.
const INLINE_RED_ZONE: usize = 512 * 1024;

/// Size of each freshly allocated stack segment for the inline poll.
const INLINE_STACK_PER_GROW: usize = 8 * 1024 * 1024;

/// Whether `process` polls a cold body inline before spawning a task for it.
///
/// On by default. `HEPH_MEMOIZER_INLINE=0` restores the always-spawn path, so
/// the two can be compared in one binary on one corpus — which is how the
/// numbers in `process`'s inline-first comment were taken.
fn inline_first() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("HEPH_MEMOIZER_INLINE").as_deref() != Ok("0"))
}

/// Spawn `fut` as this cell's driving task and store the handle.
///
/// The caller must hold the cache lock: `publish` takes it too, so releasing it
/// between the spawn and the store would let a fast body publish first and
/// leave a `Done` cell holding a live handle. A spawn that panics (shut-down
/// runtime) degrades to a poisoned cell rather than unwinding out of this frame
/// into a cdylib seam.
fn spawn_body<V, F>(handle: &tokio::runtime::Handle, cell: &Arc<TaskCell<V>>, fut: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let spawned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        super::spawn_on_with_cycle_ctx(handle, fut)
    }));
    match spawned {
        Ok(task) => {
            *cell.task_slot() = Some(task);
        }
        Err(_) => {
            let _already_poisoned = cell
                .outcome
                .set(Err("memoized task could not spawn (runtime shut down)"));
        }
    }
}

/// An aborted predecessor for a key: its handle (awaited by the successor
/// before the successor's body runs) and its accumulated restart count.
struct Grave {
    handle: tokio::task::JoinHandle<()>,
    restarts: u32,
}

/// Independent cell maps the key space is split across. See [`Maps::cells`].
///
/// Only has to exceed the runtime's worker count by enough that two workers
/// rarely collide, so it is sized for a machine larger than any heph runs on
/// rather than tuned. 16 and 64 were measured against each other on a 12-core
/// host and came out identical — 3.44s/3.02 GB against 3.39s/3.04 GB — so the
/// larger value costs nothing here and leaves headroom on a wider one.
const SHARDS: usize = 64;
/// `SHARDS.trailing_zeros()` — the top bits of the hash used to pick a shard.
const SHARD_BITS: u32 = 6;

/// The memoizer's shared state, behind ONE `Arc`: every spawned body clones a
/// single pointer instead of two, and the inventory holds a single `Weak`.
pub(crate) struct Maps<K, V> {
    /// Live cells, sharded by key hash. Also the arbiter lock between publish
    /// and the cancel decision (module docs).
    ///
    /// Sharded because the arbiter was the last process-wide serialization
    /// point on the resolution path, and it is taken on *every* memoized call —
    /// hits included. Measured on a warm 85k-target `validate` with one map: the
    /// runtime's workers spent 19% of their non-idle time parked in
    /// `__psynch_mutexwait` on this one lock, with `publish`,
    /// `collect_transitive_deps`, `get_spec_no_track` and `get_def_no_track` the
    /// four largest waiters — while only ~5.8 of 12 cores were ever busy.
    ///
    /// Every operation on this map is confined to one key (lookup, insert,
    /// publish, evict), so per-key locking is not a weakening of the arbiter: it
    /// *is* the arbiter, since publish and the cancel decision for a given key
    /// land on the same shard. Nothing takes two shards at once, so the nesting
    /// discipline (cache → task-slot, cache → graves) is unchanged and no
    /// shard-order deadlock is reachable.
    cells: [Mutex<FxHashMap<K, Arc<TaskCell<V>>>>; SHARDS],
    /// Aborted-but-possibly-still-dying predecessors, per key. Entries are
    /// consumed by the next caller for the key; unconsumed entries die with
    /// the memoizer (bounded by cancelled keys per request).
    ///
    /// Deliberately *not* sharded: it is touched only on the cancellation path,
    /// which is rare, and it is empty in the steady state.
    graves: Mutex<FxHashMap<K, Grave>>,
}

impl<K, V> Maps<K, V> {
    /// The shard owning `key`.
    ///
    /// Generic over a borrowed form of `K` for the same reason `HashMap::get`
    /// is: the hit path looks a key up without owning one. `Borrow`'s contract
    /// (equal values hash equally) is what makes that sound here — a `&str` and
    /// the `String` it borrows from must land on the same shard, or a lookup
    /// would take one lock and miss a cell living under another.
    ///
    /// Takes the hash's **top** `SHARD_BITS` rather than its low bits: the inner
    /// `FxHashMap` buckets on the low bits, so splitting on those would confine
    /// every key of a shard to a narrow slice of that shard's bucket array.
    #[expect(
        clippy::indexing_slicing,
        reason = "a u64 shifted right by 64 - SHARD_BITS cannot exceed \
                  2^SHARD_BITS - 1 = SHARDS - 1, so the index is in range by \
                  construction"
    )]
    fn shard<Q>(&self, key: &Q) -> &Mutex<FxHashMap<K, Arc<TaskCell<V>>>>
    where
        Q: std::hash::Hash + ?Sized,
    {
        use std::hash::BuildHasher as _;
        let h = rustc_hash::FxBuildHasher.hash_one(key);
        &self.cells[(h >> (u64::BITS - SHARD_BITS)) as usize]
    }
}

/// Task-backed implementation behind `Memoizer`. Same map + interest protocol
/// as the poll implementation; the computation lifecycle is the module docs'
/// state machine.
pub(crate) struct TaskInner<K, V> {
    maps: Arc<Maps<K, V>>,
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
        let maps = Arc::new(Maps {
            cells: std::array::from_fn(|_| Mutex::new(FxHashMap::default())),
            graves: Mutex::new(FxHashMap::default()),
        });
        super::register_source(Box::new(TaskSource {
            tag,
            maps: Arc::downgrade(&maps),
        }));
        Self { maps, tag, handle }
    }

    pub(crate) fn tag(&self) -> &'static str {
        self.tag
    }

    /// Remove `key` iff its cell is completed and its value satisfies `pred`.
    /// An in-flight cell is never touched — same contract as the poll path's
    /// cycle-error eviction.
    pub(crate) fn evict_if(&self, key: &K, pred: impl FnOnce(&V) -> bool) {
        let mut cache = self.cache_lock(key);
        if cache
            .get(key)
            .is_some_and(|cell| cell.peek().is_some_and(pred))
        {
            cache.remove(key);
        }
    }

    /// Lock the arbiter shard owning `key` (see [`Maps::cells`]).
    ///
    /// Borrowed-key generic like `HashMap::get`, so the hit path need not build
    /// an owned key just to find its shard.
    fn cache_lock<Q>(&self, key: &Q) -> MutexGuard<'_, FxHashMap<K, Arc<TaskCell<V>>>>
    where
        K: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        self.maps
            .shard(key)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn peek(&self, key: &K) -> Option<V> {
        self.cache_lock(key)
            .get(key)
            .and_then(|c| c.peek().cloned())
    }

    pub(crate) async fn process<F, Fut>(&self, key: K, f: F) -> V
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = V> + Send + 'static,
    {
        let cell = 'cell: {
            let mut cache = self.cache_lock(&key);
            // The hit path *borrows* the key. `entry` needs an owned one, so
            // asking for it up front cloned on every call — including the
            // hits, which are the common case — to look up a key the map
            // already holds an equal copy of. For the allocating key types
            // (`String`, `PkgBuf`, the `(Addr, String)` tuples) that clone is
            // a malloc + copy per memoized call, discarded microseconds later.
            //
            // The cold path pays a second hash for the `insert` below, which
            // is noise next to the task spawn it sits in front of.
            if let Some(existing) = cache.get(&key) {
                if let Some(v) = existing.peek() {
                    return v.clone();
                }
                // Under the lock, so a cancellation racing us either sees
                // this interest and stands down, or already evicted the
                // entry and we never find it — same rule as the poll cell.
                existing.acquire_interest();
                break 'cell Arc::clone(existing);
            }
            // Lazy async blocks: `f()` only builds the state machine,
            // so constructing it under the lock is free. Built BEFORE
            // the grave is taken out of the map: a caller closure that
            // panics here would otherwise unwind with the removed
            // grave in hand, losing the predecessor's handle — the
            // next caller would then spawn against a still-dying
            // predecessor with nothing to serialize on.
            let fut = f();
            let grave = self
                .maps
                .graves
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&key);
            let restarts = grave.as_ref().map_or(0, |g| g.restarts + 1);
            let cell = Arc::new(TaskCell {
                outcome: OnceLock::new(),
                notify: tokio::sync::Notify::new(),
                interest: AtomicUsize::new(0),
                task: Mutex::new(None),
                created: Instant::now(),
                restarts,
                inline: AtomicBool::new(inline_first()),
            });
            cell.acquire_interest();
            let body = BodyTask {
                cell: Arc::clone(&cell),
                maps: Arc::clone(&self.maps),
                key: key.clone(),
                grave,
            };
            cache.insert(key.clone(), Arc::clone(&cell));

            if !inline_first() {
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
                spawn_body(&self.handle, &cell, body.run(fut));
                break 'cell cell;
            }

            // Inline-first: poll the body once on this stack, and only spawn
            // if it actually suspends.
            //
            // A memoized computation that resolves without ever yielding — an
            // in-memory lookup, a hit that only reads already-loaded state — is
            // the common case here, and paying a full tokio task for it is pure
            // overhead: `OwnedTasks` push + a global `added` atomic on spawn,
            // then a shard-mutex unlink + a global `count` atomic on
            // completion, plus the `Notify` wake machinery. Measured on a
            // 192k-target resolution, `OwnedTasks::remove` alone went from
            // 3.82s of CPU on one core to 25.88s on ten — for identical work.
            // That is contention on tokio's registry, and the only way heph can
            // shrink it is to stop handing tokio so many tasks.
            //
            // The cache lock MUST be released first: a body that completes
            // inline calls `publish`, which takes that same lock. Releasing it
            // is safe because this caller already holds an interest, so no
            // concurrent `cancel_abandoned` can evict the cell out from under
            // the poll — the abandonment decision only fires when interest
            // reaches zero.
            drop(cache);
            let mut body_fut = Box::pin(body.run(fut));
            // Under `stacker::maybe_grow`, because this is the one thing the
            // task-per-cell design was silently buying besides isolation: a
            // spawned body starts on its *own* stack, so a memoized descent
            // (`get_spec` -> `get_def` -> `get_spec` -> ...) cost O(1) stack per
            // level no matter how deep the graph. Polled inline it recurses on
            // the caller's stack instead, and a deep enough chain overflows a
            // 2 MiB worker stack — which is exactly what
            // `deep_warm_chain_completes_on_a_2mib_stack` caught, on all three
            // targets at once.
            //
            // Same wrapper, same constants, and the same reasoning as
            // `engine::grow_stack` already applies to the transparent-group
            // re-inline: a couple of instructions on the hot path, a fresh
            // segment allocated only when headroom actually runs low.
            let polled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let waker = futures::task::noop_waker();
                let mut cx = std::task::Context::from_waker(&waker);
                stacker::maybe_grow(INLINE_RED_ZONE, INLINE_STACK_PER_GROW, || {
                    body_fut.as_mut().poll(&mut cx)
                })
            }));
            match polled {
                // Completed on this stack. `run` already published under the
                // cache lock, so there is nothing to spawn and no handle to
                // store — exactly the `Done`-cell-holds-no-handle end state.
                Ok(std::task::Poll::Ready(())) => {}
                Ok(std::task::Poll::Pending) => {
                    // Suspended, so it needs a task after all. Re-take the
                    // cache lock for the same reason the non-inline path never
                    // let go of it: the handle must be stored before the body
                    // can publish.
                    //
                    // Polling with a no-op waker first is sound: a future must
                    // honour the waker from its most recent poll, and tokio
                    // polls immediately on spawn — so whatever it registered
                    // against the no-op waker is re-registered against the real
                    // one before anything could wake it.
                    let _arbiter = self.cache_lock(&key);
                    spawn_body(&self.handle, &cell, body_fut);
                }
                // The body panicked on this stack rather than inside a task.
                // Dropping `body_fut` ran `BodyTask::drop`, which poisons the
                // cell, so the wait below surfaces it loudly — the same
                // outcome as a panicking spawned body, and it must not unwind
                // further and cross a cdylib seam.
                Err(_) => {}
            }
            cell.inline.store(false, Ordering::Release);
            cell
        };

        // Cancel the computation if we turn out to be its last awaiter —
        // declared before the await so it drops after the wait is gone.
        let mut abandon = TaskAbandonGuard {
            inner: self,
            key: &key,
            cell: Arc::clone(&cell),
            armed: true,
        };
        let out =
            super::await_with_stall_check(wait_done(&cell, self.tag, &key), &key, self.tag).await;
        abandon.armed = false;
        out
    }

    /// Evict-and-abort, unless somebody wants the cell after all. The decision
    /// re-checks under the cache lock (the arbiter — see module docs), and the
    /// eviction, handle-take, and grave-park all happen inside that one
    /// critical section: a successor that finds the map vacant is thereby
    /// guaranteed to also find the grave — evict-then-unlock-then-park would
    /// open a window where it spawns grave-less against a still-live
    /// predecessor. Only the `abort()` itself runs outside the lock (it is a
    /// request, not teardown — the "never hold the cache lock across
    /// teardown" discipline is about the destructors, which run on a runtime
    /// worker here, never on this stack).
    fn cancel_abandoned(&self, key: &K, cell: &Arc<TaskCell<V>>) {
        let abort = {
            let mut cache = self.cache_lock(key);
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

            // Idempotent across two zero-crossings on the same cell: the loser
            // finds the slot empty. Lock nesting cache → task-slot and cache →
            // graves is the established order (publish and the vacant arm use
            // the same nesting); nothing takes them the other way around.
            let Some(task) = cell.task_slot().take() else {
                return;
            };
            // Grave park ordered before the abort is issued: once aborted, the
            // task can be dropped at any instant and `ReGrave` (its drop path)
            // may re-park a predecessor under this key — that insert must find
            // ours already present, never overwrite-race it.
            let abort = task.abort_handle();
            self.maps
                .graves
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(
                    key.clone(),
                    Grave {
                        handle: task,
                        restarts: cell.restarts,
                    },
                );
            abort
        };
        tracing::debug!(
            tag = self.tag,
            key = ?key,
            restarts = cell.restarts,
            "memoized computation abandoned; aborting its task"
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
    maps: Arc<Maps<K, V>>,
    key: K,
    grave: Option<Grave>,
}

impl<K, V> BodyTask<K, V>
where
    K: std::hash::Hash + Eq + Send + Sync + 'static + Clone,
    V: Clone + Send + Sync + 'static,
{
    async fn run<Fut: Future<Output = V>>(mut self, fut: Fut) {
        // Serialize on every predecessor before touching the body — not just
        // the spawn-time grave. After each observed death the map is
        // re-checked: a successor aborted mid-grave-await re-parks *its*
        // predecessor (`ReGrave` in `Drop`), and that insert runs during the
        // very drop the JoinHandle await just observed, so the re-check is
        // guaranteed to see any deeper still-dying handle. Without the loop,
        // generation N+2 can take the (already dead, body-less) N+1 handle
        // from the grave before N+1's drop re-parks the still-live N — and
        // overlap it.
        //
        // Whatever is currently being awaited sits in `self.grave`, so this
        // task's own abort re-parks it for the next generation.
        loop {
            match self.grave.as_mut() {
                Some(g) => {
                    // `&mut JoinHandle` is a future (JoinHandle is Unpin), so
                    // an abort landing mid-await leaves the handle in place
                    // for `Drop` to re-park. A JoinError here is the expected
                    // `is_cancelled` — the predecessor was aborted, that's
                    // why it's in a grave.
                    let _cancelled = (&mut g.handle).await;
                    self.grave = None;
                }
                None => {
                    let next = self
                        .maps
                        .graves
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&self.key);
                    match next {
                        Some(g) => self.grave = Some(g),
                        None => break,
                    }
                }
            }
        }
        let v = fut.await;
        self.publish(v);
    }

    fn publish(self, v: V) {
        // Under the cache lock — the arbiter between publish and the cancel
        // decision (module docs). Also drops the task's own handle: a `Done`
        // cell holds no dead task harness. (Dropping one's own JoinHandle is a
        // detach, which is exactly right — the task is finishing.)
        {
            let _arbiter = self
                .maps
                .shard(&self.key)
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let _first_publish = self.cell.outcome.set(Ok(v));
            *self.cell.task_slot() = None;
        }
        self.cell.notify.notify_waiters();
        // `self` drops disarmed: the outcome is set, so `Drop` won't poison.
        // The consumed grave (if any) is gone — nothing to re-park.
    }
}

impl<K, V> Drop for BodyTask<K, V>
where
    K: std::hash::Hash + Eq + Clone,
{
    fn drop(&mut self) {
        // Aborted while still awaiting a predecessor's grave: this task never
        // ran its body, so its own handle is the wrong thing for the next
        // generation to serialize on — re-park the predecessor's. (The
        // canceller's own grave insert always precedes the abort, so this
        // overwrite replaces *this* task's handle with the still-dying
        // predecessor's — the one that might still have body state. A
        // successor that took this task's handle *before* this overwrite
        // re-checks the map after that handle resolves — `run`'s drain loop —
        // and this insert is ordered before that resolve, so it is seen.)
        if let Some(mut g) = self.grave.take() {
            // The chain's restart count, not the re-parked handle's own: this
            // generation died too, and the next one's counter must say so.
            g.restarts = self.cell.restarts;
            self.maps
                .graves
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(self.key.clone(), g);
        }
        if self.cell.outcome.get().is_some() {
            return;
        }
        // Dropped without publishing. For an aborted cell this is ordinary
        // (evicted, zero interest — the notify wakes nobody). For a live cell
        // it means the body panicked (only reachable outside `once()`'s
        // guard) or the runtime shut down mid-flight: poison so waiters fail
        // loudly instead of parking forever.
        let _already_poisoned = self.cell.outcome.set(Err(
            "memoized task dropped without publishing (body panicked or its runtime shut down)",
        ));
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
async fn wait_done<K: std::fmt::Debug, V: Clone>(
    cell: &TaskCell<V>,
    tag: &'static str,
    key: &K,
) -> V {
    loop {
        let notified = cell.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(v) = cell.peek() {
            return v.clone();
        }
        if let Some(msg) = cell.poison_msg() {
            // Loud on purpose: a poisoned cell has no value and never will.
            // `once()` catches this and memoizes it as an `Err`; a raw
            // `process()` caller surfaces it as a panic in the joiner. A typed
            // payload, so `catch_poison` converts exactly this panic and
            // resumes every other one (the debug stall panic must stay loud).
            // The key rides along — "which target's cell died" is the first
            // question the failure raises.
            std::panic::panic_any(PoisonPanic {
                tag,
                msg: format!("{msg} (key={key:?})"),
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
    maps: std::sync::Weak<Maps<K, V>>,
}

impl<K, V> super::CellSource for TaskSource<K, V>
where
    K: std::fmt::Debug + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    fn collect(&self, out: &mut Vec<super::StuckCell>) -> bool {
        let Some(maps) = self.maps.upgrade() else {
            return false;
        };
        // Shard by shard, and one at a time: the dump must never hold two of
        // the arbiter's locks at once, or it would be the only thing in the
        // process that can order them against each other.
        for shard in &maps.cells {
            // `try_lock`: never block a diagnostic dump on the process being
            // dumped. A contended shard is skipped rather than abandoning the
            // whole dump — the other 63 still have cells worth naming, and the
            // one being written to is the *least* likely to be stuck.
            let map = match shard.try_lock() {
                Ok(m) => m,
                Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => continue,
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

    /// A borrowed key must reach the same shard as the owned key it borrows
    /// from, and every key must reach a shard that exists.
    ///
    /// The sharp edge sharding introduces: [`TaskInner::peek`] and the hit path
    /// in `process` look a cell up through `Borrow` (`&str` against a `String`
    /// map). If the two spellings hashed to different shards, the lookup would
    /// take one lock, find that shard empty, and report a miss for a cell that
    /// is very much present — a silently-lost memoization, not a crash, and one
    /// no functional test would attribute to sharding.
    ///
    /// Asserted directly on the shard pointers rather than through a `process`
    /// round trip, because the failure this guards is a *coincidence* the round
    /// trip would hide: with 64 shards a mismatched pair still agrees 1 time in
    /// 64, so a test that memoized one key and read it back would pass on most
    /// keys even with the property broken.
    #[test]
    fn a_borrowed_key_and_its_owned_form_share_a_shard() {
        let maps: Maps<String, ()> = Maps {
            cells: std::array::from_fn(|_| Mutex::new(FxHashMap::default())),
            graves: Mutex::new(FxHashMap::default()),
        };
        let mut seen = std::collections::HashSet::new();
        for i in 0..512 {
            let owned = format!("//pkg/{i}:target@v=host");
            let borrowed: &str = owned.as_str();
            assert!(
                std::ptr::eq(maps.shard(&owned), maps.shard(borrowed)),
                "`{owned}` hashes to a different shard as a &str than as a String; \
                 every borrowed lookup for it would miss"
            );
            seen.insert(std::ptr::from_ref(maps.shard(&owned)));
        }
        // Not a distribution assertion — just that the index is not stuck. A
        // shift that took the wrong end of the hash, or a mask that collapsed,
        // would land every key on one shard and quietly restore the single lock.
        assert!(
            seen.len() > SHARDS / 2,
            "512 distinct keys reached only {} of {SHARDS} shards; the shard index \
             is not spreading and the arbiter is effectively unsharded",
            seen.len()
        );
    }

    /// RAII occupancy guard for the no-overlap tests: `enter` fails the body
    /// (and thereby the test, via the poison path) if another body for the
    /// same key is still alive — including still running its destructors.
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

    /// A body that holds the occupancy guard and busy-spins on `release`
    /// WITHOUT an await point: the abort issued against it cannot take effect
    /// until the flag flips (an aborted task dies only at a yield boundary),
    /// which makes the "predecessor still alive after its abort" window a
    /// deterministic state instead of a nanosecond race.
    async fn spinning_body(
        occupancy: Arc<AtomicUsize>,
        entered: Arc<std::sync::atomic::AtomicBool>,
        release: Arc<std::sync::atomic::AtomicBool>,
    ) -> u32 {
        // Suspend before doing anything else, so `process` spawns a task for
        // this body instead of running it inline (see `inline_first`). Every
        // test using this body is about aborting a *live predecessor*, and
        // there is no predecessor to abort if the body already ran to
        // completion on the caller's stack. This is the harness constructing
        // the scenario, not a workaround: a body that never suspends is one
        // nothing can cancel, by construction.
        tokio::task::yield_now().await;
        let _occ = Occupancy::enter(&occupancy);
        entered.store(true, Ordering::SeqCst);
        // Time-bounded: if the test panics before flipping `release`, the
        // spin must still end — a poll that never returns wedges runtime
        // shutdown and turns a red test into a hung binary.
        let start = Instant::now();
        while !release.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        // First yield point after release: a pending abort lands here and
        // the guard drops with the future.
        futures::future::pending::<()>().await;
        0u32
    }

    /// The core no-overlap guarantee, deterministically: the predecessor is
    /// kept alive *through* its abort by a spin with no await point, the
    /// successor is spawned into exactly that window, and only releasing the
    /// spin may let the successor's body run. Mutation-verified: deleting the
    /// grave await in `BodyTask::run` turns this red every run (the successor
    /// enters while the predecessor's guard is live → poison → joiner fails).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn no_two_bodies_for_one_key_ever_overlap() {
        let m: Arc<TaskInner<String, u32>> = Arc::new(inner("overlap-test"));
        let occupancy = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let first = {
            let m = Arc::clone(&m);
            let body = spinning_body(
                Arc::clone(&occupancy),
                Arc::clone(&entered),
                Arc::clone(&release),
            );
            let mut fut = Box::pin(async move { m.process("k".to_string(), move || body).await });
            assert!(futures::poll!(&mut fut).is_pending());
            fut
        };
        // Wait until the predecessor is provably inside the section.
        within(async {
            while !entered.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await;

        // Last-interest abort. The predecessor CANNOT die yet — it is spinning
        // with no await point — so the no-overlap window is held open.
        drop(first);

        // Successor into the held-open window. Its body asserts sole
        // occupancy; without the grave await it runs immediately and trips.
        let successor = {
            let (m, occupancy) = (Arc::clone(&m), Arc::clone(&occupancy));
            tokio::spawn(async move {
                m.process("k".to_string(), move || async move {
                    let _occ = Occupancy::enter(&occupancy);
                    7u32
                })
                .await
            })
        };
        // Give a broken implementation ample room to run the successor's body
        // while the predecessor still holds the guard.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            occupancy.load(Ordering::SeqCst),
            1,
            "the successor's body must not have started while the predecessor lives"
        );

        // Let the predecessor reach an await point and die; the successor may
        // only now proceed.
        release.store(true, Ordering::SeqCst);
        let v = within(successor).await.expect("successor joiner");
        assert_eq!(v, 7);
    }

    /// Transitive no-overlap across three generations — the `ReGrave` path.
    /// Gen 1 is held alive by the spin; gen 2 is aborted while still awaiting
    /// gen 1's grave (it never runs its body); gen 3 must serialize on gen 1,
    /// not on gen 2's already-dead handle, and must report the chain's
    /// restart count. Deterministic by the same no-await-point construction.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_regraved_predecessor_still_serializes_generation_three() {
        let m: Arc<TaskInner<String, u32>> = Arc::new(inner("regrave-test"));
        let occupancy = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Gen 1: spinning in the section.
        let gen1 = {
            let m = Arc::clone(&m);
            let body = spinning_body(
                Arc::clone(&occupancy),
                Arc::clone(&entered),
                Arc::clone(&release),
            );
            let mut fut = Box::pin(async move { m.process("k".to_string(), move || body).await });
            assert!(futures::poll!(&mut fut).is_pending());
            fut
        };
        within(async {
            while !entered.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await;
        drop(gen1); // aborted; cannot die while spinning

        // Gen 2: its task parks awaiting gen 1's grave (gen 1 is alive).
        let gen2 = {
            let m = Arc::clone(&m);
            let mut fut =
                Box::pin(async move { m.process("k".to_string(), || async { 1u32 }).await });
            assert!(futures::poll!(&mut fut).is_pending());
            fut
        };
        // Let gen 2's spawned task actually reach the grave await.
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Abort gen 2 mid-grave-await: its Drop must re-park gen 1's handle.
        drop(gen2);

        // Gen 3: must wait for gen 1 (still spinning), and must carry
        // restarts == 2 (two dead generations before it).
        let gen3 = {
            let (m, occupancy) = (Arc::clone(&m), Arc::clone(&occupancy));
            tokio::spawn(async move {
                m.process("k".to_string(), move || async move {
                    let _occ = Occupancy::enter(&occupancy);
                    3u32
                })
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            occupancy.load(Ordering::SeqCst),
            1,
            "gen 3 must not run while gen 1 still lives — it serialized on the wrong grave"
        );
        let cell = m
            .cache_lock("k")
            .get("k")
            .cloned()
            .expect("gen 3 cell in map");
        assert_eq!(cell.restarts(), 2, "two generations died before gen 3");

        release.store(true, Ordering::SeqCst);
        let v = within(gen3).await.expect("gen 3 joiner");
        assert_eq!(v, 3);
    }

    /// A body that never suspends is finished on the caller's stack, without a
    /// task ever existing for it.
    ///
    /// This is the whole point of `inline_first`: a memoized computation that
    /// resolves without yielding used to cost a full tokio task — `OwnedTasks`
    /// push plus a global `added` atomic on spawn, a shard-mutex unlink plus a
    /// global `count` atomic on completion, and the `Notify` wake machinery in
    /// between. On a 192k-target resolution that overhead was the single
    /// largest cost in the profile, and it is contention, not work:
    /// `OwnedTasks::remove` measured 3.82s of CPU on one core against 25.88s on
    /// ten, for byte-identical work.
    ///
    /// Asserting on the *first poll* is what makes this observable: driven
    /// inline the value is already there when `process` first returns, where a
    /// spawned body could only publish from another thread later.
    ///
    /// Asserts the default. Under `HEPH_MEMOIZER_INLINE=0` — the escape hatch
    /// that restores always-spawn — this test fails, deliberately: a silent
    /// skip would let the optimization rot away unnoticed, which is the whole
    /// failure mode a regression test exists to prevent.
    #[tokio::test]
    async fn a_body_that_never_suspends_is_driven_inline() {
        let m: TaskInner<String, u32> = inner("inline-first-test");
        let mut fut = Box::pin(m.process("k".to_string(), || async { 7 }));
        let first = futures::poll!(&mut fut);
        assert_eq!(
            first,
            std::task::Poll::Ready(7),
            "a non-suspending body must complete on the first poll, not be handed to a task"
        );
        // And it left the cell in the normal terminal state.
        let cell = m.cache_lock("k").get("k").cloned().expect("cell retained");
        assert_eq!(cell.peek(), Some(&7));
        assert!(
            cell.task_slot().is_none(),
            "a cell finished inline must hold no task handle"
        );
    }

    /// A body that *does* suspend still gets a task — inline-first is an
    /// optimization for the fast path, not a removal of the task-backed model.
    /// Without this, `inline_first` could regress to "never spawn" and every
    /// suspending computation would stall with nothing driving it.
    #[tokio::test]
    async fn a_body_that_suspends_still_gets_a_task() {
        let m: Arc<TaskInner<String, u32>> = Arc::new(inner("inline-spawn-test"));
        let gate = Arc::new(tokio::sync::Notify::new());
        let mut fut = Box::pin(m.process("k".to_string(), {
            let gate = Arc::clone(&gate);
            move || async move {
                gate.notified().await;
                9
            }
        }));
        assert!(
            futures::poll!(&mut fut).is_pending(),
            "a suspending body cannot have completed inline"
        );
        {
            let cell = m.cache_lock("k").get("k").cloned().expect("cell present");
            assert!(
                cell.task_live(),
                "a suspended body must be driven by a spawned task"
            );
        }
        gate.notify_one();
        assert_eq!(within(&mut fut).await, 9);
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
            .cache_lock("k")
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
            .cache_lock("k")
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
        let cell = m.cache_lock("k").get("k").cloned().expect("completed cell");
        assert!(
            cell.task_slot().is_none(),
            "a Done cell must not retain its JoinHandle"
        );
    }

    /// Smoke test: two concurrent joiners per key both complete, 2000 keys.
    ///
    /// Honesty note: this does NOT prove the register-then-recheck ordering in
    /// `wait_done` — the check-then-register bug's window is nanoseconds
    /// inside one poll, and a black-box test cannot force a publish into it
    /// (mutation-tested: the inverted ordering still passes here). That
    /// ordering is proven by review against the documented discipline (see
    /// `wait_done`'s comment and the `WorkerPool::acquire` pattern it copies);
    /// what this test freezes is the end-to-end join/publish path staying
    /// live under churn.
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

        // The body suspends, so `process` must spawn to finish it — which is
        // what fails here. A body that completed inline would need no runtime
        // at all and would legitimately succeed, testing nothing about a dead
        // one (see `inline_first`).
        let joined = within(
            std::panic::AssertUnwindSafe(m.process("k".to_string(), || async {
                tokio::task::yield_now().await;
                1
            }))
            .catch_unwind(),
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
            let cell = m.cache_lock("k").get("k").cloned().expect("in-flight cell");
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

#[cfg(test)]
mod repro {
    use super::*;
    use std::time::Duration;

    /// SCRATCH REPRO (not for commit): the window between cancel_abandoned's
    /// eviction (cache lock released) and its graves.insert. A successor whose
    /// vacant-path graves.remove wins the graves lock in that window spawns
    /// grave-less while the predecessor has not even been aborted yet.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn repro_evict_to_grave_window_overlaps_bodies() {
        let m: Arc<TaskInner<String, u32>> = Arc::new(TaskInner::new(
            "repro-window",
            tokio::runtime::Handle::current(),
        ));
        let occupancy = Arc::new(AtomicUsize::new(0));

        for round in 0..40u32 {
            let key = format!("k{round}");
            // Gen1: enters occupancy, parks forever.
            let entered = Arc::new(tokio::sync::Notify::new());
            let first = {
                let occ = Arc::clone(&occupancy);
                let entered = Arc::clone(&entered);
                let m = Arc::clone(&m);
                let key = key.clone();
                let mut fut = Box::pin(async move {
                    m.process(key, move || async move {
                        occ.fetch_add(1, Ordering::SeqCst);
                        entered.notify_one();
                        // Park forever; occupancy released only via abort+drop.
                        struct Un(Arc<AtomicUsize>);
                        impl Drop for Un {
                            fn drop(&mut self) {
                                self.0.fetch_sub(1, Ordering::SeqCst);
                            }
                        }
                        let _un = Un(occ);
                        futures::future::pending::<()>().await;
                        0u32
                    })
                    .await
                });
                assert!(futures::poll!(&mut fut).is_pending());
                fut
            };
            entered.notified().await; // body1 is inside its occupancy section

            // Hold the graves lock so the canceller parks between evict and insert.
            let graves_guard = m.maps.graves.lock().unwrap();

            // Canceller on its own OS thread: evicts under the cache lock, then
            // blocks on graves.lock() *before* issuing the abort.
            let canceller = std::thread::spawn(move || drop(first));
            std::thread::sleep(Duration::from_millis(30)); // let it reach the block

            // Release, then immediately run the successor inline: its vacant
            // path barges the graves mutex ahead of the parked canceller's
            // OS wakeup (std Mutex allows barging on both Linux and macOS).
            let overlap_seen = Arc::new(AtomicUsize::new(0));
            drop(graves_guard);
            let v = {
                let occ = Arc::clone(&occupancy);
                let seen = Arc::clone(&overlap_seen);
                tokio::time::timeout(
                    Duration::from_secs(5),
                    m.process(key.clone(), move || async move {
                        seen.store(occ.load(Ordering::SeqCst), Ordering::SeqCst);
                        7u32
                    }),
                )
                .await
                .expect("successor completes")
            };
            assert_eq!(v, 7);
            canceller.join().expect("canceller thread");
            if overlap_seen.load(Ordering::SeqCst) != 0 {
                panic!(
                    "round {round}: successor body ran while predecessor body \
                     was still live (occupancy {})",
                    overlap_seen.load(Ordering::SeqCst)
                );
            }
            // Drain the grave (if the canceller won) so rounds stay independent.
            m.maps.graves.lock().unwrap().remove(&format!("k{round}"));
        }
    }
}

#[cfg(test)]
mod hit_path_tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_secs(5);

    async fn within<T>(fut: impl Future<Output = T>) -> T {
        tokio::time::timeout(TIMEOUT, fut)
            .await
            .expect("test future must complete within the timeout")
    }

    /// A key whose `Clone` is counted. Equality and hashing go through `name`
    /// alone, so the counter is invisible to the map.
    #[derive(Debug)]
    struct CountedKey {
        name: &'static str,
        clones: Arc<AtomicU32>,
    }

    impl CountedKey {
        fn new(name: &'static str, clones: &Arc<AtomicU32>) -> Self {
            Self {
                name,
                clones: Arc::clone(clones),
            }
        }
    }

    impl Clone for CountedKey {
        fn clone(&self) -> Self {
            self.clones.fetch_add(1, Ordering::SeqCst);
            Self {
                name: self.name,
                clones: Arc::clone(&self.clones),
            }
        }
    }

    impl PartialEq for CountedKey {
        fn eq(&self, other: &Self) -> bool {
            self.name == other.name
        }
    }
    impl Eq for CountedKey {}

    impl std::hash::Hash for CountedKey {
        fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
            self.name.hash(state);
        }
    }

    /// A memoizer hit answers from a borrow and takes no key clone.
    ///
    /// `entry` needs an owned key, so the shape this replaced cloned on every
    /// call — hits included, and hits are the common case. For the allocating
    /// key types in the engine (`String`, `PkgBuf`, the `(Addr, String)`
    /// tuples) that was a malloc + copy per memoized call, thrown away as soon
    /// as the lookup found the equal key the map already held.
    ///
    /// The cold path is deliberately not asserted on: it must take owned keys,
    /// one for the map and one for the body task, and that is not a
    /// regression to guard.
    #[tokio::test]
    async fn a_memoizer_hit_never_clones_the_key() {
        let m: TaskInner<CountedKey, u32> =
            TaskInner::new("hit-clone-test", tokio::runtime::Handle::current());
        let clones = Arc::new(AtomicU32::new(0));
        let runs = Arc::new(AtomicU32::new(0));

        // Cold: publishes the cell.
        let v = within(m.process(CountedKey::new("k", &clones), {
            let runs = Arc::clone(&runs);
            move || async move {
                runs.fetch_add(1, Ordering::SeqCst);
                7
            }
        }))
        .await;
        assert_eq!(v, 7);

        clones.store(0, Ordering::SeqCst);

        // Hit: the value is already published, so this must be answered
        // without touching the key beyond the borrow the lookup needs.
        let v = within(m.process(CountedKey::new("k", &clones), {
            let runs = Arc::clone(&runs);
            move || async move {
                runs.fetch_add(1, Ordering::SeqCst);
                9
            }
        }))
        .await;
        assert_eq!(v, 7, "the memoized value is returned, not recomputed");
        assert_eq!(runs.load(Ordering::SeqCst), 1, "no recompute");
        assert_eq!(
            clones.load(Ordering::SeqCst),
            0,
            "the hit path cloned the key"
        );
    }
}
