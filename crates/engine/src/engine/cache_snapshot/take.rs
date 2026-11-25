//! The `snapshot` command's protocol around the export (the CI cache
//! snapshots spec, "A job, end to end" step 3, and I5).
//!
//! [`Consumption::acquire`] takes, in order:
//!
//! 1. the exclusive snapshot lock, for the whole command, so two snapshots
//!    never overlap;
//! 2. the lock of every journal listed at that moment, waiting up to the
//!    given window for live recorders to release theirs. A recorder still
//!    alive after it is [`RecorderAliveError`]. A dead recorder's lock was
//!    released by the OS, so its journal is read at once, as incomplete.
//!    These are taken before any per-addr read lock the export takes.
//!
//! Under the snapshot lock, before listing, it removes the `.tmp` journals a
//! recorder left behind when it died before publishing one: unlocked, and
//! older than a recorder's create-to-lock window.
//!
//! It then reads those journals. [`Consumption::export`] exports the working
//! set, deletes exactly the journals it read once the pointer has moved, and
//! only then releases their locks. A journal created after the listing (a
//! recorder that started mid-snapshot) is never read, never locked and never
//! deleted.

use super::export::{ExportReport, SnapshotLock, try_lock_snapshot};
use super::{WorkingSet, list_journals, read_journal, working_set_dir};
use crate::engine::Engine;
use anyhow::Context as _;
use hcore::hasync::Cancellable;
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// How long `snapshot` waits for a live recorder to release its journal.
pub const RECORDER_WAIT: Duration = Duration::from_secs(10);

/// How often a held lock is retried while waiting.
const POLL: Duration = Duration::from_millis(25);

/// A recorder whose journal stayed locked for the whole wait. The `snapshot`
/// command exits 3 on it.
#[derive(Debug, thiserror::Error)]
#[error("{}", describe_alive(.recorders, *.waited))]
pub struct RecorderAliveError {
    /// Each journal still locked, sorted by path.
    pub recorders: Vec<LiveRecorder>,
    pub waited: Duration,
}

/// A journal whose recorder is still running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRecorder {
    pub journal: PathBuf,
    /// The recorder's pid, from the journal's `<pid>-<nonce>.log` name.
    pub pid: Option<u32>,
}

fn describe_alive(recorders: &[LiveRecorder], waited: Duration) -> String {
    let who = recorders
        .iter()
        .map(|r| match r.pid {
            Some(pid) => format!("pid {pid} (`ps -p {pid}`), journal {}", r.journal.display()),
            None => format!("journal {}", r.journal.display()),
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "a heph process is still recording its working set after {waited:?}: {who}. \
         Run the snapshot once every heph command of the job has exited."
    )
}

/// The command was cancelled. The `snapshot` command exits 130 on it.
#[derive(Debug, thiserror::Error)]
#[error("cache snapshot cancelled")]
pub struct SnapshotCancelled;

/// The pid a journal's `<pid>-<nonce>.log` name carries.
fn journal_pid(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    name.split_once('-')?.0.parse().ok()
}

/// A snapshot in progress: the snapshot lock, the locks of the journals it
/// read, and the working set they hold. Dropping it releases every lock and
/// deletes nothing.
#[derive(Debug)]
pub struct Consumption {
    lock: SnapshotLock,
    /// Held until the journals are deleted.
    journals: Vec<File>,
    ws: WorkingSet,
}

impl Consumption {
    /// Take the snapshot lock, then every listed journal's lock, waiting up
    /// to `wait` for live recorders, and read those journals.
    ///
    /// A journal from a newer heph is
    /// [`NewerJournalError`](super::NewerJournalError).
    pub async fn acquire(
        home: &Path,
        wait: Duration,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<Self> {
        let lock = wait_for_snapshot_lock(home, ctoken).await?;
        let listed = {
            let home = home.to_path_buf();
            hcore::blocking::run(move || {
                sweep_stale_tmp(&home);
                list_journals(&home)
            })
            .await?
        };
        let journals = lock_journals(listed, wait, ctoken).await?;
        let paths: Vec<PathBuf> = journals.iter().map(|(p, _)| p.clone()).collect();
        let contents = hcore::blocking::run(move || {
            paths
                .iter()
                .map(|p| read_journal(p))
                .collect::<anyhow::Result<Vec<_>>>()
        })
        .await?;
        Ok(Consumption {
            lock,
            journals: journals.into_iter().map(|(_, f)| f).collect(),
            ws: WorkingSet::from_journals(contents),
        })
    }

    /// The working set the journals hold.
    pub fn working_set(&self) -> &WorkingSet {
        &self.ws
    }

    /// Export the working set to `dir` through `engine`, then delete exactly
    /// the journals read, then release every lock. A failed export deletes
    /// nothing. A journal that will not delete once the snapshot is written
    /// is a [`ExportReport::cleanup_errors`] line, not a failure: the next
    /// snapshot reads it again, which makes that snapshot bigger, never
    /// wrong.
    pub async fn export(
        self,
        engine: &Engine,
        dir: &Path,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ExportReport> {
        let mut report = match engine
            .export_snapshot(&self.lock, dir, &self.ws, ctoken)
            .await
        {
            Ok(report) => report,
            Err(_) if ctoken.is_cancelled() => return Err(SnapshotCancelled.into()),
            Err(e) => return Err(e),
        };
        let journals = self.ws.journals.clone();
        let undeleted: Vec<(PathBuf, std::io::Error)> = hcore::blocking::run(move || {
            journals
                .into_iter()
                .filter_map(|journal| match std::fs::remove_file(&journal) {
                    Ok(()) => None,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => Some((journal, e)),
                })
                .collect()
        })
        .await;
        for (journal, e) in undeleted {
            tracing::warn!(
                journal = %journal.display(), error = %e,
                "cache snapshot: written, but a consumed journal could not be deleted",
            );
            report.cleanup_errors.push(format!(
                "deleting {}: {e}; the next snapshot reads it again (bigger, never wrong)",
                journal.display()
            ));
        }
        // The journal locks go only now that their files are gone; the
        // snapshot lock with them.
        drop(self.journals);
        drop(self.lock);
        Ok(report)
    }
}

/// Take the snapshot lock, waiting for a running snapshot to finish.
async fn wait_for_snapshot_lock(
    home: &Path,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<SnapshotLock> {
    loop {
        if ctoken.is_cancelled() {
            return Err(SnapshotCancelled.into());
        }
        let owned = home.to_path_buf();
        if let Some(lock) = hcore::blocking::run(move || try_lock_snapshot(&owned)).await? {
            return Ok(lock);
        }
        tokio::time::sleep(POLL).await;
    }
}

/// How old an unlocked `.tmp` journal must be before a snapshot removes it.
/// A recorder creates its temp file and only then locks it, so a young
/// unlocked one may be a live recorder between the two.
const STALE_TMP_AGE: Duration = Duration::from_secs(60);

/// Remove every `.<pid>-<nonce>.tmp` journal of `home` that no recorder holds
/// and that is older than [`STALE_TMP_AGE`]: one a recorder left behind when
/// it died before publishing it. Nobody reads those; this only frees the
/// space. Best effort: one that cannot be checked or removed stays.
fn sweep_stale_tmp(home: &Path) {
    let dir = working_set_dir(home);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let is_tmp = entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with('.') && n.ends_with(".tmp"));
        if !is_tmp {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age > STALE_TMP_AGE));
        if !old {
            continue;
        }
        let path = entry.path();
        // Locked here, it is ours until the remove: a recorder that held it
        // would have kept us out.
        let Ok(file) = File::open(&path) else {
            continue;
        };
        if file.try_lock().is_err() {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                tracing::debug!(path = %path.display(), "cache snapshot: removed a stale journal temp file")
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                path = %path.display(), error = %e,
                "cache snapshot: could not remove a stale journal temp file",
            ),
        }
    }
}

/// Lock every journal in `listed`, retrying a held lock until `wait` has
/// passed. A journal gone since the listing is skipped. Returns each locked
/// journal with its open file, sorted by path.
async fn lock_journals(
    listed: Vec<PathBuf>,
    wait: Duration,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<Vec<(PathBuf, File)>> {
    let deadline = Instant::now() + wait;
    let mut held: Vec<(PathBuf, File)> = Vec::with_capacity(listed.len());
    let mut pending: Vec<(PathBuf, File)> = hcore::blocking::run(move || {
        let mut opened = Vec::with_capacity(listed.len());
        for path in listed {
            match File::open(&path) {
                Ok(file) => opened.push((path, file)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
            }
        }
        Ok(opened)
    })
    .await?;
    loop {
        let mut still = Vec::new();
        for (path, file) in pending {
            match file.try_lock() {
                Ok(()) => held.push((path, file)),
                Err(TryLockError::WouldBlock) => still.push((path, file)),
                Err(TryLockError::Error(e)) => {
                    return Err(e).with_context(|| format!("locking {}", path.display()));
                }
            }
        }
        pending = still;
        if pending.is_empty() {
            break;
        }
        if ctoken.is_cancelled() {
            return Err(SnapshotCancelled.into());
        }
        if Instant::now() >= deadline {
            return Err(RecorderAliveError {
                recorders: pending
                    .into_iter()
                    .map(|(journal, _)| LiveRecorder {
                        pid: journal_pid(&journal),
                        journal,
                    })
                    .collect(),
                waited: wait,
            }
            .into());
        }
        tokio::time::sleep(POLL).await;
    }
    held.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(held)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_journal_name_carries_its_pid() {
        assert_eq!(journal_pid(Path::new("/w/123-abc.log")), Some(123));
        assert_eq!(journal_pid(Path::new("/w/x-abc.log")), None);
        assert_eq!(journal_pid(Path::new("/w/abc.log")), None);
    }

    /// Only an old, unlocked `.tmp` goes: a young one may be a recorder
    /// between creating and locking it, a locked one is a live recorder, and
    /// a published `.log` is never this sweep's.
    #[test]
    fn a_stale_unlocked_tmp_journal_is_removed() {
        let home = tempfile::tempdir().expect("home");
        let dir = working_set_dir(home.path());
        std::fs::create_dir_all(&dir).expect("mkdir");
        let old = SystemTime::now() - 2 * STALE_TMP_AGE;
        let make = |name: &str, mtime: SystemTime| {
            let path = dir.join(name);
            let f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .expect("create");
            f.set_modified(mtime).expect("set mtime");
            (path, f)
        };
        let (stale, _) = make(".1-stale.tmp", old);
        let (young, _) = make(".2-young.tmp", SystemTime::now());
        let (held, held_file) = make(".3-held.tmp", old);
        held_file.try_lock().expect("a recorder holds it");
        let (log, _) = make("4-old.log", old);

        sweep_stale_tmp(home.path());
        assert!(!stale.exists());
        for kept in [&young, &held, &log] {
            assert!(kept.exists(), "{}", kept.display());
        }
        drop(held_file);
    }
}
