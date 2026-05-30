//! Two-lock bridge providing atomic upgrade/downgrade out of a non-transformable
//! outer [`Lock`] and inner [`RWLock`]. Port of `heph`'s `tmutex.go`.
//!
//! Plain read locks acquire only the inner lock. Upgradable reads and write
//! locks acquire the outer lock *first*, then the inner lock — so the outer
//! lock is the single gateway to the write lock, held continuously across every
//! read↔write transition. Because that gateway is never awaited while an inner
//! guard is held, upgrade/downgrade cannot deadlock against each other: an
//! upgrader already owns the gateway, so its `inner.write()` only waits on plain
//! readers draining, and a downgrader's `inner.read()` cannot block on any
//! writer (no other task can reach the write lock without the gateway it holds).

use crate::hlock::flock::{FLock, FRWLock};
use crate::hlock::mem::{MemLock, MemRWLock};
use crate::hlock::traits::{
    Ctoken, Lock, RWLock, TLock, TRWLock, TReadGuard, TUpgradableReadGuard, TWriteGuard,
};
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

/// Plain shared read guard from a [`TBridge`]: holds only the inner read lock.
/// Cannot be upgraded — acquire a [`TBridgeUpgradableGuard`] for that. The inner
/// guard is held purely for RAII; dropping this releases the read lock.
pub struct TBridgeReadGuard<I: RWLock> {
    _inner_read: I::ReadGuard,
}

/// Upgradable read guard from a [`TBridge`]: holds the outer guard (the sole
/// gateway to the inner write lock) plus an inner read lock. At most one is
/// outstanding, coexisting with any number of plain readers.
pub struct TBridgeUpgradableGuard<O: Lock, I: RWLock> {
    outer: O,
    inner: I,
    outer_guard: Option<O::Guard>,
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

impl<O: Lock, I: RWLock> Drop for TBridgeUpgradableGuard<O, I> {
    fn drop(&mut self) {
        // Release inner first, then outer (matches the write-guard order).
        self.inner_read.take();
        self.outer_guard.take();
    }
}

impl<O: Lock, I: RWLock> Drop for TBridgeWriteGuard<O, I> {
    fn drop(&mut self) {
        // Release inner first, then outer (matches tmutex.go Unlock).
        self.inner_write.take();
        self.outer_guard.take();
    }
}

impl<I: RWLock> std::fmt::Debug for TBridgeReadGuard<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TBridgeReadGuard").finish_non_exhaustive()
    }
}

impl<O: Lock, I: RWLock> std::fmt::Debug for TBridgeUpgradableGuard<O, I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TBridgeUpgradableGuard")
            .finish_non_exhaustive()
    }
}

impl<O: Lock, I: RWLock> std::fmt::Debug for TBridgeWriteGuard<O, I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TBridgeWriteGuard").finish_non_exhaustive()
    }
}

impl<I: RWLock> TReadGuard for TBridgeReadGuard<I> {}

#[async_trait]
impl<O, I> TUpgradableReadGuard for TBridgeUpgradableGuard<O, I>
where
    O: Lock + Clone + Send + Sync + 'static,
    I: RWLock + Clone + Send + Sync + 'static,
{
    type WriteGuard = TBridgeWriteGuard<O, I>;

    async fn upgrade(mut self, ctoken: Ctoken<'_>) -> Result<Self::WriteGuard> {
        // We already hold `outer` — the sole gateway to the inner write lock —
        // so we never wait on it here. Drop our inner read, then take the inner
        // write; it can only block on *plain* readers draining, never on
        // another would-be writer (they all queue on `outer`). No deadlock.
        let outer_guard = self.outer_guard.take();
        self.inner_read.take();
        let inner_write = self
            .inner
            .write(ctoken)
            .await
            .context("bridge upgrade: acquiring inner write")?;
        Ok(TBridgeWriteGuard {
            outer: self.outer.clone(),
            inner: self.inner.clone(),
            outer_guard,
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
    type UpgradableGuard = TBridgeUpgradableGuard<O, I>;

    async fn downgrade(mut self, ctoken: Ctoken<'_>) -> Result<Self::UpgradableGuard> {
        // Keep `outer` held throughout: no other writer can slip in, and the
        // inner read below cannot block (we own the only write-lock gateway, so
        // no writer can be holding the inner lock against us).
        let outer_guard = self.outer_guard.take();
        self.inner_write.take();
        let inner_read = self
            .inner
            .read(ctoken)
            .await
            .context("bridge downgrade: acquiring inner read")?;
        Ok(TBridgeUpgradableGuard {
            outer: self.outer.clone(),
            inner: self.inner.clone(),
            outer_guard,
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
    type ReadGuard = TBridgeReadGuard<I>;
    type UpgradableGuard = TBridgeUpgradableGuard<O, I>;
    type WriteGuard = TBridgeWriteGuard<O, I>;

    async fn read(&self, ctoken: Ctoken<'_>) -> Result<Self::ReadGuard> {
        let inner_read = self.inner.read(ctoken).await.context("bridge read")?;
        Ok(TBridgeReadGuard {
            _inner_read: inner_read,
        })
    }

    async fn upgradable_read(&self, ctoken: Ctoken<'_>) -> Result<Self::UpgradableGuard> {
        // Outer first (the gateway), then the inner read. On inner failure the
        // `outer_guard` local drops, releasing the gateway.
        let outer_guard = self
            .outer
            .lock(ctoken)
            .await
            .context("bridge upgradable_read: acquiring outer")?;
        let inner_read = self
            .inner
            .read(ctoken)
            .await
            .context("bridge upgradable_read: acquiring inner read")?;
        Ok(TBridgeUpgradableGuard {
            outer: self.outer.clone(),
            inner: self.inner.clone(),
            outer_guard: Some(outer_guard),
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
            _inner_read: inner_read,
        }))
    }

    fn try_upgradable_read(&self) -> Result<Option<Self::UpgradableGuard>> {
        let Some(outer_guard) = self.outer.try_lock()? else {
            return Ok(None);
        };
        match self.inner.try_read()? {
            Some(inner_read) => Ok(Some(TBridgeUpgradableGuard {
                outer: self.outer.clone(),
                inner: self.inner.clone(),
                outer_guard: Some(outer_guard),
                inner_read: Some(inner_read),
            })),
            None => {
                // Don't keep outer held while reporting failure.
                drop(outer_guard);
                Ok(None)
            }
        }
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
            let r = l2.upgradable_read(&ct()).await.unwrap();
            r.upgrade(&ct()).await.unwrap()
        });

        // Cannot complete while another plain reader is alive.
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
    async fn upgradable_read_coexists_with_plain_readers_but_is_unique() {
        let l = mem_tlock();
        let _u = l.upgradable_read(&ct()).await.unwrap();
        // Plain readers still admitted alongside the upgradable holder.
        assert!(
            l.try_read().unwrap().is_some(),
            "plain readers coexist with upgradable"
        );
        // But only one upgradable holder at a time (it owns the outer gateway).
        assert!(
            l.try_upgradable_read().unwrap().is_none(),
            "second upgradable blocked"
        );
    }

    #[tokio::test]
    async fn downgrade_admits_readers_but_not_writers() {
        let l = mem_tlock();
        let w = l.write(&ct()).await.unwrap();
        let u = w.downgrade(&ct()).await.unwrap();

        assert!(
            l.try_read().unwrap().is_some(),
            "readers admitted after downgrade"
        );
        assert!(l.try_write().unwrap().is_none(), "writer still excluded");
        // The downgraded guard still owns the gateway: no other upgradable.
        assert!(
            l.try_upgradable_read().unwrap().is_none(),
            "downgraded guard keeps the upgrade gateway"
        );

        drop(u);
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
        let u = l.upgradable_read(&ct()).await.unwrap();

        let token = ct();
        token.cancel();
        let err = u.upgrade(&token).await.expect_err("cancelled upgrade");
        assert!(crate::commands::errors::is_cancelled(&err));

        // Lock fully released: a fresh writer succeeds.
        assert!(
            l.try_write().unwrap().is_some(),
            "lock released on failed upgrade"
        );
    }

    // Two concurrent upgraders must serialize, never deadlock. Under the old
    // lazy-outer design each held an inner read while parked on outer, so the
    // outer holder could never drain readers — a circular wait. Holding the
    // gateway across the whole upgradable lifetime removes it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_concurrent_upgraders_no_deadlock() {
        let l = Arc::new(mem_tlock());

        let spawn_upgrader = || {
            let l = Arc::clone(&l);
            tokio::spawn(async move {
                let u = l.upgradable_read(&ct()).await.unwrap();
                let w = u.upgrade(&ct()).await.unwrap();
                // Hold the write briefly so the two genuinely contend.
                tokio::time::sleep(Duration::from_millis(20)).await;
                drop(w);
            })
        };

        let a = spawn_upgrader();
        let b = spawn_upgrader();

        tokio::time::timeout(Duration::from_secs(5), async {
            a.await.unwrap();
            b.await.unwrap();
        })
        .await
        .expect("two upgraders must not deadlock");

        assert!(
            l.try_write().unwrap().is_some(),
            "lock free after both done"
        );
    }

    // An upgrader and a downgrader hammering the same lock must not deadlock —
    // the old design's mirror-image circular wait (downgrader holds outer +
    // awaits inner read; upgrader holds inner read + awaits outer).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn upgrade_vs_downgrade_no_deadlock() {
        let l = Arc::new(mem_tlock());

        let l_up = Arc::clone(&l);
        let upgrader = tokio::spawn(async move {
            for _ in 0..50 {
                let u = l_up.upgradable_read(&ct()).await.unwrap();
                let w = u.upgrade(&ct()).await.unwrap();
                drop(w);
            }
        });

        let l_dn = Arc::clone(&l);
        let downgrader = tokio::spawn(async move {
            for _ in 0..50 {
                let w = l_dn.write(&ct()).await.unwrap();
                let u = w.downgrade(&ct()).await.unwrap();
                drop(u);
            }
        });

        tokio::time::timeout(Duration::from_secs(5), async {
            upgrader.await.unwrap();
            downgrader.await.unwrap();
        })
        .await
        .expect("upgrade/downgrade must not deadlock");
    }
}
