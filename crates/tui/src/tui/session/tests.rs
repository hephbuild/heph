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

use super::cleanup::Retention;
use super::recorder::{Faults, RecorderStart};
use super::*;
use crate::tui::app::{App, AppContext, CIAppView, TUIAppView};
use crate::tui::progress::BuildState;

fn events() -> Vec<BuildEvent> {
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
                addr: "//a:ok".into(),
                driver: "exec".into(),
                cache: true,
                declared: None,
                reserved: None,
                queued_ms: None,
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
                addr: "//a:bad".into(),
                error: Some("boom".into()),
                upstream_of: None,
                exit_status: None,
                log_tail: None,
                timed_out: None,
            },
        ),
    ]
}

fn read_records(dir: &Path) -> (Vec<SessionRecord>, u64) {
    let file = std::fs::File::open(dir.join(files::EVENTS)).expect("events");
    let mut r = RecordReader::new(file);
    let recs = std::iter::from_fn(|| r.next_record().expect("read")).collect();
    (recs, r.skipped())
}

fn start() -> SessionStart {
    SessionStart {
        header: SessionHeader {
            kind: HeaderKind::Build,
            label: "run //a:...".into(),
        },
        argv: vec!["heph".into(), "run".into()],
        ..SessionStart::default()
    }
}

/// Folding `events.ndjson` gives the view's state, and the session ends with
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
    tap.log("INFO", "hello\u{1b}[31m".into());
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
            msg: "hello\u{1b}[31m".into()
        })));
    assert_eq!(
        records.last().and_then(|r| r.session.clone()),
        Some(SessionRecordKind::End {
            exit_code: Some(1),
            panicked: false
        })
    );
    let status = SessionSnapshot::decode(&std::fs::read(dir.join(files::STATUS)).expect("status"))
        .expect("decode");
    assert_eq!(status.failed, vec!["//a:bad".to_owned()]);
    assert_eq!(
        status.lock_waits.first().and_then(|w| w.holder_pid),
        Some(77)
    );
    assert_eq!(status.end.as_ref().and_then(|e| e.exit_code), Some(1));
    let meta =
        SessionMeta::decode(&std::fs::read(dir.join(files::META)).expect("meta")).expect("meta");
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
    for f in [files::META, files::LOCK, files::EVENTS, files::STATUS] {
        assert_eq!(mode(&session.dir().join(f)), 0o600, "{f}");
    }
}

/// I2: the lock is held while the session lives, released by `finish`, and
/// its fd (like the events fd) is close-on-exec, so no child keeps it alive.
#[test]
fn lock_released_on_exit_and_not_inherited() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lock = lockfile::acquire_owner_lock(&dir.path().join(files::LOCK)).expect("lock");
    let events = std::fs::OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(dir.path().join(files::EVENTS))
        .expect("events");
    for fd in [lock.as_raw_fd(), events.as_raw_fd()] {
        // SAFETY: querying the flags of an fd this test owns.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(
            flags >= 0 && flags & libc::FD_CLOEXEC != 0,
            "fd {fd} is CLOEXEC"
        );
    }
    drop(events);
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

/// A writer that fails like a full disk.
struct Enospc;

impl Write for Enospc {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::from_raw_os_error(libc::ENOSPC))
    }
    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ENOSPC))
    }
}

/// A writer that blocks until released.
struct Blocked(Arc<(Mutex<bool>, std::sync::Condvar)>);

impl Write for Blocked {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let (lock, cv) = &*self.0;
        let mut open = lock.lock().expect("lock");
        while !*open {
            open = cv.wait(open).expect("wait");
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn recorder_with(dir: &Path, events: Box<dyn Write + Send>, faults: Faults) -> Recorder {
    let lock = lockfile::acquire_owner_lock(&dir.join(files::LOCK)).expect("lock");
    Recorder::spawn(RecorderStart {
        dir: dir.to_path_buf(),
        root: dir.join("no-root"),
        lock,
        events,
        retention: Retention::default(),
        faults,
    })
    .expect("spawn")
}

/// A `MakeWriter` collecting log output, to count warnings.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
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
    tracing::subscriber::with_default(subscriber, || {
        let rec = recorder_with(dir.path(), Box::new(Enospc), Faults::default());
        let tap = rec.tap();
        // Past one BufWriter's worth, so the failure surfaces mid-stream.
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
        rec.finish(Some(0), false, Duration::from_secs(1));
    });
    let log = String::from_utf8(captured.0.lock().expect("lock").clone()).expect("utf8");
    assert_eq!(log.matches("WARN").count(), 1, "{log}");
    assert!(!lockfile::is_held(&dir.path().join(files::LOCK)).expect("probe"));
}

/// I1/A8: a write that never returns blocks neither the sender nor the
/// finisher past its timeout.
#[test]
fn recorder_blocked_write_does_not_block_app() {
    let dir = tempfile::tempdir().expect("tempdir");
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let rec = recorder_with(
        dir.path(),
        Box::new(Blocked(Arc::clone(&gate))),
        Faults::default(),
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
    rec.finish(Some(0), false, Duration::from_millis(200));
    assert!(
        finishing.elapsed() < Duration::from_secs(2),
        "finish is bounded"
    );
    assert!(!lockfile::is_held(&dir.path().join(files::LOCK)).expect("probe"));
    let (lock, cv) = &*gate;
    *lock.lock().expect("lock") = true;
    cv.notify_all();
}

/// I1: a recorder that panics costs the recording, not the run.
#[test]
fn recorder_panic_does_not_fail_app() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rec = recorder_with(
        dir.path(),
        Box::new(io::sink()),
        Faults {
            panic_on_event: true,
        },
    );
    let tap = rec.tap();
    for _ in 0..100 {
        for ev in events() {
            tap.event(ev);
        }
    }
    rec.finish(Some(0), false, Duration::from_secs(1));
    assert!(!lockfile::is_held(&dir.path().join(files::LOCK)).expect("probe"));
}

/// A10: past the cap, one `Truncated` and nothing else but `End`.
#[test]
fn events_cap_truncates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = std::fs::File::create(dir.path().join(files::EVENTS)).expect("events");
    let rec = recorder_with(dir.path(), Box::new(file), Faults::default());
    let tap = rec.tap();
    let big = "x".repeat(1024 * 1024);
    for _ in 0..70 {
        tap.log("INFO", big.clone());
    }
    tap.event(events().remove(0));
    rec.finish(Some(0), false, Duration::from_secs(10));
    let size = std::fs::metadata(dir.path().join(files::EVENTS))
        .expect("stat")
        .len();
    assert!(size <= recorder::EVENTS_CAP + 1024, "{size}");
    let (records, _) = read_records(dir.path());
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
    let status =
        SessionSnapshot::decode(&std::fs::read(dir.path().join(files::STATUS)).expect("status"))
            .expect("decode");
    assert!(status.truncated);
    assert!(
        status.counts.matched.is_some(),
        "status keeps folding past the cap"
    );
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
async fn run_in(slot: &SessionSlot, records: bool) -> bool {
    let ran = Arc::new(AtomicBool::new(false));
    let (shutdown, _rx) = hcore::shutdown::ShutdownTrigger::new();
    crate::tui::run_app_in(
        slot,
        EmitApp {
            records,
            ran: Arc::clone(&ran),
        },
        crate::tui::LogSink::new_direct(),
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
    let slot = SessionSlot::new();
    slot.configure(
        SessionRoot::from_env(None, Some(home.path().as_os_str())),
        vec!["heph".into()],
    );
    assert!(run_in(&slot, true).await);
    let dir = slot.dir().expect("a session opened");
    assert!(dir.starts_with(home.path().join(".heph").join(SESSIONS_DIR)));
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
    let slot = SessionSlot::new();
    slot.configure(
        SessionRoot::from_env(None, Some(home.path().as_os_str())),
        Vec::new(),
    );
    assert!(run_in(&slot, false).await);
    assert!(slot.dir().is_none());
    assert_eq!(entries(home.path()), 0, "not even ~/.heph");
}

/// A4: `HEPH_NO_SESSION=1` writes nothing.
#[tokio::test]
async fn no_session_env_writes_nothing() {
    let home = tempfile::tempdir().expect("tempdir");
    let slot = SessionSlot::new();
    let root = SessionRoot::from_env(Some("1".as_ref()), Some(home.path().as_os_str()));
    assert_eq!(root, Ok(None));
    slot.configure(root, Vec::new());
    assert!(run_in(&slot, true).await);
    assert!(slot.dir().is_none());
    assert_eq!(entries(home.path()), 0);
}

/// A4: no `HOME` means no session, and the build still runs.
#[tokio::test]
async fn home_unset_runs_without_session() {
    let slot = SessionSlot::new();
    let root = SessionRoot::from_env(None, None);
    assert!(root.is_err());
    slot.configure(root, Vec::new());
    assert!(run_in(&slot, true).await);
    assert!(slot.dir().is_none());
    // An unwritable root behaves the same.
    let ro = tempfile::tempdir().expect("tempdir");
    std::fs::set_permissions(ro.path(), std::fs::Permissions::from_mode(0o500)).expect("chmod");
    let slot = SessionSlot::new();
    slot.configure(
        Ok(Some(SessionRoot::new(ro.path().join("sessions")))),
        Vec::new(),
    );
    assert!(run_in(&slot, true).await);
    assert!(slot.dir().is_none());
    std::fs::set_permissions(ro.path(), std::fs::Permissions::from_mode(0o700)).expect("chmod");
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
            SessionItem::Record(_) => records += 1,
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
