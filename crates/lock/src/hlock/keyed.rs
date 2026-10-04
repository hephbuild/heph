//! Keyed locks: a registry mapping a key to a lazily-created lock instance.
//!
//! Entries clean themselves up with no background GC: each lock lives in an
//! `Arc<LockCell>` whose only strong references are the handles/guards in
//! flight. When the last one drops, the cell removes its own (now-dead) entry
//! from the registry. A live guard keeps its key resident.

use crate::hlock::traits::{
    Ctoken, Lock, RWLock, TLock, TReadGuard, TUpgradableReadGuard, TWriteGuard,
};
use anyhow::Result;
use async_trait::async_trait;
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use std::hash::Hash;
use std::ops::Deref;
use std::sync::{Arc, Weak};

type Cells<K, L> = Mutex<FxHashMap<K, Weak<LockCell<K, L>>>>;

struct LockCell<K: Eq + Hash, L> {
    key: K,
    lock: L,
    cells: Weak<Cells<K, L>>,
}

impl<K: Eq + Hash, L> Drop for LockCell<K, L> {
    fn drop(&mut self) {
        let Some(cells) = self.cells.upgrade() else {
            return;
        };
        let mut m = cells.lock();
        // Remove our entry only if it is still the (now-dead) weak pointing at
        // us — a concurrent re-insert under the same lock leaves a live entry
        // we must not clobber.
        if m.get(&self.key).is_some_and(|w| w.strong_count() == 0) {
            m.remove(&self.key);
        }
    }
}

/// Shared registry of keyed locks. Generic over the lock type `L`.
///
/// The factory receives the key, so backends that need it — notably the
/// filesystem [`FLock`](crate::hlock::FLock) — can derive a per-key lock file.
struct Registry<K: Eq + Hash, L> {
    cells: Arc<Cells<K, L>>,
    make: Arc<dyn Fn(&K) -> L + Send + Sync>,
}

impl<K, L> Registry<K, L>
where
    K: Eq + Hash + Clone,
{
    fn new(make: impl Fn(&K) -> L + Send + Sync + 'static) -> Self {
        Self {
            cells: Arc::new(Mutex::new(FxHashMap::default())),
            make: Arc::new(make),
        }
    }

    /// Get the (shared) lock cell for `key`, creating it if absent.
    fn cell(&self, key: K) -> Arc<LockCell<K, L>> {
        let mut m = self.cells.lock();
        if let Some(existing) = m.get(&key).and_then(Weak::upgrade) {
            return existing;
        }
        let lock = (self.make)(&key);
        let cell = Arc::new(LockCell {
            key: key.clone(),
            lock,
            cells: Arc::downgrade(&self.cells),
        });
        m.insert(key, Arc::downgrade(&cell));
        cell
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.cells.lock().len()
    }
}

/// A guard from a keyed lock. Holds the lock cell alive (so the key stays
/// resident while held) plus the underlying guard.
pub struct KeyedGuard<K: Eq + Hash, L, G> {
    _cell: Arc<LockCell<K, L>>,
    inner: G,
}

impl<K: Eq + Hash, L, G> KeyedGuard<K, L, G> {
    /// Borrow the underlying guard.
    pub fn get(&self) -> &G {
        &self.inner
    }
}

impl<K: Eq + Hash, L, G> Deref for KeyedGuard<K, L, G> {
    type Target = G;
    fn deref(&self) -> &G {
        &self.inner
    }
}

impl<K: Eq + Hash, L, G: std::fmt::Debug> std::fmt::Debug for KeyedGuard<K, L, G> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyedGuard")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

// Plain read guard: marker only (the cell keeps the key resident via `_cell`).
impl<K, L, G> TReadGuard for KeyedGuard<K, L, G>
where
    K: Eq + Hash + Send + Sync + 'static,
    L: Send + Sync + 'static,
    G: TReadGuard,
{
}

#[async_trait]
impl<K, L, G> TUpgradableReadGuard for KeyedGuard<K, L, G>
where
    K: Eq + Hash + Send + Sync + 'static,
    L: Send + Sync + 'static,
    G: TUpgradableReadGuard,
{
    type WriteGuard = KeyedGuard<K, L, G::WriteGuard>;

    async fn upgrade(self, ctoken: Ctoken<'_>) -> Result<Self::WriteGuard> {
        // On inner error the bridge releases the lock; `self._cell` drops here,
        // releasing the keyed cell ref (no leak).
        let inner = self.inner.upgrade(ctoken).await?;
        Ok(KeyedGuard {
            _cell: self._cell,
            inner,
        })
    }
}

#[async_trait]
impl<K, L, G> TWriteGuard for KeyedGuard<K, L, G>
where
    K: Eq + Hash + Send + Sync + 'static,
    L: Send + Sync + 'static,
    G: TWriteGuard,
{
    type UpgradableGuard = KeyedGuard<K, L, G::UpgradableGuard>;

    async fn downgrade(self, ctoken: Ctoken<'_>) -> Result<Self::UpgradableGuard> {
        let inner = self.inner.downgrade(ctoken).await?;
        Ok(KeyedGuard {
            _cell: self._cell,
            inner,
        })
    }
}

/// One key's lock, borrowed out of a keyed registry so it can be composed into
/// another lock (e.g. as one half of a [`TBridge`](crate::hlock::TBridge)) while
/// still being *the* registry's instance for that key.
///
/// Composition is why this exists. A bridge built over a fresh lock per call
/// would get a fresh in-process lock state per call: correct across processes
/// for `flock` (the kernel arbitrates between fds), but no exclusion at all for
/// the in-memory backend. Going through the registry gives every composer of
/// `key` the same instance.
///
/// A handle keeps its key resident while it lives, and so does every guard
/// acquired through it — guards are [`KeyedGuard`]s carrying the cell, so a
/// guard that outlives its handle still pins the instance it locked.
pub struct KeyedHandle<K: Eq + Hash, L> {
    cell: Arc<LockCell<K, L>>,
}

impl<K: Eq + Hash, L> Clone for KeyedHandle<K, L> {
    fn clone(&self) -> Self {
        Self {
            cell: Arc::clone(&self.cell),
        }
    }
}

impl<K: Eq + Hash, L> std::fmt::Debug for KeyedHandle<K, L> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyedHandle").finish_non_exhaustive()
    }
}

impl<K: Eq + Hash, L> KeyedHandle<K, L> {
    fn guard<G>(&self, inner: G) -> KeyedGuard<K, L, G> {
        KeyedGuard {
            _cell: Arc::clone(&self.cell),
            inner,
        }
    }
}

#[async_trait]
impl<K, L> Lock for KeyedHandle<K, L>
where
    K: Eq + Hash + Send + Sync + 'static,
    L: Lock + 'static,
{
    type Guard = KeyedGuard<K, L, L::Guard>;

    async fn lock(&self, ctoken: Ctoken<'_>) -> Result<Self::Guard> {
        Ok(self.guard(self.cell.lock.lock(ctoken).await?))
    }

    fn try_lock(&self) -> Result<Option<Self::Guard>> {
        Ok(self.cell.lock.try_lock()?.map(|g| self.guard(g)))
    }
}

#[async_trait]
impl<K, L> RWLock for KeyedHandle<K, L>
where
    K: Eq + Hash + Send + Sync + 'static,
    L: RWLock + 'static,
{
    type ReadGuard = KeyedGuard<K, L, L::ReadGuard>;
    type WriteGuard = KeyedGuard<K, L, L::WriteGuard>;

    async fn read(&self, ctoken: Ctoken<'_>) -> Result<Self::ReadGuard> {
        Ok(self.guard(self.cell.lock.read(ctoken).await?))
    }

    async fn write(&self, ctoken: Ctoken<'_>) -> Result<Self::WriteGuard> {
        Ok(self.guard(self.cell.lock.write(ctoken).await?))
    }

    fn try_read(&self) -> Result<Option<Self::ReadGuard>> {
        Ok(self.cell.lock.try_read()?.map(|g| self.guard(g)))
    }

    fn try_write(&self) -> Result<Option<Self::WriteGuard>> {
        Ok(self.cell.lock.try_write()?.map(|g| self.guard(g)))
    }
}

/// Keyed exclusive lock.
pub struct KeyedLock<K: Eq + Hash, L> {
    reg: Registry<K, L>,
}

impl<K, L> KeyedLock<K, L>
where
    K: Eq + Hash + Clone + Send + Sync,
    L: Lock,
{
    /// Create a keyed lock whose per-key locks are built by `make`.
    pub fn new(make: impl Fn(&K) -> L + Send + Sync + 'static) -> Self {
        Self {
            reg: Registry::new(make),
        }
    }

    /// Acquire the exclusive lock for `key`, waiting until free or cancelled.
    pub async fn lock(&self, key: K, ctoken: Ctoken<'_>) -> Result<KeyedGuard<K, L, L::Guard>> {
        let cell = self.reg.cell(key);
        let inner = cell.lock.lock(ctoken).await?;
        Ok(KeyedGuard { _cell: cell, inner })
    }

    /// This registry's lock for `key`, as a composable [`KeyedHandle`].
    pub fn handle(&self, key: K) -> KeyedHandle<K, L> {
        KeyedHandle {
            cell: self.reg.cell(key),
        }
    }

    /// Try to acquire the exclusive lock for `key` without waiting.
    pub fn try_lock(&self, key: K) -> Result<Option<KeyedGuard<K, L, L::Guard>>> {
        let cell = self.reg.cell(key);
        Ok(cell
            .lock
            .try_lock()?
            .map(|inner| KeyedGuard { _cell: cell, inner }))
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.reg.len()
    }
}

/// Keyed reader/writer lock.
pub struct KeyedRWLock<K: Eq + Hash, L> {
    reg: Registry<K, L>,
}

impl<K, L> KeyedRWLock<K, L>
where
    K: Eq + Hash + Clone + Send + Sync,
    L: RWLock,
{
    /// Create a keyed reader/writer lock built by `make`.
    pub fn new(make: impl Fn(&K) -> L + Send + Sync + 'static) -> Self {
        Self {
            reg: Registry::new(make),
        }
    }

    /// Acquire a shared read lock for `key`.
    pub async fn read(&self, key: K, ctoken: Ctoken<'_>) -> Result<KeyedGuard<K, L, L::ReadGuard>> {
        let cell = self.reg.cell(key);
        let inner = cell.lock.read(ctoken).await?;
        Ok(KeyedGuard { _cell: cell, inner })
    }

    /// Acquire the exclusive write lock for `key`.
    pub async fn write(
        &self,
        key: K,
        ctoken: Ctoken<'_>,
    ) -> Result<KeyedGuard<K, L, L::WriteGuard>> {
        let cell = self.reg.cell(key);
        let inner = cell.lock.write(ctoken).await?;
        Ok(KeyedGuard { _cell: cell, inner })
    }

    /// This registry's lock for `key`, as a composable [`KeyedHandle`].
    pub fn handle(&self, key: K) -> KeyedHandle<K, L> {
        KeyedHandle {
            cell: self.reg.cell(key),
        }
    }

    /// Try to acquire a shared read lock for `key`.
    pub fn try_read(&self, key: K) -> Result<Option<KeyedGuard<K, L, L::ReadGuard>>> {
        let cell = self.reg.cell(key);
        Ok(cell
            .lock
            .try_read()?
            .map(|inner| KeyedGuard { _cell: cell, inner }))
    }

    /// Try to acquire the exclusive write lock for `key`.
    pub fn try_write(&self, key: K) -> Result<Option<KeyedGuard<K, L, L::WriteGuard>>> {
        let cell = self.reg.cell(key);
        Ok(cell
            .lock
            .try_write()?
            .map(|inner| KeyedGuard { _cell: cell, inner }))
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.reg.len()
    }
}

/// Keyed transformable lock. Upgradable-read guards can be upgraded and write
/// guards downgraded via the [`TUpgradableReadGuard`]/[`TWriteGuard`] impls on
/// [`KeyedGuard`].
pub struct KeyedTLock<K: Eq + Hash, L> {
    reg: Registry<K, L>,
}

impl<K, L> KeyedTLock<K, L>
where
    K: Eq + Hash + Clone + Send + Sync,
    L: TLock,
{
    /// Create a keyed transformable lock built by `make`.
    pub fn new(make: impl Fn(&K) -> L + Send + Sync + 'static) -> Self {
        Self {
            reg: Registry::new(make),
        }
    }

    /// Acquire a plain shared read guard for `key`.
    pub async fn read(&self, key: K, ctoken: Ctoken<'_>) -> Result<KeyedGuard<K, L, L::ReadGuard>> {
        let cell = self.reg.cell(key);
        let inner = cell.lock.read(ctoken).await?;
        Ok(KeyedGuard { _cell: cell, inner })
    }

    /// Acquire an upgradable read guard for `key`.
    pub async fn upgradable_read(
        &self,
        key: K,
        ctoken: Ctoken<'_>,
    ) -> Result<KeyedGuard<K, L, L::UpgradableGuard>> {
        let cell = self.reg.cell(key);
        let inner = cell.lock.upgradable_read(ctoken).await?;
        Ok(KeyedGuard { _cell: cell, inner })
    }

    /// Acquire a write guard for `key`.
    pub async fn write(
        &self,
        key: K,
        ctoken: Ctoken<'_>,
    ) -> Result<KeyedGuard<K, L, L::WriteGuard>> {
        let cell = self.reg.cell(key);
        let inner = cell.lock.write(ctoken).await?;
        Ok(KeyedGuard { _cell: cell, inner })
    }

    /// Try to acquire a plain shared read guard for `key`.
    pub fn try_read(&self, key: K) -> Result<Option<KeyedGuard<K, L, L::ReadGuard>>> {
        let cell = self.reg.cell(key);
        Ok(cell
            .lock
            .try_read()?
            .map(|inner| KeyedGuard { _cell: cell, inner }))
    }

    /// Try to acquire an upgradable read guard for `key`.
    pub fn try_upgradable_read(
        &self,
        key: K,
    ) -> Result<Option<KeyedGuard<K, L, L::UpgradableGuard>>> {
        let cell = self.reg.cell(key);
        Ok(cell
            .lock
            .try_upgradable_read()?
            .map(|inner| KeyedGuard { _cell: cell, inner }))
    }

    /// Try to acquire a write guard for `key`.
    pub fn try_write(&self, key: K) -> Result<Option<KeyedGuard<K, L, L::WriteGuard>>> {
        let cell = self.reg.cell(key);
        Ok(cell
            .lock
            .try_write()?
            .map(|inner| KeyedGuard { _cell: cell, inner }))
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.reg.len()
    }
}

/// Keyed transformable reader/writer lock. Same surface as [`KeyedTLock`]; the
/// `L: TRWLock` bound documents that read guards are shared.
pub type KeyedTRWLock<K, L> = KeyedTLock<K, L>;

impl<K: Eq + Hash, L> std::fmt::Debug for KeyedLock<K, L> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyedLock").finish_non_exhaustive()
    }
}

impl<K: Eq + Hash, L> std::fmt::Debug for KeyedRWLock<K, L> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyedRWLock").finish_non_exhaustive()
    }
}

impl<K: Eq + Hash, L> std::fmt::Debug for KeyedTLock<K, L> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyedTLock").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hlock::bridge::{TBridge, mem_tlock};
    use crate::hlock::flock::FLock;
    use crate::hlock::mem::{MemLock, MemRWLock};
    use hcore::hasync::StdCancellationToken;

    fn ct() -> StdCancellationToken {
        StdCancellationToken::new()
    }

    #[tokio::test]
    async fn same_key_shares_one_lock() {
        let kl: KeyedLock<String, MemLock> = KeyedLock::new(|_| MemLock::new());
        let g = kl.lock("a".to_string(), &ct()).await.unwrap();
        assert!(
            kl.try_lock("a".to_string()).unwrap().is_none(),
            "same key blocks"
        );
        drop(g);
        assert!(kl.try_lock("a".to_string()).unwrap().is_some());
    }

    /// A handle is the registry's own instance, not a copy: a bridge composed
    /// over two handles of one key excludes a direct acquire of that key — the
    /// in-memory backend has nothing else to provide that exclusion — and a
    /// guard pins the instance after its handle is gone.
    #[tokio::test]
    async fn a_handle_is_the_registrys_instance() {
        let kl: KeyedLock<String, MemLock> = KeyedLock::new(|_| MemLock::new());
        let rw: KeyedRWLock<String, MemRWLock> = KeyedRWLock::new(|_| MemRWLock::new());

        let bridge = TBridge::new(kl.handle("a".to_string()), rw.handle("r".to_string()));
        let w = bridge.write(&ct()).await.unwrap();
        drop(bridge);
        assert!(
            kl.try_lock("a".to_string()).unwrap().is_none(),
            "gateway shared"
        );
        assert!(
            rw.try_read("r".to_string()).unwrap().is_none(),
            "inner shared"
        );

        drop(w);
        assert_eq!(kl.len(), 0, "the last guard out evicts the key");
        assert!(kl.try_lock("a".to_string()).unwrap().is_some());
    }

    #[tokio::test]
    async fn distinct_keys_are_independent() {
        let kl: KeyedLock<String, MemLock> = KeyedLock::new(|_| MemLock::new());
        let _g1 = kl.lock("a".to_string(), &ct()).await.unwrap();
        assert!(
            kl.try_lock("b".to_string()).unwrap().is_some(),
            "distinct key independent"
        );
    }

    #[tokio::test]
    async fn keyed_fs_lock_derives_path_per_key() {
        // The factory uses the key to build a distinct lock file per key.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let kl: KeyedLock<String, FLock> =
            KeyedLock::new(move |key: &String| FLock::new(root.join(format!("{key}.lock"))));

        let g = kl.lock("a".to_string(), &ct()).await.unwrap();
        // Same key contends; distinct key is independent (separate file).
        assert!(kl.try_lock("a".to_string()).unwrap().is_none());
        assert!(kl.try_lock("b".to_string()).unwrap().is_some());
        assert!(
            dir.path().join("a.lock").exists(),
            "per-key lock file created"
        );
        drop(g);
        assert!(kl.try_lock("a".to_string()).unwrap().is_some());
    }

    #[tokio::test]
    async fn entries_are_cleaned_up_when_idle() {
        let kl: KeyedLock<String, MemLock> = KeyedLock::new(|_| MemLock::new());
        {
            let _g = kl.lock("a".to_string(), &ct()).await.unwrap();
            assert_eq!(kl.len(), 1, "key resident while held");
        }
        assert_eq!(kl.len(), 0, "key evicted after release");
        // Re-acquiring recreates a fresh instance.
        let _g = kl.lock("a".to_string(), &ct()).await.unwrap();
        assert_eq!(kl.len(), 1);
    }

    #[tokio::test]
    async fn live_guard_keeps_key_resident() {
        let kl: KeyedRWLock<String, MemRWLock> = KeyedRWLock::new(|_| MemRWLock::new());
        let g = kl.read("a".to_string(), &ct()).await.unwrap();
        assert_eq!(kl.len(), 1);
        // Another reader joins the same underlying lock.
        let g2 = kl.read("a".to_string(), &ct()).await.unwrap();
        assert_eq!(kl.len(), 1, "still one entry");
        drop(g);
        assert_eq!(kl.len(), 1, "resident while other guard alive");
        drop(g2);
        assert_eq!(kl.len(), 0, "evicted once all guards drop");
    }

    #[tokio::test]
    async fn keyed_transformable_upgrade_carries_cell() {
        let kl: KeyedTLock<String, TBridge<MemLock, MemRWLock>> = KeyedTLock::new(|_| mem_tlock());
        let r = kl.upgradable_read("a".to_string(), &ct()).await.unwrap();
        assert_eq!(kl.len(), 1);
        let w = r.upgrade(&ct()).await.unwrap();
        assert_eq!(kl.len(), 1, "cell preserved across upgrade");
        assert!(
            kl.try_read("a".to_string()).unwrap().is_none(),
            "now exclusive"
        );
        drop(w);
        assert_eq!(kl.len(), 0);
    }
}
