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
//! Built from [`crate::hlock`]'s keyed transformable locks, which lazily create
//! and self-evict a per-key lock instance. The default filesystem backend
//! serializes writers across *processes* via two `flock(2)` lock files per addr
//! under `<home>/lock/` (an outer "gateway" and an inner reader/writer file);
//! the in-memory backend serializes only within this process.

use crate::hasync::Cancellable;
use crate::hlock::{
    FLock, FRWLock, KeyedGuard, KeyedTLock, MemLock, MemRWLock, TBridge, TBridgeReadGuard,
    TBridgeUpgradableGuard, TBridgeWriteGuard, TUpgradableReadGuard, TWriteGuard, fs_tlock,
    mem_tlock,
};
use crate::htaddr::Addr;
use anyhow::Result;
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

type FsBridge = TBridge<FLock, FRWLock>;
type MemBridge = TBridge<MemLock, MemRWLock>;

type FsReadGuard = KeyedGuard<Addr, FsBridge, TBridgeReadGuard<FRWLock>>;
type MemReadGuard = KeyedGuard<Addr, MemBridge, TBridgeReadGuard<MemRWLock>>;
type FsUpgradableGuard = KeyedGuard<Addr, FsBridge, TBridgeUpgradableGuard<FLock, FRWLock>>;
type MemUpgradableGuard = KeyedGuard<Addr, MemBridge, TBridgeUpgradableGuard<MemLock, MemRWLock>>;
type FsWriteGuard = KeyedGuard<Addr, FsBridge, TBridgeWriteGuard<FLock, FRWLock>>;
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
                    fs_tlock(outer_lock_path(&dir, addr), inner_lock_path(&dir, addr))
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
            ResultLock::Fs { dir, lock } => {
                let guard = lock.upgradable_read(addr.clone(), ctoken).await?;
                stamp_pid(&outer_lock_path(dir, addr));
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
            ResultLock::Fs { dir, lock } => {
                let guard = lock.write(addr.clone(), ctoken).await?;
                stamp_pid(&outer_lock_path(dir, addr));
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
            ResultLock::Fs { dir, lock } => match lock.try_write(addr.clone())? {
                Some(guard) => {
                    stamp_pid(&outer_lock_path(dir, addr));
                    Ok(Some(ResultWriteGuard::Fs(guard)))
                }
                None => Ok(None),
            },
            ResultLock::Mem(kl) => Ok(kl.try_write(addr.clone())?.map(ResultWriteGuard::Mem)),
        }
    }

    /// Best-effort pid of the process currently holding (or last to hold) the
    /// gateway for `addr`. For the filesystem backend this reads the pid the
    /// holder stamped into the gateway lock file; for the in-memory backend the
    /// holder is always this process. `None` when the holder is unknown.
    pub fn holder_pid(&self, addr: &Addr) -> Option<u32> {
        match self {
            ResultLock::Fs { dir, .. } => read_pid(&outer_lock_path(dir, addr)),
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
/// cross-process contention diagnostics. Only called while the gateway is held
/// (cold path), so it never burdens cache hits. A failure is logged, not fatal.
fn stamp_pid(path: &Path) {
    if let Err(err) = std::fs::write(path, std::process::id().to_string().as_bytes()) {
        tracing::debug!(error = %err, path = %path.display(), "stamping pid into lock file");
    }
}

/// Read a pid previously stamped by the lock holder. `None` on any read/parse
/// failure (missing file, empty, non-numeric).
fn read_pid(path: &Path) -> Option<u32> {
    let mut s = String::new();
    std::fs::File::open(path)
        .ok()?
        .read_to_string(&mut s)
        .ok()?;
    s.trim().parse().ok()
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
    use crate::hasync::StdCancellationToken;
    use crate::htpkg::PkgBuf;
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
