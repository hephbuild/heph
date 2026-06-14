//! Lock trait surface for the `hklock` module.
//!
//! Four capabilities, each backed by an in-memory and a filesystem
//! implementation:
//!
//! - [`Lock`] — exclusive lock.
//! - [`RWLock`] — reader/writer lock (shared reads, exclusive write).
//! - [`TLock`] — transformable lock: an *upgradable* read guard can be
//!   atomically upgraded read→write, and a write guard downgraded back to an
//!   upgradable read guard.
//! - [`TRWLock`] — transformable reader/writer lock (marker super-trait of
//!   [`TLock`]; the inner lock permits concurrent readers).
//!
//! Acquire methods are `async` and take a [`Cancellable`] token so a blocking
//! wait can be interrupted. Non-blocking `try_*` methods take no token. All
//! fallible operations return [`anyhow::Result`].

use anyhow::Result;
use async_trait::async_trait;
use hcore::hasync::Cancellable;

/// Convenience alias for the cancellation token accepted by every blocking
/// acquire.
pub type Ctoken<'a> = &'a (dyn Cancellable + Send + Sync);

/// An exclusive lock.
#[async_trait]
pub trait Lock: Send + Sync {
    /// Guard released on drop.
    type Guard: Send;

    /// Acquire the lock, waiting until it is free or `ctoken` is cancelled.
    async fn lock(&self, ctoken: Ctoken<'_>) -> Result<Self::Guard>;

    /// Try to acquire without waiting. Returns `Ok(None)` if already held.
    fn try_lock(&self) -> Result<Option<Self::Guard>>;
}

/// A reader/writer lock: many concurrent readers or a single writer.
#[async_trait]
pub trait RWLock: Send + Sync {
    /// Shared read guard.
    type ReadGuard: Send;
    /// Exclusive write guard.
    type WriteGuard: Send;

    /// Acquire a shared read lock, waiting until available or cancelled.
    async fn read(&self, ctoken: Ctoken<'_>) -> Result<Self::ReadGuard>;
    /// Acquire the exclusive write lock, waiting until available or cancelled.
    async fn write(&self, ctoken: Ctoken<'_>) -> Result<Self::WriteGuard>;

    /// Try to acquire a shared read lock without waiting.
    fn try_read(&self) -> Result<Option<Self::ReadGuard>>;
    /// Try to acquire the exclusive write lock without waiting.
    fn try_write(&self) -> Result<Option<Self::WriteGuard>>;
}

/// A transformable lock whose guards can switch between an upgradable "read"
/// mode and an exclusive "write" mode via the consuming transforms on
/// [`TUpgradableReadGuard`] / [`TWriteGuard`].
///
/// Three guard flavours: a *plain* read guard ([`Self::ReadGuard`], shared, no
/// transform), an *upgradable* read guard ([`Self::UpgradableGuard`], at most
/// one outstanding, transformable to write), and a write guard
/// ([`Self::WriteGuard`], exclusive, downgradable back to upgradable). Only the
/// upgradable guard can become a writer; this is what makes upgrade/downgrade
/// deadlock-free — the gateway to the write lock is held continuously across
/// every transition, never acquired while another inner guard is in hand.
#[async_trait]
pub trait TLock: Send + Sync {
    /// Plain shared read guard. Cannot be upgraded; acquire an
    /// [`Self::UpgradableGuard`] instead when an upgrade may be needed.
    type ReadGuard: TReadGuard + Send;
    /// Upgradable read guard; can be upgraded to [`Self::WriteGuard`]. At most
    /// one is outstanding at a time (it coexists with plain readers).
    type UpgradableGuard: TUpgradableReadGuard<WriteGuard = Self::WriteGuard> + Send;
    /// Write guard; can be downgraded to [`Self::UpgradableGuard`].
    type WriteGuard: TWriteGuard<UpgradableGuard = Self::UpgradableGuard> + Send;

    /// Acquire a plain shared read guard, waiting until available or cancelled.
    async fn read(&self, ctoken: Ctoken<'_>) -> Result<Self::ReadGuard>;
    /// Acquire an upgradable read guard, waiting until available or cancelled.
    async fn upgradable_read(&self, ctoken: Ctoken<'_>) -> Result<Self::UpgradableGuard>;
    /// Acquire a write guard, waiting until available or cancelled.
    async fn write(&self, ctoken: Ctoken<'_>) -> Result<Self::WriteGuard>;

    /// Try to acquire a plain shared read guard without waiting.
    fn try_read(&self) -> Result<Option<Self::ReadGuard>>;
    /// Try to acquire an upgradable read guard without waiting.
    fn try_upgradable_read(&self) -> Result<Option<Self::UpgradableGuard>>;
    /// Try to acquire a write guard without waiting.
    fn try_write(&self) -> Result<Option<Self::WriteGuard>>;
}

/// A transformable reader/writer lock. Identical surface to [`TLock`]; the
/// distinction is that read guards are *shared* (many concurrent readers).
pub trait TRWLock: TLock {}

/// A plain shared read guard. Marker only — it carries no transform; to upgrade,
/// acquire a [`TUpgradableReadGuard`] up front.
pub trait TReadGuard: Sized + Send {}

/// An upgradable read guard that can be atomically upgraded to a write guard.
#[async_trait]
pub trait TUpgradableReadGuard: Sized + Send {
    /// The write guard produced by [`Self::upgrade`].
    type WriteGuard: Send;

    /// Atomically upgrade read→write, consuming the upgradable guard.
    ///
    /// Waits until all other readers drain — but never blocks on the upgrade
    /// gateway, which this guard already holds, so it cannot deadlock against a
    /// concurrent upgrade/downgrade. On error (including cancellation) the lock
    /// is released — the caller must re-acquire.
    async fn upgrade(self, ctoken: Ctoken<'_>) -> Result<Self::WriteGuard>;
}

/// A write guard that can be atomically downgraded to an upgradable read guard.
#[async_trait]
pub trait TWriteGuard: Sized + Send {
    /// The upgradable read guard produced by [`Self::downgrade`].
    type UpgradableGuard: Send;

    /// Atomically downgrade write→upgradable-read, consuming the write guard. No
    /// other writer can acquire during the transition.
    async fn downgrade(self, ctoken: Ctoken<'_>) -> Result<Self::UpgradableGuard>;
}
