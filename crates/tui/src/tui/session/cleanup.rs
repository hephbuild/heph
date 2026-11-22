//! Removing ended sessions.
//!
//! Runs only at session start, on the recorder thread (design amendment A7):
//! the owner of a new session is the only writer of the sessions directory, and
//! a viewer stays strictly read-only.
//!
//! The decision reads the lock, modification times and a recording's 100-byte
//! header, never its contents. The `lock` file name and its `flock` meaning are
//! frozen for every format, so a session of a format this heph cannot read is
//! still cleaned correctly, and one whose lock is held is a live session of
//! unknown format, never counted and never removed.
//!
//! An ended session (lock free) is kept while it was last written less than
//! [`Retention::max_age`] ago. Then the byte budget: the recordings kept so far,
//! the newest included, must total at most [`Retention::max_bytes`], and the
//! oldest go until they do. There is no count cap.
//!
//! A lost session (lock free, recording still in WAL mode, in a format this heph
//! knows) is **sealed** with [`store::seal_lost`] under its lock taken
//! exclusively before it is counted, so the byte budget counts the
//! self-contained file. A recording this heph cannot seal (an unknown format,
//! not a heph recording, killed before its schema was written, or a seal that
//! failed) is never written: it is removed by age only, and left out of the
//! byte budget.
//!
//! A directory whose `lock` file is younger than [`STARTING`], or that has no
//! `lock` file and was written less than [`STARTING`] ago, may be a session
//! still starting (`mkdir`, then the lock): neither counted nor removed.
//!
//! The lock state is read with a shared probe, released at once; a directory
//! sealed or removed is then locked exclusively by this process, so two
//! cleanups racing each other never both seal or remove one session, and a
//! session cannot start inside a directory being removed (ids are never
//! reused). A stale tomb (see [`TOMB_PREFIX`]) or reader copy (see
//! [`store::tmp_dir`]) is deleted with no lock, so two cleanups can both delete
//! one; that is harmless, one of them finds it gone.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::format::files;
use super::id::SessionId;
use super::lockfile;
use super::store;

/// How long, and how many bytes of, ended recordings are kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub max_age: Duration,
    pub max_bytes: u64,
}

impl Retention {
    /// What `.hephconfig`'s `session.keep` and `session.keepBytes` set, each
    /// defaulting to [`Retention::default`]'s.
    pub fn from_config(keep: Option<Duration>, keep_bytes: Option<u64>) -> Self {
        let default = Self::default();
        Self {
            max_age: keep.unwrap_or(default.max_age),
            max_bytes: keep_bytes.unwrap_or(default.max_bytes),
        }
    }

    /// What a private root's cleanup keeps: no ended session, whatever its age.
    pub const PRIVATE: Self = Self {
        max_age: Duration::ZERO,
        max_bytes: 0,
    };
}

impl Default for Retention {
    /// 7 days, and the newest 2 GiB of recordings.
    fn default() -> Self {
        Self {
            max_age: Duration::from_secs(7 * 24 * 60 * 60),
            max_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

/// What a cleanup did, for tests and a `debug!`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CleanupReport {
    pub removed: Vec<SessionId>,
    /// Lost sessions sealed (whether or not retention then removed them).
    pub sealed: Vec<SessionId>,
    /// Directories skipped because their lock is held (or another cleanup has
    /// it).
    pub live: usize,
    /// Stale tombs removed: see [`TOMB_PREFIX`].
    pub tombs: usize,
    /// Stale reader copies removed from [`store::tmp_dir`].
    pub tmp: usize,
}

/// How long a session may take to start: a directory with no `lock` file, or a
/// free `lock` file, younger than this may be one whose owner has not taken its
/// lock yet. Also how old a lost session's `lock` file must be before cleanup
/// seals it.
pub const STARTING: Duration = Duration::from_secs(60);

/// How long a tomb or a reader copy is left alone: either may be in use right
/// now (a removal in progress, a viewer reading its copy).
pub const SCRATCH_GRACE: Duration = Duration::from_secs(10 * 60);

/// The name prefix of a session directory being removed (`.removing-<id>-<pid>`):
/// [`remove`] renames it there first, then deletes it. A tomb outlives that only
/// when the delete failed or the process died in it, and nothing else ever
/// reads one, so a tomb last written longer than [`SCRATCH_GRACE`] ago is
/// removed outright. One a cleanup is removing right now may be removed by
/// two at once, which costs a `debug!` and nothing else.
const TOMB_PREFIX: &str = ".removing-";

/// Remove the ended sessions `retention` does not keep, under `root`, as of
/// `now`, sealing lost ones first. Per-directory failures are skipped (a
/// vanished directory is another cleanup's work); only an unreadable `root` is
/// an error.
pub fn cleanup(
    root: &Path,
    now: SystemTime,
    retention: Retention,
) -> std::io::Result<CleanupReport> {
    let mut sessions: Vec<(SessionId, PathBuf)> = Vec::new();
    let mut tombs: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(root)?.filter_map(Result::ok) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(TOMB_PREFIX) {
            tombs.push(entry.path());
        } else if let Ok(id) = SessionId::parse(name) {
            sessions.push((id, entry.path()));
        }
    }
    // Newest first: an id sorts by start time.
    sessions.sort_by_key(|s| std::cmp::Reverse(s.0));

    let mut report = CleanupReport::default();
    for tomb in tombs {
        if age(now, last_write_and_size(&tomb).0) < SCRATCH_GRACE {
            continue;
        }
        match std::fs::remove_dir_all(&tomb) {
            Ok(()) => report.tombs += 1,
            Err(err) => {
                tracing::debug!(dir = %tomb.display(), error = %err, "cannot remove a stale tomb");
            }
        }
    }
    report.tmp = sweep_tmp(&store::tmp_dir(root), now);

    // Classify, newest first, from the lock, the metadata and the header. Every
    // directory is probed: only the probe tells an ended one from a live one.
    let mut ended: Vec<Ended> = Vec::new();
    for (id, dir) in sessions {
        let lock_path = dir.join(files::LOCK);
        let lock_written = match std::fs::metadata(&lock_path) {
            Ok(meta) => Some(meta.modified().unwrap_or(UNIX_EPOCH)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                tracing::debug!(dir = %dir.display(), error = %e, "skipping session in cleanup");
                continue;
            }
        };
        if lock_written.is_some() {
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
        let (last_write, _) = last_write_and_size(&dir);
        // Possibly still starting: its owner may not hold the lock yet.
        let starting = match lock_written {
            Some(written) => age(now, written) < STARTING,
            None => age(now, last_write) < STARTING,
        };
        if starting {
            continue;
        }
        let keep = age(now, last_write) < retention.max_age;
        // Sealing one that age removes anyway would be wasted work.
        let budgeted = if keep && lock_written.is_some() {
            seal_if_lost(&dir, id, &mut report)
        } else {
            false
        };
        // Measured after the seal: the WAL is folded into the file.
        let bytes = if budgeted {
            last_write_and_size(&dir).1
        } else {
            0
        };
        ended.push(Ended {
            id,
            dir,
            bytes,
            keep,
            budgeted,
        });
    }

    // The byte budget over every sealed recording still kept, oldest dropped
    // first. A recording this heph cannot seal is left to age.
    let mut total: u64 = ended
        .iter()
        .filter(|e| e.keep && e.budgeted)
        .fold(0, |sum, e| sum.saturating_add(e.bytes));
    for e in ended.iter_mut().rev() {
        if total <= retention.max_bytes {
            break;
        }
        if e.keep && e.budgeted {
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
    /// A sealed recording in a format this heph knows: counted in the byte
    /// budget. Anything else is removed by age only.
    budgeted: bool,
}

/// Seal `dir`'s recording if it is lost (WAL mode, a known format), under its
/// lock taken exclusively. `true` when the recording is sealed (now or
/// already) in a format this heph knows, so the byte budget may count it.
fn seal_if_lost(dir: &Path, id: SessionId, report: &mut CleanupReport) -> bool {
    let path = dir.join(store::FILE);
    let header = match store::read_header(&path) {
        Ok(Some(h)) if h.is_known() => h,
        // Missing, empty, not SQLite, not ours, or a newer format: never
        // written, left to age.
        Ok(_) => return false,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(file = %path.display(), error = %e, "cannot read a recording's header");
            }
            return false;
        }
    };
    if !header.wal {
        return true;
    }
    let lock_path = dir.join(files::LOCK);
    let held = match lock_exclusive(&lock_path) {
        Ok(Some(held)) => held,
        // A viewer copying it, or another cleanup sealing it: next time.
        Ok(None) => return false,
        Err(e) => {
            tracing::debug!(dir = %dir.display(), error = %e, "cannot lock a lost session");
            return false;
        }
    };
    let sealed = match store::seal_lost(&path) {
        Ok(store::Sealed::Done) => {
            report.sealed.push(id);
            true
        }
        Ok(store::Sealed::CutShort) => {
            tracing::debug!(file = %path.display(), "sealing a lost session was cut short");
            false
        }
        Err(e) => {
            tracing::debug!(file = %path.display(), error = %format!("{e:#}"), "cannot seal a lost session");
            false
        }
    };
    drop(held);
    sealed
}

/// Remove the files in `tmp` last written [`SCRATCH_GRACE`] or longer ago: a
/// viewer's copy outlives it only when the viewer died. How many went.
fn sweep_tmp(tmp: &Path, now: SystemTime) -> usize {
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.filter_map(Result::ok) {
        let Ok(meta) = entry.metadata() else { continue };
        if age(now, meta.modified().unwrap_or(UNIX_EPOCH)) < SCRATCH_GRACE {
            continue;
        }
        let path = entry.path();
        let result = if meta.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::debug!(file = %path.display(), error = %e, "cannot remove a stale session copy");
            }
        }
    }
    removed
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
    let tomb = root.join(format!("{TOMB_PREFIX}{}-{}", e.id, std::process::id()));
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

/// How long before `now` `written` was; zero for a time in the future.
fn age(now: SystemTime, written: SystemTime) -> Duration {
    now.duration_since(written).unwrap_or(Duration::ZERO)
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
    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    /// The 100-byte header of a sealed recording (rollback mode) in `format`,
    /// padded to `bytes`: all cleanup reads of it.
    fn sealed_header(format: u32, bytes: usize) -> Vec<u8> {
        let mut b = vec![0u8; bytes.max(100)];
        b[..16].copy_from_slice(b"SQLite format 3\0");
        b[18] = 1;
        b[19] = 1;
        b[60..64].copy_from_slice(&format.to_be_bytes());
        b[68..72].copy_from_slice(&store::APPLICATION_ID.to_be_bytes());
        b
    }

    /// A session directory started `started_ago` before `now`, holding
    /// `recording` as its `session.db`, its lock file present and free.
    fn session_with(
        root: &Path,
        now: SystemTime,
        started_ago: Duration,
        rand: u8,
        recording: &[u8],
    ) -> SessionId {
        let start = now.checked_sub(started_ago).expect("time");
        let ms = u64::try_from(start.duration_since(UNIX_EPOCH).expect("epoch").as_millis())
            .expect("ms");
        let id = SessionId::new(ms, rand);
        let dir = root.join(id.encode());
        std::fs::create_dir(&dir).expect("mkdir");
        std::fs::write(dir.join(files::LOCK), b"").expect("lock");
        std::fs::write(dir.join(store::FILE), recording).expect("recording");
        id
    }

    /// An ended session with a sealed recording of `bytes`.
    fn ended(
        root: &Path,
        now: SystemTime,
        started_ago: Duration,
        rand: u8,
        bytes: usize,
    ) -> SessionId {
        session_with(
            root,
            now,
            started_ago,
            rand,
            &sealed_header(store::FORMAT, bytes),
        )
    }

    fn hold(root: &Path, id: SessionId) -> std::fs::File {
        lockfile::acquire_owner_lock(&root.join(id.encode()).join(files::LOCK)).expect("hold")
    }

    fn remaining(root: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(root)
            .expect("read")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn sorted(mut v: Vec<SessionId>) -> Vec<SessionId> {
        v.sort();
        v
    }

    /// I3: a session whose lock is held is never removed, however old.
    #[test]
    fn cleanup_skips_live_sessions() {
        let root = tempfile::tempdir().expect("tempdir");
        let now = SystemTime::now();
        let live = ended(root.path(), now, Duration::ZERO, 1, 100);
        let old = ended(root.path(), now, Duration::ZERO, 2, 100);
        let lock = hold(root.path(), live);
        // Eight days later, both are past their age; only the free one goes.
        let report = cleanup(root.path(), now + 8 * DAY, Retention::default()).expect("cleanup");
        assert_eq!(report.removed, vec![old]);
        assert_eq!(report.live, 1);
        assert_eq!(remaining(root.path()), vec![live.encode()]);
        drop(lock);
    }

    /// Retention: ended recordings are kept 7 days, with no count cap.
    #[test]
    fn cleanup_keeps_seven_days_uncapped() {
        let root = tempfile::tempdir().expect("tempdir");
        let now = SystemTime::now();
        let ids: Vec<SessionId> = (0..40u8)
            .map(|i| ended(root.path(), now, Duration::ZERO, i, 100))
            .collect();
        // Six days on: none is old, and 40 is no reason to drop any.
        let report = cleanup(root.path(), now + 6 * DAY, Retention::default()).expect("cleanup");
        assert!(report.removed.is_empty(), "{report:?}");
        assert_eq!(remaining(root.path()).len(), ids.len());

        // Eight days on: every one is past the age.
        let report = cleanup(root.path(), now + 8 * DAY, Retention::default()).expect("cleanup");
        assert_eq!(sorted(report.removed), ids);
        assert!(remaining(root.path()).is_empty());
    }

    /// The byte budget counts every ended recording kept — the newest
    /// included — and never a live one; the oldest go until the rest fit.
    /// All of these are recent, so only the budget removes anything.
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
        let lock = hold(root.path(), live);
        let retention = Retention {
            max_bytes: 700,
            ..Retention::default()
        };
        let report = cleanup(root.path(), now + 2 * MIN, retention).expect("cleanup");
        assert_eq!(
            sorted(report.removed),
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

    /// Cleanup never writes or budgets what it cannot seal: garbage, a
    /// recording in a newer format, a file killed before its header was
    /// written. Each is removed by age only, and a held lock over garbage is
    /// a live session.
    #[test]
    fn cleanup_leaves_unknown_recordings_to_age() {
        let root = tempfile::tempdir().expect("tempdir");
        let now = SystemTime::now();
        let garbage: &[u8] = b"SQLite format 3\0 but not really";
        let newer = sealed_header(store::FORMAT + 1, 4096);
        let mut newer_wal = newer.clone();
        newer_wal[18] = 2;
        newer_wal[19] = 2;
        let live = session_with(root.path(), now, Duration::ZERO, 1, garbage);
        let unknown = [
            session_with(root.path(), now, Duration::ZERO, 2, garbage),
            session_with(root.path(), now, Duration::ZERO, 3, &newer),
            session_with(root.path(), now, Duration::ZERO, 4, &newer_wal),
            session_with(root.path(), now, Duration::ZERO, 5, b""),
        ];
        let lock = hold(root.path(), live);
        let before: Vec<Vec<u8>> = unknown
            .iter()
            .map(|id| std::fs::read(root.path().join(id.encode()).join(store::FILE)).expect("read"))
            .collect();

        // A zero budget removes nothing it cannot count.
        let retention = Retention {
            max_bytes: 0,
            ..Retention::default()
        };
        let report = cleanup(root.path(), now + 2 * MIN, retention).expect("cleanup");
        assert!(report.removed.is_empty(), "{report:?}");
        assert!(report.sealed.is_empty(), "{report:?}");
        for (id, bytes) in unknown.iter().zip(&before) {
            let after =
                std::fs::read(root.path().join(id.encode()).join(store::FILE)).expect("read");
            assert_eq!(&after, bytes, "{id} was written");
        }

        let report = cleanup(root.path(), now + 8 * DAY, retention).expect("cleanup");
        assert_eq!(sorted(report.removed), sorted(unknown.to_vec()));
        assert_eq!(remaining(root.path()), vec![live.encode()]);
        drop(lock);
    }

    /// A lost session (lock free, WAL mode, known format) is sealed under its
    /// lock before it is counted, but only once its lock file is older than a
    /// minute: younger, it may be a session still starting, and is neither
    /// sealed, counted nor removed.
    #[test]
    fn cleanup_seals_lost_sessions() {
        let root = tempfile::tempdir().expect("tempdir");
        let now = SystemTime::now();
        let id = SessionId::new(
            u64::try_from(now.duration_since(UNIX_EPOCH).expect("epoch").as_millis()).expect("ms"),
            1,
        );
        let dir = root.path().join(id.encode());
        std::fs::create_dir(&dir).expect("mkdir");
        std::fs::write(dir.join(files::LOCK), b"").expect("lock");
        super::super::tests::unsealed_recording(&dir, id, &[]);
        let path = dir.join(store::FILE);
        assert!(
            store::read_header(&path)
                .expect("header")
                .expect("sqlite")
                .wal,
            "a lost recording is in WAL mode"
        );

        let early = cleanup(
            root.path(),
            now + Duration::from_secs(30),
            Retention::default(),
        )
        .expect("cleanup");
        assert!(
            early.sealed.is_empty() && early.removed.is_empty(),
            "{early:?}"
        );
        assert!(
            store::read_header(&path)
                .expect("header")
                .expect("sqlite")
                .wal
        );

        let report = cleanup(root.path(), now + 2 * MIN, Retention::default()).expect("cleanup");
        assert_eq!(report.sealed, vec![id]);
        assert!(report.removed.is_empty(), "{report:?}");
        let header = store::read_header(&path).expect("header").expect("sqlite");
        assert!(!header.wal, "sealed: rollback mode");
        assert!(!store::sibling(&path, "-wal").exists());
        let conn = store::open_sealed(&path).expect("open sealed");
        let sealed: bool = conn
            .query_row("SELECT sealed FROM session", [], |r| r.get(0))
            .expect("sealed");
        assert!(sealed);
    }

    /// The private root keeps no ended session, but leaves one that may still
    /// be starting.
    #[test]
    fn private_retention_keeps_nothing_ended() {
        let root = tempfile::tempdir().expect("tempdir");
        let now = SystemTime::now();
        let ended_one = ended(root.path(), now, 2 * MIN, 1, 100);
        let starting = SessionId::new(
            u64::try_from(now.duration_since(UNIX_EPOCH).expect("epoch").as_millis()).expect("ms"),
            2,
        );
        std::fs::create_dir(root.path().join(starting.encode())).expect("mkdir");
        // The lock file of the first was written now: two minutes on, it is
        // past the start grace; the lockless one was written now as well.
        let report = cleanup(root.path(), now + 2 * MIN, Retention::PRIVATE).expect("cleanup");
        assert_eq!(sorted(report.removed), sorted(vec![ended_one, starting]));
        let root2 = tempfile::tempdir().expect("tempdir");
        let young = ended(root2.path(), now, Duration::ZERO, 3, 100);
        let report = cleanup(root2.path(), now, Retention::PRIVATE).expect("cleanup");
        assert!(report.removed.is_empty(), "{report:?}");
        assert_eq!(remaining(root2.path()), vec![young.encode()]);
    }

    /// A directory killed before its lock existed is an ended session,
    /// removed by age, and names that are not ids are not ours to touch.
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
        // Past the start grace, but nothing to count: age decides.
        let report = cleanup(root.path(), now + 60 * MIN, Retention::default()).expect("cleanup");
        assert!(report.removed.is_empty(), "{report:?}");
        let report = cleanup(root.path(), now + 8 * DAY, Retention::default()).expect("cleanup");
        assert_eq!(report.removed, vec![id]);
        assert_eq!(remaining(root.path()), vec!["not-a-session".to_owned()]);
    }

    /// A tomb left by a removal that failed or died is removed once it is
    /// older than ten minutes, and kept before that (a cleanup may be
    /// removing it right now).
    #[test]
    fn cleanup_removes_stale_tombs() {
        let root = tempfile::tempdir().expect("tempdir");
        let now = SystemTime::now();
        let tomb = root
            .path()
            .join(format!("{TOMB_PREFIX}{}-42", SessionId::new(1, 1)));
        std::fs::create_dir(&tomb).expect("mkdir");
        std::fs::write(tomb.join(store::FILE), b"x").expect("recording");

        let report = cleanup(root.path(), now + 9 * MIN, Retention::default()).expect("cleanup");
        assert_eq!(report.tombs, 0);
        assert!(tomb.exists(), "a fresh tomb is kept");

        let report = cleanup(root.path(), now + 11 * MIN, Retention::default()).expect("cleanup");
        assert_eq!(report.tombs, 1);
        assert!(remaining(root.path()).is_empty(), "the stale tomb is gone");
    }

    /// A reader's copy in `.tmp` older than ten minutes is swept: its viewer
    /// died. A younger one may be open right now.
    #[test]
    fn cleanup_sweeps_stale_reader_copies() {
        let root = tempfile::tempdir().expect("tempdir");
        let now = SystemTime::now();
        let tmp = store::tmp_dir(root.path());
        std::fs::create_dir(&tmp).expect("mkdir");
        std::fs::write(tmp.join("copy.db"), b"x").expect("copy");
        std::fs::write(tmp.join("copy.db-wal"), b"x").expect("copy");

        let report = cleanup(root.path(), now + 9 * MIN, Retention::default()).expect("cleanup");
        assert_eq!(report.tmp, 0);
        let report = cleanup(root.path(), now + 11 * MIN, Retention::default()).expect("cleanup");
        assert_eq!(report.tmp, 2);
        assert_eq!(std::fs::read_dir(&tmp).expect("read").count(), 0);
        assert_eq!(remaining(root.path()), vec![store::TMP_DIR.to_owned()]);
    }

    /// Two sessions starting at once both clean: neither errors, each dead
    /// directory is removed exactly once, and the live one survives.
    #[test]
    fn concurrent_cleanups() {
        let root = tempfile::tempdir().expect("tempdir");
        let now = SystemTime::now();
        let ids: Vec<SessionId> = (0..40u8)
            .map(|i| ended(root.path(), now, Duration::ZERO, i, 100))
            .collect();
        let live = ids.last().copied().expect("one");
        let lock = hold(root.path(), live);
        let later = now + 8 * DAY;
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let root = root.path().to_path_buf();
                std::thread::spawn(move || cleanup(&root, later, Retention::default()))
            })
            .collect();
        let removed: Vec<SessionId> = threads
            .into_iter()
            .flat_map(|t| t.join().expect("join").expect("cleanup").removed)
            .collect();
        let expected: Vec<SessionId> = ids.iter().copied().filter(|i| *i != live).collect();
        assert_eq!(
            sorted(removed),
            sorted(expected),
            "each dead session removed once"
        );
        assert_eq!(remaining(root.path()), vec![live.encode()]);
        drop(lock);
    }
}
