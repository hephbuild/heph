//! The recording side, end to end through the files it writes.
#![expect(
    clippy::unwrap_in_result,
    reason = "test `Write` impls; a poisoned test mutex should fail the test"
)]

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use hcore::events::{BuildEvent, BuildEventKind};

use super::cleanup::{self, Retention};
use super::recorder::{Faults, Gate, RecorderStart};
use super::*;
use crate::tui::app::{App, AppContext, CIAppView, TUIAppView};
use crate::tui::progress::BuildState;

pub(super) fn events() -> Vec<BuildEvent> {
    let ev = |at, kind| BuildEvent {
        at_unix_ms: at,
        kind,
    };
    vec![
        ev(
            1,
            BuildEventKind::Matched {
                addrs: vec!["//a:ok".into(), "//a:bad".into()],
                complete: true,
            },
        ),
        ev(
            2,
            BuildEventKind::ResultStart {
                addr: "//a:ok".into(),
            },
        ),
        ev(
            2,
            BuildEventKind::ResultStart {
                addr: "//a:bad".into(),
            },
        ),
        ev(
            3,
            BuildEventKind::ExecuteStart {
                output_groups: Vec::new(),
                log_not_streamed: false,
                addr: "//a:ok".into(),
                driver: "exec".into(),
                cache: true,
                declared: None,
                reserved: None,
                queued_ms: None,
                admitted: None,
            },
        ),
        ev(
            4,
            BuildEventKind::ResultLockWaitStart {
                addr: "//a:bad".into(),
                holder_pid: Some(77),
                in_use_by_readers: false,
            },
        ),
        ev(
            5,
            BuildEventKind::ResultEnd {
                spans: Vec::new(),
                left_behind: None,
                cancelled: false,
                addr: "//a:bad".into(),
                error: Some("boom".into()),
                upstream_of: None,
                exit_status: None,
                exit_code: None,
                log_tail: None,
                timed_out: None,
            },
        ),
    ]
}

/// A build event of about 1 MiB, to reach the cap with (a `Log` outlives it).
fn big_event() -> BuildEvent {
    BuildEvent {
        at_unix_ms: 1,
        kind: BuildEventKind::ResultEnd {
            spans: Vec::new(),
            left_behind: None,
            cancelled: false,
            addr: "//a:big".into(),
            error: Some("x".repeat(1024 * 1024)),
            upstream_of: None,
            exit_status: None,
            exit_code: None,
            log_tail: None,
            timed_out: None,
        },
    }
}

/// A record the machine sampler wrote: a test that counts what the build
/// sent leaves these out, since how many land depends on the clock.
pub(super) fn is_machine(record: &SessionRecord) -> bool {
    matches!(
        record.session,
        Some(SessionRecordKind::MachineInfo(_) | SessionRecordKind::MachineSample(_))
    )
}

/// The local recording in `dir`, opened as a reader would (the sessions root
/// is its parent).
pub(super) fn open(dir: &Path) -> store::Opened {
    store::open_local(dir.parent().expect("a sessions root"), dir).expect("open the recording")
}

/// Every record in `dir`'s recording, in order, and how many did not decode.
pub(super) fn read_records(dir: &Path) -> (Vec<SessionRecord>, u64) {
    let mut r = store::RecordCursor::new(open(dir));
    let recs = std::iter::from_fn(|| r.next_record().expect("read")).collect();
    (recs, r.skipped())
}

/// The recording's `status` row, decoded, and its JSON size.
pub(super) fn read_status(dir: &Path) -> Option<(usize, SessionSnapshot)> {
    let json = store::read_status(&open(dir).conn).expect("read status")?;
    Some((
        json.len(),
        SessionSnapshot::decode(json.as_bytes()).expect("decode status"),
    ))
}

/// The recording's session row.
pub(super) fn read_meta(dir: &Path) -> SessionMeta {
    store::read_meta(&open(dir).conn).expect("read the session row")
}

/// A read-write connection on `dir`'s recording, for a test that alters it
/// as another heph (or damage) would.
pub(super) fn alter(dir: &Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(dir.join(store::FILE)).expect("open rw");
    conn.busy_timeout(store::BUSY_TIMEOUT)
        .expect("busy timeout");
    conn
}

/// A session row for `id`, as an owner writes it.
pub(super) fn meta_for(id: SessionId) -> SessionMeta {
    SessionMeta {
        format: META_FORMAT,
        id,
        owner_pid: std::process::id(),
        started_unix_ms: id.started_unix_ms(),
        heph_version: hcore::version::current().to_owned(),
        argv: vec!["heph".into(), "run".into()],
        cwd: None,
        workspace_root: None,
        home_dir: None,
        label: "run //a:...".into(),
        header: HeaderKind::Build,
        interactive: false,
        capabilities: vec![CAPABILITY_INFLIGHT.to_owned()],
    }
}

/// `ev` as the record the recorder writes for it.
pub(super) fn event_record(ev: BuildEvent) -> SessionRecord {
    SessionRecord {
        at_unix_ms: ev.at_unix_ms,
        kind: Some(ev.kind),
        session: None,
    }
}

/// A recording in `dir` holding `records`, left as a `kill -9` leaves one:
/// committed, unsealed (WAL mode), no `End`. The caller owns the lock.
pub(super) fn unsealed_recording(dir: &Path, id: SessionId, records: &[SessionRecord]) {
    let (conn, _rw) =
        store::create(dir, &meta_for(id), &store::Provenance::default(), false).expect("create");
    let mut writer = store::Writer::new(conn, id);
    for r in records {
        let body = serde_json::to_string(r).expect("encode");
        writer.insert(r, &body).expect("insert");
    }
    writer.commit().expect("commit");
}

pub(super) fn start() -> SessionStart {
    SessionStart {
        header: SessionHeader {
            kind: HeaderKind::Build,
            label: "run //a:...".into(),
        },
        argv: vec!["heph".into(), "run".into()],
        ..SessionStart::default()
    }
}

/// Folding the recorded records gives the view's state, and the session ends with
/// `End`, a final status carrying the exit, and a released lock.
#[test]
fn recorder_round_trip_matches_live_view() {
    let root = tempfile::tempdir().expect("tempdir");
    let session = Session::open(&SessionRoot::new(root.path()), start()).expect("open");
    let dir = session.dir().to_path_buf();
    let mut live = BuildState::new();
    let tap = session.tap();
    for ev in events() {
        live.apply(&ev);
        tap.event(ev);
    }
    drop(tap.log(1, "INFO", "hello\u{1b}[31m".into()));
    session.finish(Some(1), false);

    let (records, skipped) = read_records(&dir);
    assert_eq!(skipped, 0);
    let mut replayed = BuildState::new();
    for ev in records.iter().filter_map(SessionRecord::build_event) {
        replayed.apply(&ev);
    }
    assert_eq!(replayed.snapshot(), live.snapshot(), "recorded == folded");
    assert!(records.iter().any(|r| r.session
        == Some(SessionRecordKind::Log {
            level: "INFO".into(),
            msg: "hello\u{1b}[31m".into(),
            seq: 1,
        })));
    assert_eq!(
        records.last().and_then(|r| r.session.clone()),
        Some(SessionRecordKind::End {
            exit_code: Some(1),
            panicked: false
        })
    );
    let (_, status) = read_status(&dir).expect("a status row");
    assert_eq!(status.failed, vec!["//a:bad".to_owned()]);
    assert_eq!(
        status.lock_waits.first().and_then(|w| w.holder_pid),
        Some(77)
    );
    assert_eq!(status.end.as_ref().and_then(|e| e.exit_code), Some(1));
    let opened = open(&dir);
    assert!(!opened.live && !opened.is_copy(), "sealed: read in place");
    let end = store::read_row_end(&opened.conn).expect("row end");
    assert!(end.sealed && !end.truncated, "{end:?}");
    drop(opened);
    let meta = read_meta(&dir);
    assert_eq!(meta.id, session.id());
    assert_eq!(meta.owner_pid, std::process::id());
    assert!(meta.has_capability(CAPABILITY_INFLIGHT));
    assert!(
        !lockfile::is_held(&dir.join(files::LOCK)).expect("probe"),
        "released after End"
    );
}

/// I6: the root, the session directory and every file are user-only.
#[test]
fn files_are_user_only() {
    let root = tempfile::tempdir().expect("tempdir");
    let sessions = root.path().join("sessions");
    let session = Session::open(&SessionRoot::new(&sessions), start()).expect("open");
    session.tap().event(events().remove(0));
    session.finish(Some(0), false);
    let mode = |p: &Path| std::fs::metadata(p).expect("stat").permissions().mode() & 0o777;
    assert_eq!(mode(&sessions), 0o700);
    assert_eq!(mode(session.dir()), 0o700);
    for f in [files::LOCK, store::FILE] {
        assert_eq!(mode(&session.dir().join(f)), 0o600, "{f}");
    }
    // Whatever else the run left (a `-wal`, a `-shm` the seal did not
    // remove) is user-only too.
    for entry in std::fs::read_dir(session.dir()).expect("read dir") {
        let path = entry.expect("entry").path();
        assert_eq!(mode(&path), 0o600, "{}", path.display());
    }
}

/// A lock that cannot be taken (NFS `ENOLCK`) fails the open, says which step
/// failed, and leaves no session directory behind — every run, not a leak each.
#[test]
fn failed_lock_leaves_no_directory() {
    let root = tempfile::tempdir().expect("tempdir");
    let sessions = root.path().join("sessions");
    let faults = Faults {
        fail_lock: true,
        ..Faults::default()
    };
    for _ in 0..3 {
        let err = Session::open_with(
            &SessionRoot::new(&sessions),
            start(),
            Retention::default(),
            faults.clone(),
        )
        .expect_err("the lock fails");
        assert!(format!("{err:#}").contains("lock"), "{err:#}");
    }
    assert_eq!(entries(&sessions), 0, "a failed open leaked a directory");
}

/// An ended session whose `status` row a newer heph wrote is listed as
/// unreadable, never as having no status (which reads `lost`).
#[test]
fn newer_status_is_unreadable_not_missing() {
    let root = tempfile::tempdir().expect("tempdir");
    let session = Session::open(&SessionRoot::new(root.path()), start()).expect("open");
    session.finish(Some(0), false);
    alter(session.dir())
        .execute(
            "UPDATE status SET snapshot = ?1 WHERE id = 1",
            [format!(r#"{{"version": {}}}"#, SNAPSHOT_VERSION + 1)],
        )
        .expect("write");
    let listed = source::list_dir(root.path()).expect("list");
    assert_eq!(listed.len(), 1);
    assert!(!listed[0].live);
    assert!(listed[0].status.is_none());
    assert!(listed[0].status_unreadable);
}

/// A read-only `HOME`: the root cannot be created. The open fails naming the
/// directory, and nothing is written. Root ignores the mode bits, so this
/// says nothing there.
#[test]
fn unwritable_root_fails_cleanly() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root ignores directory modes");
        return;
    }
    let home = tempfile::tempdir().expect("tempdir");
    let dot_heph = home.path().join(".heph");
    std::fs::create_dir(&dot_heph).expect("mkdir");
    std::fs::set_permissions(&dot_heph, std::fs::Permissions::from_mode(0o500)).expect("chmod");
    let sessions = dot_heph.join("sessions");
    let err = Session::open(&SessionRoot::new(&sessions), start()).expect_err("unwritable");
    std::fs::set_permissions(&dot_heph, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    assert!(format!("{err:#}").contains("creating"), "{err:#}");
    assert!(!sessions.exists());
}

/// I2: the lock is held while the session lives, released by `finish`, and
/// its fd is close-on-exec, so no child keeps it alive. (The recording's
/// SQLite fds are SQLite's, opened `O_CLOEXEC`; a child holding one would
/// not hold the lock.)
#[test]
fn lock_released_on_exit_and_not_inherited() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lock = lockfile::acquire_owner_lock(&dir.path().join(files::LOCK)).expect("lock");
    // SAFETY: querying the flags of an fd this test owns.
    let flags = unsafe { libc::fcntl(lock.as_raw_fd(), libc::F_GETFD) };
    assert!(
        flags >= 0 && flags & libc::FD_CLOEXEC != 0,
        "the lock fd is CLOEXEC"
    );
    drop(lock);

    let root = tempfile::tempdir().expect("tempdir");
    let session = Session::open(&SessionRoot::new(root.path()), start()).expect("open");
    let lock_path = session.dir().join(files::LOCK);
    assert!(lockfile::is_held(&lock_path).expect("probe"));
    // A child spawned now does not inherit it: the lock frees on finish even
    // while the child lives.
    let mut child = std::process::Command::new("sleep")
        .arg("5")
        .spawn()
        .expect("spawn");
    session.finish(Some(0), false);
    assert!(
        !lockfile::is_held(&lock_path).expect("probe"),
        "child holds nothing"
    );
    child.kill().expect("kill");
    child.wait().expect("wait");
}

/// A session under `root` whose recorder runs with `faults`.
pub(super) fn recorder_with(root: &Path, faults: Faults) -> Session {
    Session::open_with(
        &SessionRoot::new(root),
        start(),
        Retention::default(),
        faults,
    )
    .expect("open")
}

/// A test's caps: [`recorder::FULL_CAP`] and [`recorder::CEILING`] small
/// enough to reach with a few [`big_event`]s.
fn capped(full_cap: u64, ceiling: u64) -> Faults {
    Faults {
        full_cap: Some(full_cap),
        ceiling: Some(ceiling),
        ..Faults::default()
    }
}

const MIB: u64 = 1024 * 1024;

/// A `MakeWriter` collecting log output, to count warnings.
#[derive(Clone, Default)]
pub(super) struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Captured {
    /// The log, once every subscriber writing to it is gone, so nothing more
    /// can land in it.
    ///
    /// The recorder thread logs through its own clone of the test's
    /// dispatcher, and sets the stopped latch *before* its `warn!` (A9): a
    /// test that sees `is_stopped()` can read the log before the warning is
    /// in it. The clone is dropped only when the thread exits, so the
    /// subscriber (and the writer it holds) outliving the test's own
    /// `with_default` is exactly "the recorder may still log".
    #[track_caller]
    pub(super) fn settled_log(&self) -> String {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while Arc::strong_count(&self.0) > 1 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            Arc::strong_count(&self.0),
            1,
            "the recorder thread is still running"
        );
        String::from_utf8(self.0.lock().expect("lock").clone()).expect("utf8")
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

/// I1/A9: a full disk stops the recording with exactly one warning, the
/// senders keep going without error, and finish still releases the lock.
#[test]
fn enospc_warns_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        .finish();
    let lock = tracing::subscriber::with_default(subscriber, || {
        let rec = recorder_with(
            dir.path(),
            Faults {
                fail_writes: true,
                ..Faults::default()
            },
        );
        let tap = rec.tap();
        for _ in 0..2000 {
            for ev in events() {
                tap.event(ev);
            }
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !tap.is_stopped() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(tap.is_stopped(), "the failure latched");
        for ev in events() {
            tap.event(ev);
        }
        rec.recorder.finish(Some(0), false, Duration::from_secs(1));
        rec.dir().join(files::LOCK)
    });
    let log = captured.settled_log();
    assert_eq!(log.matches("WARN").count(), 1, "{log}");
    assert!(!lockfile::is_held(&lock).expect("probe"));
}

/// I1: a transaction past [`recorder::COMMIT_BYTES`] commits without waiting
/// for the flush tick, so what the exiting process waits on is bounded. The
/// flush and status ticks are an hour here (the first status upsert, which
/// commits before any event, holds no record), so records a reader sees were
/// committed by size; with the transaction left to the ticks, it sees none.
#[test]
fn a_full_transaction_commits_before_the_tick() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = recorder_with(
        dir.path(),
        Faults {
            ticks: Some(Duration::from_secs(3600)),
            commit_bytes: Some(1),
            ..Faults::default()
        },
    );
    let db = session.dir().join(store::FILE);
    let tap = session.tap();
    let sent = events();
    let want = i64::try_from(sent.len()).expect("count");
    for ev in sent {
        tap.event(ev);
    }
    let reader = rusqlite::Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .expect("a read-only connection");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut seen = 0;
    while seen < want && std::time::Instant::now() < deadline {
        seen = reader
            .query_row("SELECT count(*) FROM records", [], |r| r.get(0))
            .expect("count records");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        seen, want,
        "every record committed by size, before the tick"
    );
    drop(reader);
    session.finish(Some(0), false);
}

/// I1: a write that fails half way through a batch (the record went in, the
/// explorer row it completes did not) lands none of that batch. A trigger
/// refuses the `spans` row of `//boom`'s `ResultEnd`, after its record was
/// inserted in the same transaction. What is on disk afterwards is whole
/// batches: `//boom`'s `ResultEnd` record is not there, and every
/// `ResultEnd` record that is has its `heph.result` span. A fail path that
/// committed what it had (or a writer in autocommit) leaves the record
/// without its span and fails here.
#[test]
fn failed_derived_insert_rolls_back_the_batch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        .finish();
    let session_dir = tracing::subscriber::with_default(subscriber, || {
        let rec = recorder_with(dir.path(), Faults::default());
        let tap = rec.tap();
        let result = |addr: &str| {
            tap.event(BuildEvent {
                at_unix_ms: hcore::events::now_unix_ms(),
                kind: BuildEventKind::ResultStart { addr: addr.into() },
            });
            tap.event(BuildEvent {
                at_unix_ms: hcore::events::now_unix_ms(),
                kind: BuildEventKind::ResultEnd {
                    addr: addr.into(),
                    error: None,
                    upstream_of: None,
                    exit_status: None,
                    exit_code: None,
                    log_tail: None,
                    timed_out: None,
                    cancelled: false,
                    left_behind: None,
                    spans: Vec::new(),
                },
            });
        };
        result("//p:before");
        tap.flush(Duration::from_secs(10)).expect("flush");
        alter(rec.dir())
            .execute_batch(
                "CREATE TRIGGER boom BEFORE INSERT ON spans WHEN NEW.addr = '//boom'
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )
            .expect("the trigger");
        result("//p:same_batch");
        result("//boom");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !tap.is_stopped() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(tap.is_stopped(), "the failure latched");
        rec.recorder.finish(Some(0), false, Duration::from_secs(1));
        rec.dir().to_path_buf()
    });
    let log = captured.settled_log();
    assert_eq!(log.matches("WARN").count(), 1, "{log}");
    assert!(log.contains("injected"), "{log}");

    let opened = open(&session_dir);
    let ends: Vec<String> = opened
        .conn
        .prepare("SELECT addr FROM records WHERE kind = 'ResultEnd' ORDER BY seq")
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    let spans: Vec<String> = opened
        .conn
        .prepare("SELECT addr FROM spans WHERE name = 'heph.result' ORDER BY span_id")
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert!(ends.iter().any(|a| a == "//p:before"), "{ends:?}");
    assert!(!ends.iter().any(|a| a == "//boom"), "{ends:?}");
    assert_eq!(ends, spans, "every committed ResultEnd has its span");
}

/// I1/A8: a recorder that falls more than its cap of messages behind stops
/// with one warning naming it, every later send is dropped without blocking,
/// and finish still releases the lock. (The cap lowered to 8 here; the write
/// gate keeps the thread from draining.)
#[test]
fn falling_behind_the_pending_cap_stops_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        .finish();
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let lock = tracing::subscriber::with_default(subscriber, || {
        let rec = recorder_with(
            dir.path(),
            Faults {
                gate: Some(Arc::clone(&gate)),
                max_pending: Some(8),
                ..Faults::default()
            },
        );
        let tap = rec.tap();
        let mut sent = 0;
        while !tap.is_stopped() && sent < 100 {
            for ev in events() {
                tap.event(ev);
                sent += 1;
            }
        }
        assert!(tap.is_stopped(), "a full queue latched the stop");
        assert!(sent < 100, "stopped after {sent} sends, cap 8");
        for ev in events() {
            tap.event(ev);
        }
        let (open, cv) = &*gate;
        *open.lock().expect("gate") = true;
        cv.notify_all();
        rec.recorder.finish(Some(0), false, Duration::from_secs(5));
        rec.dir().join(files::LOCK)
    });
    let log = captured.settled_log();
    assert_eq!(log.matches("WARN").count(), 1, "{log}");
    assert!(log.contains("fell more than"), "{log}");
    assert!(!lockfile::is_held(&lock).expect("probe"));
}

/// A send after `finish` (a log line on the way out: `--pprof-cpu`'s, the
/// telemetry's) is dropped silently. `End` is counted like any other message,
/// so the pending count cannot underflow into a false "fell behind" stop.
#[test]
fn send_after_finish_is_dropped_silently() {
    let dir = tempfile::tempdir().expect("tempdir");
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let rec = recorder_with(dir.path(), Faults::default());
        let tap = rec.tap();
        assert!(tap.log(1, "INFO", "before".into()).is_none());
        rec.recorder.finish(Some(0), false, Duration::from_secs(5));
        assert!(tap.log(2, "INFO", "after".into()).is_none());
        for ev in events() {
            tap.event(ev);
        }
        assert!(
            !tap.is_stopped(),
            "a send after finish stopped the recording"
        );
    });
    let log = captured.settled_log();
    assert_eq!(log.matches("WARN").count(), 0, "{log}");
}

/// I1/A8: a write that never returns blocks neither the sender nor the
/// finisher past its timeout.
#[test]
fn recorder_blocked_write_does_not_block_app() {
    let dir = tempfile::tempdir().expect("tempdir");
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let rec = recorder_with(
        dir.path(),
        Faults {
            gate: Some(Arc::clone(&gate)),
            ..Faults::default()
        },
    );
    let tap = rec.tap();
    let started = std::time::Instant::now();
    for _ in 0..5000 {
        for ev in events() {
            tap.event(ev);
        }
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "sends never block"
    );
    let finishing = std::time::Instant::now();
    rec.recorder
        .finish(Some(0), false, Duration::from_millis(200));
    assert!(
        finishing.elapsed() < Duration::from_secs(2),
        "finish is bounded"
    );
    // I2, I3: the thread still holds the write connection and will commit
    // `End`, so the lock stays held until it has.
    let lock_path = rec.dir().join(files::LOCK);
    assert!(
        lockfile::is_held(&lock_path).expect("probe"),
        "a timed-out finish released the lock under a live writer"
    );
    let (lock, cv) = &*gate;
    *lock.lock().expect("lock") = true;
    cv.notify_all();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while lockfile::is_held(&lock_path).expect("probe") {
        assert!(
            std::time::Instant::now() < deadline,
            "the recorder thread never released the lock"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let opened = open(rec.dir());
    assert!(!opened.is_copy(), "the thread sealed it");
    let end = store::read_row_end(&opened.conn).expect("end");
    assert!(end.sealed, "{end:?}");
    drop(opened);
    let (read, _) = read_records(rec.dir());
    assert!(
        matches!(
            read.last().and_then(|r| r.session.as_ref()),
            Some(SessionRecordKind::End { .. })
        ),
        "End landed last"
    );
}

/// A finish that timed out leaves the lock to the recorder thread. If the
/// thread then stops before it reaches `End` (here: the hung write fails), it
/// releases the lock as it exits, so a process that lives on after `finish`
/// does not keep its session looking live. The session reads as lost: no
/// `End` landed.
#[test]
fn a_stop_after_a_timed_out_finish_releases_the_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let rec = recorder_with(
        dir.path(),
        Faults {
            gate: Some(Arc::clone(&gate)),
            fail_writes: true,
            ..Faults::default()
        },
    );
    let tap = rec.tap();
    for ev in events() {
        tap.event(ev);
    }
    rec.recorder
        .finish(Some(0), false, Duration::from_millis(200));
    let lock_path = rec.dir().join(files::LOCK);
    assert!(
        lockfile::is_held(&lock_path).expect("probe"),
        "a timed-out finish released the lock under a live writer"
    );
    // The hung write now fails: the thread stops without handling `End`.
    let (lock, cv) = &*gate;
    *lock.lock().expect("lock") = true;
    cv.notify_all();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while lockfile::is_held(&lock_path).expect("probe") {
        assert!(
            std::time::Instant::now() < deadline,
            "the stopped recorder thread kept the lock while the process lives on"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // `rec` (and so the recorder's shared state) is still alive here.
    drop(rec);
}

/// A recorder that stopped (here: it fell behind its pending cap while a
/// write hangs) still has its thread, and the thread its write connection,
/// until that write returns. `finish` must not free the lock under it: the
/// connection's close checkpoints and deletes the `-wal`, so a viewer of
/// another process that saw the lock free could copy a half-checkpointed
/// `session.db` with its `-wal` gone. The lock goes once the thread has
/// exited, its connection and registration closed first.
#[test]
fn finish_after_a_stop_keeps_the_lock_until_the_thread_exits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let gate: Gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let rec = recorder_with(
        dir.path(),
        Faults {
            gate: Some(Arc::clone(&gate)),
            max_pending: Some(8),
            ..Faults::default()
        },
    );
    let tap = rec.tap();
    let mut evs = events().into_iter().cycle();
    // The thread takes the first event into its write, which the gate holds,
    // before the stop is latched: a stop latched first would end the thread
    // before it wrote anything.
    tap.event(evs.next().expect("an event"));
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while tap.pending() > 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the thread never took the first event"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    for ev in evs.by_ref().take(100) {
        tap.event(ev);
        if tap.is_stopped() {
            break;
        }
    }
    assert!(tap.is_stopped(), "a full queue latched the stop");
    rec.recorder
        .finish(Some(0), false, Duration::from_millis(200));
    let lock_path = rec.dir().join(files::LOCK);
    let db = rec.dir().join(store::FILE);
    assert!(
        store::rw_open::is_open(&db).expect("stat"),
        "the stopped thread's write is still held at the gate"
    );
    assert!(
        lockfile::is_held(&lock_path).expect("probe"),
        "finish freed the lock under the stopped thread's open write connection"
    );
    let (open, cv) = &*gate;
    *open.lock().expect("gate") = true;
    cv.notify_all();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while lockfile::is_held(&lock_path).expect("probe") {
        assert!(
            std::time::Instant::now() < deadline,
            "the stopped thread's exit never released the lock"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !store::rw_open::is_open(&db).expect("stat"),
        "the lock was freed before the thread closed its write connection"
    );
    drop(rec);
}

/// I1: a lost recording left beside a new session is cleanup's to seal, on
/// its own thread. With that cleanup held up, the new session's records still
/// commit at once and its `finish` seals it without waiting.
#[test]
fn cleanup_does_not_hold_up_the_recording() {
    let root = tempfile::tempdir().expect("tempdir");
    let lost = SessionId::new(hcore::events::now_unix_ms() - 120_000, 7);
    let lost_dir = root.path().join(lost.encode());
    std::fs::create_dir(&lost_dir).expect("mkdir");
    let lost_lock = lost_dir.join(files::LOCK);
    std::fs::write(&lost_lock, b"").expect("lock");
    let records: Vec<SessionRecord> = (0..200).flat_map(|_| events()).map(event_record).collect();
    unsealed_recording(&lost_dir, lost, &records);
    // Past the start grace: cleanup seals it.
    std::fs::File::options()
        .write(true)
        .open(&lost_lock)
        .and_then(|f| f.set_modified(std::time::SystemTime::now() - Duration::from_secs(120)))
        .expect("age the lock");
    let lost_wal = || {
        store::read_header(&lost_dir.join(store::FILE))
            .expect("header")
            .expect("heph")
            .wal
    };
    assert!(lost_wal(), "precondition: lost and unsealed");

    let gate: Gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let rec = recorder_with(
        root.path(),
        Faults {
            cleanup_gate: Some(Arc::clone(&gate)),
            ..Faults::default()
        },
    );
    let tap = rec.tap();
    for ev in events() {
        tap.event(ev);
    }
    tap.flush(Duration::from_secs(5))
        .expect("the first records commit while cleanup is held up");
    assert_eq!(read_records(rec.dir()).0.len(), events().len());
    let finishing = std::time::Instant::now();
    rec.recorder.finish(Some(0), false, Duration::from_secs(5));
    assert!(
        finishing.elapsed() < Duration::from_secs(2),
        "finish waited on cleanup: {:?}",
        finishing.elapsed()
    );
    let opened = open(rec.dir());
    assert!(!opened.is_copy(), "the new session sealed");
    assert!(store::read_row_end(&opened.conn).expect("end").sealed);
    drop(opened);
    assert!(lost_wal(), "cleanup is still held up");

    let (open_gate, cv) = &*gate;
    *open_gate.lock().expect("gate") = true;
    cv.notify_all();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while lost_wal() {
        assert!(
            std::time::Instant::now() < deadline,
            "cleanup never sealed the lost session"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// I1 and the PR 2 gate: a foreign connection on the live file (a `sqlite3`
/// shell, an editor's SQLite browser) keeps the seal from switching the
/// journal mode. `finish` gives the seal [`recorder::SEAL_BUDGET`] and goes,
/// with `End` committed; the next cleanup seals the recording.
#[test]
fn a_foreign_connection_does_not_hold_up_finish() {
    let root = tempfile::tempdir().expect("tempdir");
    let session = Session::open(&SessionRoot::new(root.path()), start()).expect("open");
    let id = session.id();
    let dir = session.dir().to_path_buf();
    let db = dir.join(store::FILE);
    let tap = session.tap();
    for ev in events() {
        tap.event(ev);
    }
    tap.flush(Duration::from_secs(5))
        .expect("records committed");
    drop(tap);
    let foreign = rusqlite::Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .expect("a foreign read-only connection");
    foreign
        .execute_batch("BEGIN; SELECT count(*) FROM records;")
        .expect("a read held open");

    let finishing = std::time::Instant::now();
    session.finish(Some(0), false);
    let took = finishing.elapsed();
    assert!(
        took < Duration::from_millis(600),
        "finish waited {took:?} on a foreign connection"
    );
    assert!(
        !lockfile::is_held(&dir.join(files::LOCK)).expect("probe"),
        "the lock is free once finish returns"
    );
    foreign.execute_batch("COMMIT").expect("end the read");
    let end = store::read_row_end(&foreign).expect("row end");
    assert!(end.sealed, "End is committed: {end:?}");
    let last: String = foreign
        .query_row(
            "SELECT kind FROM records ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .expect("the last record");
    assert_eq!(last, "End");
    drop(foreign);
    assert!(
        store::read_header(&db).expect("header").expect("heph").wal,
        "the cut-short seal left the file in WAL mode"
    );

    let report = cleanup::cleanup(
        root.path(),
        std::time::SystemTime::now() + cleanup::STARTING * 2,
        Retention::default(),
    )
    .expect("cleanup");
    assert_eq!(report.sealed, vec![id], "{report:?}");
    assert!(
        !store::read_header(&db).expect("header").expect("heph").wal,
        "cleanup sealed it"
    );
}

/// I1: a recorder that panics costs the recording, not the run.
#[test]
fn recorder_panic_does_not_fail_app() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rec = recorder_with(
        dir.path(),
        Faults {
            panic_on_event: true,
            ..Faults::default()
        },
    );
    let tap = rec.tap();
    for _ in 0..100 {
        for ev in events() {
            tap.event(ev);
        }
    }
    rec.recorder.finish(Some(0), false, Duration::from_secs(1));
    assert!(!lockfile::is_held(&rec.dir().join(files::LOCK)).expect("probe"));
}

/// A10/D15: past the full cap a `Truncated`, and only the critical kinds;
/// past the ceiling a second `Truncated` (the session row's `truncated_seq`)
/// and nothing else but `End`. The records stay under the ceiling.
#[test]
fn caps_truncate_twice() {
    let root = tempfile::tempdir().expect("tempdir");
    let rec = recorder_with(root.path(), capped(4 * MIB, 16 * MIB));
    let tap = rec.tap();
    for _ in 0..70 {
        tap.event(big_event());
    }
    tap.event(events().remove(0));
    rec.recorder.finish(Some(0), false, Duration::from_secs(10));
    let dir = rec.dir();

    let opened = open(dir);
    let bodies: i64 = opened
        .conn
        .query_row("SELECT sum(length(body)) FROM records", [], |r| r.get(0))
        .expect("size");
    assert!(
        u64::try_from(bodies).expect("size") <= 16 * MIB + 1024,
        "{bodies}"
    );
    // The caps count record bodies (and the explorer rows' estimated bytes),
    // not SQLite's pages: the sealed file is larger than what was counted,
    // by its page overhead, but stays in proportion to it. Here, with 1 MiB
    // bodies, within a quarter over the ceiling (SESSIONS.md, Recording).
    let on_disk = std::fs::metadata(dir.join(store::FILE))
        .expect("stat")
        .len();
    assert!(
        on_disk <= 16 * MIB + 16 * MIB / 4,
        "{on_disk} bytes on disk for a 16 MiB ceiling"
    );
    let truncated: Vec<i64> = opened
        .conn
        .prepare("SELECT seq FROM records WHERE kind = 'Truncated' ORDER BY seq")
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(truncated.len(), 2, "{truncated:?}");
    let end = store::read_row_end(&opened.conn).expect("row end");
    assert!(end.truncated && end.sealed, "{end:?}");
    assert_eq!(end.truncated_seq, truncated.last().copied());
    drop(opened);

    let (records, _) = read_records(dir);
    let kinds: Vec<_> = records
        .iter()
        .rev()
        .take(2)
        .filter_map(|r| r.session.clone())
        .collect();
    assert_eq!(
        kinds,
        vec![
            SessionRecordKind::End {
                exit_code: Some(0),
                panicked: false
            },
            SessionRecordKind::Truncated
        ]
    );
    let first = records
        .iter()
        .position(|r| r.session == Some(SessionRecordKind::Truncated))
        .expect("a first Truncated");
    assert!(
        records[first + 1..records.len() - 2]
            .iter()
            .all(recorder::is_critical),
        "only critical kinds between the two caps"
    );
    assert!(
        records.len() - first > 3,
        "big events are critical: they are kept past the full cap"
    );
    let (_, status) = read_status(dir).expect("a status row");
    assert!(status.truncated);
    assert!(
        status.counts.matched.is_some(),
        "status keeps folding past the cap"
    );
}

/// Past the full cap, the view records a truncated session still needs at its
/// end are written: a label change, every approval prompt (its notices'
/// sources, its clear, the standing decision) and `Finished`. Output, log
/// lines and a progress-only build event are dropped (D15).
#[test]
fn full_cap_keeps_the_ending_view_records() {
    let root = tempfile::tempdir().expect("tempdir");
    let rec = recorder_with(root.path(), capped(4 * MIB, recorder::CEILING));
    let tap = rec.tap();
    for _ in 0..5 {
        tap.event(big_event());
    }
    let kept = [
        SessionRecordKind::Approval {
            prompt: Some(Box::new(ApprovalPrompt {
                queue: vec![crate::tui::session::ApprovalQueued {
                    seq: 1,
                    addr: "//a:gate".into(),
                    ..Default::default()
                }],
            })),
            standing: None,
        },
        SessionRecordKind::Label {
            label: "run //a:... (4 matched)".into(),
        },
        SessionRecordKind::Approval {
            prompt: None,
            standing: Some(ApprovalStanding::Approve),
        },
        SessionRecordKind::Finished { stopped: true },
    ];
    // `Matched` is progress-only.
    tap.event(events().remove(0));
    for kind in kept.iter().cloned() {
        tap.view(1, kind);
    }
    tap.view(
        1,
        SessionRecordKind::Output {
            text: "failed: //a:bad\n".into(),
        },
    );
    assert!(tap.log(1, "INFO", "after the cap".into()).is_none());
    rec.recorder.finish(Some(1), false, Duration::from_secs(10));

    let (records, _) = read_records(rec.dir());
    let tail: Vec<_> = records
        .iter()
        .skip_while(|r| r.session != Some(SessionRecordKind::Truncated))
        .collect();
    assert!(
        tail.iter()
            .filter(|r| r.kind.is_some())
            .all(|r| recorder::is_critical(r)),
        "only critical build events past the cap"
    );
    let kinds: Vec<_> = tail.iter().filter_map(|r| r.session.clone()).collect();
    let mut want = vec![SessionRecordKind::Truncated];
    want.extend(kept);
    want.push(SessionRecordKind::End {
        exit_code: Some(1),
        panicked: false,
    });
    assert_eq!(kinds, want);
}

/// The `status` row stays bounded however many targets wait: 50k lock waits
/// and 50k admission waits through a real recorder, read back from the row it
/// wrote — while live and after `End` — have every list cut to
/// `STATUS_LIST_CAP` and every total and count in full.
#[test]
fn status_row_is_capped_through_the_recorder() {
    const N: u64 = 50_000;
    let dir = tempfile::tempdir().expect("tempdir");
    let rec = recorder_with(dir.path(), Faults::default());
    let tap = rec.tap();
    let ev = |kind| BuildEvent {
        at_unix_ms: 1,
        kind,
    };
    // A target in flight with an open operation: it counts as running.
    let start = |addr: &str| {
        tap.event(ev(BuildEventKind::ResultStart { addr: addr.into() }));
        tap.event(ev(BuildEventKind::ExecuteStart {
            output_groups: Vec::new(),
            log_not_streamed: false,
            addr: addr.into(),
            driver: "exec".into(),
            cache: true,
            declared: None,
            reserved: None,
            queued_ms: None,
            admitted: None,
        }));
    };
    for i in 0..N {
        let addr = format!("//l:t{i:05}");
        start(&addr);
        tap.event(ev(BuildEventKind::ResultLockWaitStart {
            addr,
            holder_pid: Some(4242),
            in_use_by_readers: false,
        }));
        let addr = format!("//w:t{i:05}");
        start(&addr);
        tap.event(ev(BuildEventKind::AdmissionWaitStart {
            addr,
            pool: "cpu".to_owned(),
            need: 1,
            free: 0,
            ahead: 0,
            removals_pending: None,
            yielding_to: None,
        }));
    }
    let read = || read_status(rec.dir());
    let check = |when: &str, size: usize, s: &SessionSnapshot| {
        assert!(size < 48 * 1024, "{when}: the status row is {size} bytes");
        let cap = STATUS_LIST_CAP;
        assert_eq!(s.running.len(), cap, "{when}");
        assert_eq!(s.waiting.len(), cap, "{when}");
        assert_eq!(s.lock_waits.len(), cap, "{when}");
        assert_eq!(s.admission_waits.len(), cap, "{when}");
        for (name, total, want) in [
            ("running", s.totals.running, 2 * N),
            ("waiting", s.totals.waiting, 2 * N),
            ("lock_waits", s.totals.lock_waits, N),
            ("admission_waits", s.totals.admission_waits, N),
        ] {
            assert_eq!(total.total, want, "{when}: {name}");
            assert!(total.truncated, "{when}: {name}");
        }
        assert!(!s.totals.failed.truncated, "{when}");
        assert_eq!(s.counts.in_flight, 2 * N, "{when}");
        // Every running target waits: the count says so though the lists
        // were cut.
        assert_eq!(s.counts.running_unblocked, Some(0), "{when}");
        assert_eq!(s.lock_waits[0].addr, "//l:t00000", "{when}");
        assert_eq!(s.lock_waits[0].holder_pid, Some(4242), "{when}");
    };

    // Live: the periodic write, once it has folded everything.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let live = loop {
        if let Some((size, s)) = read()
            && s.totals.admission_waits.total == N
        {
            break (size, s);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the recorder never wrote a status row with every wait"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(live.1.end.is_none());
    check("live", live.0, &live.1);

    rec.recorder.finish(Some(0), false, Duration::from_secs(30));
    let (size, last) = read().expect("the final status row");
    assert!(last.end.is_some(), "the final write carries End");
    check("after End", size, &last);
}

/// A12: a taken id is skipped for a fresh draw.
#[test]
fn id_collision_retries() {
    let root = tempfile::tempdir().expect("tempdir");
    let taken = SessionId::new(1_000, 5);
    std::fs::create_dir(root.path().join(taken.encode())).expect("mkdir");
    let mut draws = [5u8, 5, 6].into_iter();
    let (id, dir) =
        create_session_dir(root.path(), 1_000, || draws.next().unwrap_or(0)).expect("create");
    assert_eq!(id, SessionId::new(1_000, 6));
    assert!(dir.is_dir());
}

#[derive(Default)]
struct NullView;

impl CIAppView for NullView {
    fn apply(&mut self, _ev: &BuildEvent) {}
}

impl TUIAppView for NullView {
    fn apply(&mut self, _ev: &BuildEvent) {}
    fn rows(&self, _h: u16) -> u16 {
        1
    }
    fn render(&self, _s: &str, _n: u64, _w: u16, _h: u16) -> Vec<ratatui::text::Line<'static>> {
        Vec::new()
    }
}

struct EmitApp {
    records: bool,
    ran: Arc<AtomicBool>,
}

#[async_trait]
impl App for EmitApp {
    type Output = ();
    type TuiView = NullView;
    type CiView = NullView;

    fn tui_view(&self) -> NullView {
        NullView
    }
    fn ci_view(&self) -> NullView {
        NullView
    }
    fn records_session(&self) -> bool {
        self.records
    }
    async fn run(self, ctx: AppContext) -> anyhow::Result<()> {
        if let Some(tx) = ctx.event_sender() {
            for ev in events() {
                tx.send(ev).expect("send");
            }
        }
        self.ran.store(true, Ordering::Release);
        Ok(())
    }
}

/// Run an app on the line backend against `slot`; whether it ran.
async fn run_in(slot: &Arc<SessionSlot>, records: bool) -> bool {
    let ran = Arc::new(AtomicBool::new(false));
    let (shutdown, _rx) = hcore::shutdown::ShutdownTrigger::new();
    crate::tui::run_app(
        EmitApp {
            records,
            ran: Arc::clone(&ran),
        },
        crate::tui::LogSink::new_direct_with(Arc::clone(slot)),
        slot,
        false,
        shutdown,
    )
    .await
    .expect("app");
    ran.load(Ordering::Acquire)
}

fn entries(p: &Path) -> usize {
    std::fs::read_dir(p).map_or(0, Iterator::count)
}

/// The line backend records what the app emits, through the slot, and `finish`
/// closes it.
#[tokio::test]
async fn run_app_records_through_the_slot() {
    let home = tempfile::tempdir().expect("tempdir");
    let slot = Arc::new(SessionSlot::new());
    slot.configure(
        SessionRoot::from_env(None, Some(home.path().as_os_str())),
        vec!["heph".into()],
    );
    // The cwd heph resolved (`HEPH_CWD`), not the process's own.
    let ws = tempfile::tempdir().expect("tempdir");
    let cwd = ws.path().join("sub");
    slot.set_workspace(&cwd, ws.path(), &ws.path().join(".heph"));
    assert!(run_in(&slot, true).await);
    let dir = slot.dir().expect("a session opened");
    assert!(dir.starts_with(home.path().join(".heph").join(SESSIONS_DIR)));
    let meta = read_meta(&dir);
    assert_eq!(meta.cwd.as_deref(), Some(&*cwd.to_string_lossy()));
    assert_eq!(
        meta.workspace_root.as_deref(),
        Some(&*ws.path().to_string_lossy())
    );
    slot.finish(Some(0));
    let (records, _) = read_records(&dir);
    assert_eq!(
        records.iter().filter(|r| r.kind.is_some()).count(),
        events().len()
    );
}

/// A3: a viewer's app records nothing.
#[tokio::test]
async fn viewer_does_not_record_itself() {
    let home = tempfile::tempdir().expect("tempdir");
    let slot = Arc::new(SessionSlot::new());
    slot.configure(
        SessionRoot::from_env(None, Some(home.path().as_os_str())),
        Vec::new(),
    );
    assert!(run_in(&slot, false).await);
    assert!(slot.dir().is_none());
    assert_eq!(entries(home.path()), 0, "not even ~/.heph");
}

/// What a `kill -9` leaves of a private session: its directory, a free lock,
/// records and no `End`.
fn killed_private_session(private: &Path) -> std::path::PathBuf {
    let id = SessionId::new(hcore::events::now_unix_ms(), 7);
    let dir = private.join(id.encode());
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join(files::LOCK), b"").expect("lock");
    unsealed_recording(&dir, id, &[event_record(events().remove(0))]);
    // Killed more than a minute ago: a younger free lock may be a session
    // still starting, which cleanup leaves alone.
    let started = std::time::SystemTime::now()
        .checked_sub(2 * cleanup::STARTING)
        .expect("time");
    std::fs::File::options()
        .write(true)
        .open(dir.join(files::LOCK))
        .and_then(|f| f.set_modified(started))
        .expect("backdate the lock");
    dir
}

/// `HEPH_NO_SESSION=1` (injected through `from_env`: never `set_var` in a
/// shared test binary) records into a private directory, outside the
/// sessions root the session list reads, so it is not listed, even beside a
/// public session that is; a public start leaves it alone while it is live,
/// and it is removed when the session finishes. After a `kill -9` (its
/// directory left with the lock free), the next start sweeps it, whether
/// that run records publicly or privately.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_session_private_dir() {
    let home = tempfile::tempdir().expect("tempdir");
    let slot = Arc::new(SessionSlot::new());
    let root = SessionRoot::from_env(Some("1".as_ref()), Some(home.path().as_os_str()));
    let private = home.path().join(".heph").join(PRIVATE_SESSIONS_DIR);
    assert_eq!(root, Ok(Some(SessionRoot::private(&private))));
    slot.configure(root, Vec::new());
    assert!(run_in(&slot, true).await);
    let dir = slot.dir().expect("a private session");
    assert!(dir.starts_with(&private), "{}", dir.display());
    assert!(!home.path().join(".heph").join(SESSIONS_DIR).exists());

    let public = SessionRoot::for_reading(Some(home.path().as_os_str())).expect("home");
    let other = Session::open(&public, start()).expect("open");
    let listed = FileSource::new(public.path()).list().await.expect("list");
    let ids: Vec<SessionId> = listed.iter().map(|e| e.id).collect();
    assert_eq!(ids, vec![other.id()], "only the public session is listed");
    // `End` is acknowledged after the start's sweep, which ran first.
    other.finish(Some(0), false);
    assert!(dir.exists(), "a live private session is never swept");

    slot.finish(Some(0));
    assert!(!dir.exists(), "removed at exit");

    // Each start below cleans: the hourly gate is `cleanup_runs_at_most_hourly`'s.
    let every_start = Faults {
        cleanup_interval: Some(Duration::ZERO),
        ..Faults::default()
    };
    for next in [public, SessionRoot::private(&private)] {
        let killed = killed_private_session(&private);
        let session = Session::open_with(&next, start(), Retention::default(), every_start.clone())
            .expect("open");
        session.finish(Some(0), false);
        // The sweep is on the detached cleanup thread.
        assert!(
            eventually(|| !killed.exists()),
            "a {} start sweeps a killed private session",
            if next.is_private() {
                "private"
            } else {
                "public"
            }
        );
    }
}

/// Poll `cond` for up to 20 s; `false` if it never held.
pub(super) fn eventually(mut cond: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !cond() {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

/// Backdate `root`'s cleanup stamp past the interval, as an hour going by
/// would.
fn expire_stamp(root: &Path) {
    let then = std::time::SystemTime::now()
        .checked_sub(cleanup::INTERVAL + Duration::from_secs(60))
        .expect("time");
    std::fs::File::options()
        .write(true)
        .open(root.join(cleanup::STAMP))
        .and_then(|f| f.set_modified(then))
        .expect("backdate the stamp");
}

/// Cleanup runs at most once an hour per root, off the build's path (user
/// decision 2026-11-24). The first start in a root cleans it; a start within
/// the hour leaves an ended session it would remove alone; once the stamp is
/// an hour old, the next start removes it. The private root beside a public
/// one is cleaned in the same pass, under its own stamp.
#[test]
fn cleanup_runs_at_most_hourly() {
    let home = tempfile::tempdir().expect("tempdir");
    let public = SessionRoot::for_reading(Some(home.path().as_os_str())).expect("home");
    let private = home.path().join(".heph").join(PRIVATE_SESSIONS_DIR);

    let first = killed_private_session(&private);
    Session::open(&public, start())
        .expect("open")
        .finish(Some(0), false);
    assert!(
        eventually(|| !first.exists()),
        "the first start in a root cleans it"
    );

    let second = killed_private_session(&private);
    Session::open(&public, start())
        .expect("open")
        .finish(Some(0), false);
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        second.exists(),
        "a start within the hour of the last cleanup leaves the root alone"
    );

    expire_stamp(public.path());
    expire_stamp(&private);
    Session::open(&public, start())
        .expect("open")
        .finish(Some(0), false);
    assert!(
        eventually(|| !second.exists()),
        "a start an hour after the last cleanup cleans again"
    );
}

/// A4: no `HOME` means no session, and the build still runs.
#[tokio::test]
async fn home_unset_runs_without_session() {
    let slot = Arc::new(SessionSlot::new());
    let root = SessionRoot::from_env(None, None);
    assert!(root.is_err());
    slot.configure(root, Vec::new());
    assert!(run_in(&slot, true).await);
    assert!(slot.dir().is_none());
    // An unwritable root behaves the same.
    let ro = tempfile::tempdir().expect("tempdir");
    std::fs::set_permissions(ro.path(), std::fs::Permissions::from_mode(0o500)).expect("chmod");
    let slot = Arc::new(SessionSlot::new());
    slot.configure(
        Ok(Some(SessionRoot::new(ro.path().join("sessions")))),
        Vec::new(),
    );
    assert!(run_in(&slot, true).await);
    assert!(slot.dir().is_none());
    std::fs::set_permissions(ro.path(), std::fs::Permissions::from_mode(0o700)).expect("chmod");
}

/// I1: a filesystem whose `journal_mode=WAL` answers another mode stops the
/// recording before it starts, with one warning; the build still runs and
/// ends as it would have, and the session's directory is gone.
#[tokio::test]
async fn wal_refused_stops_the_recorder() {
    let home = tempfile::tempdir().expect("tempdir");
    let root = home.path().join("sessions");
    let slot = Arc::new(SessionSlot::new());
    slot.configure(Ok(Some(SessionRoot::new(&root))), vec!["heph".into()]);
    slot.set_faults(Faults {
        refuse_wal: true,
        ..Faults::default()
    });
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    let ran = run_in(&slot, true).await;
    drop(guard);
    assert!(ran, "the build ran");
    assert!(slot.dir().is_none(), "no session");
    let log = captured.settled_log();
    assert_eq!(log.matches("WARN").count(), 1, "{log}");
    assert!(log.contains("WAL"), "{log}");
    assert_eq!(entries(&root), 0, "nothing left under the root");
}

/// The file source follows a live session from its directory: the records so
/// far, `CaughtUp`, each new record, then `Ended` with the exit code once the
/// owner writes `End` — and `list` reads the lock, meta and status only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_source_follows_until_end() {
    use futures::StreamExt as _;
    let root = tempfile::tempdir().expect("tempdir");
    let session = Session::open(&SessionRoot::new(root.path()), start()).expect("open");
    let tap = session.tap();
    let mut evs = events().into_iter();
    tap.event(evs.next().expect("one event"));

    let source = FileSource::new(root.path());
    let listed = source.list().await.expect("list");
    assert_eq!(listed.len(), 1);
    let entry = listed.first().expect("entry");
    assert!(entry.live && entry.id == session.id());

    let mut stream = source.subscribe(session.id()).await.expect("subscribe");
    let mut records = 0usize;
    let mut caught_up = false;
    for ev in evs {
        tap.event(ev);
    }
    session.finish(Some(3), false);
    let ended = loop {
        match stream.next().await.expect("item").expect("ok") {
            SessionItem::Record(r) => records += usize::from(!is_machine(&r)),
            SessionItem::CaughtUp { skipped } => {
                assert_eq!(skipped, 0);
                caught_up = true;
            }
            SessionItem::Ended { exit, lost } => break (exit, lost),
        }
    };
    assert!(caught_up);
    assert_eq!(ended, (Some(3), false));
    // Every event plus the `End` record.
    assert_eq!(records, events().len() + 1);
    let after = source.snapshot(session.id()).await.expect("snapshot");
    let after = after.expect("still there");
    assert!(!after.live);
    assert_eq!(
        after.status.and_then(|s| s.end).and_then(|e| e.exit_code),
        Some(3)
    );
}

/// Read a follow stream up to its `CaughtUp`, counting the records before it.
/// An `Ended` first fails the test: the cases below need the follower to be
/// past its first read before the owner acts.
async fn until_caught_up(
    stream: &mut futures::stream::BoxStream<'static, anyhow::Result<SessionItem>>,
) -> usize {
    use futures::StreamExt as _;
    let mut records = 0;
    loop {
        match stream.next().await.expect("item").expect("ok") {
            SessionItem::Record(r) => records += usize::from(!is_machine(&r)),
            SessionItem::CaughtUp { skipped } => {
                assert_eq!(skipped, 0);
                return records;
            }
            SessionItem::Ended { .. } => panic!("ended before catching up"),
        }
    }
}

/// `End` written while the follower is already polling the tail (not found
/// in its first read): every record after `CaughtUp` arrives, then `Ended`
/// with the exit code, not lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follow_sees_an_end_written_mid_follow() {
    use futures::StreamExt as _;
    let root = tempfile::tempdir().expect("tempdir");
    let session = Session::open(&SessionRoot::new(root.path()), start()).expect("open");
    let tap = session.tap();
    let mut evs = events().into_iter();
    tap.event(evs.next().expect("one event"));
    // Recorded before subscribing, so the first read finds it.
    while read_records(session.dir()).0.is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let source = FileSource::new(root.path());
    let mut stream = source.subscribe(session.id()).await.expect("subscribe");
    assert_eq!(until_caught_up(&mut stream).await, 1);

    // The follower is in its tail loop now; the rest of the run happens.
    let rest = evs.len();
    for ev in evs {
        tap.event(ev);
    }
    session.finish(Some(2), false);

    let mut records = 0usize;
    let ended = loop {
        let item = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("the follower sees the end")
            .expect("item")
            .expect("ok");
        match item {
            SessionItem::Record(r) => records += usize::from(!is_machine(&r)),
            SessionItem::CaughtUp { .. } => panic!("caught up twice"),
            SessionItem::Ended { exit, lost } => break (exit, lost),
        }
    };
    assert_eq!(ended, (Some(2), false));
    assert_eq!(records, rest + 1, "the remaining events plus `End`");
}

/// The owner's lock freed without an `End` while the follower polls (a
/// `SIGKILL`ed heph): the follower keeps waiting while the lock is held, and
/// reads the release as lost, with no exit code.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follow_reads_a_lock_freed_without_end_as_lost() {
    use futures::StreamExt as _;
    // A hand-made owner, so it can die without writing `End`: the lock, then
    // a recording with a first record.
    let root = tempfile::tempdir().expect("tempdir");
    let id = SessionId::new(hcore::events::now_unix_ms(), 3);
    let dir = root.path().join(id.encode());
    std::fs::create_dir(&dir).expect("mkdir");
    let lock = lockfile::acquire_owner_lock(&dir.join(files::LOCK)).expect("lock");
    unsealed_recording(&dir, id, &[event_record(events().remove(0))]);

    let source = FileSource::new(root.path());
    let mut stream = source.subscribe(id).await.expect("subscribe");
    assert_eq!(until_caught_up(&mut stream).await, 1);
    // Held: several ticks pass with nothing to say.
    assert!(
        tokio::time::timeout(source::FOLLOW_TICK * 3, stream.next())
            .await
            .is_err(),
        "a held lock is a live session"
    );

    drop(lock);
    let item = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("the follower sees the release")
        .expect("item")
        .expect("ok");
    assert!(
        matches!(
            item,
            SessionItem::Ended {
                exit: None,
                lost: true
            }
        ),
        "{item:?}"
    );
}

/// A live session whose owner never said it answers in-flight requests (an
/// older heph, no `inflight` capability) is refused by name, and the viewer
/// writes no request file into it: nobody would ever answer one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inflight_is_refused_without_the_capability() {
    let root = tempfile::tempdir().expect("tempdir");
    let session = Session::open(&SessionRoot::new(root.path()), start()).expect("open");
    assert!(
        read_meta(session.dir()).has_capability(CAPABILITY_INFLIGHT),
        "precondition"
    );
    alter(session.dir())
        .execute("UPDATE session SET capabilities = '[]'", [])
        .expect("write the session row");

    let err = FileSource::new(root.path())
        .inflight(session.id(), Duration::from_secs(5))
        .await
        .expect_err("refused");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("does not answer in-flight requests"),
        "names the reason: {msg}"
    );
    assert!(
        !session.dir().join(files::INFLIGHT_REQUEST).exists(),
        "no request is left for an owner that never reads one"
    );
    session.finish(Some(0), false);
}

fn lock_wait(addr: &str, pid: u32) -> BuildEvent {
    BuildEvent {
        at_unix_ms: 1,
        kind: BuildEventKind::ResultLockWaitStart {
            addr: addr.into(),
            holder_pid: Some(pid),
            in_use_by_readers: false,
        },
    }
}

/// The backend's receiver resolves a holder pid before it folds the event that
/// first carries it, so the view names the session on that very event while
/// reading only the cache; the second event with the pid is not resolved again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receiver_resolves_the_holder_before_the_fold() {
    let root = tempfile::tempdir().expect("tempdir");
    let holder = Session::open(&SessionRoot::new(root.path()), start()).expect("holder");
    let pid = std::process::id();
    let names = HolderNames::new(Some(SessionRoot::new(root.path())), None);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut events = TappedReceiver::new(rx, None, names.clone());
    tx.send(lock_wait("//a:x", pid)).expect("send");
    tx.send(lock_wait("//a:y", pid)).expect("send");

    let mut seen = None;
    events
        .recv(|_| seen = Some(names.get(pid)))
        .await
        .expect("an event");
    assert_eq!(seen, Some(Some(holder.id())), "named on the first fold");
    assert!(!names.needs(pid), "resolved once, cached");
    seen = None;
    events
        .recv(|_| seen = Some(names.get(pid)))
        .await
        .expect("an event");
    assert_eq!(seen, Some(Some(holder.id())));
    holder.finish(Some(0), false);
}

fn wait_addr(ev: &BuildEvent) -> String {
    match &ev.kind {
        BuildEventKind::ResultLockWaitStart { addr, .. } => addr.clone(),
        other => panic!("only lock waits are sent: {other:?}"),
    }
}

/// A holder lookup a test controls: it counts its calls, answers `answer`, and
/// blocks until [`GatedLookup::open`] when built gated.
#[derive(Clone)]
struct GatedLookup {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
}

impl GatedLookup {
    fn new(open: bool) -> Self {
        Self {
            calls: Arc::default(),
            gate: Arc::new((Mutex::new(open), std::sync::Condvar::new())),
        }
    }

    /// Names resolved through this lookup: `pid` maps to `answer(pid)`.
    fn names(&self, answer: fn(u32) -> Option<SessionId>) -> HolderNames {
        let this = self.clone();
        HolderNames::with_lookup(super::holder::FakeLookup(Arc::new(move |pid| {
            this.calls.fetch_add(1, Ordering::SeqCst);
            let (open, cv) = &*this.gate;
            let mut open = open.lock().unwrap();
            while !*open {
                open = cv.wait(open).unwrap();
            }
            answer(pid)
        })))
    }

    fn open(&self) {
        let (open, cv) = &*self.gate;
        *open.lock().unwrap() = true;
        cv.notify_all();
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

fn some_session(pid: u32) -> Option<SessionId> {
    Some(SessionId::new(1_700_000_000_000 + u64::from(pid), 0))
}

/// Wait (bounded) until the detached scan has stored `pid`'s answer.
async fn until_named(names: &HolderNames, pid: u32) {
    for _ in 0..500 {
        if names.get(pid).is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("pid {pid} was never named");
}

/// A `recv` dropped while its event's holder resolves (a `select!` arm losing)
/// loses nothing and reorders nothing: the event is parked, and once its scan
/// has answered the teardown drain folds it first, named, exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receiver_keeps_a_parked_event_across_a_dropped_recv() {
    use futures::FutureExt as _;
    let lookup = GatedLookup::new(false);
    let names = lookup.names(some_session);
    let pid = 4242;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut events = TappedReceiver::new(rx, None, names.clone());
    tx.send(lock_wait("//a:parked", pid)).expect("send");
    tx.send(lock_wait("//a:next", pid)).expect("send");

    let mut folded: Vec<(String, Option<SessionId>)> = Vec::new();
    // One poll takes the event and parks it on the gated scan; then the future
    // is dropped.
    let first = events
        .recv(|ev| folded.push((wait_addr(ev), names.get(pid))))
        .now_or_never();
    assert!(first.is_none(), "the event is parked, not folded");
    assert!(folded.is_empty(), "{folded:?}");
    lookup.open();
    until_named(&names, pid).await;
    drop(tx);
    while events
        .try_recv(|ev| folded.push((wait_addr(ev), names.get(pid))))
        .is_ok()
    {}
    assert_eq!(
        folded,
        [
            ("//a:parked".to_owned(), some_session(pid)),
            ("//a:next".to_owned(), some_session(pid)),
        ]
    );
    assert_eq!(lookup.calls(), 1, "scanned once");
}

/// A scan that hangs (a hung `~/.heph`) holds the event for the bounded wait
/// only: it is folded showing the pid, the pid is not rescanned on the next
/// event, and the answer that comes later still names it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receiver_folds_after_a_slow_scan_with_the_pid() {
    let lookup = GatedLookup::new(false);
    let names = lookup.names(some_session);
    let pid = 4242;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut events = TappedReceiver::new(rx, None, names.clone());
    tx.send(lock_wait("//a:x", pid)).expect("send");
    tx.send(lock_wait("//a:y", pid)).expect("send");

    let started = std::time::Instant::now();
    let mut seen = None;
    tokio::time::timeout(
        Duration::from_secs(10),
        events.recv(|_| seen = Some(names.get(pid))),
    )
    .await
    .expect("the wait is bounded")
    .expect("an event");
    assert_eq!(seen, Some(None), "folded showing the pid");
    assert!(
        started.elapsed() >= super::recorder::HOLDER_SCAN_WAIT,
        "waited for the scan first: {:?}",
        started.elapsed()
    );
    // The next event with the pid is folded at once: the pid is claimed.
    let next = std::time::Instant::now();
    events
        .recv(|_| seen = Some(names.get(pid)))
        .await
        .expect("an event");
    assert!(next.elapsed() < super::recorder::HOLDER_SCAN_WAIT);
    assert_eq!(lookup.calls(), 1, "not rescanned");
    // The detached scan finishes on its own and names the pid from then on.
    lookup.open();
    until_named(&names, pid).await;
    assert_eq!(lookup.calls(), 1);
}

/// A pid with no session is cached as such: the next event is not rescanned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receiver_caches_a_negative_answer() {
    let lookup = GatedLookup::new(true);
    let names = lookup.names(|_| None);
    let pid = 4242;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut events = TappedReceiver::new(rx, None, names.clone());
    tx.send(lock_wait("//a:x", pid)).expect("send");
    tx.send(lock_wait("//a:y", pid)).expect("send");
    for _ in 0..2 {
        events.recv(|_| {}).await.expect("an event");
    }
    assert_eq!(lookup.calls(), 1, "scanned once");
    assert!(!names.needs(pid), "the none is cached");
    assert_eq!(names.get(pid), None);
}

/// Two distinct holder pids in a row: each is scanned once and named on its
/// own first event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receiver_names_two_pids_in_a_row() {
    let lookup = GatedLookup::new(true);
    let names = lookup.names(some_session);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut events = TappedReceiver::new(rx, None, names.clone());
    for (addr, pid) in [("//a:x", 1), ("//a:y", 2), ("//a:z", 1)] {
        tx.send(lock_wait(addr, pid)).expect("send");
    }
    let mut seen = Vec::new();
    for _ in 0..3 {
        events
            .recv(|ev| {
                let pid = super::holder::event_holder_pid(ev).expect("a lock wait");
                seen.push(names.get(pid));
            })
            .await
            .expect("an event");
    }
    assert_eq!(seen, [some_session(1), some_session(2), some_session(1)]);
    assert_eq!(lookup.calls(), 2, "each pid scanned once");
}

/// `recv_record_only` (the line backend after its summary) records a parked
/// event first, then the rest in order, without waiting on its scan.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_only_takes_the_parked_event_first() {
    use futures::FutureExt as _;
    let root = tempfile::tempdir().expect("tempdir");
    let session = Session::open(&SessionRoot::new(root.path()), start()).expect("open");
    let lookup = GatedLookup::new(false);
    let names = lookup.names(some_session);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut events = TappedReceiver::new(rx, Some(session.tap()), names);
    tx.send(lock_wait("//a:parked", 7)).expect("send");
    tx.send(lock_wait("//a:next", 7)).expect("send");
    assert!(events.recv(|_| {}).now_or_never().is_none(), "parked");
    drop(tx);
    tokio::time::timeout(Duration::from_secs(10), async {
        while events.recv_record_only().await.is_some() {}
    })
    .await
    .expect("record-only never waits on the scan");
    session.finish(Some(0), false);
    lookup.open();
    let (recs, _) = read_records(session.dir());
    let addrs: Vec<String> = recs
        .iter()
        .filter_map(SessionRecord::build_event)
        .map(|ev| wait_addr(&ev))
        .collect();
    assert_eq!(addrs, ["//a:parked", "//a:next"]);
}

/// An `Output` record is cut to 1 MiB with a marker, so the failure report
/// written past the event log's cap keeps that overshoot bounded. The cut
/// falls on a character boundary.
#[test]
fn output_records_are_capped() {
    let short = "failed: //a:bad\n";
    assert_eq!(capped_output(short), short, "a short report is verbatim");
    // Two-byte characters after one ASCII byte: the cap falls mid-character.
    let huge = format!("x{}", "é".repeat(OUTPUT_CAP));
    let capped = capped_output(&huge);
    let (kept, marker) = capped.rsplit_once("\n[session: ").expect("a marker line");
    assert!(kept.len() <= OUTPUT_CAP && kept.len() > OUTPUT_CAP - 4);
    assert!(huge.starts_with(kept), "the kept part is a prefix");
    assert_eq!(
        marker,
        format!(
            "output cut at 1 MiB; {} more bytes were printed but not recorded]\n",
            huge.len() - kept.len()
        )
    );
}

/// Every row of the explorer tables, each as its columns' debug text, sorted.
fn explorer_rows(conn: &rusqlite::Connection) -> Vec<String> {
    let mut rows = Vec::new();
    for table in ["spans", "span_events", "metrics"] {
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM {table}"))
            .expect("prepare");
        let n = stmt.column_count();
        let mut q = stmt.query([]).expect("query");
        while let Some(row) = q.next().expect("row") {
            let cols: Vec<String> = (0..n)
                .map(|i| format!("{:?}", row.get_ref(i).expect("column")))
                .collect();
            rows.push(format!("{table}: {}", cols.join(" | ")));
        }
    }
    rows.sort();
    rows
}

/// I4: re-deriving a sealed recording from its records gives exactly the
/// explorer rows the owner derived live — for a whole run, for one past its
/// full cap only (derivation goes on over the critical kinds), and for one
/// cut at the ceiling, where both stop at the same record. Each case checks
/// it reached the cap it claims (the `Truncated` count, `truncated_seq`
/// set only at the ceiling), and that past the full cap only critical kinds
/// were kept, so a cap that stopped firing turns a capped case into a whole
/// run and fails here rather than passing vacuously.
#[test]
fn rederive_matches_the_live_derivation() {
    let cases = [
        (Faults::default(), 0),
        (capped(2 * MIB, 64 * MIB), 1),
        (capped(2 * MIB, 4 * MIB), 2),
    ];
    for (faults, cuts) in cases {
        let root = tempfile::tempdir().expect("tempdir");
        let rec = recorder_with(root.path(), faults.clone());
        let tap = rec.tap();
        for ev in events() {
            tap.event(ev);
        }
        for i in 0..8 {
            let addr = format!("//b:t{i}");
            tap.event(BuildEvent {
                at_unix_ms: 10 + i,
                kind: BuildEventKind::ResultStart { addr: addr.clone() },
            });
            let mut end = big_event();
            end.at_unix_ms = 11 + i;
            if let BuildEventKind::ResultEnd { addr: a, .. } = &mut end.kind {
                *a = addr;
            }
            tap.event(end);
        }
        // Progress-only and a log line: dropped once past the full cap.
        tap.event(events().remove(0));
        assert!(tap.log(1, "INFO", "late".into()).is_none());
        rec.recorder.finish(Some(1), false, Duration::from_secs(10));

        let (records, skipped) = read_records(rec.dir());
        assert_eq!(skipped, 0);
        let truncated: Vec<usize> = records
            .iter()
            .enumerate()
            .filter(|(_, r)| r.session == Some(SessionRecordKind::Truncated))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(truncated.len(), cuts, "{faults:?}");
        if let Some(&first) = truncated.first() {
            assert!(
                records[first + 1..].iter().all(recorder::is_critical),
                "only critical kinds past the full cap ({faults:?})"
            );
            assert!(
                records[first + 1..]
                    .iter()
                    .any(|r| matches!(r.kind, Some(BuildEventKind::ResultEnd { .. }))),
                "a critical kind is kept past the full cap ({faults:?})"
            );
        }
        let conn = alter(rec.dir());
        let end = store::read_row_end(&conn).expect("row end");
        // The session row's `truncated` is the ceiling's mark (I8); the
        // full cap alone shows in the `status` row and a `Truncated` record.
        assert_eq!(end.truncated, cuts == 2, "{faults:?}");
        assert_eq!(end.truncated_seq.is_some(), cuts == 2, "{faults:?}");
        let (_, status) = read_status(rec.dir()).expect("a status row");
        assert_eq!(status.truncated, cuts > 0, "{faults:?}");
        let live = explorer_rows(&conn);
        assert!(
            live.iter().any(|r| r.starts_with("spans: ")),
            "the live derivation wrote spans ({faults:?})"
        );
        store::rederive(&conn).expect("rederive");
        assert_eq!(explorer_rows(&conn), live, "{faults:?}");
    }
}

/// A recording a `kill -9` left (WAL mode, no `End`, `sealed=0`) is sealed
/// in place by `seal_lost`: rollback mode, `sealed`, its last record's time
/// as its end, no exit code. It runs during a build, so it repairs nothing
/// (I1, D14): the explorer rows stay what the crash left, and the derivation
/// is stamped stale for a session command to re-derive.
#[test]
fn seal_lost_seals_a_killed_recording() {
    let root = tempfile::tempdir().expect("tempdir");
    let id = SessionId::new(hcore::events::now_unix_ms(), 9);
    let dir = root.path().join(id.encode());
    std::fs::create_dir(&dir).expect("mkdir");
    std::fs::write(dir.join(files::LOCK), b"").expect("lock");
    let records: Vec<SessionRecord> = events().into_iter().map(event_record).collect();
    unsealed_recording(&dir, id, &records);
    let path = dir.join(store::FILE);
    let before = {
        let conn = alter(&dir);
        let rows = explorer_rows(&conn);
        assert!(
            rows.iter().any(|r| r.starts_with("spans: ")),
            "precondition: the live derivation wrote result spans"
        );
        rows
    };
    assert!(
        store::read_header(&path)
            .expect("header")
            .expect("heph")
            .wal,
        "precondition: left in WAL mode"
    );

    assert_eq!(store::seal_lost(&path).expect("seal"), store::Sealed::Done);
    assert!(
        !store::read_header(&path)
            .expect("header")
            .expect("heph")
            .wal
    );
    let opened = open(&dir);
    assert!(!opened.is_copy(), "sealed: read in place");
    let end = store::read_row_end(&opened.conn).expect("row end");
    assert!(end.sealed, "{end:?}");
    let last = records.last().map(|r| r.at_unix_ms).expect("records");
    assert_eq!(end.ended_ms, Some(i64::try_from(last).expect("ms")));
    let exit: Option<i64> = opened
        .conn
        .query_row("SELECT exit_code FROM session", [], |r| r.get(0))
        .expect("exit code");
    assert_eq!(exit, None);
    assert_eq!(
        explorer_rows(&opened.conn),
        before,
        "a build's cleanup re-derives nothing"
    );
    let derivation: u32 = opened
        .conn
        .query_row("SELECT derivation FROM session", [], |r| r.get(0))
        .expect("derivation");
    assert_eq!(derivation, store::STALE_DERIVATION);
    drop(opened);
    let (read, _) = read_records(&dir);
    assert_eq!(read.len(), records.len(), "every record kept");
}
