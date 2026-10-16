//! The cross-process lock a sqlite store's schema migration runs under.
//!
//! Shared by the two lazily-migrated stores, the engine's `cache.db` and the
//! walker's `fswalk.db`: each serializes its migration on an exclusive `flock`
//! of `<db>-migrate.lock` and re-reads the schema version under it.

use anyhow::Context as _;
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a migrator waits for another process's migration before failing.
///
/// Matches the stores' `busy_timeout`: a migration is a handful of statements,
/// so a lock held for longer than the busy handler would wait is held by a
/// process that is stuck (or stopped), not one that is about to finish — and an
/// untimed `flock` would hang this process behind it with no diagnostic.
pub const MIGRATE_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// The migration lock file of the database at `db_path`.
pub fn migrate_lock_path(db_path: &Path) -> PathBuf {
    let mut lock_path = db_path.as_os_str().to_owned();
    lock_path.push("-migrate.lock");
    PathBuf::from(lock_path)
}

/// Take the migration lock of the database at `db_path`, waiting at most
/// `timeout` for another holder. The lock is held until the returned file is
/// dropped. On timeout the error names the lock file, so a stuck holder can be
/// found (or the file removed).
pub fn lock_migration(db_path: &Path, timeout: Duration) -> anyhow::Result<File> {
    let lock_path = migrate_lock_path(db_path);
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening migration lock {}", lock_path.display()))?;
    let deadline = Instant::now() + timeout;
    let mut backoff = Duration::from_millis(5);
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => {
                return Err(e)
                    .with_context(|| format!("locking migration lock {}", lock_path.display()));
            }
        }
        let now = Instant::now();
        if now >= deadline {
            anyhow::bail!(
                "timed out after {timeout:?} waiting for migration lock {}: another process \
                 holds it (a stuck heph?)",
                lock_path.display()
            );
        }
        std::thread::sleep(backoff.min(deadline - now));
        backoff = (backoff * 2).min(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A held lock is waited for, bounded: the error names the lock file and
    /// arrives at the timeout, not never. Released, it is taken.
    #[test]
    fn a_held_lock_times_out_naming_the_file_then_is_taken_once_released() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("x.db");
        let held = lock_migration(&db, MIGRATE_LOCK_TIMEOUT).expect("first lock");

        let timeout = Duration::from_millis(200);
        let start = Instant::now();
        let err = lock_migration(&db, timeout).expect_err("must time out while held");
        let waited = start.elapsed();
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&migrate_lock_path(&db).display().to_string()),
            "{msg}"
        );
        assert!(waited >= timeout, "gave up early: {waited:?}");
        assert!(
            waited < Duration::from_secs(5),
            "waited far past: {waited:?}"
        );

        drop(held);
        lock_migration(&db, timeout).expect("taken once released");
    }
}
