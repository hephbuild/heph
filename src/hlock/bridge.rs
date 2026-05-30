//! Two-lock bridge providing atomic upgrade/downgrade out of a non-transformable
//! outer [`Lock`] and inner [`RWLock`]. Port of `heph`'s `tmutex.go`.
//!
//! Read locks acquire only the inner lock. Write locks acquire the outer lock
//! first, then the inner write lock — so at most one writer ever holds the
//! inner lock, and a downgraded writer keeps the outer lock to bar other
//! writers while readers proceed.

use crate::hlock::flock::{FLock, FRWLock};
use crate::hlock::mem::{MemLock, MemRWLock};
use crate::hlock::traits::{Ctoken, Lock, RWLock, TLock, TRWLock, TReadGuard, TWriteGuard};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::Path;

/// A transformable lock composed of an outer exclusive lock and an inner
/// reader/writer lock.
#[derive(Clone, Debug)]
pub struct TBridge<O, I> {
    outer: O,
    inner: I,
}

impl<O, I> TBridge<O, I> {
    /// Build a bridge from an `outer` exclusive lock and an `inner`
    /// reader/writer lock. They must be distinct underlying locks (the write
    /// path holds both at once).
    pub fn new(outer: O, inner: I) -> Self {
        Self { outer, inner }
    }
}

/// In-memory transformable reader/writer lock.
pub fn mem_tlock() -> TBridge<MemLock, MemRWLock> {
    TBridge::new(MemLock::new(), MemRWLock::new())
}

/// Filesystem transformable reader/writer lock. `outer_path` and `inner_path`
/// must be different files.
pub fn fs_tlock(
    outer_path: impl AsRef<Path>,
    inner_path: impl AsRef<Path>,
) -> TBridge<FLock, FRWLock> {
    TBridge::new(FLock::new(outer_path), FRWLock::new(inner_path))
}

/// Read guard from a [`TBridge`]: holds only the inner read lock.
pub struct TBridgeReadGuard<O: Lock, I: RWLock> {
    outer: O,
    inner: I,
    inner_read: Option<I::ReadGuard>,
}

/// Write guard from a [`TBridge`]: holds the outer guard and the inner write
/// lock.
pub struct TBridgeWriteGuard<O: Lock, I: RWLock> {
    outer: O,
    inner: I,
    outer_guard: Option<O::Guard>,
    inner_write: Option<I::WriteGuard>,
}

impl<O: Lock, I: RWLock> Drop for TBridgeWriteGuard<O, I> {
    fn drop(&mut self) {
        // Release inner first, then outer (matches tmutex.go Unlock).
        self.inner_write.take();
        self.outer_guard.take();
    }
}

impl<O: Lock, I: RWLock> std::fmt::Debug for TBridgeReadGuard<O, I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TBridgeReadGuard").finish_non_exhaustive()
    }
}

impl<O: Lock, I: RWLock> std::fmt::Debug for TBridgeWriteGuard<O, I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TBridgeWriteGuard").finish_non_exhaustive()
    }
}

#[async_trait]
impl<O, I> TReadGuard for TBridgeReadGuard<O, I>
where
    O: Lock + Clone + Send + Sync + 'static,
    I: RWLock + Clone + Send + Sync + 'static,
{
    type WriteGuard = TBridgeWriteGuard<O, I>;

    async fn upgrade(mut self, ctoken: Ctoken<'_>) -> Result<Self::WriteGuard> {
        // Keep our inner read held until outer is acquired. On any error this
        // local drops, releasing the lock entirely.
        let inner_read = self.inner_read.take();
        let outer_guard = self
            .outer
            .lock(ctoken)
            .await
            .context("bridge upgrade: acquiring outer")?;
        drop(inner_read);
        let inner_write = self
            .inner
            .write(ctoken)
            .await
            .context("bridge upgrade: acquiring inner write")?;
        Ok(TBridgeWriteGuard {
            outer: self.outer.clone(),
            inner: self.inner.clone(),
            outer_guard: Some(outer_guard),
            inner_write: Some(inner_write),
        })
    }
}

#[async_trait]
impl<O, I> TWriteGuard for TBridgeWriteGuard<O, I>
where
    O: Lock + Clone + Send + Sync + 'static,
    I: RWLock + Clone + Send + Sync + 'static,
{
    type ReadGuard = TBridgeReadGuard<O, I>;

    async fn downgrade(mut self, ctoken: Ctoken<'_>) -> Result<Self::ReadGuard> {
        let inner_write = self.inner_write.take();
        let outer_guard = self.outer_guard.take();
        drop(inner_write);
        let inner_read = self
            .inner
            .read(ctoken)
            .await
            .context("bridge downgrade: acquiring inner read")?;
        // Release outer last: no other writer can slip in during the window.
        drop(outer_guard);
        Ok(TBridgeReadGuard {
            outer: self.outer.clone(),
            inner: self.inner.clone(),
            inner_read: Some(inner_read),
        })
    }
}

#[async_trait]
impl<O, I> TLock for TBridge<O, I>
where
    O: Lock + Clone + Send + Sync + 'static,
    I: RWLock + Clone + Send + Sync + 'static,
{
    type ReadGuard = TBridgeReadGuard<O, I>;
    type WriteGuard = TBridgeWriteGuard<O, I>;

    async fn read(&self, ctoken: Ctoken<'_>) -> Result<Self::ReadGuard> {
        let inner_read = self.inner.read(ctoken).await.context("bridge read")?;
        Ok(TBridgeReadGuard {
            outer: self.outer.clone(),
            inner: self.inner.clone(),
            inner_read: Some(inner_read),
        })
    }

    async fn write(&self, ctoken: Ctoken<'_>) -> Result<Self::WriteGuard> {
        let outer_guard = self
            .outer
            .lock(ctoken)
            .await
            .context("bridge write: acquiring outer")?;
        let inner_write = self
            .inner
            .write(ctoken)
            .await
            .context("bridge write: acquiring inner")?;
        Ok(TBridgeWriteGuard {
            outer: self.outer.clone(),
            inner: self.inner.clone(),
            outer_guard: Some(outer_guard),
            inner_write: Some(inner_write),
        })
    }

    fn try_read(&self) -> Result<Option<Self::ReadGuard>> {
        Ok(self.inner.try_read()?.map(|inner_read| TBridgeReadGuard {
            outer: self.outer.clone(),
            inner: self.inner.clone(),
            inner_read: Some(inner_read),
        }))
    }

    fn try_write(&self) -> Result<Option<Self::WriteGuard>> {
        let Some(outer_guard) = self.outer.try_lock()? else {
            return Ok(None);
        };
        match self.inner.try_write()? {
            Some(inner_write) => Ok(Some(TBridgeWriteGuard {
                outer: self.outer.clone(),
                inner: self.inner.clone(),
                outer_guard: Some(outer_guard),
                inner_write: Some(inner_write),
            })),
            None => {
                // Don't keep outer held while reporting failure.
                drop(outer_guard);
                Ok(None)
            }
        }
    }
}

impl<O, I> TRWLock for TBridge<O, I>
where
    O: Lock + Clone + Send + Sync + 'static,
    I: RWLock + Clone + Send + Sync + 'static,
{
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hasync::StdCancellationToken;
    use std::sync::Arc;
    use std::time::Duration;

    fn ct() -> StdCancellationToken {
        StdCancellationToken::new()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upgrade_blocks_until_readers_drain() {
        let l = Arc::new(mem_tlock());
        let reader = l.read(&ct()).await.unwrap();

        let l2 = Arc::clone(&l);
        let up = tokio::spawn(async move {
            let r = l2.read(&ct()).await.unwrap();
            r.upgrade(&ct()).await.unwrap()
        });

        // Cannot complete while another reader is alive.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!up.is_finished(), "upgrade must wait for readers");

        drop(reader);
        let w = tokio::time::timeout(Duration::from_secs(2), up)
            .await
            .expect("did not hang")
            .unwrap();
        // Now exclusive.
        assert!(
            l.try_read().unwrap().is_none(),
            "upgraded guard is exclusive"
        );
        drop(w);
        assert!(l.try_read().unwrap().is_some());
    }

    #[tokio::test]
    async fn downgrade_admits_readers_but_not_writers() {
        let l = mem_tlock();
        let w = l.write(&ct()).await.unwrap();
        let r = w.downgrade(&ct()).await.unwrap();

        assert!(
            l.try_read().unwrap().is_some(),
            "readers admitted after downgrade"
        );
        assert!(l.try_write().unwrap().is_none(), "writer still excluded");

        drop(r);
        assert!(l.try_write().unwrap().is_some(), "writer after read drains");
    }

    #[tokio::test]
    async fn writers_serialize() {
        let l = mem_tlock();
        let _w = l.write(&ct()).await.unwrap();
        assert!(l.try_write().unwrap().is_none(), "second writer blocked");
    }

    #[tokio::test]
    async fn failed_upgrade_releases_lock() {
        let l = mem_tlock();
        let r = l.read(&ct()).await.unwrap();

        let token = ct();
        token.cancel();
        let err = r.upgrade(&token).await.expect_err("cancelled upgrade");
        assert!(crate::commands::errors::is_cancelled(&err));

        // Lock fully released: a fresh writer succeeds.
        assert!(
            l.try_write().unwrap().is_some(),
            "lock released on failed upgrade"
        );
    }
}
