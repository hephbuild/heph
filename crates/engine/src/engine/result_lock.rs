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
//! Built from [`hlock::hlock`]'s keyed transformable locks, which lazily create
//! and self-evict a per-key lock instance. The default filesystem backend
//! serializes writers across *processes* via two `flock(2)` lock files per addr
//! under `<home>/lock/` (an outer "gateway" and an inner reader/writer file);
//! the in-memory backend serializes only within this process.

use anyhow::Result;
use async_trait::async_trait;
use hcore::hasync::Cancellable;
use hlock::hlock::{
    Ctoken, FLock, FRWLock, FWriteGuard, KeyedGuard, KeyedTLock, Lock, MemLock, MemRWLock, TBridge,
    TBridgeReadGuard, TBridgeUpgradableGuard, TBridgeWriteGuard, TUpgradableReadGuard, TWriteGuard,
    mem_tlock,
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

type FsBridge = TBridge<GatewayLock, FRWLock>;
type MemBridge = TBridge<MemLock, MemRWLock>;

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

type FsReadGuard = KeyedGuard<Addr, FsBridge, TBridgeReadGuard<FRWLock>>;
type MemReadGuard = KeyedGuard<Addr, MemBridge, TBridgeReadGuard<MemRWLock>>;
type FsUpgradableGuard = KeyedGuard<Addr, FsBridge, TBridgeUpgradableGuard<GatewayLock, FRWLock>>;
type MemUpgradableGuard = KeyedGuard<Addr, MemBridge, TBridgeUpgradableGuard<MemLock, MemRWLock>>;
type FsWriteGuard = KeyedGuard<Addr, FsBridge, TBridgeWriteGuard<GatewayLock, FRWLock>>;
type MemWriteGuard = KeyedGuard<Addr, MemBridge, TBridgeWriteGuard<MemLock, MemRWLock>>;

/// Plain shared read guard on a target's cache entry. Held for as long as the
/// artifacts are in use; the lock releases on drop. `Send + Sync` so it can ride
/// inside an `Arc<dyn Content>` shared across tasks.
#[derive(Debug)]
pub enum ResultReadGuard {
    Fs(FsReadGuard),
    Mem(MemReadGuard),
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
    Mem(MemWriteGuard),
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

/// Keyed transformable lock. Both backends are keyed by the target [`Addr`]; the
/// filesystem backend names its two lock files after the addr's content hash
/// (filesystem-safe), the in-memory backend keys async locks directly. The same
/// addr maps to the same lock across requests and — for the filesystem backend —
/// across processes.
pub enum ResultLock {
    Fs {
        /// Directory holding the per-key lock files. Kept so [`holder_pid`] can
        /// locate the gateway file independently of the keyed registry.
        ///
        /// [`holder_pid`]: ResultLock::holder_pid
        dir: PathBuf,
        lock: KeyedTLock<Addr, FsBridge>,
    },
    Mem(KeyedTLock<Addr, MemBridge>),
}

impl ResultLock {
    /// Build the configured backend. For [`LockBackend::Fs`], `dir` must already
    /// exist; per-key lock files are created lazily on first acquisition.
    pub fn new(backend: LockBackend, dir: PathBuf) -> Self {
        match backend {
            LockBackend::Fs => ResultLock::Fs {
                dir: dir.clone(),
                lock: KeyedTLock::new(move |addr: &Addr| {
                    TBridge::new(
                        GatewayLock::new(outer_lock_path(&dir, addr)),
                        FRWLock::new(inner_lock_path(&dir, addr)),
                    )
                }),
            },
            LockBackend::Mem => ResultLock::Mem(KeyedTLock::new(|_| mem_tlock())),
        }
    }

    /// Acquire a plain shared read guard for `addr`. Cheap and fully concurrent —
    /// the hot-path guard taken optimistically before a cache lookup and attached
    /// to the returned artifacts.
    pub async fn read(
        &self,
        addr: &Addr,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> Result<ResultReadGuard> {
        match self {
            ResultLock::Fs { lock, .. } => {
                Ok(ResultReadGuard::Fs(lock.read(addr.clone(), ctoken).await?))
            }
            ResultLock::Mem(kl) => Ok(ResultReadGuard::Mem(kl.read(addr.clone(), ctoken).await?)),
        }
    }

    /// Acquire the upgradable read guard for `addr` (the gateway), waiting until
    /// free or `ctoken` is cancelled. Taken on the cold path when a build may be
    /// needed. On the filesystem backend the holder stamps its pid into the
    /// gateway lock file so a *different* process blocked on the same addr can
    /// name the holder via [`holder_pid`](ResultLock::holder_pid). Best-effort:
    /// a write failure never fails the acquire.
    pub async fn upgradable_read(
        &self,
        addr: &Addr,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> Result<ResultUpgradableGuard> {
        match self {
            ResultLock::Fs { lock, .. } => {
                let guard = lock.upgradable_read(addr.clone(), ctoken).await?;
                stamp_pid(guard.outer_guard());
                Ok(ResultUpgradableGuard::Fs(guard))
            }
            ResultLock::Mem(kl) => Ok(ResultUpgradableGuard::Mem(
                kl.upgradable_read(addr.clone(), ctoken).await?,
            )),
        }
    }

    /// Acquire the exclusive write guard for `addr`. Used by the non-cacheable
    /// (force/shell) path that executes without a long-lived read lock. Stamps
    /// pid like [`upgradable_read`](ResultLock::upgradable_read).
    pub async fn write(
        &self,
        addr: &Addr,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> Result<ResultWriteGuard> {
        match self {
            ResultLock::Fs { lock, .. } => {
                let guard = lock.write(addr.clone(), ctoken).await?;
                stamp_pid(guard.outer_guard());
                Ok(ResultWriteGuard::Fs(guard))
            }
            ResultLock::Mem(kl) => Ok(ResultWriteGuard::Mem(kl.write(addr.clone(), ctoken).await?)),
        }
    }

    /// Non-blocking exclusive write acquire for `addr`. Returns `Ok(None)` when
    /// the addr is currently contended (any reader/writer holds it) instead of
    /// waiting. Used by the post-write GC trim, which must never block the hot
    /// path. Stamps pid on success like [`write`](ResultLock::write).
    pub fn try_write(&self, addr: &Addr) -> Result<Option<ResultWriteGuard>> {
        match self {
            ResultLock::Fs { lock, .. } => match lock.try_write(addr.clone())? {
                Some(guard) => {
                    stamp_pid(guard.outer_guard());
                    Ok(Some(ResultWriteGuard::Fs(guard)))
                }
                None => Ok(None),
            },
            ResultLock::Mem(kl) => Ok(kl.try_write(addr.clone())?.map(ResultWriteGuard::Mem)),
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

/// Path of the per-addr inner reader/writer lock file.
fn inner_lock_path(dir: &Path, addr: &Addr) -> PathBuf {
    dir.join(format!("{}.inner.lock", addr.hash_str()))
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

        let _r1 = lock.read(&addr("a"), &ct()).await.expect("r1");
        let _r2 = lock.read(&addr("a"), &ct()).await.expect("r2");
        // The optimistic gateway coexists with the plain readers.
        let _u = lock
            .upgradable_read(&addr("a"), &ct())
            .await
            .expect("upgradable");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_upgradable_blocks_until_first_drops() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = Arc::new(fs(&dir));

        let held = lock
            .upgradable_read(&addr("a"), &ct())
            .await
            .expect("first");

        let lock2 = Arc::clone(&lock);
        let handle = tokio::spawn(async move {
            let tok = StdCancellationToken::new();
            lock2.upgradable_read(&addr("a"), &tok).await.map(|_| ())
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
            .upgradable_read(&addr("a"), &ct())
            .await
            .expect("upgradable")
            .upgrade(&ct())
            .await
            .expect("upgrade");

        let lock2 = Arc::clone(&lock);
        let handle = tokio::spawn(async move {
            let tok = StdCancellationToken::new();
            lock2.read(&addr("a"), &tok).await.map(|_| ())
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

        let w = lock.write(&addr("a"), &ct()).await.expect("write");
        let up = w.downgrade(&ct()).await.expect("downgrade");
        let read = lock
            .read(&addr("a"), &ct())
            .await
            .expect("plain read coexists");
        drop(up);

        // A writer cannot proceed while the plain read is held...
        assert!(
            lock.write(&addr("a"), &cancelled_ct()).await.is_err(),
            "writer blocked while shared read held (cancelled wait)"
        );
        drop(read);
        // ...but succeeds once it drains.
        lock.write(&addr("a"), &ct())
            .await
            .expect("writer after read drains");
    }

    #[tokio::test]
    async fn try_write_none_while_held_then_some_after_release() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = fs(&dir);

        // Free → acquires.
        let g = lock
            .try_write(&addr("a"))
            .expect("try_write ok")
            .expect("free addr acquires");

        // Held → non-blocking, returns None rather than waiting.
        assert!(
            lock.try_write(&addr("a")).expect("try_write ok").is_none(),
            "must not acquire while a write guard is held"
        );

        drop(g);

        // Released → acquires again.
        assert!(
            lock.try_write(&addr("a")).expect("try_write ok").is_some(),
            "must acquire once the prior guard drops"
        );
    }

    #[tokio::test]
    async fn try_write_none_while_plain_read_held() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = fs(&dir);
        let _r = lock.read(&addr("a"), &ct()).await.expect("read");
        assert!(
            lock.try_write(&addr("a")).expect("try_write ok").is_none(),
            "writer must not acquire while a shared read is held"
        );
    }

    #[tokio::test]
    async fn distinct_addrs_independent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = fs(&dir);
        let _w = lock.write(&addr("a"), &ct()).await.expect("a");
        // A different addr is independent — it acquires without blocking on `a`.
        let _b = lock.write(&addr("b"), &ct()).await.expect("b");
    }

    #[tokio::test]
    async fn fs_holder_pid_reports_stamped_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = fs(&dir);

        // No holder yet → unknown.
        assert_eq!(lock.holder_pid(&addr("a")), None);

        // While the gateway is held, it carries this process's pid.
        let held = lock
            .upgradable_read(&addr("a"), &ct())
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

        let held = lock.write(&addr("a"), &ct()).await.expect("write");
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
            .try_write(&addr("a"))
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
        let reading = watcher.read(&a, &ct()).await.expect("plain read");

        let b = Arc::clone(&builder);
        let handle = tokio::spawn(async move {
            let tok = StdCancellationToken::new();
            b.write(&addr("a"), &tok).await
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
