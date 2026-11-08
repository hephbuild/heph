//! The codegen file index: which files a `copy` write-back put in the tree,
//! with their content hash (spec: "Codegen file index").
//!
//! The split: **the managed `.gitignore` section says which paths are
//! generated territory; the index says which bytes heph wrote there.**
//!
//! This module is engine-independent. What is here so far:
//!
//! - [`section`]: the managed section compiled into a git-semantics matcher,
//!   with point lookups and a walk-carry API, reporting which line matched.
//! - [`fold`]: the fold key for other casings / normalisations of a path.
//! - [`paths`]: normalised root-relative paths, this checkout's db file, and
//!   the `<home>/lock/codegen/` lock files.
//! - [`fs`]: the filesystem calls the index makes, injectable for tests.
//! - [`store`]: the sqlite db (schema, migrations, creation under the import
//!   lock, and the write transaction that bumps `generation`).
//! - [`snapshot`]: the immutable base + generation-tagged overlay a reader
//!   holds, and the publish / reload / fold rules over it.
//! - [`CodegenIndex`]: one checkout's index: the store if its db exists, the
//!   snapshot state behind a swap-only lock, the reload single-flight, and
//!   the compiled section keyed by the root `.gitignore`.
//!
//! Locking: walk does not depend on `hlock` (it pulls in `plugin` and
//! `core`). The engine takes the flocks at [`CodegenLockPaths`] with `hlock`'s
//! async, cancellable acquire and hands the guard over type-erased in a
//! [`TargetLockGuard`] / [`ImportLockGuard`]; an index call that needs a lock
//! takes one of those, so the guard lives exactly as long as the job holding it.

pub mod fold;
pub mod fs;
mod index;
pub mod paths;
pub mod section;
pub mod snapshot;
pub mod store;

pub use fold::fold_hash;
pub use fs::{FileStat, IndexFs, RealFs, StatKind};
pub use index::{CodegenIndex, FOLD_THRESHOLD, IndexOptions, IndexStats, SECTION_RACY_SLACK};
pub use paths::{CodegenLockPaths, codegen_dir, db_path, normalize_rel, rel_path};
pub use section::{Section, SectionMatch, SectionProbe};
pub use snapshot::Snapshot;
pub use store::{Committed, Record, Row, RowKind, RowState, Store, StoreOptions, WriteTxn};

use std::any::Any;

/// Proof, by construction, that the caller holds one target's codegen lock
/// (`CodegenLockPaths::target(addr)`): it wraps the `hlock` guard, which is
/// released when this is dropped. Moved into the blocking job that runs a
/// write-back's steps 2–4.
pub struct TargetLockGuard {
    addr: String,
    _guard: Box<dyn Any + Send>,
}

impl TargetLockGuard {
    /// `guard` must be the held lock on `CodegenLockPaths::target(addr)`.
    pub fn new(addr: impl Into<String>, guard: Box<dyn Any + Send>) -> Self {
        Self {
            addr: addr.into(),
            _guard: guard,
        }
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }
}

impl std::fmt::Debug for TargetLockGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TargetLockGuard")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

/// Proof that the caller holds the import lock (`CodegenLockPaths::import()`),
/// under which the db is created and the import runs.
pub struct ImportLockGuard {
    _guard: Box<dyn Any + Send>,
}

impl ImportLockGuard {
    /// `guard` must be the held lock on `CodegenLockPaths::import()`.
    pub fn new(guard: Box<dyn Any + Send>) -> Self {
        Self { _guard: guard }
    }
}

impl std::fmt::Debug for ImportLockGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportLockGuard").finish_non_exhaustive()
    }
}
