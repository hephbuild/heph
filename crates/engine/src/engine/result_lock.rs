//! Per-addr transformable reader/writer lock guarding a target's cache entry and
//! its execute phase.
//!
//! A target's artifacts are protected by a read lock for as long as they are in
//! use, and (re)built under an exclusive write lock. Concretely:
//!
//! - **read** — a plain shared read guard ([`ResultReadGuard`]), held for the
//!   lifetime that artifacts are referenced. Many coexist (across requests with
//!   the in-memory backend, across processes with the filesystem backend).
//! - **upgradable_read** — the optimistic guard used when a build may be needed
//!   ([`ResultUpgradableGuard`]); at most one per addr, but coexists with plain
//!   readers, and can [`upgrade`](ResultUpgradableGuard::upgrade) to a writer.
//! - **write** — the exclusive guard held across execute + `cache_locally`
//!   ([`ResultWriteGuard`]); [`downgrade`](ResultWriteGuard::downgrade)s back to an
//!   upgradable read.
//!
//! ## Two scopes: the target, and the revision
//!
//! The lock is a [`TBridge`] of two halves keyed at different granularities:
//!
//! - the **gateway** (outer, exclusive) is per *target* — one per [`Addr`]. Every
//!   writer and upgradable reader takes it first, so at most one request in any
//!   process is building a given target at a time, whichever revision it is
//!   building. Plain readers never touch it.
//! - the **revision** lock (inner, reader/writer) is per *revision* —
//!   `(addr, hashin)`, the same key the cache stores an entry under. It is what
//!   a riding read holds, and what a writer must drain before replacing or
//!   deleting that entry.
//!
//! Keying the riding read by revision is the point. A request holds a read on
//! every revision whose artifacts it is still using, and a long-running
//! command (`heph run //app:serve`) can use them for a very long time. With the inner
//! lock per addr, a second `heph` that needed to build a *new* revision of any
//! of those targets — one whose input changed — parked behind readers of the
//! *old* one until the first command exited, though the two entries share no
//! bytes. Now it waits only for a concurrent *build* of the same target (the
//! gateway) and, for a rebuild of the very same revision (`--force`), for that
//! revision's readers.
//!
//! The halves share one instance per key across every bridge composed over
//! them ([`KeyedHandle`]), so exclusion holds in-process as well as across
//! processes. The default filesystem backend serializes across *processes* via
//! `flock(2)` lock files under `<home>/lock/` — `<addr>.outer.lock` and
//! `<addr>.<revision>.inner.lock`; the in-memory backend serializes only within
//! this process.

use anyhow::Result;
use async_trait::async_trait;
use hcore::hasync::Cancellable;
use hlock::hlock::{
    Ctoken, FLock, FRWLock, FReadGuard, FWriteGuard, KeyedGuard, KeyedHandle, KeyedLock,
    KeyedRWLock, Lock, MemGuard, MemLock, MemRWLock, MemReadGuard, MemWriteGuard, RWLock, TBridge,
    TBridgeUpgradableGuard, TBridgeWriteGuard, TLock, TUpgradableReadGuard, TWriteGuard,
};
use hmodel::htaddr::Addr;
use std::io::Read as _;
use std::path::{Path, PathBuf};

/// Which lock backend guards the cache/execute phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LockBackend {
    /// `flock(2)` lock files under `<home>/lock/`. Mutually exclusive across
    /// processes on the same machine. Default.
    #[default]
    Fs,
    /// In-process async locks. Single-process only.
    Mem,
}

/// One revision of a target: the key of its cache entry, and of its inner lock.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RevisionKey {
    addr: Addr,
    hashin: String,
}

type FsGateway = KeyedHandle<Addr, GatewayLock>;
type FsRevision = KeyedHandle<RevisionKey, FRWLock>;
type MemGateway = KeyedHandle<Addr, MemLock>;
type MemRevision = KeyedHandle<RevisionKey, MemRWLock>;

/// The per-addr gateway lock: an [`FLock`] plus one policy — **acquiring it
/// empties the lock file.**
///
/// The gateway file's contents are the holder's pid stamp, and they describe
/// *the current holder*. They only ever outlive their writer when a holder is
/// killed without running `Drop`: nothing unlinks the gateway file then, and
/// nothing sweeps `<home>/lock/`. The next exclusive acquire is the first moment
/// anyone can clear that, so it is where the clearing belongs.
///
/// This is what closes the *wide* window. [`TBridge`] takes the gateway first
/// and only then parks on the inner lock — a wait bounded by a whole build —
/// and [`stamp_pid`] runs after both. A waiter in another process reading the
/// file in between now sees an empty one ("holder unknown") rather than the name
/// of the previous, dead holder.
///
/// It also means the common cross-process shape — we hold the gateway and are
/// waiting for *other* processes' read guards on the inner lock to drain —
/// reports "holder unknown". That is the point: stamping at outer-acquire
/// instead would make it report this very process as the holder, which is worse
/// than admitting we do not know who is in the way.
///
/// Deliberately here and not in [`FLock`]: `FLock` is also `driver-support`'s
/// staging lock, which keeps nothing in its file and should not pay a syscall
/// per acquire for a stamp it never writes.
#[derive(Clone, Debug)]
pub struct GatewayLock(FLock);

impl GatewayLock {
    fn new(path: PathBuf) -> Self {
        Self(FLock::new(path))
    }
}

/// Empty a freshly-acquired gateway file. Best-effort for the same reason
/// [`stamp_pid`] is: the contents are a diagnostic, and failing a build lock
/// because a pid could not be *erased* would trade a stale pid for a dead build.
/// A failure leaves exactly the behaviour that shipped before this existed.
///
/// One `ftruncate` — `write_all_at` on an empty slice issues no syscall — on the
/// gateway acquire, which is the cold path (a warm cache hit takes only the
/// inner read lock and never reaches here).
fn blank_stamp(gateway: &FWriteGuard) {
    if let Err(err) = gateway.write_contents(b"") {
        tracing::debug!(error = %err, "blanking the gateway pid stamp on acquire");
    }
}

#[async_trait]
impl Lock for GatewayLock {
    type Guard = FWriteGuard;

    async fn lock(&self, ctoken: Ctoken<'_>) -> Result<FWriteGuard> {
        let guard = self.0.lock(ctoken).await?;
        blank_stamp(&guard);
        Ok(guard)
    }

    fn try_lock(&self) -> Result<Option<FWriteGuard>> {
        let guard = self.0.try_lock()?;
        if let Some(guard) = &guard {
            blank_stamp(guard);
        }
        Ok(guard)
    }
}

type FsReadGuard = KeyedGuard<RevisionKey, FRWLock, FReadGuard>;
type MemRevReadGuard = KeyedGuard<RevisionKey, MemRWLock, MemReadGuard>;
type FsUpgradableGuard = TBridgeUpgradableGuard<FsGateway, FsRevision>;
type MemUpgradableGuard = TBridgeUpgradableGuard<MemGateway, MemRevision>;
type FsWriteGuard = TBridgeWriteGuard<FsGateway, FsRevision>;
type MemBridgeWriteGuard = TBridgeWriteGuard<MemGateway, MemRevision>;

/// Plain shared read guard on one revision's cache entry. Held for as long as
/// the artifacts are in use; the lock releases on drop. `Send + Sync` so it can
/// ride inside an `Arc<dyn Content>` shared across tasks.
///
/// Holds the revision lock only — never the target's gateway — so it blocks a
/// writer of *this* revision and nothing else.
#[derive(Debug)]
pub enum ResultReadGuard {
    Fs(FsReadGuard),
    Mem(MemRevReadGuard),
}

/// The target-scoped gateway alone: "nobody else is building this target".
///
/// For the callers that act on a target as a whole rather than on one
/// revision — GC, which enumerates and deletes revisions under it, and the
/// credential path, which needs only the per-target execute exclusion. A
/// revision is deleted through [`ResultLock::try_write_revision`], which
/// demands this guard as proof that no build can race the delete.
#[derive(Debug)]
pub enum TargetGuard {
    Fs(KeyedGuard<Addr, GatewayLock, FWriteGuard>),
    Mem(KeyedGuard<Addr, MemLock, MemGuard>),
}

/// A run of the target in progress. See [`ResultLock::lock_execute`].
#[derive(Debug)]
pub enum ExecuteGuard {
    Fs(KeyedGuard<Addr, FLock, FWriteGuard>),
    Mem(KeyedGuard<Addr, MemLock, MemGuard>),
}

/// Exclusive hold on one revision, taken under a [`TargetGuard`]. Its readers
/// have drained, and none can start until it drops.
#[derive(Debug)]
pub enum RevisionWriteGuard {
    Fs(KeyedGuard<RevisionKey, FRWLock, FWriteGuard>),
    Mem(KeyedGuard<RevisionKey, MemRWLock, MemWriteGuard>),
}

/// Upgradable read guard: the optimistic gateway holder. At most one per addr,
/// coexists with plain readers, and can be [`upgrade`](Self::upgrade)d to a
/// writer without risk of deadlock.
#[derive(Debug)]
pub enum ResultUpgradableGuard {
    Fs(FsUpgradableGuard),
    Mem(MemUpgradableGuard),
}

/// Exclusive write guard held across the execute + cache cycle.
#[derive(Debug)]
pub enum ResultWriteGuard {
    Fs(FsWriteGuard),
    Mem(MemBridgeWriteGuard),
}

impl ResultUpgradableGuard {
    /// Atomically upgrade read→write. Waits for plain readers to drain but never
    /// blocks on the gateway (already held), so it cannot deadlock against a
    /// concurrent upgrade/downgrade. On error the lock is released.
    pub async fn upgrade(
        self,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> Result<ResultWriteGuard> {
        match self {
            ResultUpgradableGuard::Fs(g) => Ok(ResultWriteGuard::Fs(g.upgrade(ctoken).await?)),
            ResultUpgradableGuard::Mem(g) => Ok(ResultWriteGuard::Mem(g.upgrade(ctoken).await?)),
        }
    }
}

impl ResultWriteGuard {
    /// Atomically downgrade write→upgradable-read. No other writer can slip in
    /// during the transition.
    pub async fn downgrade(
        self,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> Result<ResultUpgradableGuard> {
        match self {
            ResultWriteGuard::Fs(g) => Ok(ResultUpgradableGuard::Fs(g.downgrade(ctoken).await?)),
            ResultWriteGuard::Mem(g) => Ok(ResultUpgradableGuard::Mem(g.downgrade(ctoken).await?)),
        }
    }
}

/// The two registries a backend draws its locks from: one gateway per target,
/// one reader/writer lock per revision. See the module doc.
pub struct Registries<G, R, E> {
    gateways: KeyedLock<Addr, G>,
    revisions: KeyedRWLock<RevisionKey, R>,
    /// One per target, held across a run of it — see
    /// [`ResultLock::lock_execute`].
    executes: KeyedLock<Addr, E>,
}

impl<G, R, E> Registries<G, R, E>
where
    G: Lock + 'static,
    R: RWLock + 'static,
    E: Lock + 'static,
{
    fn new(
        gateway: impl Fn(&Addr) -> G + Send + Sync + 'static,
        revision: impl Fn(&RevisionKey) -> R + Send + Sync + 'static,
        execute: impl Fn(&Addr) -> E + Send + Sync + 'static,
    ) -> Self {
        Self {
            gateways: KeyedLock::new(gateway),
            revisions: KeyedRWLock::new(revision),
            executes: KeyedLock::new(execute),
        }
    }

    /// The transformable lock over one revision: this target's gateway, that
    /// revision's reader/writer lock.
    fn bridge(
        &self,
        addr: &Addr,
        hashin: &str,
    ) -> TBridge<KeyedHandle<Addr, G>, KeyedHandle<RevisionKey, R>> {
        TBridge::new(
            self.gateways.handle(addr.clone()),
            self.revisions.handle(revision_key(addr, hashin)),
        )
    }
}

fn revision_key(addr: &Addr, hashin: &str) -> RevisionKey {
    RevisionKey {
        addr: addr.clone(),
        hashin: hashin.to_owned(),
    }
}

/// Keyed transformable lock: a per-target gateway over per-revision
/// reader/writer locks (see the module doc). The filesystem backend names its
/// lock files after content hashes (filesystem-safe), the in-memory backend
/// keys async locks directly. The same key maps to the same lock across
/// requests and — for the filesystem backend — across processes.
pub enum ResultLock {
    Fs {
        /// Directory holding the per-key lock files. Kept so [`holder_pid`] can
        /// locate the gateway file independently of the keyed registry.
        ///
        /// [`holder_pid`]: ResultLock::holder_pid
        dir: PathBuf,
        locks: Registries<GatewayLock, FRWLock, FLock>,
    },
    Mem(Registries<MemLock, MemRWLock, MemLock>),
}

impl ResultLock {
    /// Build the configured backend. For [`LockBackend::Fs`], `dir` must already
    /// exist; per-key lock files are created lazily on first acquisition.
    pub fn new(backend: LockBackend, dir: PathBuf) -> Self {
        match backend {
            LockBackend::Fs => ResultLock::Fs {
                dir: dir.clone(),
                locks: Registries::new(
                    enclose::enclose!((dir) move |addr: &Addr| {
                        GatewayLock::new(outer_lock_path(&dir, addr))
                    }),
                    enclose::enclose!((dir) move |rev: &RevisionKey| {
                        FRWLock::new(inner_lock_path(&dir, &rev.addr, &rev.hashin))
                    }),
                    move |addr: &Addr| FLock::new(execute_lock_path(&dir, addr)),
                ),
            },
            LockBackend::Mem => ResultLock::Mem(Registries::new(
                |_| MemLock::default(),
                |_| MemRWLock::default(),
                |_| MemLock::default(),
            )),
        }
    }

    /// Acquire `addr`'s execute lock: exclusive across every run of the target,
    /// in any process, from just before its sandbox is claimed until its outputs
    /// are cached. The sandbox directory is per addr, so two runs of one target
    /// at once would tear down each other's tree.
    ///
    /// Separate from the gateway because one run happens *without* it: a cache
    /// hit whose blobs turn out to be unavailable rebuilds under its riding read
    /// of the revision, and waiting on the gateway there would deadlock against
    /// a `--force` or `clean` of that same revision — which holds the gateway
    /// and waits for that read. So this lock is a leaf: it is taken after every
    /// result lock a run needs and after the run's deps have resolved, and
    /// nothing is acquired while it is held — so it cannot take part in a cycle.
    pub async fn lock_execute(
        &self,
        addr: &Addr,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> Result<ExecuteGuard> {
        match self {
            ResultLock::Fs { locks, .. } => Ok(ExecuteGuard::Fs(
                locks.executes.lock(addr.clone(), ctoken).await?,
            )),
            ResultLock::Mem(locks) => Ok(ExecuteGuard::Mem(
                locks.executes.lock(addr.clone(), ctoken).await?,
            )),
        }
    }

    /// Acquire a plain shared read guard on revision `(addr, hashin)`. Cheap and
    /// fully concurrent — the hot-path guard taken optimistically before a cache
    /// lookup and attached to the returned artifacts. Takes the revision lock
    /// only, never the target's gateway.
    pub async fn read(
        &self,
        addr: &Addr,
        hashin: &str,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> Result<ResultReadGuard> {
        let key = revision_key(addr, hashin);
        match self {
            ResultLock::Fs { locks, .. } => Ok(ResultReadGuard::Fs(
                locks.revisions.read(key, ctoken).await?,
            )),
            ResultLock::Mem(locks) => Ok(ResultReadGuard::Mem(
                locks.revisions.read(key, ctoken).await?,
            )),
        }
    }

    /// Acquire the upgradable read guard on revision `(addr, hashin)` — the
    /// target's gateway plus a read on the revision — waiting until free or
    /// `ctoken` is cancelled. On the filesystem backend the holder stamps its pid
    /// into the gateway lock file so a *different* process blocked on the same
    /// target can name the holder via [`holder_pid`](ResultLock::holder_pid).
    /// Best-effort: a write failure never fails the acquire.
    pub async fn upgradable_read(
        &self,
        addr: &Addr,
        hashin: &str,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> Result<ResultUpgradableGuard> {
        match self {
            ResultLock::Fs { locks, .. } => {
                let guard = locks.bridge(addr, hashin).upgradable_read(ctoken).await?;
                stamp_pid(guard.outer_guard().map(|g| g.get()));
                Ok(ResultUpgradableGuard::Fs(guard))
            }
            ResultLock::Mem(locks) => Ok(ResultUpgradableGuard::Mem(
                locks.bridge(addr, hashin).upgradable_read(ctoken).await?,
            )),
        }
    }

    /// Acquire the exclusive write guard on revision `(addr, hashin)`: the
    /// target's gateway (no other build of this target, of any revision), then
    /// the revision's write lock (its readers drained). Held across execute +
    /// cache. Stamps pid like [`upgradable_read`](ResultLock::upgradable_read).
    pub async fn write(
        &self,
        addr: &Addr,
        hashin: &str,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> Result<ResultWriteGuard> {
        match self {
            ResultLock::Fs { locks, .. } => {
                let guard = locks.bridge(addr, hashin).write(ctoken).await?;
                stamp_pid(guard.outer_guard().map(|g| g.get()));
                Ok(ResultWriteGuard::Fs(guard))
            }
            ResultLock::Mem(locks) => Ok(ResultWriteGuard::Mem(
                locks.bridge(addr, hashin).write(ctoken).await?,
            )),
        }
    }

    /// Non-blocking [`write`](ResultLock::write). `Ok(None)` when the target is
    /// being built or the revision is in use.
    pub fn try_write(&self, addr: &Addr, hashin: &str) -> Result<Option<ResultWriteGuard>> {
        match self {
            ResultLock::Fs { locks, .. } => match locks.bridge(addr, hashin).try_write()? {
                Some(guard) => {
                    stamp_pid(guard.outer_guard().map(|g| g.get()));
                    Ok(Some(ResultWriteGuard::Fs(guard)))
                }
                None => Ok(None),
            },
            ResultLock::Mem(locks) => Ok(locks
                .bridge(addr, hashin)
                .try_write()?
                .map(ResultWriteGuard::Mem)),
        }
    }

    /// Acquire the target's gateway alone — exclusive against every build of
    /// `addr`, in any process, and against nothing else. Stamps pid.
    pub async fn lock_target(
        &self,
        addr: &Addr,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> Result<TargetGuard> {
        match self {
            ResultLock::Fs { locks, .. } => {
                let guard = locks.gateways.lock(addr.clone(), ctoken).await?;
                stamp_pid(Some(guard.get()));
                Ok(TargetGuard::Fs(guard))
            }
            ResultLock::Mem(locks) => Ok(TargetGuard::Mem(
                locks.gateways.lock(addr.clone(), ctoken).await?,
            )),
        }
    }

    /// Non-blocking [`lock_target`](ResultLock::lock_target). `Ok(None)` while
    /// any request is building `addr`.
    pub fn try_lock_target(&self, addr: &Addr) -> Result<Option<TargetGuard>> {
        match self {
            ResultLock::Fs { locks, .. } => match locks.gateways.try_lock(addr.clone())? {
                Some(guard) => {
                    stamp_pid(Some(guard.get()));
                    Ok(Some(TargetGuard::Fs(guard)))
                }
                None => Ok(None),
            },
            ResultLock::Mem(locks) => {
                Ok(locks.gateways.try_lock(addr.clone())?.map(TargetGuard::Mem))
            }
        }
    }

    /// Non-blocking exclusive hold on revision `(addr, hashin)`, for deleting it.
    /// `Ok(None)` while anything reads it — a request riding its artifacts.
    ///
    /// `_target` is the proof that the caller holds `addr`'s gateway: every
    /// writer takes the gateway before the revision lock, so holding it is what
    /// rules out a build re-creating the revision mid-delete, and what keeps this
    /// acquire in the same order as every other writer's.
    pub fn try_write_revision(
        &self,
        _target: &TargetGuard,
        addr: &Addr,
        hashin: &str,
    ) -> Result<Option<RevisionWriteGuard>> {
        let key = revision_key(addr, hashin);
        match self {
            ResultLock::Fs { locks, .. } => {
                Ok(locks.revisions.try_write(key)?.map(RevisionWriteGuard::Fs))
            }
            ResultLock::Mem(locks) => {
                Ok(locks.revisions.try_write(key)?.map(RevisionWriteGuard::Mem))
            }
        }
    }

    /// Blocking [`try_write_revision`](ResultLock::try_write_revision): waits for
    /// the revision's readers to drain. Only for an explicit, user-requested
    /// delete (`heph clean`); the background GC paths must not park behind a
    /// reader that may be a command running for hours.
    pub async fn write_revision(
        &self,
        _target: &TargetGuard,
        addr: &Addr,
        hashin: &str,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> Result<RevisionWriteGuard> {
        let key = revision_key(addr, hashin);
        match self {
            ResultLock::Fs { locks, .. } => Ok(RevisionWriteGuard::Fs(
                locks.revisions.write(key, ctoken).await?,
            )),
            ResultLock::Mem(locks) => Ok(RevisionWriteGuard::Mem(
                locks.revisions.write(key, ctoken).await?,
            )),
        }
    }

    /// Who a waiter on `addr` — on revision `hashin`, when the wait is for one —
    /// is waiting for, as `(holder_pid, in_use_by_readers)`. Best-effort, for the
    /// lock-wait notice.
    ///
    /// This process's own stamp never names the holder (on the filesystem
    /// backend, where a pid means another process). A waiter that already holds
    /// the gateway — `heph clean` waiting on one revision's readers — stamped its
    /// own pid there, and reporting that named the waiter itself. With no other
    /// pid to name, the revision's readers are reported when they hold it.
    pub fn wait_holder(&self, addr: &Addr, hashin: Option<&str>) -> (Option<u32>, bool) {
        let pid = self
            .holder_pid(addr)
            .filter(|pid| *pid != std::process::id() || matches!(self, ResultLock::Mem(_)));
        if pid.is_none() && hashin.is_some_and(|hashin| self.revision_in_use(addr, hashin)) {
            return (None, true);
        }
        (pid, false)
    }

    /// Whether some guard — typically a riding read in another process — holds
    /// revision `(addr, hashin)` right now. A snapshot for diagnostics, like
    /// [`holder_pid`](ResultLock::holder_pid): it names *why* a writer waits when
    /// the gateway cannot name a pid, and is never an admission decision.
    pub fn revision_in_use(&self, addr: &Addr, hashin: &str) -> bool {
        match self {
            ResultLock::Fs { dir, .. } => {
                match FRWLock::is_path_locked(inner_lock_path(dir, addr, hashin)) {
                    Ok(locked) => locked,
                    Err(err) => {
                        tracing::debug!(error = %err, "probing revision lock");
                        false
                    }
                }
            }
            ResultLock::Mem(locks) => locks
                .revisions
                .try_write(revision_key(addr, hashin))
                .ok()
                .is_some_and(|g| g.is_none()),
        }
    }

    /// Best-effort pid of the process **currently holding** the gateway for
    /// `addr`, or `None` when the holder is unknown. For the in-memory backend
    /// the holder is always this process.
    ///
    /// For the filesystem backend this is two questions, asked **in this order**:
    ///
    /// 1. *Is the gateway held at all?* — [`FLock::is_path_held`] probes the
    ///    `flock` itself. A stamp with nobody holding the lock is a stamp its
    ///    writer left behind when it was killed; the kernel dropped that
    ///    process's lock at exit, but nothing unlinked the file and nothing
    ///    sweeps `<home>/lock/`, so the pid would otherwise be readable forever.
    /// 2. *Who stamped it?* — [`read_pid`] on the gateway file, which
    ///    [`GatewayLock`] empties at acquire, so a holder that has not stamped
    ///    yet reads as unknown rather than as its predecessor.
    ///
    /// **Probe first, then read.** Reading first leaves a window: the pid is
    /// captured, the holder releases (and unlinks), a new holder takes the
    /// gateway, and the probe then reports "held" — naming a process that holds
    /// nothing and whose pid may already have been recycled. That is the exact
    /// failure this function exists to prevent, so the order is load-bearing
    /// rather than incidental. Probe-first has no such window: after a
    /// confirmed "held", the read yields the confirmed holder's stamp, a newer
    /// holder's stamp, or nothing — never a released holder's.
    ///
    /// For the same reason the read is *not* fused into the probe by reusing
    /// its fd, which would save an `open`: that fd names the inode that was
    /// held, and if the holder releases in between, reading it returns the
    /// stamp of a lock nobody holds. Re-resolving the path is what makes the
    /// stale answer unreachable.
    ///
    /// Probing the lock is the liveness check, deliberately in place of
    /// `kill(pid, 0)` on the stamped pid: `kill` answers for a *pid*, which is a
    /// recycled name — a reused pid, or a zombie whose pid still answers,
    /// reports a live process that never held anything. The lock is the thing we
    /// actually want to know about, and it costs one `open` + one `flock` on a
    /// path that has already spent `RESULT_LOCK_NOTICE` waiting.
    ///
    /// It stays best-effort by nature. The probe is a snapshot — the holder may
    /// release the instant after — and a pid is only a hint for the user, never
    /// something the engine acts on.
    ///
    /// What this still cannot report: a wait on the *inner* lock, which is the
    /// common cross-process shape. Plain read guards are not stamped at all, so
    /// "who is holding the artifacts I want to rebuild" reads as unknown. See
    /// [`GatewayLock`].
    pub fn holder_pid(&self, addr: &Addr) -> Option<u32> {
        match self {
            ResultLock::Fs { dir, .. } => {
                let path = outer_lock_path(dir, addr);
                match FLock::is_path_held(&path) {
                    Ok(true) => read_pid(&path),
                    Ok(false) => None,
                    Err(err) => {
                        // The probe is the only caller of those contexts; without
                        // this the diagnostic path is itself undiagnosable.
                        tracing::debug!(error = %err, "probing gateway lock liveness");
                        None
                    }
                }
            }
            ResultLock::Mem(_) => Some(std::process::id()),
        }
    }
}

/// Path of the per-addr gateway (outer exclusive) lock file.
fn outer_lock_path(dir: &Path, addr: &Addr) -> PathBuf {
    dir.join(format!("{}.outer.lock", addr.hash_str()))
}

/// Path of the per-addr execute lock file. See [`ResultLock::lock_execute`].
fn execute_lock_path(dir: &Path, addr: &Addr) -> PathBuf {
    dir.join(format!("{}.execute.lock", addr.hash_str()))
}

/// Path of the per-revision inner reader/writer lock file.
///
/// The `hashin` is hashed rather than spliced in: it is opaque to this module,
/// and only a fixed-width hex digest is known to be a safe, bounded file-name
/// component. Prefixed by the addr's hash so a target's files sort together.
///
/// A revision's file is unlinked by its last *write* release — which a GC delete
/// is — and survives plain read releases (see `hlock::flock`). So the directory
/// holds about one file per retained revision, not one per revision ever read.
fn inner_lock_path(dir: &Path, addr: &Addr, hashin: &str) -> PathBuf {
    dir.join(format!(
        "{}.{:x}.inner.lock",
        addr.hash_str(),
        xxhash_rust::xxh3::xxh3_64(hashin.as_bytes())
    ))
}

/// Best-effort stamp of this process's pid into the gateway lock file, for
/// cross-process contention diagnostics. A failure is logged, not fatal.
///
/// Writes through the gateway guard's *already-open* file description rather
/// than re-opening the lock file by path: it drops the `open`/`close` pair (and
/// the path resolution that comes with them) from every gateway acquire, and it
/// makes the stamp structurally incapable of landing on a file this process does
/// not hold. That last part is robustness, not a bug fixed — at the instant this
/// runs we hold the gateway exclusively and are the only party that unlinks it,
/// so path and locked inode cannot yet diverge here. They are *allowed* to in
/// this design (`release_write` unlinks while still holding the lock), so the
/// stronger form is worth having before some future caller opens that window.
///
/// The payload is newline-framed, and `write_contents` empties the file before
/// writing. Together those two make every state a concurrent reader can observe
/// either a complete stamp or an unterminated prefix — never a *blend* that
/// parses. Without the frame, a shorter pid written over a longer stale one left
/// `<new><stale tail>`, all digits and perfectly parseable, naming a pid that
/// belongs to nobody; without the truncate-first order, a partially visible
/// write could still land inside the old frame. See [`read_pid`].
///
/// That bounds what a *torn* read can report. Freshness is a separate question,
/// and neither half of it is answered here: this runs only after the whole
/// bridge acquire completes, so between the gateway acquire and this line the
/// file holds whatever the last holder left; and a stamp outlives its writer
/// whenever that writer is killed without running `Drop`. Both are handled at
/// the other end — [`GatewayLock`] empties the file the moment the gateway is
/// acquired, and [`holder_pid`] reports a pid only while the lock is genuinely
/// held. Both of those are best-effort too: a blank that fails re-opens the
/// window for this addr until the next acquire, which is why the liveness probe
/// is a second, independent check rather than a belt on the first.
///
/// [`holder_pid`]: ResultLock::holder_pid
fn stamp_pid(gateway: Option<&FWriteGuard>) {
    debug_assert!(
        gateway.is_some(),
        "pid stamp with no held gateway guard: the guard owns the gateway for \
         its whole observable lifetime"
    );
    let Some(gateway) = gateway else {
        tracing::debug!("gateway guard unavailable for pid stamp");
        return;
    };
    let stamp = format!("{}\n", std::process::id());
    if let Err(err) = gateway.write_contents(stamp.as_bytes()) {
        tracing::debug!(error = %err, "stamping pid into gateway lock file");
    }
}

/// Read a pid previously stamped by the lock holder. `None` on any read/parse
/// failure (missing file, empty, non-numeric, not UTF-8, past `u32`) and on a
/// torn read.
///
/// Only the bytes before the first newline are a pid; an unterminated payload is
/// not one. That covers a half-visible write, and also a stamp left by a binary
/// from before the frame existed — the two are indistinguishable from here, so
/// both read as "holder unknown". That costs a correct pid in the second case,
/// during a rollout, on a file that is transient anyway; naming a pid the user
/// might `kill` in the first case costs more.
///
/// The read is capped: the payload is a pid and a newline, while the directory
/// it sits in could hold a stray or corrupted file of any size.
pub(crate) fn read_pid(path: &Path) -> Option<u32> {
    const MAX_STAMP: u64 = 64;
    let mut s = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_STAMP)
        .read_to_string(&mut s)
        .ok()?;
    s.split_once('\n')?.0.trim().parse().ok()
}

impl std::fmt::Debug for ResultLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let backend = match self {
            ResultLock::Fs { .. } => "Fs",
            ResultLock::Mem(_) => "Mem",
        };
        f.debug_struct("ResultLock")
            .field("backend", &backend)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hmodel::htpkg::PkgBuf;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    fn ct() -> StdCancellationToken {
        StdCancellationToken::new()
    }

    fn addr(name: &str) -> Addr {
        Addr::new(PkgBuf::from("pkg"), name.to_string(), BTreeMap::new())
    }

    fn fs(dir: &tempfile::TempDir) -> ResultLock {
        ResultLock::new(LockBackend::Fs, dir.path().to_path_buf())
    }

    /// Hold the gateway for `addr` the way another *process* would: a raw
    /// [`FLock`] on the outer path, bypassing both [`GatewayLock`]'s blank and
    /// [`stamp_pid`]. That lets a test plant exact bytes in the gateway file and
    /// still have [`ResultLock::holder_pid`]'s liveness probe see a held lock, so
    /// the assertion is about the *parsing* and nothing else.
    async fn hold_raw_gateway(dir: &tempfile::TempDir, a: &Addr) -> FWriteGuard {
        FLock::new(outer_lock_path(dir.path(), a))
            .lock(&ct())
            .await
            .expect("raw gateway")
    }

    // ResultReadGuard must be Send + Sync — it lives inside Arc<dyn Content>
    // shared across tasks.
    #[test]
    fn read_guard_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ResultReadGuard>();
    }

    #[tokio::test]
    async fn plain_reads_coexist_with_each_other_and_upgradable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = fs(&dir);

        let _r1 = lock.read(&addr("a"), "h", &ct()).await.expect("r1");
        let _r2 = lock.read(&addr("a"), "h", &ct()).await.expect("r2");
        // The optimistic gateway coexists with the plain readers.
        let _u = lock
            .upgradable_read(&addr("a"), "h", &ct())
            .await
            .expect("upgradable");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_upgradable_blocks_until_first_drops() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = Arc::new(fs(&dir));

        let held = lock
            .upgradable_read(&addr("a"), "h", &ct())
            .await
            .expect("first");

        let lock2 = Arc::clone(&lock);
        let handle = tokio::spawn(async move {
            let tok = StdCancellationToken::new();
            lock2
                .upgradable_read(&addr("a"), "h", &tok)
                .await
                .map(|_| ())
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !handle.is_finished(),
            "second gateway must block while first held"
        );

        drop(held);
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("did not hang")
            .expect("join")
            .expect("acquires after release");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_excludes_plain_reads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = Arc::new(fs(&dir));

        let w = lock
            .upgradable_read(&addr("a"), "h", &ct())
            .await
            .expect("upgradable")
            .upgrade(&ct())
            .await
            .expect("upgrade");

        let lock2 = Arc::clone(&lock);
        let handle = tokio::spawn(async move {
            let tok = StdCancellationToken::new();
            lock2.read(&addr("a"), "h", &tok).await.map(|_| ())
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !handle.is_finished(),
            "reader must block under an active writer"
        );

        drop(w);
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("did not hang")
            .expect("join")
            .expect("reader admitted after write released");
    }

    #[tokio::test]
    async fn downgrade_then_convert_to_plain_read() {
        // The execute_and_cache conversion: write → downgrade → acquire a plain
        // read while holding the gateway → drop the gateway, leaving a shared
        // read. A fresh writer is then blocked until that read drains.
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = fs(&dir);

        let w = lock.write(&addr("a"), "h", &ct()).await.expect("write");
        let up = w.downgrade(&ct()).await.expect("downgrade");
        let read = lock
            .read(&addr("a"), "h", &ct())
            .await
            .expect("plain read coexists");
        drop(up);

        // A writer cannot proceed while the plain read is held...
        assert!(
            lock.write(&addr("a"), "h", &cancelled_ct()).await.is_err(),
            "writer blocked while shared read held (cancelled wait)"
        );
        drop(read);
        // ...but succeeds once it drains.
        lock.write(&addr("a"), "h", &ct())
            .await
            .expect("writer after read drains");
    }

    #[tokio::test]
    async fn try_write_none_while_held_then_some_after_release() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = fs(&dir);

        // Free → acquires.
        let g = lock
            .try_write(&addr("a"), "h")
            .expect("try_write ok")
            .expect("free addr acquires");

        // Held → non-blocking, returns None rather than waiting.
        assert!(
            lock.try_write(&addr("a"), "h")
                .expect("try_write ok")
                .is_none(),
            "must not acquire while a write guard is held"
        );

        drop(g);

        // Released → acquires again.
        assert!(
            lock.try_write(&addr("a"), "h")
                .expect("try_write ok")
                .is_some(),
            "must acquire once the prior guard drops"
        );
    }

    #[tokio::test]
    async fn try_write_none_while_plain_read_held() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = fs(&dir);
        let _r = lock.read(&addr("a"), "h", &ct()).await.expect("read");
        assert!(
            lock.try_write(&addr("a"), "h")
                .expect("try_write ok")
                .is_none(),
            "writer must not acquire while a shared read is held"
        );
    }

    #[tokio::test]
    async fn distinct_addrs_independent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = fs(&dir);
        let _w = lock.write(&addr("a"), "h", &ct()).await.expect("a");
        // A different addr is independent — it acquires without blocking on `a`.
        let _b = lock.write(&addr("b"), "h", &ct()).await.expect("b");
    }

    /// An acquire that must not wait. Not a cancelled token: the filesystem
    /// backend checks cancellation before its first attempt, so that would fail
    /// even an uncontended acquire. Generous, since the failure it catches is a
    /// wait that never ends.
    async fn promptly<T>(fut: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        tokio::time::timeout(Duration::from_secs(5), fut)
            .await
            .map_err(|_elapsed| anyhow::anyhow!("blocked"))?
    }

    /// Both backends, so every revision-scope test below runs against the
    /// in-process one too: it is the backend where sharing the gateway instance
    /// across revisions is the *only* thing providing exclusion.
    fn both(dir: &tempfile::TempDir) -> [ResultLock; 2] {
        [
            fs(dir),
            ResultLock::new(LockBackend::Mem, dir.path().to_path_buf()),
        ]
    }

    /// **The bug this scope split fixes.** A command riding a read on one
    /// revision — `heph run` of a long-lived target holds one on every dep for
    /// its whole life — must not block another command from building a *new*
    /// revision of the same target. The two entries share no bytes.
    #[tokio::test]
    async fn a_reader_of_one_revision_does_not_block_a_build_of_another() {
        let dir = tempfile::tempdir().expect("tempdir");
        for lock in both(&dir) {
            let _riding = lock.read(&addr("a"), "old", &ct()).await.expect("read");
            let w = promptly(lock.write(&addr("a"), "new", &ct()))
                .await
                .expect("a build of another revision must not wait on old readers");
            // And the hand-off to its own riding read is just as free.
            let up = w.downgrade(&ct()).await.expect("downgrade");
            let _read = lock.read(&addr("a"), "new", &ct()).await.expect("read");
            drop(up);
            assert!(lock.try_write(&addr("a"), "old").expect("try").is_none());
        }
    }

    /// One build of a target at a time, whichever revision each is building:
    /// the gateway is per target. Two builds of one addr also share its sandbox
    /// directory, so this is correctness, not only policy.
    #[tokio::test]
    async fn builds_of_two_revisions_of_one_target_still_serialize() {
        let dir = tempfile::tempdir().expect("tempdir");
        for lock in both(&dir) {
            let w = lock.write(&addr("a"), "h1", &ct()).await.expect("first");
            assert!(
                lock.write(&addr("a"), "h2", &cancelled_ct()).await.is_err(),
                "a second build of the target must wait (cancelled wait)"
            );
            assert!(lock.try_write(&addr("a"), "h2").expect("try").is_none());
            assert!(lock.try_lock_target(&addr("a")).expect("try").is_none());
            drop(w);
            lock.write(&addr("a"), "h2", &ct())
                .await
                .expect("acquires once the first build releases");
        }
    }

    /// A rebuild of the *same* revision (`--force`) still waits for its readers:
    /// it rewrites the entry they are reading.
    #[tokio::test]
    async fn a_rebuild_of_the_same_revision_waits_for_its_readers() {
        let dir = tempfile::tempdir().expect("tempdir");
        for lock in both(&dir) {
            let r = lock.read(&addr("a"), "h", &ct()).await.expect("read");
            assert!(lock.write(&addr("a"), "h", &cancelled_ct()).await.is_err());
            drop(r);
            lock.write(&addr("a"), "h", &ct())
                .await
                .expect("after drain");
        }
    }

    /// GC's shape: the target lock, then each revision without waiting. A read
    /// revision is refused; an unread one is granted; and the target lock
    /// itself shuts out builds but not readers.
    #[tokio::test]
    async fn revision_writes_under_the_target_lock_skip_only_read_revisions() {
        let dir = tempfile::tempdir().expect("tempdir");
        for lock in both(&dir) {
            let _riding = lock.read(&addr("a"), "read", &ct()).await.expect("read");
            let target = lock.lock_target(&addr("a"), &ct()).await.expect("target");
            assert!(
                lock.try_write_revision(&target, &addr("a"), "read")
                    .expect("try")
                    .is_none(),
                "a revision being read is not deletable"
            );
            assert!(
                lock.try_write_revision(&target, &addr("a"), "idle")
                    .expect("try")
                    .is_some()
            );
            assert!(
                lock.write(&addr("a"), "other", &cancelled_ct())
                    .await
                    .is_err(),
                "no build while the target lock is held"
            );
            promptly(lock.read(&addr("a"), "other", &ct()))
                .await
                .expect("readers never take the target lock");
        }
    }

    #[tokio::test]
    async fn wait_holder_does_not_name_the_waiter_itself() {
        // `heph clean`'s shape: it holds (and stamped) the target lock, then
        // waits on one revision's readers. Two `ResultLock`s over one dir are
        // two processes as far as `flock` is concerned.
        let dir = tempfile::tempdir().expect("tempdir");
        let (cleaner, other) = (fs(&dir), fs(&dir));
        let _target = cleaner
            .lock_target(&addr("a"), &ct())
            .await
            .expect("target");
        let _riding = other.read(&addr("a"), "h", &ct()).await.expect("read");
        assert_eq!(
            cleaner.wait_holder(&addr("a"), Some("h")),
            (None, true),
            "the readers are the blocker, not our own stamp"
        );
        assert_eq!(cleaner.wait_holder(&addr("a"), Some("idle")), (None, false));
    }

    #[tokio::test]
    async fn wait_holder_names_another_processs_gateway() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = fs(&dir);
        let a = addr("a");
        let held = hold_raw_gateway(&dir, &a).await;
        held.write_contents(b"4242\n").expect("stamp");
        let _riding = lock.read(&a, "h", &ct()).await.expect("read");
        assert_eq!(
            lock.wait_holder(&a, Some("h")),
            (Some(4242), false),
            "a live foreign holder is named even when readers are present"
        );
        drop(held);
        assert_eq!(lock.wait_holder(&a, None), (None, false));
    }

    /// The execute lock is the leaf every run takes: one run per target, and
    /// independent of the result locks — taken while holding a riding read (the
    /// rebuild path) or the target lock (a build) without waiting on either.
    #[tokio::test]
    async fn the_execute_lock_serializes_runs_and_nothing_else() {
        let dir = tempfile::tempdir().expect("tempdir");
        for lock in both(&dir) {
            let _riding = lock.read(&addr("a"), "h", &ct()).await.expect("read");
            let _target = lock.lock_target(&addr("a"), &ct()).await.expect("target");
            let run = promptly(lock.lock_execute(&addr("a"), &ct()))
                .await
                .expect("not blocked by result locks");
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(100),
                    lock.lock_execute(&addr("a"), &ct())
                )
                .await
                .is_err(),
                "a second run of the target waits"
            );
            promptly(lock.lock_execute(&addr("b"), &ct()))
                .await
                .expect("other targets are independent");
            drop(run);
            promptly(lock.lock_execute(&addr("a"), &ct()))
                .await
                .expect("free after the run");
        }
    }

    /// What turns "holder unknown" into "in use by another command": the probe
    /// sees shared holders, which [`FLock::is_path_held`] cannot.
    #[tokio::test]
    async fn revision_in_use_sees_a_reader_and_only_that_revision() {
        let dir = tempfile::tempdir().expect("tempdir");
        for lock in both(&dir) {
            assert!(!lock.revision_in_use(&addr("a"), "h"), "never locked");
            let r = lock.read(&addr("a"), "h", &ct()).await.expect("read");
            assert!(lock.revision_in_use(&addr("a"), "h"));
            assert!(!lock.revision_in_use(&addr("a"), "other"));
            drop(r);
            assert!(!lock.revision_in_use(&addr("a"), "h"), "released");
        }
    }

    #[tokio::test]
    async fn fs_holder_pid_reports_stamped_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = fs(&dir);

        // No holder yet → unknown.
        assert_eq!(lock.holder_pid(&addr("a")), None);

        // While the gateway is held, it carries this process's pid.
        let held = lock
            .upgradable_read(&addr("a"), "h", &ct())
            .await
            .expect("acquire");
        assert_eq!(lock.holder_pid(&addr("a")), Some(std::process::id()));
        drop(held);
    }

    // The two live production stamp sites. `upgradable_read` above has no
    // production caller today, so without these the whole
    // `outer_guard()` → `stamp_pid` → `write_contents` chain would be asserted
    // only through a path nothing but a test walks — a mis-wired guard on either
    // live site would ship green.
    #[tokio::test]
    async fn fs_holder_pid_reports_the_pid_stamped_by_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = fs(&dir);

        assert_eq!(lock.holder_pid(&addr("a")), None, "no holder yet");

        let held = lock.write(&addr("a"), "h", &ct()).await.expect("write");
        assert_eq!(lock.holder_pid(&addr("a")), Some(std::process::id()));

        // Releasing the write unlinks the gateway file, so the holder is
        // unknown again rather than stale.
        drop(held);
        assert_eq!(lock.holder_pid(&addr("a")), None, "unknown after release");
    }

    #[tokio::test]
    async fn fs_holder_pid_reports_the_pid_stamped_by_try_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = fs(&dir);

        assert_eq!(lock.holder_pid(&addr("a")), None, "no holder yet");

        let held = lock
            .try_write(&addr("a"), "h")
            .expect("try_write ok")
            .expect("free addr acquires");
        assert_eq!(lock.holder_pid(&addr("a")), Some(std::process::id()));

        drop(held);
        assert_eq!(lock.holder_pid(&addr("a")), None, "unknown after release");
    }

    // `write_contents` writes positionally, so a reader in another process can
    // catch the gateway file mid-stamp. A killed holder leaves a 7-digit pid
    // behind (Linux `pid_max` defaults to 4194304); the next holder stamps a
    // 3-digit one over it, and this is what a third process sees before the
    // truncate lands. Unframed, `4214304` parses cleanly and the TUI names a pid
    // the user might kill.
    //
    // The gateway is genuinely held throughout, so the liveness probe passes and
    // the framing is the only thing that can decide the answer.
    #[tokio::test]
    async fn holder_pid_ignores_a_torn_stamp_rather_than_reporting_a_wrong_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = fs(&dir);
        let a = addr("a");

        let held = hold_raw_gateway(&dir, &a).await;
        std::fs::write(outer_lock_path(dir.path(), &a), b"421\n304\n").expect("torn stamp");

        assert_eq!(
            lock.holder_pid(&a),
            Some(421),
            "the framed pid, never the concatenation with the stale tail"
        );
        drop(held);
    }

    #[tokio::test]
    async fn holder_pid_is_unknown_for_an_unterminated_stamp() {
        // A write only half visible has no terminator. Naming `42` while the
        // holder is really `4211592` is worse than naming nobody.
        //
        // The planted pid is this process's own and the gateway is held, so
        // neither the liveness probe nor a dead pid can account for the `None` —
        // only the missing frame can.
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = fs(&dir);
        let a = addr("a");

        let held = hold_raw_gateway(&dir, &a).await;
        std::fs::write(
            outer_lock_path(dir.path(), &a),
            std::process::id().to_string(),
        )
        .expect("partial stamp");

        assert_eq!(
            lock.holder_pid(&a),
            None,
            "an unframed payload is not a pid"
        );
        drop(held);
    }

    #[test]
    fn holder_pid_is_unknown_when_the_stamp_outlives_its_holder() {
        // A holder killed mid-build leaves the gateway file behind: no `Drop`
        // runs, so nothing unlinks it, and nothing sweeps `<home>/lock/`. The
        // kernel does drop its `flock` at exit — so the lock is free while the
        // stamp is not, and without a liveness check that pid stays readable
        // forever.
        //
        // The stamp planted here is *this* process's pid: live, well-framed, and
        // exactly what a `kill(pid, 0)` check would happily report. Only probing
        // the lock itself gets this right.
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = fs(&dir);
        let a = addr("a");

        std::fs::write(
            outer_lock_path(dir.path(), &a),
            format!("{}\n", std::process::id()),
        )
        .expect("stamp from a holder that is gone");

        assert_eq!(
            lock.holder_pid(&a),
            None,
            "a stamp nobody holds names nobody"
        );
    }

    // The gateway's contents describe its *current* holder. `TBridge` acquires
    // the gateway, then parks on the inner lock for as long as a build takes,
    // and only then stamps — so whatever the previous holder left must be gone
    // at the *first* of those three, not the last.
    //
    // The seeded pid is this process's own, and the lock is held once we
    // acquire, so nothing but the blank itself can make it unreadable.
    #[tokio::test]
    async fn gateway_lock_blanks_a_stale_stamp_at_acquire() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("gateway.lock");
        std::fs::write(&path, format!("{}\n", std::process::id())).expect("stale stamp");

        let gw = GatewayLock::new(path.clone());
        let held = gw.lock(&ct()).await.expect("gateway");

        assert_eq!(
            std::fs::read(&path).expect("gateway readable"),
            b"",
            "acquiring the gateway must empty the stamp"
        );
        assert_eq!(read_pid(&path), None, "leaving no pid to report");
        drop(held);
    }

    // The wide window, end to end through the *shipped* `ResultLock` rather than
    // through `GatewayLock` directly.
    //
    // Two `ResultLock`s on one directory model two processes. A killed
    // predecessor's stamp is still in the gateway file; the builder takes the
    // gateway (blanking it) and then parks on the inner lock behind the
    // watcher's read guard — a wait bounded by a whole build. For that entire
    // wait, `holder_pid` used to name the predecessor.
    //
    // This is also the only test that pins `GatewayLock` into the bridge: the
    // two tests above construct one themselves, so reverting `FsBridge` to
    // `TBridge<FLock, FRWLock>` leaves them green. Here the planted pid is this
    // process's own — live, framed, and sitting under a gateway that genuinely
    // is held — so nothing but the blank can produce the `None`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn holder_pid_is_unknown_while_the_gateway_holder_waits_on_the_inner_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = addr("a");
        let watcher = fs(&dir);
        let builder = Arc::new(fs(&dir));

        std::fs::write(
            outer_lock_path(dir.path(), &a),
            format!("{}\n", std::process::id()),
        )
        .expect("stamp from a holder that is gone");

        // Another process still has the artifacts open, so the inner write
        // cannot be taken yet.
        let reading = watcher.read(&a, "h", &ct()).await.expect("plain read");

        let b = Arc::clone(&builder);
        let handle = tokio::spawn(async move {
            let tok = StdCancellationToken::new();
            b.write(&addr("a"), "h", &tok).await
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !handle.is_finished(),
            "the builder must still be parked on the inner lock"
        );
        assert_eq!(
            watcher.holder_pid(&a),
            None,
            "gateway held, but its new holder has not stamped yet"
        );

        drop(reading);
        let held = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("did not hang")
            .expect("join")
            .expect("acquires once the read drains");
        assert_eq!(
            builder.holder_pid(&a),
            Some(std::process::id()),
            "and the stamp lands once the acquire completes"
        );
        drop(held);
    }

    // `try_write` is a live production stamp site (the GC trim), and it reaches
    // the gateway through `try_lock`, not `lock`. Without this the blank could
    // be dropped from that arm and ship green.
    #[test]
    fn gateway_try_lock_blanks_a_stale_stamp_at_acquire() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("gateway.lock");
        std::fs::write(&path, format!("{}\n", std::process::id())).expect("stale stamp");

        let gw = GatewayLock::new(path.clone());
        let held = gw
            .try_lock()
            .expect("try_lock ok")
            .expect("free gateway acquires");

        assert_eq!(
            std::fs::read(&path).expect("gateway readable"),
            b"",
            "try_lock must empty the stamp too"
        );
        drop(held);
    }

    // Everything `read_pid` documents as `None`, in one place. The interesting
    // half is that a malformed payload must not become a pid: `holder_pid` feeds
    // a number the user may act on.
    #[test]
    fn read_pid_accepts_only_a_framed_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cases: &[(&[u8], Option<u32>, &str)] = &[
            (b"", None, "fresh gateway file, not yet stamped"),
            (b"\n", None, "frame around an empty pid"),
            (b"abc\n", None, "non-numeric"),
            (b"99999999999\n", None, "past u32"),
            (&[0xff, 0xfe, b'\n'], None, "not UTF-8"),
            (b"42", None, "unterminated: a half-visible write"),
            (b"  42\n", Some(42), "surrounding whitespace tolerated"),
            (
                b"421\n304\n",
                Some(421),
                "torn: new pid over a longer stale tail",
            ),
        ];

        for (i, (bytes, want, why)) in cases.iter().enumerate() {
            let path = dir.path().join(format!("case{i}.lock"));
            std::fs::write(&path, bytes).expect("case bytes");
            assert_eq!(read_pid(&path), *want, "{why}");
        }

        assert_eq!(read_pid(&dir.path().join("absent.lock")), None, "no file");
    }

    // The stamp must go through the gateway guard's open fd, never a second
    // `open` of the path. Proven by swapping a decoy inode in at the path while
    // the guard holds the original: a path-based stamp would land on the decoy.
    // Asserted in both directions — the pid reached the held inode, and the
    // decoy was untouched — so a `stamp_pid` that wrote nothing cannot pass.
    #[tokio::test]
    async fn stamp_pid_writes_through_the_held_fd_not_the_path() {
        use std::os::unix::fs::FileExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("gateway.lock");
        let gateway = FLock::new(&path);
        let held = gateway
            .lock(&StdCancellationToken::new())
            .await
            .expect("gateway");

        // A second handle on the held inode, opened before the unlink, so what
        // lands there stays readable once the path names a different file.
        let held_inode = std::fs::File::open(&path).expect("reopen held inode");
        std::fs::remove_file(&path).expect("unlink held lock file");
        std::fs::write(&path, b"decoy").expect("decoy");

        stamp_pid(Some(&held));

        let expected = format!("{}\n", std::process::id());
        let mut buf = vec![0u8; expected.len()];
        held_inode
            .read_exact_at(&mut buf, 0)
            .expect("stamp landed on the held inode");
        assert_eq!(
            buf,
            expected.as_bytes(),
            "the framed pid must land on the held inode"
        );
        assert_eq!(
            std::fs::read(&path).expect("decoy readable"),
            b"decoy",
            "stamp_pid must not re-open the lock path"
        );
        drop(held);
    }

    #[tokio::test]
    async fn mem_holder_pid_is_current_process() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = ResultLock::new(LockBackend::Mem, dir.path().to_path_buf());
        assert_eq!(lock.holder_pid(&addr("a")), Some(std::process::id()));
    }

    fn cancelled_ct() -> StdCancellationToken {
        let t = StdCancellationToken::new();
        t.cancel();
        t
    }
}
