//! The codegen file index: which files a `copy` write-back put in the tree,
//! with their content hash (spec: "Codegen file index").
//!
//! The split: **the managed `.gitignore` section says which paths are
//! generated territory; the index says which bytes heph wrote there.**
//!
//! # Vocabulary
//!
//! The section (committed, the same for everyone on a commit):
//!
//! - **Section**: the block of the root `.gitignore` between heph's BEGIN and
//!   END markers, one **line** per `copy` output with the target that declares
//!   it in a comment above. [`section`] compiles it into a matcher.
//! - **Territory**: a path some section line covers, under git's rules: it
//!   matches a line, or sits under a directory that does, and no later `!`
//!   line re-includes it. Territory is where generated files are *expected*.
//!   It says nothing about whether the file on disk is the one heph wrote.
//!   Only the index can say that. A walk carries territory down: an entry
//!   under a territory directory is territory without a match call
//!   ([`SectionMatch::Inherited`]).
//! - **Foreign line**: a section line heph did not write. It still makes
//!   territory; `codegen status` lists it.
//!
//! The index (per checkout, never committed):
//!
//! - **Row**: one recorded file: path, the target that wrote it (`addr`), its
//!   content hash, a stat, and a [`RowState`]: `written`, `imported`,
//!   `dropped` (the target no longer produces it), or pending (a write-back
//!   is in flight or crashed).
//! - **Status**: what a reader makes of one path from its row, the file on
//!   disk and its territory ([`lookup`]): `Generated` and `Modified` are
//!   excluded from globs and `file` targets, while `Released` and `Source`
//!   are read as source.
//! - **Generation**: a counter in the db that every write transaction bumps.
//!   A reader that loaded "through generation `G`" has every commit up to `G`,
//!   so it can tell a commit of its own from a **foreign commit** (another
//!   process's), which forces a reload.
//!
//! The in-process view ([`snapshot`]):
//!
//! - **Snapshot**: what a reader holds for one glob or one lookup. It is
//!   immutable, so a concurrent publish never changes its answers.
//! - **Base**: the rows as of the last **reload** (a full read of the db).
//! - **Overlay**: rows this process **published** (wrote) since the base, each
//!   tagged with its generation, deletions as tombstones. A lookup takes
//!   whichever of base and overlay has the higher generation.
//! - **Fold** (of the overlay): once the overlay passes [`FOLD_THRESHOLD`]
//!   rows, it merges into a new base. Not to be confused with the **fold
//!   key** ([`fold`]): the case-folded, normalised form of a path that finds
//!   other casings of it on a case-insensitive filesystem.
//! - **Region**: one directory's rows, loaded on their own. A glob over `p/`
//!   needs only `p/`'s rows, so it reads those ([`CodegenIndex::snapshot_under`])
//!   instead of the whole index. A region is a [`snapshot::Base`] of that
//!   directory alone. After `MAX_REGIONS` (16) of them in one generation,
//!   the next load reads the whole index: a run globbing that many packages
//!   probably globs most of them.
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
//! - [`lookup`]: `Snapshot::status`, what one path is (`Generated`,
//!   `Released`, `Modified`, `Source`), read-only (J6).
//! - [`write`]: a `copy` write-back's protocol around the engine's
//!   byte-mover: step 1 without a lock, steps 2 and 4 under the target's
//!   codegen lock, and a command's settling of pending rows.
//! - [`import`]: `heph tool codegen import`'s scan of the older heph's xattrs,
//!   and its create-or-merge into the db.
//!
//! Locking: walk does not depend on `hlock` (it pulls in `plugin` and
//! `core`). The engine takes the flocks at [`CodegenLockPaths`] with `hlock`'s
//! async, cancellable acquire and hands the guard over type-erased in a
//! [`TargetLockGuard`] / [`ImportLockGuard`]; an index call that needs a lock
//! takes one of those, so the guard lives exactly as long as the job holding it.

pub mod commands;
pub mod fold;
pub mod fs;
pub mod import;
mod index;
pub mod lookup;
pub mod paths;
pub mod section;
pub mod snapshot;
#[cfg(test)]
mod startup_instrument;
pub mod store;
#[cfg(test)]
mod testfs;
pub mod write;

pub use fold::fold_hash;
pub use fs::{FileStat, IndexFs, RealFs, StatKind};
pub use index::{CodegenIndex, FOLD_THRESHOLD, IndexOptions, IndexStats, SECTION_RACY_SLACK};
pub use lookup::{LookupMeta, Status, StatusKind, StatusReason};
pub use paths::{CodegenLockPaths, codegen_dir, db_path, normalize_rel, rel_path};
pub use section::{Section, SectionMatch, SectionProbe};
pub use snapshot::{LookupEnv, Snapshot};
pub use store::{Committed, Record, Row, RowKind, RowState, Store, StoreOptions, WriteTxn};
pub use write::{
    EntryAction, Landing, LandingContent, PendingOutcome, PendingSettled, Plan, SettleReport,
    WriteBack,
};

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
