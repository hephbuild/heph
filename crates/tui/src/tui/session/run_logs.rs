//! Finding a running target's live log through the sessions that recorded it.
//!
//! A heph that runs a target announces its live log as a `RunLog` record
//! (`addr`, `hashin`, `file` relative to its home). A reader in another
//! process (the log API's follower, `heph log`, an attached dashboard) finds
//! it here. The rules (spec I4, "Who is followed"):
//!
//! - **Only this workspace.** Sessions are per user, across checkouts: a
//!   session counts only when its `meta.json` names the reader's own home,
//!   exactly, and lists [`CAPABILITY_RUN_LOGS`]. A build of the same address
//!   in another checkout never answers.
//! - **Only a live session.** A record is trusted only while the session's
//!   lock is held (probed before anything is read, as `list` does): a
//!   crashed or ended heph's records read as absent.
//! - **The newest run.** The last `RunLog` for the addr is the one; an
//!   `ExecuteEnd` for the addr after it means that run ended.
//! - **A file gone means ended.** The live file is unlinked once what
//!   replaces it (the cache entry's `Log`, the failure slot) is in place,
//!   and the `ExecuteEnd` can land up to 250 ms later: a record still open
//!   over a missing file is [`SessionRunLog::Ended`], never "no log".
//! - **`file` cannot leave `<home>/runs/`.** It must be `runs/<dir>/<name>`
//!   ([`RunLogRecord::relative_file`]); `runs/` and `runs/<dir>/` must be
//!   real directories, not symlinks; the file is opened `O_NOFOLLOW` (and
//!   `O_NONBLOCK`, so a planted FIFO cannot hang the reader) and must be a
//!   regular file. Anything else reads as absent.
//!
//! Past the event log's 64 MiB cap, `RunLog` is still written but
//! `ExecuteEnd` is not, so there a run's end shows only as its file being
//! gone (or, for a `cache = false` success, whose file stays until the heph
//! exits, as the session ending).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use hcore::events::BuildEventKind;
use hcore::session_record::{LiveRunLogLocator, LocatedRunLog};

use super::format::{CAPABILITY_RUN_LOGS, RunLogRecord, SessionMeta, SessionRecordKind, files};
use super::{RecordReader, SessionId, SessionRoot, lockfile};

/// What a live session says about an addr's newest run log.
#[derive(Debug)]
pub enum SessionRunLog {
    /// The run is going on: its live file, opened. Read it to EOF and keep
    /// following until the run ends.
    Live {
        record: RunLogRecord,
        /// When the record was written.
        at_unix_ms: u64,
        /// `<home>/<file>`.
        path: PathBuf,
        file: File,
    },
    /// The run ended: an `ExecuteEnd` for the addr followed its record, or
    /// its live file is gone. What replaced the file is found by
    /// `record.hashin` (the cache entry's `Log`, else the failure slot).
    Ended {
        record: RunLogRecord,
        at_unix_ms: u64,
    },
}

impl SessionRunLog {
    pub fn record(&self) -> &RunLogRecord {
        match self {
            Self::Live { record, .. } | Self::Ended { record, .. } => record,
        }
    }

    pub fn at_unix_ms(&self) -> u64 {
        match self {
            Self::Live { at_unix_ms, .. } | Self::Ended { at_unix_ms, .. } => *at_unix_ms,
        }
    }
}

/// A run log found by [`find_run_log`], and the session that recorded it
/// (the status line names it).
#[derive(Debug)]
pub struct FoundRunLog {
    pub session: SessionId,
    pub log: SessionRunLog,
}

/// What the live sessions under a root say about an addr's run log:
/// [`lookup_run_log`].
#[derive(Debug)]
pub enum RunLogLookup {
    /// The newest trusted record of the addr, and its session.
    Found(FoundRunLog),
    /// The addr is running in `session`, which records run logs, and has
    /// written nothing yet. Answered over an older [`Self::Found`], and
    /// always over [`Self::Unsupported`].
    Started {
        session: SessionId,
        /// When its `ExecuteStart` was recorded.
        at_unix_ms: u64,
    },
    /// No session recorded a log of the addr or is running it, but a live
    /// session of this workspace runs a heph that records no live logs: it
    /// may be running the addr unseen. The newest such session is named.
    Unsupported { session: SessionId },
    /// Nothing: no live session of this workspace, or none that recorded
    /// the addr.
    None,
}

/// The newest trusted run log of `addr` across every live session under
/// `root` recorded for `home`. `None` when no such session recorded one.
/// A session that cannot be read is skipped (logged at debug), never an
/// error: only an unreadable `root` is.
///
/// Blocking: probes each session's lock and reads the `events.ndjson` of
/// those that are live and in this workspace.
pub fn find_run_log(
    root: &SessionRoot,
    home: &Path,
    addr: &str,
) -> anyhow::Result<Option<FoundRunLog>> {
    Ok(match lookup_run_log(root, home, addr)? {
        RunLogLookup::Found(found) => Some(found),
        RunLogLookup::Started { .. } | RunLogLookup::Unsupported { .. } | RunLogLookup::None => {
            None
        }
    })
}

/// [`find_run_log`], also saying when nothing was found but a live session
/// of `home` cannot record run logs ([`RunLogLookup::Unsupported`]). The
/// trust rules are the same: such a session counts only while its lock is
/// held and its `meta.json` names `home`.
pub fn lookup_run_log(root: &SessionRoot, home: &Path, addr: &str) -> anyhow::Result<RunLogLookup> {
    let root = root.path();
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(RunLogLookup::None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", root.display())),
    };
    let mut newest: Option<FoundRunLog> = None;
    let mut started: Option<(u64, SessionId)> = None;
    let mut unsupported: Option<SessionId> = None;
    for entry in entries.filter_map(Result::ok) {
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|s| SessionId::parse(s).ok())
        else {
            continue;
        };
        let log = match session_lookup(&entry.path(), home, addr) {
            Ok(SessionLookup::Log(log)) => log,
            Ok(SessionLookup::NoRunLogs) => {
                if unsupported.is_none_or(|u| id.encode() > u.encode()) {
                    unsupported = Some(id);
                }
                continue;
            }
            Ok(SessionLookup::Started(at)) => {
                if started.is_none_or(|(t, s)| (at, id.encode()) > (t, s.encode())) {
                    started = Some((at, id));
                }
                continue;
            }
            Ok(SessionLookup::Nothing) => continue,
            Err(e) => {
                tracing::debug!(session = %id, error = %format!("{e:#}"), "skipping session");
                continue;
            }
        };
        let newer = newest.as_ref().is_none_or(|n| {
            (log.at_unix_ms(), id.encode()) > (n.log.at_unix_ms(), n.session.encode())
        });
        if newer {
            newest = Some(FoundRunLog { session: id, log });
        }
    }
    Ok(match (newest, started, unsupported) {
        // A run that started after the newest record is going on, with
        // nothing written yet.
        (found, Some((at_unix_ms, session)), _)
            if found
                .as_ref()
                .is_none_or(|f| at_unix_ms > f.log.at_unix_ms()) =>
        {
            RunLogLookup::Started {
                session,
                at_unix_ms,
            }
        }
        (Some(found), _, _) => RunLogLookup::Found(found),
        (None, _, Some(session)) => RunLogLookup::Unsupported { session },
        (None, _, None) => RunLogLookup::None,
    })
}

/// What one session says about an addr.
enum SessionLookup {
    Log(SessionRunLog),
    /// Running here since the `ExecuteStart` at this time, nothing written.
    Started(u64),
    /// Live, of this home, from a heph that records no run logs.
    NoRunLogs,
    Nothing,
}

/// What the session in `dir` says about `addr`'s newest run log, when it is
/// live, was recorded for `home` and records run logs. See the module docs
/// for every rule; `None` is "this session has no trusted log of `addr`".
pub fn session_run_log(
    dir: &Path,
    home: &Path,
    addr: &str,
) -> anyhow::Result<Option<SessionRunLog>> {
    Ok(match session_lookup(dir, home, addr)? {
        SessionLookup::Log(log) => Some(log),
        SessionLookup::Started(_) | SessionLookup::NoRunLogs | SessionLookup::Nothing => None,
    })
}

fn session_lookup(dir: &Path, home: &Path, addr: &str) -> anyhow::Result<SessionLookup> {
    // First, as `list` does: an `End` written before the release is then in
    // what is read next.
    if !lockfile::is_held(&dir.join(files::LOCK)).context("probing the session's lock")? {
        return Ok(SessionLookup::Nothing);
    }
    let meta = match std::fs::read(dir.join(files::META)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(SessionLookup::Nothing),
        Err(e) => return Err(e).context("reading meta.json"),
    };
    // A newer format or a corrupt file: not one this heph follows.
    let Ok(meta) = SessionMeta::decode(&meta) else {
        return Ok(SessionLookup::Nothing);
    };
    if meta.home_dir.as_deref().map(Path::new) != Some(home) {
        return Ok(SessionLookup::Nothing);
    }
    if !meta.has_capability(CAPABILITY_RUN_LOGS) {
        return Ok(SessionLookup::NoRunLogs);
    }
    let events = match File::open(dir.join(files::EVENTS)) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(SessionLookup::Nothing),
        Err(e) => return Err(e).context("opening events.ndjson"),
    };
    let mut reader = RecordReader::new(events);
    let mut newest: Option<(u64, RunLogRecord)> = None;
    let mut ended = false;
    // The `ExecuteStart` of a run of the addr not followed by its end: it
    // is going on, and has written nothing unless a record came after.
    let mut started: Option<u64> = None;
    while let Some(rec) = reader.next_record().context("reading events.ndjson")? {
        match (rec.kind, rec.session) {
            (_, Some(SessionRecordKind::RunLog(run)))
                if run.addr == addr && run.relative_file().is_some() =>
            {
                newest = Some((rec.at_unix_ms, *run));
                ended = false;
            }
            (Some(BuildEventKind::ExecuteStart { addr: a, .. }), _) if a == addr => {
                started = Some(rec.at_unix_ms);
            }
            (Some(BuildEventKind::ExecuteEnd { addr: a, .. }), _) if a == addr => {
                ended = true;
                started = None;
            }
            _ => {}
        }
    }
    // A record not ended is the run going on, whichever of its record and
    // its start was written first; a start after an ended run is a new run
    // that has written nothing yet.
    let Some((at_unix_ms, record)) = newest.filter(|_| !ended || started.is_none()) else {
        return Ok(started.map_or(SessionLookup::Nothing, SessionLookup::Started));
    };
    if ended {
        return Ok(SessionLookup::Log(SessionRunLog::Ended {
            record,
            at_unix_ms,
        }));
    }
    Ok(
        match open_live(home, &record).context("opening a live run log")? {
            Opened::File(path, file) => SessionLookup::Log(SessionRunLog::Live {
                record,
                at_unix_ms,
                path,
                file,
            }),
            Opened::Gone => SessionLookup::Log(SessionRunLog::Ended { record, at_unix_ms }),
            Opened::Refused => SessionLookup::Nothing,
        },
    )
}

/// The engine log API's [`LiveRunLogLocator`] over the sessions under one
/// root: [`lookup_run_log`], and nothing else. `heph log` and the dashboard
/// hand one to the engine, which cannot depend on this crate.
#[derive(Debug, Clone)]
pub struct SessionRunLogLocator {
    root: SessionRoot,
}

impl SessionRunLogLocator {
    pub fn new(root: SessionRoot) -> Self {
        Self { root }
    }
}

impl LiveRunLogLocator for SessionRunLogLocator {
    fn locate(&self, home: &Path, addr: &str) -> anyhow::Result<Option<LocatedRunLog>> {
        let found = match lookup_run_log(&self.root, home, addr)? {
            RunLogLookup::Found(found) => found,
            RunLogLookup::Started {
                session,
                at_unix_ms,
            } => {
                return Ok(Some(LocatedRunLog::Started {
                    at_unix_ms,
                    session: session.encode(),
                }));
            }
            RunLogLookup::Unsupported { session } => {
                return Ok(Some(LocatedRunLog::Unsupported {
                    session: session.encode(),
                }));
            }
            RunLogLookup::None => return Ok(None),
        };
        let session = found.session.encode();
        Ok(Some(match found.log {
            SessionRunLog::Live {
                record,
                at_unix_ms,
                file,
                ..
            } => LocatedRunLog::Live {
                record,
                at_unix_ms,
                session,
                file,
            },
            SessionRunLog::Ended { record, at_unix_ms } => LocatedRunLog::Ended {
                record,
                at_unix_ms,
                session,
            },
        }))
    }
}

/// The log API's [`LiveRunLogLocator`] for `heph session attach`: the one
/// session it follows, answered from the `RunLog` records its stream already
/// carried ([`SessionFeed::run_log`]) instead of rescanning `events.ndjson`,
/// so the LAST LINE column can ask for every visible row each batch.
///
/// The rules are this module's, applied to that one session: only for the
/// home its `meta.json` names, only a live session, the newest record wins,
/// an `ExecuteEnd` after it or a file gone means ended, and `file` is opened
/// as [`find_run_log`] opens it.
///
/// "Live" is the lock probe the viewer made before it read the session
/// (`SessionEntry::live`), taken once: a session live then is followed until
/// it ends, and the viewer ends with it. One that was already over (crashed,
/// or ended) answers nothing, as [`lookup_run_log`] would: its replayed
/// records may name a file a crashed heph left behind, and that file is no
/// run's live log.
///
/// [`SessionFeed::run_log`]: super::SessionFeed::run_log
#[derive(Debug, Clone)]
pub struct FeedRunLogLocator {
    feed: super::SessionFeed,
    id: SessionId,
    /// The session's home, from its `meta.json`.
    home: PathBuf,
    /// Its `meta.json` lists [`CAPABILITY_RUN_LOGS`].
    records_run_logs: bool,
    /// Its lock was held when the viewer probed it, before reading it.
    live: bool,
}

impl FeedRunLogLocator {
    pub fn new(
        feed: super::SessionFeed,
        id: SessionId,
        meta: &SessionMeta,
        live: bool,
    ) -> Option<Self> {
        Some(Self {
            feed,
            id,
            home: PathBuf::from(meta.home_dir.as_deref()?),
            records_run_logs: meta.has_capability(CAPABILITY_RUN_LOGS),
            live,
        })
    }
}

impl LiveRunLogLocator for FeedRunLogLocator {
    fn locate(&self, home: &Path, addr: &str) -> anyhow::Result<Option<LocatedRunLog>> {
        if !self.live || home != self.home {
            return Ok(None);
        }
        let session = self.id.encode();
        if !self.records_run_logs {
            return Ok(Some(LocatedRunLog::Unsupported { session }));
        }
        let Some(run) = self.feed.run_log(addr) else {
            return Ok(None);
        };
        let ended = LocatedRunLog::Ended {
            record: run.record.clone(),
            at_unix_ms: run.at_unix_ms,
            session: session.clone(),
        };
        if run.ended {
            return Ok(Some(ended));
        }
        Ok(
            match open_live(home, &run.record).context("opening a live run log")? {
                Opened::File(_, file) => Some(LocatedRunLog::Live {
                    record: run.record,
                    at_unix_ms: run.at_unix_ms,
                    session,
                    file,
                }),
                Opened::Gone => Some(ended),
                Opened::Refused => None,
            },
        )
    }
}

/// What opening a record's `file` under the home came to.
#[derive(Debug)]
enum Opened {
    File(PathBuf, File),
    /// The file (or its directory) is not there: the run ended.
    Gone,
    /// Not a plain file under real `runs/` directories: absent.
    Refused,
}

/// Open `record.file` under `home` without following a symlink on the way.
fn open_live(home: &Path, record: &RunLogRecord) -> io::Result<Opened> {
    let Some(rel) = record.relative_file() else {
        return Ok(Opened::Refused);
    };
    let Some(parent) = rel.parent() else {
        return Ok(Opened::Refused);
    };
    // `runs/`, then `runs/<dir>/`: each a real directory.
    let mut dir = home.to_path_buf();
    for part in parent.components() {
        dir.push(part);
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(_) => return Ok(Opened::Refused),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Opened::Gone),
            Err(e) => return Err(e),
        }
    }
    let path = home.join(rel);
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Opened::Gone),
        // A symlink in the file's place.
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Ok(Opened::Refused),
        Err(e) => return Err(e),
    };
    if !file.metadata()?.file_type().is_file() {
        return Ok(Opened::Refused);
    }
    Ok(Opened::File(path, file))
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use hcore::events::BuildEventKind;
    use hcore::session_record::RunLogAnnouncer as _;

    use super::super::format::{META_FORMAT, SessionRecord};
    use super::super::{SessionHeader, SessionSlot, recorder};
    use super::*;

    const ADDR: &str = "//p:t";

    /// A session directory as a recording heph leaves it: `meta.json` for
    /// `home` with `caps`, `events.ndjson` holding `records`, and its lock,
    /// held while the returned file is.
    fn session(
        root: &Path,
        n: u8,
        home: &Path,
        caps: &[&str],
        records: &[SessionRecord],
    ) -> (PathBuf, File) {
        let id = SessionId::new(1_800_000_000_000 + u64::from(n), n);
        let dir = root.join(id.encode());
        std::fs::create_dir_all(&dir).expect("session dir");
        let lock = lockfile::acquire_owner_lock(&dir.join(files::LOCK)).expect("lock");
        recorder::write_atomically(&dir, files::META, &meta(id, home, caps)).expect("meta");
        append(&dir, records);
        (dir, lock)
    }

    fn meta(id: SessionId, home: &Path, caps: &[&str]) -> SessionMeta {
        SessionMeta {
            format: META_FORMAT,
            id,
            owner_pid: std::process::id(),
            started_unix_ms: id.started_unix_ms(),
            heph_version: "test".into(),
            argv: Vec::new(),
            cwd: None,
            workspace_root: None,
            home_dir: Some(home.to_string_lossy().into_owned()),
            label: String::new(),
            header: super::super::HeaderKind::Build,
            interactive: false,
            capabilities: caps.iter().map(|&c| c.to_owned()).collect(),
        }
    }

    fn append(dir: &Path, records: &[SessionRecord]) {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(files::EVENTS))
            .expect("events");
        for r in records {
            writeln!(f, "{}", serde_json::to_string(r).expect("encode")).expect("write");
        }
    }

    fn run_log(at: u64, addr: &str, hashin: &str, file: &str) -> SessionRecord {
        SessionRecord::session(
            at,
            SessionRecordKind::RunLog(Box::new(RunLogRecord {
                addr: addr.into(),
                hashin: hashin.into(),
                file: file.into(),
            })),
        )
    }

    fn execute_end(at: u64, addr: &str) -> SessionRecord {
        SessionRecord {
            at_unix_ms: at,
            kind: Some(BuildEventKind::ExecuteEnd {
                addr: addr.into(),
                error: None,
            }),
            session: None,
        }
    }

    fn execute_start(at: u64, addr: &str) -> SessionRecord {
        SessionRecord {
            at_unix_ms: at,
            kind: Some(BuildEventKind::ExecuteStart {
                addr: addr.into(),
                driver: "exec".into(),
                cache: true,
                declared: None,
                reserved: None,
                queued_ms: None,
                admitted: None,
                output_groups: Vec::new(),
            }),
            session: None,
        }
    }

    /// `<home>/<rel>` with `bytes`.
    fn live_file(home: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = home.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("runs dir");
        std::fs::write(&path, bytes).expect("live file");
        path
    }

    fn hashin_of(log: Option<SessionRunLog>) -> Option<(bool, String)> {
        log.map(|l| {
            (
                matches!(l, SessionRunLog::Live { .. }),
                l.record().hashin.clone(),
            )
        })
    }

    /// I4: the newest `RunLog` of the addr is the one; an `ExecuteEnd` after
    /// it, or its file gone, is "ended"; other addrs' records are not read
    /// as this one's.
    #[test]
    fn run_log_reader_follows_the_newest_run() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let root = tmp.path().join("sessions");
        live_file(&home, "runs/e/1.log", b"second\n");
        let (dir, _lock) = session(
            &root,
            1,
            &home,
            &[CAPABILITY_RUN_LOGS],
            &[
                run_log(10, ADDR, "h0", "runs/e/0.log"),
                execute_end(11, ADDR),
                run_log(12, ADDR, "h1", "runs/e/1.log"),
                run_log(13, "//p:tt", "other", "runs/e/2.log"),
                execute_end(14, "//p:tt"),
            ],
        );
        let log = session_run_log(&dir, &home, ADDR).expect("read");
        let Some(SessionRunLog::Live {
            path, at_unix_ms, ..
        }) = &log
        else {
            panic!("{log:?}");
        };
        assert_eq!((path.clone(), *at_unix_ms), (home.join("runs/e/1.log"), 12));
        // A near-miss addr never answers for this one.
        assert_eq!(
            hashin_of(session_run_log(&dir, &home, "//p:").expect("read")),
            None
        );

        // Its file gone while no end is recorded yet: ended, not absent.
        std::fs::remove_file(home.join("runs/e/1.log")).expect("unlink");
        assert_eq!(
            hashin_of(session_run_log(&dir, &home, ADDR).expect("read")),
            Some((false, "h1".into()))
        );
        live_file(&home, "runs/e/1.log", b"again\n");
        append(&dir, &[execute_end(15, ADDR)]);
        assert_eq!(
            hashin_of(session_run_log(&dir, &home, ADDR).expect("read")),
            Some((false, "h1".into())),
            "an ExecuteEnd after the record ends the run, file or not"
        );
        // A later run of the addr is live again.
        live_file(&home, "runs/e/3.log", b"third\n");
        append(&dir, &[run_log(16, ADDR, "h2", "runs/e/3.log")]);
        assert_eq!(
            hashin_of(session_run_log(&dir, &home, ADDR).expect("read")),
            Some((true, "h2".into()))
        );
    }

    /// I4: a record is trusted only while the session's lock is held; a
    /// session of another home (another checkout), or one whose heph does not
    /// record run logs, never answers.
    #[test]
    fn run_log_reader_trusts_only_live_sessions_of_this_home() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let other = tmp.path().join("other").join("home");
        let root = tmp.path().join("sessions");
        live_file(&home, "runs/e/0.log", b"x");
        let records = [run_log(10, ADDR, "h0", "runs/e/0.log")];

        let (dir, lock) = session(&root, 1, &home, &[CAPABILITY_RUN_LOGS], &records);
        assert!(session_run_log(&dir, &home, ADDR).expect("read").is_some());
        assert!(
            session_run_log(&dir, &other, ADDR).expect("read").is_none(),
            "another checkout's home"
        );
        drop(lock);
        assert!(
            session_run_log(&dir, &home, ADDR).expect("read").is_none(),
            "a session whose lock is free (crashed or ended)"
        );

        let (dir, lock) = session(&root, 2, &home, &["inflight"], &records);
        assert!(
            session_run_log(&dir, &home, ADDR).expect("read").is_none(),
            "a heph that does not advertise run_logs"
        );
        let sessions = SessionRoot::new(&root);
        assert!(
            find_run_log(&sessions, &home, ADDR)
                .expect("find")
                .is_none()
        );
        // ... but it is live and of this home: "records no live logs".
        let lookup = lookup_run_log(&sessions, &home, ADDR).expect("lookup");
        assert!(
            matches!(&lookup, RunLogLookup::Unsupported { session } if *session == SessionId::new(1_800_000_000_002, 2)),
            "{lookup:?}"
        );
        assert!(
            matches!(
                lookup_run_log(&sessions, &other, ADDR).expect("lookup"),
                RunLogLookup::None
            ),
            "another checkout's old heph says nothing"
        );
        // A trusted record of the addr wins over it.
        let (_dir3, lock3) = session(&root, 3, &home, &[CAPABILITY_RUN_LOGS], &records);
        assert!(matches!(
            lookup_run_log(&sessions, &home, ADDR).expect("lookup"),
            RunLogLookup::Found(_)
        ));
        drop(lock3);
        drop(lock);
        assert!(
            matches!(
                lookup_run_log(&sessions, &home, ADDR).expect("lookup"),
                RunLogLookup::None
            ),
            "an ended old heph says nothing"
        );
    }

    /// A run going on in a session that records run logs, before its first
    /// byte (no file, no `RunLog` record yet), is "running, nothing written":
    /// never "records no live logs" because some other live session of the
    /// workspace runs an older heph.
    #[test]
    fn a_started_run_that_wrote_nothing_is_not_unsupported() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let root = tmp.path().join("sessions");
        let sessions = SessionRoot::new(&root);
        // An older heph, live on the same workspace.
        let (_old, _old_lock) = session(&root, 1, &home, &["inflight"], &[]);
        let (dir, _lock) = session(
            &root,
            2,
            &home,
            &[CAPABILITY_RUN_LOGS],
            &[execute_start(10, ADDR)],
        );
        let lookup = lookup_run_log(&sessions, &home, ADDR).expect("lookup");
        let ours = SessionId::new(1_800_000_000_002, 2);
        assert!(
            matches!(&lookup, RunLogLookup::Started { session, at_unix_ms: 10 } if *session == ours),
            "{lookup:?}"
        );
        // Not another addr's start, nor a near miss.
        assert!(matches!(
            lookup_run_log(&sessions, &home, "//p:").expect("lookup"),
            RunLogLookup::Unsupported { .. }
        ));

        // Its first byte: the record makes it live.
        live_file(&home, "runs/e/0.log", b"x");
        append(&dir, &[run_log(11, ADDR, "h0", "runs/e/0.log")]);
        assert!(matches!(
            lookup_run_log(&sessions, &home, ADDR).expect("lookup"),
            RunLogLookup::Found(_)
        ));
        // Ended, then a new run that has written nothing: started again,
        // over the older record.
        append(&dir, &[execute_end(12, ADDR), execute_start(13, ADDR)]);
        assert!(matches!(
            lookup_run_log(&sessions, &home, ADDR).expect("lookup"),
            RunLogLookup::Started { at_unix_ms: 13, .. }
        ));
        // That run ended without a byte: the older record, ended.
        append(&dir, &[execute_end(14, ADDR)]);
        let lookup = lookup_run_log(&sessions, &home, ADDR).expect("lookup");
        assert!(
            matches!(&lookup, RunLogLookup::Found(f) if matches!(f.log, SessionRunLog::Ended { .. })),
            "{lookup:?}"
        );
        // A record written before its run's start is still that run's.
        append(
            &dir,
            &[
                run_log(15, ADDR, "h1", "runs/e/0.log"),
                execute_start(16, ADDR),
            ],
        );
        assert!(matches!(
            lookup_run_log(&sessions, &home, ADDR).expect("lookup"),
            RunLogLookup::Found(FoundRunLog {
                log: SessionRunLog::Live { .. },
                ..
            })
        ));
    }

    /// Attached to a session that was already over when the viewer probed
    /// it, the LAST LINE locator names no live file: the replayed record's
    /// file is still on disk (a crashed heph leaves it), and it is no run's
    /// live log. The same records for a live session answer `Live`; another
    /// home never answers.
    #[test]
    fn attached_dead_session_logs_read_as_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        live_file(&home, "runs/e/0.log", b"left behind\n");
        let id = SessionId::new(1_800_000_000_001, 1);
        let meta = meta(id, &home, &[CAPABILITY_RUN_LOGS]);
        let feed = super::super::SessionFeed::default();
        drop(feed.route(run_log(10, ADDR, "h0", "runs/e/0.log")));
        let locate = |live: bool, home: &Path| {
            FeedRunLogLocator::new(feed.clone(), id, &meta, live)
                .expect("meta names a home")
                .locate(home, ADDR)
                .expect("locate")
        };

        let found = locate(true, &home);
        assert!(
            matches!(&found, Some(LocatedRunLog::Live { record, .. }) if record.hashin == "h0"),
            "{found:?}"
        );
        assert!(locate(true, &tmp.path().join("other")).is_none());
        let dead = locate(false, &home);
        assert!(dead.is_none(), "a dead session named a log: {dead:?}");
    }

    /// Across sessions, the newest record wins, and the session that recorded
    /// it is named.
    #[test]
    fn find_run_log_picks_the_newest_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let root = tmp.path().join("sessions");
        live_file(&home, "runs/a/0.log", b"older");
        live_file(&home, "runs/b/0.log", b"newer");
        let (_d1, _l1) = session(
            &root,
            1,
            &home,
            &[CAPABILITY_RUN_LOGS],
            &[run_log(20, ADDR, "h-new", "runs/b/0.log")],
        );
        let (_d2, _l2) = session(
            &root,
            2,
            &home,
            &[CAPABILITY_RUN_LOGS],
            &[run_log(10, ADDR, "h-old", "runs/a/0.log")],
        );
        let found = find_run_log(&SessionRoot::new(&root), &home, ADDR)
            .expect("find")
            .expect("found");
        assert_eq!(found.session, SessionId::new(1_800_000_000_001, 1));
        assert_eq!(found.log.record().hashin, "h-new");
        assert!(
            find_run_log(&SessionRoot::new(tmp.path().join("none")), &home, ADDR)
                .expect("a missing root is no session")
                .is_none()
        );
    }

    /// A `RunLog` whose `file` has `..`, is absolute, or resolves through a
    /// symlink on the way (`runs/`, `runs/<dir>/`, the file itself) reads as
    /// absent and is never opened; a FIFO in its place neither hangs the
    /// reader nor reads as a log.
    #[test]
    fn run_log_file_field_cannot_escape_home() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("sessions");
        let outside = tmp.path().join("outside");
        live_file(&outside, "e/0.log", b"secret");

        let absent = |n: u8, home: &Path, file: &str| {
            let (dir, _lock) = session(
                &root,
                n,
                home,
                &[CAPABILITY_RUN_LOGS],
                &[run_log(10, ADDR, "h", file)],
            );
            let log = session_run_log(&dir, home, ADDR).expect("read");
            assert!(log.is_none(), "{file:?} under {}: {log:?}", home.display());
        };

        let home = tmp.path().join("h1");
        live_file(&home, "runs/e/0.log", b"mine");
        let escaped = outside.join("e/0.log");
        absent(1, &home, "runs/../../outside/e/0.log");
        absent(2, &home, &escaped.to_string_lossy());
        absent(3, &home, "runs/e/../e/0.log");

        // `runs/` itself a symlink out of the home.
        let home = tmp.path().join("h2");
        std::fs::create_dir_all(&home).expect("home");
        std::os::unix::fs::symlink(&outside, home.join("runs")).expect("symlink");
        absent(4, &home, "runs/e/0.log");

        // `runs/<dir>/` a symlink.
        let home = tmp.path().join("h3");
        std::fs::create_dir_all(home.join("runs")).expect("runs");
        std::os::unix::fs::symlink(outside.join("e"), home.join("runs/e")).expect("symlink");
        absent(5, &home, "runs/e/0.log");

        // The file a symlink.
        let home = tmp.path().join("h4");
        std::fs::create_dir_all(home.join("runs/e")).expect("runs");
        std::os::unix::fs::symlink(&escaped, home.join("runs/e/0.log")).expect("symlink");
        absent(6, &home, "runs/e/0.log");

        // A FIFO: opened non-blocking, refused as not a regular file.
        let home = tmp.path().join("h5");
        std::fs::create_dir_all(home.join("runs/e")).expect("runs");
        let fifo = home.join("runs/e/0.log");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).expect("cstr");
        // SAFETY: `c` is a valid NUL-terminated path for the call.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");
        absent(7, &home, "runs/e/0.log");
    }

    /// Past the 64 MiB cap (`Truncated` written), a run that starts still
    /// writes its `RunLog` record through the session slot, and a follower
    /// finds it; once the session ends, it reads as absent.
    #[test]
    fn run_log_record_outlives_cap() {
        let user = tempfile::tempdir().expect("tempdir");
        let ws = tempfile::tempdir().expect("tempdir");
        let home = ws.path().join(".heph");
        let slot = Arc::new(SessionSlot::new());
        let root = SessionRoot::from_env(None, Some(user.path().as_os_str()));
        let sessions = root.clone().expect("root").expect("recording on");
        slot.configure(root, vec!["heph".into()]);
        slot.set_workspace(ws.path(), ws.path(), &home);
        let opened = slot
            .open(
                SessionHeader {
                    kind: super::super::HeaderKind::Build,
                    label: "run".into(),
                },
                false,
            )
            .expect("a session opened");
        // Build events, not log lines: those outlive the cap.
        let big = "x".repeat(1024 * 1024);
        let laps = recorder::EVENTS_CAP / (1024 * 1024) + 6;
        for _ in 0..laps {
            opened.tap.event(hcore::events::BuildEvent {
                at_unix_ms: 1,
                kind: hcore::events::BuildEventKind::ResultStart { addr: big.clone() },
            });
        }
        // A gated target past the cap: its prompt carries the notice's
        // sources, which an attached viewer renders the notice again from.
        // Recorded before the RunLog, so it has landed once the RunLog is
        // found.
        let sources = vec![hcore::approval_notice::NoticeSource {
            addr: "//a:plan".into(),
            origin: hcore::approval_notice::NoticeOrigin::Cache {
                addr: "//a:plan".into(),
                revision: "h1".into(),
                hashout: "o1".into(),
            },
        }];
        opened.tap.view(
            2,
            SessionRecordKind::Approval {
                prompt: Some(Box::new(super::super::ApprovalPrompt {
                    addr: ADDR.into(),
                    queued_behind: 0,
                    notices: vec![super::super::ApprovalPromptNotice {
                        name: "plan".into(),
                        sources: sources.clone(),
                    }],
                })),
                standing: None,
            },
        );
        live_file(&home, "runs/e/0.log", b"out\n");
        slot.run_log_created(RunLogRecord {
            addr: ADDR.into(),
            hashin: "h0".into(),
            file: "runs/e/0.log".into(),
        });

        let deadline = Instant::now() + Duration::from_secs(30);
        let found = loop {
            if let Some(found) = find_run_log(&sessions, &home, ADDR).expect("find") {
                break found;
            }
            assert!(Instant::now() < deadline, "the RunLog record never landed");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(found.session, opened.id);
        assert!(
            matches!(&found.log, SessionRunLog::Live { record, .. } if record.hashin == "h0"),
            "{found:?}"
        );
        let dir = slot.dir().expect("dir");
        let file = File::open(dir.join(files::EVENTS)).expect("events");
        let mut r = RecordReader::new(file);
        let kinds: Vec<_> = std::iter::from_fn(|| r.next_record().expect("read"))
            .filter_map(|rec| rec.session)
            .filter(|k| !matches!(k, SessionRecordKind::Log { .. }))
            .collect();
        let [
            SessionRecordKind::Truncated,
            SessionRecordKind::Approval {
                prompt: Some(prompt),
                ..
            },
            SessionRecordKind::RunLog(_),
        ] = kinds.as_slice()
        else {
            panic!("{kinds:?}");
        };
        assert_eq!(
            prompt.notices[0].sources, sources,
            "the notice's sources are recorded past the cap"
        );

        slot.finish(Some(0));
        assert!(
            find_run_log(&sessions, &home, ADDR)
                .expect("find")
                .is_none(),
            "an ended session's records are not trusted"
        );
    }
}
