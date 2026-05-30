//! Lock trait surface for the `hklock` module.
//!
//! Four capabilities, each backed by an in-memory and a filesystem
//! implementation:
//!
//! - [`Lock`] — exclusive lock.
//! - [`RWLock`] — reader/writer lock (shared reads, exclusive write).
//! - [`TLock`] — transformable lock: a held guard can be atomically
//!   upgraded read→write or downgraded write→read.
//! - [`TRWLock`] — transformable reader/writer lock (marker super-trait of
//!   [`TLock`]; the inner lock permits concurrent readers).
//!
//! Acquire methods are `async` and take a [`Cancellable`] token so a blocking
//! wait can be interrupted. Non-blocking `try_*` methods take no token. All
//! fallible operations return [`anyhow::Result`].

use crate::hasync::Cancellable;
use anyhow::Result;
use async_trait::async_trait;

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

/// A transformable lock whose guards can switch between shared "read" and
/// exclusive "write" modes via the consuming transforms on [`TReadGuard`] /
/// [`TWriteGuard`].
#[async_trait]
pub trait TLock: Send + Sync {
    /// Read guard; can be upgraded to [`Self::WriteGuard`].
    type ReadGuard: TReadGuard<WriteGuard = Self::WriteGuard> + Send;
    /// Write guard; can be downgraded to [`Self::ReadGuard`].
    type WriteGuard: TWriteGuard<ReadGuard = Self::ReadGuard> + Send;

    /// Acquire a read guard, waiting until available or cancelled.
    async fn read(&self, ctoken: Ctoken<'_>) -> Result<Self::ReadGuard>;
    /// Acquire a write guard, waiting until available or cancelled.
    async fn write(&self, ctoken: Ctoken<'_>) -> Result<Self::WriteGuard>;

    /// Try to acquire a read guard without waiting.
    fn try_read(&self) -> Result<Option<Self::ReadGuard>>;
    /// Try to acquire a write guard without waiting.
    fn try_write(&self) -> Result<Option<Self::WriteGuard>>;
}

/// A transformable reader/writer lock. Identical surface to [`TLock`]; the
/// distinction is that read guards are *shared* (many concurrent readers).
pub trait TRWLock: TLock {}

/// A read guard that can be atomically upgraded to a write guard.
#[async_trait]
pub trait TReadGuard: Sized + Send {
    /// The write guard produced by [`Self::upgrade`].
    type WriteGuard: Send;

    /// Atomically upgrade read→write, consuming the read guard.
    ///
    /// Waits until all other readers drain. On error (including cancellation)
    /// the lock is released — the caller must re-acquire.
    async fn upgrade(self, ctoken: Ctoken<'_>) -> Result<Self::WriteGuard>;
}

/// A write guard that can be atomically downgraded to a read guard.
#[async_trait]
pub trait TWriteGuard: Sized + Send {
    /// The read guard produced by [`Self::downgrade`].
    type ReadGuard: Send;

    /// Atomically downgrade write→read, consuming the write guard. No other
    /// writer can acquire during the transition.
    async fn downgrade(self, ctoken: Ctoken<'_>) -> Result<Self::ReadGuard>;
}
