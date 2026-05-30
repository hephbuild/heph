//! Per-addr exclusive lock guarding the execute phase.
//!
//! At most one execute runs concurrently for a given target addr. The default
//! filesystem backend serializes across *processes* via `flock(2)` lock files
//! under `<home>/lock/`; the in-memory backend serializes only within this
//! process. Built from [`crate::hlock`]'s keyed locks, which lazily create and
//! self-evict a per-key lock instance.

use crate::hasync::Cancellable;
use crate::hlock::{FLock, KeyedLock, MemLock};
use crate::htaddr::Addr;
use anyhow::Result;
use std::io::Read as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Which lock backend guards the execute phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LockBackend {
    /// `flock(2)` lock files under `<home>/lock/`. Mutually exclusive across
    /// processes on the same machine. Default.
    #[default]
    Fs,
    /// In-process async mutexes. Single-process only.
    Mem,
}

/// Opaque guard; the execute lock is released when this drops. `Send` so it can
/// be held across the `await` points of the execute phase.
pub type ExecuteLockGuard = Box<dyn Send>;

/// Keyed execute lock. Both backends are keyed by the target [`Addr`]; each one
/// derives its own per-key lock instance from the addr — the filesystem backend
/// names a lock file after the addr's content hash (filesystem-safe), while the
/// in-memory backend keys an async mutex directly. The same addr maps to the
/// same lock across requests and — for the filesystem backend — across
/// processes.
pub enum ExecuteLock {
    Fs {
        /// Directory holding the per-key lock files. Kept so [`holder_pid`] can
        /// locate a lock file independently of the keyed registry.
        ///
        /// [`holder_pid`]: ExecuteLock::holder_pid
        dir: PathBuf,
        lock: KeyedLock<Addr, FLock>,
    },
    Mem(KeyedLock<Addr, MemLock>),
}

impl ExecuteLock {
    /// Build the configured backend. For [`LockBackend::Fs`], `dir` must already
    /// exist; per-key lock files are created lazily on first acquisition.
    pub fn new(backend: LockBackend, dir: PathBuf) -> Self {
        match backend {
            // Each key's lock file is named after the addr's content hash, so the
            // name is stable and filesystem-safe.
            LockBackend::Fs => ExecuteLock::Fs {
                dir: dir.clone(),
                lock: KeyedLock::new(move |addr: &Addr| FLock::new(lock_file_path(&dir, addr))),
            },
            LockBackend::Mem => ExecuteLock::Mem(KeyedLock::new(|_| MemLock::new())),
        }
    }

    /// Acquire the exclusive lock for `addr`, waiting until free or `ctoken` is
    /// cancelled. Hold the returned guard for the critical section; drop
    /// releases.
    ///
    /// On the filesystem backend, the holder records its pid in the lock file
    /// once acquired so a *different* process blocked on the same addr can name
    /// the holder via [`holder_pid`](ExecuteLock::holder_pid). Best-effort: a
    /// write failure never fails the acquire.
    pub async fn lock(
        &self,
        addr: &Addr,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> Result<ExecuteLockGuard> {
        match self {
            ExecuteLock::Fs { dir, lock } => {
                let guard = lock.lock(addr.clone(), ctoken).await?;
                write_pid(&lock_file_path(dir, addr));
                Ok(Box::new(guard))
            }
            ExecuteLock::Mem(kl) => Ok(Box::new(kl.lock(addr.clone(), ctoken).await?)),
        }
    }

    /// Best-effort pid of the process currently holding (or last to hold) the
    /// lock for `addr`. For the filesystem backend this reads the pid the holder
    /// stamped into the lock file; for the in-memory backend the holder is always
    /// this process. `None` when the holder is unknown (no lock file yet, or an
    /// unreadable/garbage file).
    pub fn holder_pid(&self, addr: &Addr) -> Option<u32> {
        match self {
            ExecuteLock::Fs { dir, .. } => read_pid(&lock_file_path(dir, addr)),
            ExecuteLock::Mem(_) => Some(std::process::id()),
        }
    }
}

/// Path of the per-addr lock file: `<dir>/<addr-hash>.lock`.
fn lock_file_path(dir: &Path, addr: &Addr) -> PathBuf {
    dir.join(format!("{}.lock", addr.hash_str()))
}

/// Stamp the current pid into the lock file. Best-effort — errors are swallowed
/// because failing to advertise the holder must never fail the lock itself.
/// Written through a fresh handle; the `flock(2)` held by the backend is
/// advisory and does not block this write.
fn write_pid(path: &Path) {
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .and_then(|mut f| f.write_all(std::process::id().to_string().as_bytes()));
}

/// Read a pid previously written by [`write_pid`]. `None` on any read/parse
/// failure (missing file, empty, non-numeric).
fn read_pid(path: &Path) -> Option<u32> {
    let mut s = String::new();
    std::fs::File::open(path)
        .ok()?
        .read_to_string(&mut s)
        .ok()?;
    s.trim().parse().ok()
}

impl std::fmt::Debug for ExecuteLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let backend = match self {
            ExecuteLock::Fs { .. } => "Fs",
            ExecuteLock::Mem(_) => "Mem",
        };
        f.debug_struct("ExecuteLock")
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fs_serializes_same_addr() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = Arc::new(ExecuteLock::new(LockBackend::Fs, dir.path().to_path_buf()));

        let held = lock.lock(&addr("a"), &ct()).await.expect("acquire");

        // A second acquire for the same addr must block while the first is held.
        let lock2 = Arc::clone(&lock);
        let handle = tokio::spawn(async move {
            let tok = StdCancellationToken::new();
            lock2.lock(&addr("a"), &tok).await.map(|_| ())
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !handle.is_finished(),
            "second acquire must block while same addr is held"
        );

        drop(held);
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("did not hang")
            .expect("join")
            .expect("acquires after release");
    }

    #[tokio::test]
    async fn mem_distinct_addrs_independent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = ExecuteLock::new(LockBackend::Mem, dir.path().to_path_buf());

        let _a = lock.lock(&addr("a"), &ct()).await.expect("a");
        // A different addr is independent — it acquires without blocking on `a`.
        let _b = lock.lock(&addr("b"), &ct()).await.expect("b");
    }

    #[tokio::test]
    async fn fs_holder_pid_reports_stamped_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = ExecuteLock::new(LockBackend::Fs, dir.path().to_path_buf());

        // No holder yet → unknown.
        assert_eq!(lock.holder_pid(&addr("a")), None);

        // While held, the lock file carries this process's pid.
        let held = lock.lock(&addr("a"), &ct()).await.expect("acquire");
        assert_eq!(lock.holder_pid(&addr("a")), Some(std::process::id()));

        drop(held);
    }

    #[tokio::test]
    async fn mem_holder_pid_is_current_process() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = ExecuteLock::new(LockBackend::Mem, dir.path().to_path_buf());
        assert_eq!(lock.holder_pid(&addr("a")), Some(std::process::id()));
    }
}
