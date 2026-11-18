//! Cache scopes: how long a provider may keep what it read of the workspace.
//!
//! A provider's caches of workspace state (BUILD contents, a package list,
//! `go.mod`, `go list` decodes) answer only the calls of the **cache scope**
//! that filled them. A scope is one engine root request; the states derived
//! from it (`with_parent`, `speculative`, the nested hash-only and gc states)
//! share it. When the last holder drops, the host calls
//! [`Provider::scope_ended`](crate::provider::Provider::scope_ended) once on
//! every provider, and the provider drops that scope's entries.
//!
//! - [`ScopeId`] names a scope: minted by the host from 1, never 0. A request
//!   carries it as `Option<ScopeId>` — `None` when no scope was sent (a call
//!   that is not the engine's, or a host that predates scopes).
//! - [`CacheScope`] is the host's handle: minting registers the id as live,
//!   dropping the last clone ends it. Request constructors take it by
//!   reference (`with_cache_scope`) and mark it touched, so a scope that no
//!   provider was ever asked about ends without a hook.
//! - [`ScopeMap`] is the provider's side: one bucket per live scope, a
//!   throwaway for an ended one, and a single unscoped bucket for `None`.
//!
//! Nothing about a scope may reach a hash, a target's env or its argv: two
//! scopes over an unchanged tree give the same answers.

use parking_lot::RwLock;
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// One engine cache scope. Minted by the host, from 1; 0 is never an id, so
/// the wire refuses it rather than reading it as a scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ScopeId(NonZeroU64);

impl ScopeId {
    /// The scope with this raw id, `None` for 0.
    pub const fn new(raw: u64) -> Option<Self> {
        match NonZeroU64::new(raw) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// The raw id, as it crosses the wire.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl fmt::Display for ScopeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What [`Provider::scope_ended`](crate::provider::Provider::scope_ended)
/// is told: the scope that ended, and the host's watermark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScopeEnded {
    /// The scope whose last holder dropped.
    pub scope: ScopeId,
    /// Every scope with an id below this has ended, whether or not its own
    /// hook was sent (an untouched scope sends none). A [`ScopeMap`] forgets
    /// the ended ids below it, which is what keeps its ended set bounded.
    pub ended_below: u64,
}

impl ScopeEnded {
    pub fn new(scope: ScopeId, ended_below: u64) -> Self {
        Self { scope, ended_below }
    }
}

/// A provider's answer to `scope_ended`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScopeEndedAck {
    /// The provider keeps no per-scope state (the trait's default). The host
    /// reports it once per provider and engine: such a provider's caches, if
    /// it has any, are never told to drop.
    Unsupported,
    /// The provider released what it held for the scope (possibly nothing).
    Released,
}

/// The process-wide scope registry: the next id, and every id minted and not
/// yet ended. Process-wide (like request ids) so two engines in one process
/// never mint the same id, and the watermark is right for both.
struct Registry {
    next: NonZeroU64,
    live: BTreeSet<u64>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    next: NonZeroU64::MIN,
    live: BTreeSet::new(),
});

fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether any scope minted in this process is still live. A plugin cdylib has
/// its own copy of the registry, which the host never mints into, so there it
/// is always false.
fn any_scope_live() -> bool {
    !registry().live.is_empty()
}

impl Registry {
    /// Every id below this has ended: the oldest live id, or the next one to
    /// be minted when none is live.
    fn ended_below(&self) -> u64 {
        self.live
            .first()
            .copied()
            .unwrap_or_else(|| self.next.get())
    }
}

/// What the host runs when a scope ends: the scope and watermark, and whether
/// any request was ever built with the scope.
type EndHook = Box<dyn FnOnce(ScopeEnded, bool) + Send + Sync>;

/// The host's handle on one cache scope. Cloning shares the scope; dropping
/// the last clone ends it. Built only by the host ([`CacheScope::mint`]);
/// a provider sees the [`ScopeId`] its requests carry.
#[derive(Clone)]
pub struct CacheScope(Arc<ScopeInner>);

struct ScopeInner {
    id: ScopeId,
    /// Set when a request is built with this scope (`with_cache_scope`), so an
    /// untouched scope ends without sending any hook. Relaxed suffices: the
    /// `Arc`'s release/acquire on the last drop orders it before `Drop`.
    touched: AtomicBool,
    on_end: Option<EndHook>,
}

impl CacheScope {
    /// Mint a new scope. The id is registered live in the same critical
    /// section that mints it, so no watermark computed while this handle is
    /// alive can pass it. `on_end` runs once, synchronously on the thread
    /// that drops the last clone, after the id has left the live set; it must
    /// not block (the engine spawns its hook from there).
    pub fn mint(on_end: impl FnOnce(ScopeEnded, bool) + Send + Sync + 'static) -> Self {
        let id = {
            let mut reg = registry();
            let id = reg.next;
            reg.next = id.saturating_add(1);
            reg.live.insert(id.get());
            ScopeId(id)
        };
        Self(Arc::new(ScopeInner {
            id,
            touched: AtomicBool::new(false),
            on_end: Some(Box::new(on_end)),
        }))
    }

    pub fn id(&self) -> ScopeId {
        self.0.id
    }

    /// Whether a request was built with this scope.
    pub fn is_touched(&self) -> bool {
        self.0.touched.load(Ordering::Relaxed)
    }

    pub(crate) fn mark_touched(&self) {
        self.0.touched.store(true, Ordering::Relaxed);
    }

    /// The process-wide watermark right now: every scope with a lower id has
    /// ended.
    pub fn ended_below() -> u64 {
        registry().ended_below()
    }
}

impl fmt::Debug for CacheScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CacheScope")
            .field("id", &self.0.id)
            .field("touched", &self.is_touched())
            .finish()
    }
}

impl Drop for ScopeInner {
    fn drop(&mut self) {
        let ended_below = {
            let mut reg = registry();
            reg.live.remove(&self.id.get());
            reg.ended_below()
        };
        if let Some(on_end) = self.on_end.take() {
            on_end(
                ScopeEnded::new(self.id, ended_below),
                self.touched.load(Ordering::Relaxed),
            );
        }
    }
}

/// A provider's per-scope state: one `T` per live scope, under one lock.
///
/// One per provider instance, never a `static`. Each provider entry point
/// resolves its bucket once with [`enter`](Self::enter), before its first
/// await, and passes `&T` down — a hot path never looks the scope up per
/// call. `T` owns the scope's caches (its memoizers among them: dropping a
/// whole memoizer is safe where removing an entry is not).
///
/// - A live scope's bucket is created on first `enter` and dropped by
///   [`end`](Self::end).
/// - A call for an ended scope gets a fresh `T` that is never inserted, so a
///   late call or a late write cannot recreate the bucket.
/// - `None` (no scope sent) shares one unscoped bucket that never evicts:
///   today's behaviour, for a call that is not the engine's or an old host.
///
/// The lock is a read-write lock: an `enter` whose bucket exists (every call
/// of a scope but its first) takes only the read side, so the provider's
/// calls do not serialize on it.
///
/// Ended ids are remembered as a watermark (every id below it ended, from the
/// host's `ended_below`) plus the out-of-order ids above it. The registry is
/// process-wide and its watermark is the oldest live scope, so the set holds
/// the scopes ended since the oldest still-live one was minted: one long-lived
/// root request pins the watermark, and every scope that ends after it stays
/// in the set until it does.
pub struct ScopeMap<T> {
    owner: Arc<str>,
    inner: RwLock<MapInner<T>>,
}

struct MapInner<T> {
    live: HashMap<ScopeId, Arc<T>>,
    unscoped: Option<Arc<T>>,
    /// Every id below this has ended.
    watermark: u64,
    /// Ended ids at or above `watermark`.
    ended: BTreeSet<u64>,
}

impl<T> MapInner<T> {
    fn is_ended(&self, scope: ScopeId) -> bool {
        scope.get() < self.watermark || self.ended.contains(&scope.get())
    }
}

impl<T: Send + Sync + 'static> ScopeMap<T> {
    /// `owner` names the provider in this map's diagnostics.
    pub fn new(owner: impl Into<Arc<str>>) -> Self {
        Self {
            owner: owner.into(),
            inner: RwLock::new(MapInner {
                live: HashMap::new(),
                unscoped: None,
                watermark: 1,
                ended: BTreeSet::new(),
            }),
        }
    }

    /// The bucket for `scope`, made with `make` when there is none. A call
    /// for an ended scope gets a fresh `make()` that is never stored.
    pub fn enter_with(&self, scope: Option<ScopeId>, make: impl FnOnce() -> T) -> Arc<T> {
        // Fast path: the bucket exists. `live` never holds an ended scope —
        // `end` removes a bucket under the write lock in the same critical
        // section that records the scope ended — so a hit here is exactly the
        // answer the write path would give.
        {
            let inner = self.inner.read();
            let hit = match scope {
                None => inner.unscoped.as_ref(),
                Some(scope) => inner.live.get(&scope),
            };
            if let Some(bucket) = hit {
                return Arc::clone(bucket);
            }
        }

        let mut inner = self.inner.write();
        let Some(scope) = scope else {
            if let Some(bucket) = &inner.unscoped {
                return Arc::clone(bucket);
            }
            let bucket = Arc::clone(inner.unscoped.insert(Arc::new(make())));
            // Has this map been sent a scope before? A host that sends scopes
            // and then a call without one is a caller that bypasses them.
            let saw_scopes =
                !inner.live.is_empty() || inner.watermark > 1 || !inner.ended.is_empty();
            drop(inner);
            if saw_scopes || any_scope_live() {
                tracing::warn!(
                    owner = %self.owner,
                    "provider called without a cache scope while scopes are in use: this call and \
                     every later unscoped one share a cache that is never released, so they can \
                     see workspace state older than the current request",
                );
            } else {
                tracing::debug!(
                    owner = %self.owner,
                    "unscoped cache first used: calls without a cache scope share one cache that \
                     is never released",
                );
            }
            return bucket;
        };
        if inner.is_ended(scope) {
            drop(inner);
            tracing::debug!(
                owner = %self.owner,
                scope = scope.get(),
                "call for an ended cache scope answered uncached",
            );
            return Arc::new(make());
        }
        if let Some(bucket) = inner.live.get(&scope) {
            return Arc::clone(bucket);
        }
        let bucket = Arc::new(make());
        inner.live.insert(scope, Arc::clone(&bucket));
        drop(inner);
        tracing::debug!(
            owner = %self.owner,
            scope = scope.get(),
            "cache scope first used",
        );
        bucket
    }

    /// [`enter_with`](Self::enter_with) for a `T` with a default.
    pub fn enter(&self, scope: Option<ScopeId>) -> Arc<T>
    where
        T: Default,
    {
        self.enter_with(scope, T::default)
    }

    /// End `ended.scope`: drop its bucket, and every bucket below the host's
    /// watermark. Returns how many buckets were released. Ending a scope
    /// twice, or one that never entered, is a no-op that still records it as
    /// ended.
    ///
    /// The buckets are dropped after the lock is released, on the blocking
    /// pool (`hcore::blocking`): a bucket can hold a whole scope's evaluations,
    /// and freeing that must stall neither this map nor a runtime worker. A
    /// call still holding a bucket keeps it alive until that call returns.
    /// Needs a tokio runtime context, as `hcore::blocking::run` does.
    pub async fn end(&self, ended: ScopeEnded) -> usize {
        let released = {
            let mut inner = self.inner.write();
            let mut released: Vec<Arc<T>> = inner.live.remove(&ended.scope).into_iter().collect();
            if ended.ended_below > inner.watermark {
                let w = ended.ended_below;
                inner.watermark = w;
                inner.ended = inner.ended.split_off(&w);
                let below: Vec<ScopeId> = inner
                    .live
                    .keys()
                    .copied()
                    .filter(|id| id.get() < w)
                    .collect();
                released.extend(below.iter().filter_map(|id| inner.live.remove(id)));
            }
            if ended.scope.get() >= inner.watermark {
                inner.ended.insert(ended.scope.get());
            }
            released
        };
        let n = released.len();
        if n > 0 {
            hcore::blocking::run(move || drop(released)).await;
        }
        n
    }

    /// How many scope buckets are live (the unscoped one not counted).
    pub fn live_scopes(&self) -> usize {
        self.inner.read().live.len()
    }

    /// How many ended ids this map remembers above its watermark.
    pub fn remembered_ended(&self) -> usize {
        self.inner.read().ended.len()
    }
}

impl<T> fmt::Debug for ScopeMap<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScopeMap")
            .field("owner", &self.owner)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;
    use std::sync::Weak;

    fn id(n: u64) -> ScopeId {
        ScopeId::new(n).unwrap()
    }

    /// The ids `0..n` in a fixed shuffled order (xorshift Fisher-Yates), so the
    /// test is reproducible without a `rand` dependency.
    fn shuffled(n: u64) -> Vec<u64> {
        let mut v: Vec<u64> = (1..=n).collect();
        let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
        for i in (1..v.len()).rev() {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let j = usize::try_from(s % (i as u64 + 1)).unwrap();
            v.swap(i, j);
        }
        v
    }

    #[tokio::test]
    async fn ended_scope_ids_do_not_accumulate() {
        const N: u64 = 10_000;
        // In order: each scope ends with the host's watermark right past it.
        let map = ScopeMap::<u32>::new("t");
        for n in 1..=N {
            map.enter(Some(id(n)));
            assert_eq!(map.end(ScopeEnded::new(id(n), n + 1)).await, 1);
        }
        assert_eq!(map.live_scopes(), 0);
        assert_eq!(map.remembered_ended(), 0);

        // Shuffled: every scope enters, then they end in a random order with
        // the watermark the host would compute (the oldest still live).
        let map = ScopeMap::<u32>::new("t");
        for n in 1..=N {
            map.enter(Some(id(n)));
        }
        let mut live: BTreeSet<u64> = (1..=N).collect();
        let mut peak = 0;
        for n in shuffled(N) {
            live.remove(&n);
            let below = live.first().copied().unwrap_or(N + 1);
            map.end(ScopeEnded::new(id(n), below)).await;
            peak = peak.max(map.remembered_ended());
        }
        assert_eq!(map.live_scopes(), 0);
        assert_eq!(
            map.remembered_ended(),
            0,
            "every ended id is below the final watermark"
        );
        assert!(peak <= N as usize);
        // An id below the watermark is still answered as ended.
        map.enter(Some(id(17)));
        assert_eq!(map.live_scopes(), 0);
    }

    #[tokio::test]
    async fn scope_map_end_before_enter_and_double_end() {
        let map = ScopeMap::<u32>::new("t");
        // A hook that arrives before the scope's first call (or a call that
        // raced past it) never makes a bucket afterwards.
        assert_eq!(map.end(ScopeEnded::new(id(5), 1)).await, 0);
        let late = map.enter_with(Some(id(5)), || 7);
        assert_eq!(*late, 7);
        assert_eq!(map.live_scopes(), 0);

        // A live scope keeps one bucket across calls.
        let a = map.enter_with(Some(id(6)), || 1);
        let b = map.enter_with(Some(id(6)), || 2);
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(map.end(ScopeEnded::new(id(6), 1)).await, 1);
        // Ending it again is a no-op.
        assert_eq!(map.end(ScopeEnded::new(id(6), 1)).await, 0);
        assert_eq!(map.live_scopes(), 0);

        // The unscoped bucket is shared and outlives every end.
        let u1 = map.enter_with(None, || 3);
        map.end(ScopeEnded::new(id(8), 100)).await;
        let u2 = map.enter_with(None, || 4);
        assert!(Arc::ptr_eq(&u1, &u2));
    }

    /// A bucket whose `Drop` checks the map's lock is free and that it runs on
    /// the blocking pool.
    struct Probe {
        map: Arc<OnceLock<Weak<ScopeMap<Probe>>>>,
        seen: Arc<Mutex<Option<(bool, bool)>>>,
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            let unlocked = self
                .map
                .get()
                .and_then(Weak::upgrade)
                .is_some_and(|m| m.inner.try_write().is_some());
            *self.seen.lock().unwrap() = Some((unlocked, hcore::blocking::in_blocking_job()));
        }
    }

    #[tokio::test]
    async fn bucket_drop_runs_outside_the_lock() {
        let cell = Arc::new(OnceLock::new());
        let seen = Arc::new(Mutex::new(None));
        let map = Arc::new(ScopeMap::<Probe>::new("t"));
        assert!(cell.set(Arc::downgrade(&map)).is_ok());
        drop(map.enter_with(Some(id(3)), || Probe {
            map: Arc::clone(&cell),
            seen: Arc::clone(&seen),
        }));
        assert_eq!(map.end(ScopeEnded::new(id(3), 1)).await, 1);
        assert_eq!(
            *seen.lock().unwrap(),
            Some((true, true)),
            "the bucket dropped with the map unlocked, on the blocking pool",
        );
    }

    #[test]
    fn a_minted_scope_holds_the_watermark_until_it_drops() {
        let ends = Arc::new(Mutex::new(Vec::new()));
        let rec = Arc::clone(&ends);
        let scope = CacheScope::mint(move |e, touched| rec.lock().unwrap().push((e, touched)));
        let n = scope.id().get();
        assert!(CacheScope::ended_below() <= n);
        scope.mark_touched();
        let clone = scope.clone();
        drop(scope);
        assert!(ends.lock().unwrap().is_empty(), "a clone still holds it");
        drop(clone);
        let ends = ends.lock().unwrap();
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0].0.scope.get(), n);
        assert!(ends[0].1);
    }
}
