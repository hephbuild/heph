//! Keyed lock primitives with in-memory and filesystem backends.
//!
//! Four capabilities — [`Lock`], [`RWLock`], [`TLock`], [`TRWLock`] — each with
//! an async blocking acquire returning `Result<Guard>` and a non-blocking
//! `try_*` returning `Result<Option<Guard>>`. Backends: [`mem`] (in-process,
//! tokio) and [`flock`] (cross-process, `flock(2)`). The [`TBridge`] composes a
//! plain outer [`Lock`] and inner [`RWLock`] into a transformable lock with
//! atomic upgrade/downgrade. The keyed wrappers map a key to a lazily-created,
//! self-cleaning lock instance.

pub mod bridge;
mod cancel;
pub mod flock;
pub mod keyed;
pub mod mem;
pub mod traits;

#[cfg(test)]
mod tests;

pub use bridge::{
    TBridge, TBridgeReadGuard, TBridgeUpgradableGuard, TBridgeWriteGuard, fs_tlock, mem_tlock,
};
pub use flock::{FLock, FRWLock, FReadGuard, FWriteGuard};
pub use keyed::{KeyedGuard, KeyedLock, KeyedRWLock, KeyedTLock, KeyedTRWLock};
pub use mem::{MemGuard, MemLock, MemRWLock, MemReadGuard, MemWriteGuard};
pub use traits::{
    Ctoken, Lock, RWLock, TLock, TRWLock, TReadGuard, TUpgradableReadGuard, TWriteGuard,
};
