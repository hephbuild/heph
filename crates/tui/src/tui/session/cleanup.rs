//! Removing ended sessions.
//!
//! Runs only at session start, on the recorder thread (design amendment A7):
//! the owner of a new session is the only writer of the sessions directory, and
//! a viewer stays strictly read-only.
//!
//! The decision reads the lock and modification times, **never** `meta.json`:
//! a newer heph may have changed `meta.json`, but the `lock` file name and its
//! `flock` meaning are frozen for every format, so a session of a format this
//! heph cannot read is still cleaned correctly — and one whose lock is held is
//! a live session of unknown format, never removed.
//!
//! An ended session (lock free) is kept while it was last written less than
//! [`Retention::max_age`] ago and is among the [`Retention::max_ended`] most
//! recent ended sessions. Then the byte budget (design amendment A10): the
//! sessions kept so far — every ended one, the newest included; never a live
//! one — must total at most [`Retention::max_bytes`], and the oldest go until
//! they do. A directory with no `lock` file written less than `max_age` ago is
//! a session being created (`mkdir`, `meta.json`, then the lock): neither
//! counted nor removed.
//!
//! The lock state is read with a shared probe, released at once; a directory
//! removed is then locked exclusively by this process, so two cleanups racing
//! each other never both act on one directory, and a session cannot start
//! inside a directory being removed (ids are never reused).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::format::files;
use super::id::SessionId;
use super::lockfile;

/// How long and how many ended sessions are kept.
#[derive(Debug, Clone, Copy)]
pub struct Retention {
    pub max_age: Duration,
    pub max_ended: usize,
    pub max_bytes: u64,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            max_age: Duration::from_secs(10 * 60),
            max_ended: 20,
            max_bytes: 256 * 1024 * 1024,
        }
    }
}

/// What a cleanup did, for tests and a `debug!`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CleanupReport {
    pub removed: Vec<SessionId>,
    /// Directories skipped because their lock is held (or another cleanup has
    /// it).
    pub live: usize,
}

/// Remove the ended sessions `retention` does not keep, under `root`, as of
/// `now`. Per-directory failures are skipped (a vanished directory is another
/// cleanup's work); only an unreadable `root` is an error.
pub fn cleanup(
    root: &Path,
    now: SystemTime,
    retention: Retention,
) -> std::io::Result<CleanupReport> {
    let mut sessions: Vec<(SessionId, PathBuf)> = std::fs::read_dir(root)?
        .filter_map(Result::ok)
        .filter_map(|e| {
            let id = SessionId::parse(e.file_name().to_str()?).ok()?;
            Some((id, e.path()))
        })
        .collect();
    // Newest first: an id sorts by start time.
    sessions.sort_by_key(|s| std::cmp::Reverse(s.0));

    let mut report = CleanupReport::default();

    // Classify, newest first, from the lock and metadata alone. Every directory
    // is probed: the byte budget counts the newest ended sessions too, and only
    // the probe tells an ended one from a live one.
    let mut ended: Vec<Ended> = Vec::new();
    let mut kept = 0usize;
    for (id, dir) in sessions {
        let lock_path = dir.join(files::LOCK);
        let has_lock = lock_path.exists();
        if has_lock {
            match lockfile::is_held(&lock_path) {
                Ok(false) => {}
                Ok(true) => {
                    report.live += 1;
                    continue;
                }
                Err(e) => {
                    tracing::debug!(dir = %dir.display(), error = %e, "skipping session in cleanup");
                    continue;
                }
            }
        }
        let (last_write, bytes) = last_write_and_size(&dir);
        let age = now.duration_since(last_write).unwrap_or(Duration::ZERO);
        // No lock file yet and freshly written: a session being created right
        // now. Older than that, it was killed before taking its lock: ended.
        if !has_lock && age < retention.max_age {
            continue;
        }
        let keep = age < retention.max_age && kept < retention.max_ended;
        kept += usize::from(keep);
        ended.push(Ended {
            id,
            dir,
            bytes,
            keep,
        });
    }

    // The byte budget over every ended session still kept, oldest dropped first.
    let mut total: u64 = ended
        .iter()
        .filter(|e| e.keep)
        .fold(0, |sum, e| sum.saturating_add(e.bytes));
    for e in ended.iter_mut().rev() {
        if total <= retention.max_bytes {
            break;
        }
        if e.keep {
            e.keep = false;
            total = total.saturating_sub(e.bytes);
        }
    }

    for e in ended.iter().filter(|e| !e.keep) {
        if remove(root, e) {
            report.removed.push(e.id);
        }
    }
    Ok(report)
}

/// An ended session, as classified, and whether retention keeps it.
#[derive(Debug)]
struct Ended {
    id: SessionId,
    dir: PathBuf,
    bytes: u64,
    keep: bool,
}

/// Remove one ended session under its own lock. `true` when this call removed
/// it.
fn remove(root: &Path, e: &Ended) -> bool {
    let lock_path = e.dir.join(files::LOCK);
    let held = match lock_exclusive(&lock_path) {
        Ok(Some(held)) => Some(held),
        // No lock file: killed between `mkdir` and taking the lock (or another
        // cleanup already renamed it away, which the rename below finds).
        Ok(None) if !lock_path.exists() => None,
        // Taken since the probe: another cleanup is removing it.
        Ok(None) => return false,
        Err(e) => {
            tracing::debug!(dir = %lock_path.display(), error = %e, "skipping session in cleanup");
            return false;
        }
    };
    // Rename first: exactly one cleanup's rename succeeds, so a directory is
    // removed once even when another cleanup reached it after its lock file
    // was already unlinked by the first. The name is not an id, so no cleanup
    // or viewer reads it as a session.
    let tomb = root.join(format!(".removing-{}-{}", e.id, std::process::id()));
    let removed = match std::fs::rename(&e.dir, &tomb) {
        Ok(()) => {
            if let Err(err) = std::fs::remove_dir_all(&tomb) {
                tracing::debug!(dir = %tomb.display(), error = %err, "cannot remove ended session");
            }
            true
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => {
            tracing::debug!(dir = %e.dir.display(), error = %err, "cannot remove ended session");
            false
        }
    };
    // Released only once the directory is gone.
    drop(held);
    removed
}

/// Take `path`'s lock exclusively, riding out another cleanup's (or a
/// viewer's) momentary shared probe. `Ok(None)`: held, or no such file.
fn lock_exclusive(path: &Path) -> std::io::Result<Option<std::fs::File>> {
    const ATTEMPTS: u32 = 5;
    for attempt in 1..=ATTEMPTS {
        match lockfile::try_lock_free(path)? {
            Some(file) => return Ok(Some(file)),
            None if attempt == ATTEMPTS || !path.exists() => return Ok(None),
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    Ok(None)
}

/// The latest modification time of `dir` and its files, and their total size.
/// Reads metadata only — never a file's contents.
fn last_write_and_size(dir: &Path) -> (SystemTime, u64) {
    let mut latest = std::fs::metadata(dir)
        .and_then(|m| m.modified())
        .unwrap_or(UNIX_EPOCH);
    let mut bytes = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for meta in entries
            .filter_map(Result::ok)
            .filter_map(|e| e.metadata().ok())
        {
            bytes = bytes.saturating_add(meta.len());
            if let Ok(m) = meta.modified() {
                latest = latest.max(m);
            }
        }
    }
    (latest, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);

    /// A session directory started `started_ago` before `now`, with `bytes` of
    /// events, its lock file present and free.
    fn ended(
        root: &Path,
        now: SystemTime,
        started_ago: Duration,
        rand: u8,
        bytes: usize,
    ) -> SessionId {
        let start = now.checked_sub(started_ago).expect("time");
        let ms = u64::try_from(start.duration_since(UNIX_EPOCH).expect("epoch").as_millis())
            .expect("ms");
        let id = SessionId::new(ms, rand);
        let dir = root.join(id.encode());
        std::fs::create_dir(&dir).expect("mkdir");
        std::fs::write(dir.join(files::LOCK), b"").expect("lock");
        std::fs::write(dir.join(files::EVENTS), vec![b'x'; bytes]).expect("events");
        id
    }

    fn remaining(root: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(root)
            .expect("read")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    /// I3: a session whose lock is held is never removed, however old.
    #[test]
    fn cleanup_skips_live_sessions() {
        let root = tempfile::tempdir().expect("tempdir");
        let now = SystemTime::now();
        let live = ended(root.path(), now, Duration::ZERO, 1, 10);
        let old = ended(root.path(), now, Duration::ZERO, 2, 10);
        let lock = lockfile::acquire_owner_lock(&root.path().join(live.encode()).join(files::LOCK))
            .expect("hold");
        // An hour later, both are past their age; only the free one goes.
        let report = cleanup(root.path(), now + 60 * MIN, Retention::default()).expect("cleanup");
        assert_eq!(report.removed, vec![old]);
        assert_eq!(report.live, 1);
        assert_eq!(remaining(root.path()), vec![live.encode()]);
        drop(lock);
    }

    /// Retention: ended sessions are kept 10 minutes, at most 20, and within the
    /// byte budget counted from the newest.
    #[test]
    fn cleanup_keeps_recent_ended() {
        let root = tempfile::tempdir().expect("tempdir");
        let now = SystemTime::now();
        let ids: Vec<SessionId> = (0..25u8)
            .map(|i| ended(root.path(), now, Duration::ZERO, i, 1))
            .collect();
        // One minute on: none is old, but only the 20 newest stay. The probe
        // skip must not keep the 5 oldest — they are outside the newest 20.
        let report = cleanup(root.path(), now + MIN, Retention::default()).expect("cleanup");
        let mut removed = report.removed.clone();
        removed.sort();
        assert_eq!(removed, ids.get(..5).expect("five").to_vec());
        assert_eq!(remaining(root.path()).len(), 20);

        // Eleven minutes on: every one is past the age.
        let report = cleanup(root.path(), now + 11 * MIN, Retention::default()).expect("cleanup");
        assert_eq!(report.removed.len(), 20);
        assert!(remaining(root.path()).is_empty());
    }

    /// A10: the byte budget counts every ended session — the newest, which age
    /// and count keep, included — and never a live one; the oldest go until the
    /// rest fit. All of these are recent and well inside the newest 20, so
    /// only the budget removes anything.
    #[test]
    fn cleanup_total_byte_budget() {
        let root = tempfile::tempdir().expect("tempdir");
        let now = SystemTime::now();
        let oldest = ended(root.path(), now, 3 * MIN, 1, 300);
        let old = ended(root.path(), now, 2 * MIN, 2, 600);
        let mid = ended(root.path(), now, MIN, 3, 300);
        let new = ended(root.path(), now, Duration::ZERO, 4, 300);
        // Live and far over the budget on its own: neither counted nor removed.
        let live = ended(root.path(), now, Duration::ZERO, 5, 5_000);
        let lock = lockfile::acquire_owner_lock(&root.path().join(live.encode()).join(files::LOCK))
            .expect("hold");
        let retention = Retention {
            max_bytes: 700,
            ..Retention::default()
        };
        let report = cleanup(root.path(), now, retention).expect("cleanup");
        let mut removed = report.removed.clone();
        removed.sort();
        assert_eq!(
            removed,
            vec![oldest, old],
            "oldest first, until 600 bytes fit"
        );
        assert_eq!(report.live, 1);
        assert_eq!(remaining(root.path()), {
            let mut v = vec![mid.encode(), new.encode(), live.encode()];
            v.sort();
            v
        });
        drop(lock);
    }

    /// A7: cleanup decides on the lock and mtimes alone. A `meta.json` that is
    /// garbage (or from a format this heph cannot read) changes nothing, and a
    /// held lock with garbage meta is a live session.
    #[test]
    fn cleanup_never_parses_meta() {
        let root = tempfile::tempdir().expect("tempdir");
        let now = SystemTime::now();
        let live = ended(root.path(), now, Duration::ZERO, 1, 1);
        let dead = ended(root.path(), now, Duration::ZERO, 2, 1);
        for id in [live, dead] {
            std::fs::write(
                root.path().join(id.encode()).join(files::META),
                b"{\"format\": 99, \"not\": json",
            )
            .expect("meta");
        }
        let lock = lockfile::acquire_owner_lock(&root.path().join(live.encode()).join(files::LOCK))
            .expect("hold");
        let report = cleanup(root.path(), now + 60 * MIN, Retention::default()).expect("cleanup");
        assert_eq!(report.removed, vec![dead]);
        assert_eq!(remaining(root.path()), vec![live.encode()]);
        drop(lock);
    }

    /// A7: a directory killed before its lock existed is an ended session, and
    /// names that are not ids are not ours to touch.
    #[test]
    fn cleanup_removes_lockless_and_ignores_foreign_names() {
        let root = tempfile::tempdir().expect("tempdir");
        let now = SystemTime::now();
        let id = SessionId::new(1, 1);
        std::fs::create_dir(root.path().join(id.encode())).expect("mkdir");
        std::fs::create_dir(root.path().join("not-a-session")).expect("mkdir");
        // Freshly written: it may be a session being created right now.
        let report = cleanup(root.path(), now, Retention::default()).expect("cleanup");
        assert!(report.removed.is_empty(), "{report:?}");
        let report = cleanup(root.path(), now + 60 * MIN, Retention::default()).expect("cleanup");
        assert_eq!(report.removed, vec![id]);
        assert_eq!(remaining(root.path()), vec!["not-a-session".to_owned()]);
    }

    /// Two sessions starting at once both clean: neither errors, each dead
    /// directory is removed exactly once, and the live one survives.
    #[test]
    fn concurrent_cleanups() {
        let root = tempfile::tempdir().expect("tempdir");
        let now = SystemTime::now();
        let ids: Vec<SessionId> = (0..40u8)
            .map(|i| ended(root.path(), now, Duration::ZERO, i, 1))
            .collect();
        let live = ids.last().copied().expect("one");
        let lock = lockfile::acquire_owner_lock(&root.path().join(live.encode()).join(files::LOCK))
            .expect("hold");
        let later = now + 60 * MIN;
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let root = root.path().to_path_buf();
                std::thread::spawn(move || cleanup(&root, later, Retention::default()))
            })
            .collect();
        let mut removed: Vec<SessionId> = threads
            .into_iter()
            .flat_map(|t| t.join().expect("join").expect("cleanup").removed)
            .collect();
        removed.sort();
        let mut expected: Vec<SessionId> = ids.iter().copied().filter(|i| *i != live).collect();
        expected.sort();
        assert_eq!(removed, expected, "each dead session removed once");
        assert_eq!(remaining(root.path()), vec![live.encode()]);
        drop(lock);
    }
}
