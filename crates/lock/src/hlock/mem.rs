//! In-memory lock backend, built on tokio's owned-guard synchronization
//! primitives.
//!
//! Guards hold their own `Arc` clone (via tokio's `*_owned` APIs), so they are
//! `'static + Send` and borrow nothing from `&self` — required for the boxed
//! futures produced by `#[async_trait]`.

use crate::hlock::cancel::acquire_cancellable;
use crate::hlock::traits::{Ctoken, Lock, RWLock};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{
    Mutex, OwnedMutexGuard, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock as TokioRwLock,
};

/// In-memory exclusive lock.
#[derive(Clone, Default, Debug)]
pub struct MemLock {
    inner: Arc<Mutex<()>>,
}

impl MemLock {
    /// Create a new, unlocked in-memory lock.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Guard for [`MemLock`]; releases on drop.
#[derive(Debug)]
pub struct MemGuard {
    _g: OwnedMutexGuard<()>,
}

#[async_trait]
impl Lock for MemLock {
    type Guard = MemGuard;

    async fn lock(&self, ctoken: Ctoken<'_>) -> Result<MemGuard> {
        let inner = Arc::clone(&self.inner);
        acquire_cancellable(ctoken, "acquiring in-memory lock", async move {
            Ok(MemGuard {
                _g: inner.lock_owned().await,
            })
        })
        .await
    }

    fn try_lock(&self) -> Result<Option<MemGuard>> {
        Ok(Arc::clone(&self.inner)
            .try_lock_owned()
            .ok()
            .map(|g| MemGuard { _g: g }))
    }
}

/// In-memory reader/writer lock.
#[derive(Clone, Default, Debug)]
pub struct MemRWLock {
    inner: Arc<TokioRwLock<()>>,
}

impl MemRWLock {
    /// Create a new, unlocked in-memory reader/writer lock.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Shared read guard for [`MemRWLock`]; releases on drop.
#[derive(Debug)]
pub struct MemReadGuard {
    _g: OwnedRwLockReadGuard<()>,
}

/// Exclusive write guard for [`MemRWLock`]; releases on drop.
#[derive(Debug)]
pub struct MemWriteGuard {
    _g: OwnedRwLockWriteGuard<()>,
}

#[async_trait]
impl RWLock for MemRWLock {
    type ReadGuard = MemReadGuard;
    type WriteGuard = MemWriteGuard;

    async fn read(&self, ctoken: Ctoken<'_>) -> Result<MemReadGuard> {
        let inner = Arc::clone(&self.inner);
        acquire_cancellable(ctoken, "acquiring in-memory read lock", async move {
            Ok(MemReadGuard {
                _g: inner.read_owned().await,
            })
        })
        .await
    }

    async fn write(&self, ctoken: Ctoken<'_>) -> Result<MemWriteGuard> {
        let inner = Arc::clone(&self.inner);
        acquire_cancellable(ctoken, "acquiring in-memory write lock", async move {
            Ok(MemWriteGuard {
                _g: inner.write_owned().await,
            })
        })
        .await
    }

    fn try_read(&self) -> Result<Option<MemReadGuard>> {
        Ok(Arc::clone(&self.inner)
            .try_read_owned()
            .ok()
            .map(|g| MemReadGuard { _g: g }))
    }

    fn try_write(&self) -> Result<Option<MemWriteGuard>> {
        Ok(Arc::clone(&self.inner)
            .try_write_owned()
            .ok()
            .map(|g| MemWriteGuard { _g: g }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;

    fn ct() -> StdCancellationToken {
        StdCancellationToken::new()
    }

    #[tokio::test]
    async fn lock_is_mutually_exclusive() {
        let l = MemLock::new();
        let g = l.lock(&ct()).await.unwrap();
        assert!(
            l.try_lock().unwrap().is_none(),
            "held lock must not re-lock"
        );
        drop(g);
        assert!(
            l.try_lock().unwrap().is_some(),
            "released lock re-acquirable"
        );
    }

    #[tokio::test]
    async fn rwlock_allows_many_readers_excludes_writer() {
        let l = MemRWLock::new();
        let r1 = l.read(&ct()).await.unwrap();
        let r2 = l.read(&ct()).await.unwrap();
        assert!(l.try_write().unwrap().is_none(), "readers block writer");
        drop((r1, r2));
        assert!(
            l.try_write().unwrap().is_some(),
            "writer after readers drain"
        );
    }

    #[tokio::test]
    async fn rwlock_writer_excludes_readers() {
        let l = MemRWLock::new();
        let w = l.write(&ct()).await.unwrap();
        assert!(l.try_read().unwrap().is_none(), "writer blocks readers");
        drop(w);
        assert!(l.try_read().unwrap().is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_acquire_errors_and_does_not_leak() {
        let l = MemLock::new();
        let held = l.lock(&ct()).await.unwrap();

        let token = ct();
        token.cancel();
        let err = l.lock(&token).await.expect_err("must be cancelled");
        assert!(hplugin::error::is_cancelled(&err));

        // The cancelled acquire must not have taken the lock.
        drop(held);
        assert!(l.try_lock().unwrap().is_some(), "lock not leaked by cancel");
    }
}
