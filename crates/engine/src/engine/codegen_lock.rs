//! The codegen index's cross-process locks (spec: codegen file index, J9,
//! Decision 10): one per target (`<home>/lock/codegen/<hash of addr>`), the
//! section lock and the import lock, all from
//! [`hwalk::codegen_index::CodegenLockPaths`].
//!
//! Each is an exclusive `flock` taken with `hlock`'s async acquire, which
//! polls with backoff on the async side under the request's cancel token: a
//! waiter holds no blocking-pool permit, and a cancelled wait returns before
//! anything is written. The guard is then handed, type-erased, to the
//! blocking job that runs under it ([`TargetLockGuard`], [`ImportLockGuard`]).
//! The directory is created on the first acquire, so a run that never takes
//! one (a warm run) creates nothing.

use crate::engine::result_lock::LockBackend;
use anyhow::Context as _;
use hlock::hlock::{FLock, KeyedLock, MemLock};
use hwalk::codegen_index::{CodegenLockPaths, ImportLockGuard, TargetLockGuard};
use std::any::Any;
use std::path::PathBuf;

/// The lock primitive, keyed by lock file path.
enum Keyed {
    /// `flock(2)` files under `<home>/lock/codegen/`.
    Fs(KeyedLock<PathBuf, FLock>),
    /// In-process only: tests, and anything that opted out of file locking.
    Mem(KeyedLock<PathBuf, MemLock>),
}

/// The codegen locks of one home.
pub struct CodegenLocks {
    paths: CodegenLockPaths,
    lock: Keyed,
}

impl std::fmt::Debug for CodegenLocks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodegenLocks")
            .field("dir", &self.paths.dir())
            .finish_non_exhaustive()
    }
}

impl CodegenLocks {
    pub fn new(backend: LockBackend, home: &std::path::Path) -> Self {
        let lock = match backend {
            LockBackend::Fs => Keyed::Fs(KeyedLock::new(|p: &PathBuf| FLock::new(p))),
            LockBackend::Mem => Keyed::Mem(KeyedLock::new(|_| MemLock::new())),
        };
        Self {
            paths: CodegenLockPaths::new(home),
            lock,
        }
    }

    pub fn paths(&self) -> &CodegenLockPaths {
        &self.paths
    }

    /// Wait for the lock file at `path`, cancellably, on the async side.
    async fn acquire(
        &self,
        path: PathBuf,
        ctoken: hlock::hlock::Ctoken<'_>,
    ) -> anyhow::Result<Box<dyn Any + Send>> {
        let what = path.display().to_string();
        Ok(match &self.lock {
            Keyed::Fs(l) => {
                std::fs::create_dir_all(self.paths.dir()).with_context(|| {
                    format!("create codegen lock dir {}", self.paths.dir().display())
                })?;
                Box::new(
                    l.lock(path, ctoken)
                        .await
                        .with_context(|| format!("acquire codegen lock {what}"))?,
                )
            }
            Keyed::Mem(l) => Box::new(
                l.lock(path, ctoken)
                    .await
                    .with_context(|| format!("acquire codegen lock {what}"))?,
            ),
        })
    }

    /// One target's lock: held for one write-back (steps 2–4) or one
    /// command's handling of its rows.
    pub async fn target(
        &self,
        addr: &str,
        ctoken: hlock::hlock::Ctoken<'_>,
    ) -> anyhow::Result<TargetLockGuard> {
        let guard = self.acquire(self.paths.target(addr), ctoken).await?;
        Ok(TargetLockGuard::new(addr, guard))
    }

    /// The import lock: one import or one db creation. Outermost: never
    /// awaited while a target lock is held.
    pub async fn import(
        &self,
        ctoken: hlock::hlock::Ctoken<'_>,
    ) -> anyhow::Result<ImportLockGuard> {
        let guard = self.acquire(self.paths.import(), ctoken).await?;
        Ok(ImportLockGuard::new(guard))
    }

    /// The section lock: one read, change and write of `.gitignore`.
    pub async fn section(
        &self,
        ctoken: hlock::hlock::Ctoken<'_>,
    ) -> anyhow::Result<Box<dyn Any + Send>> {
        self.acquire(self.paths.section(), ctoken).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use std::time::Duration;

    /// A write-back waiting on a held target lock holds no blocking-pool
    /// permit: with a pool of one thread, a blocking job still runs while it
    /// waits, and cancelling the wait ends it with an error.
    #[test]
    fn codegen_lock_wait_is_cancellable_and_holds_no_blocking_permit() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let home = tempfile::tempdir().unwrap();
            let locks = std::sync::Arc::new(CodegenLocks::new(LockBackend::Fs, home.path()));
            // Another process's write-back of the target: a second lock
            // registry on the same files.
            let other = CodegenLocks::new(LockBackend::Fs, home.path());
            let never = StdCancellationToken::new();
            let held = other.target("//p:gen", &never).await.unwrap();
            assert!(locks.paths().target("//p:gen").exists());

            let ct = StdCancellationToken::new();
            let waiter = tokio::spawn({
                let (locks, ct) = (std::sync::Arc::clone(&locks), ct.clone());
                async move { locks.target("//p:gen", &ct).await.map(drop) }
            });
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(!waiter.is_finished(), "the lock is held elsewhere");

            // The only blocking thread is free: a job runs to completion.
            let ran = tokio::time::timeout(Duration::from_secs(10), hcore::blocking::run(|| 7))
                .await
                .expect("a blocking job runs while a write-back waits on its lock");
            assert_eq!(ran, 7);

            ct.cancel();
            let res = tokio::time::timeout(Duration::from_secs(10), waiter)
                .await
                .expect("a cancelled wait returns")
                .unwrap();
            assert!(res.is_err(), "a cancelled wait is an error");
            assert!(ct.is_cancelled());
            drop(held);

            // Released: the next acquire succeeds at once.
            let again =
                tokio::time::timeout(Duration::from_secs(10), locks.target("//p:gen", &never))
                    .await
                    .expect("the released lock is acquired")
                    .unwrap();
            assert_eq!(again.addr(), "//p:gen");
        });
    }
}
