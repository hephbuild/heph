use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt as _;

use super::*;
use crate::tui::session::format::files;
use crate::tui::session::recorder::Gate;
use crate::tui::session::recording::RecordingState;
use crate::tui::session::tests::{Captured, start};
use crate::tui::session::{
    HeaderKind, META_FORMAT, SealExport, Session, SessionHeader, SessionMeta, SessionRoot,
    SessionStart, lockfile, testing,
};
use crate::tui::session::{cleanup, recorder};

fn no_read(_: &NoticeRoots, _: &[hcore::approval_notice::NoticeSource]) -> Option<String> {
    None
}

fn seal_export(dir: &Path, bound: Duration, hold: Option<Hold>) -> SealExport {
    SealExport {
        dir: dir.to_path_buf(),
        read: no_read,
        bound,
        hold: hold.unwrap_or_default(),
    }
}

fn open_gate(gate: &Gate) {
    let (open, opened) = &**gate;
    *open.lock().expect("gate") = true;
    opened.notify_all();
}

/// A session under `root` exporting to `out` at seal.
fn exporting(root: &SessionRoot, export: SealExport) -> Session {
    Session::open(
        root,
        SessionStart {
            export: Some(export),
            ..start()
        },
    )
    .expect("open")
}

/// The names in `dir`, sorted; none when it is missing.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => panic!("reading {}: {e}", dir.display()),
    };
    names.sort();
    names
}

/// Spec row `export_dir_one_file_per_session` (D7): every session that sees
/// the export directory adds its own `<id>.hephsession`, a nested one
/// included, none over another, each a whole sealed recording; no temp file
/// is left beside them.
#[test]
fn export_dir_one_file_per_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = SessionRoot::new(tmp.path().join("sessions"));
    let out = tmp.path().join("exports");
    // A nested run: the inner session ends before the outer one.
    let outer = exporting(&root, seal_export(&out, EXPORT_DIR_BOUND, None));
    let inner = exporting(&root, seal_export(&out, EXPORT_DIR_BOUND, None));
    inner.finish(Some(0), false);
    outer.finish(Some(2), false);

    let mut want = vec![file_name(outer.id()), file_name(inner.id())];
    want.sort();
    assert_eq!(entries(&out), want);
    for (id, code) in [(outer.id(), 2), (inner.id(), 0)] {
        let rec = Recording::open_foreign(&out.join(file_name(id)), &tmp.path().join("ftmp"))
            .expect("open the export");
        assert_eq!(rec.meta().expect("meta").id, id);
        assert_eq!(
            rec.state().expect("state"),
            RecordingState::Finished {
                exit_code: Some(code),
                panicked: false
            }
        );
    }
    assert!(entries(&store::tmp_dir(root.path())).is_empty());
}

/// A private session (`HEPH_NO_SESSION=1`) keeps no recording, so it never
/// exports one.
#[test]
fn a_private_session_is_never_exported() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = tmp.path().join("exports");
    let session = exporting(
        &SessionRoot::private(tmp.path().join("private")),
        seal_export(&out, EXPORT_DIR_BOUND, None),
    );
    session.finish(Some(0), false);
    assert_eq!(entries(&out), Vec::<String>::new());
}

/// Spec row `export_is_silent_and_bounded`: a seal-time export that succeeds
/// logs nothing; one past its bound is stopped with one warning, its temp
/// file beside the destination removed, nothing written even once it would
/// have gone on, and the session's end recorded as it was.
#[test]
fn export_is_silent_and_bounded() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = SessionRoot::new(tmp.path().join("sessions"));
    let out = tmp.path().join("exports");
    let gate: Gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        .finish();
    let (fast, slow) = tracing::subscriber::with_default(subscriber, || {
        let fast = exporting(&root, seal_export(&out, EXPORT_DIR_BOUND, None));
        fast.finish(Some(0), false);
        let slow = exporting(
            &root,
            seal_export(
                &out,
                Duration::from_millis(200),
                Some(Hold {
                    compressing: Some(gate.clone()),
                    ..Hold::default()
                }),
            ),
        );
        slow.finish(Some(3), false);
        // Held past its bound, with its temp file already made beside the
        // destination: stopped, and every temp file gone while the export
        // is still held (the process may exit now), its snapshot included.
        assert_eq!(entries(&out), [file_name(fast.id())]);
        assert_eq!(entries(&store::tmp_dir(root.path())), Vec::<String>::new());
        (fast.id(), slow.id())
    });
    // Let the stopped export go on: it sees the abort and writes nothing.
    {
        let (open, opened) = &*gate;
        *open.lock().expect("gate") = true;
        opened.notify_all();
    }
    let log = captured.settled_log();
    assert_eq!(entries(&out), [file_name(fast)]);
    assert!(entries(&store::tmp_dir(root.path())).is_empty());
    assert_eq!(log.matches("WARN").count(), 1, "{log}");
    assert!(log.contains("did not finish in time"), "{log}");
    // Where, and why: the one line a CI log keeps.
    assert!(
        log.contains(&format!("dir={}", out.display())) && log.contains("reason="),
        "{log}"
    );
    let rec =
        Recording::open_local(root.path(), &root.path().join(slow.encode())).expect("open local");
    assert_eq!(
        rec.state().expect("state"),
        RecordingState::Finished {
            exit_code: Some(3),
            panicked: false
        }
    );
}

/// I1 over a stalled filesystem: an export stuck renaming its file into
/// place, under the lock its temp files are kept by, never holds up the
/// exit. The abort returns promptly without that lock; once the write
/// returns, the export finds itself stopped and removes what it made, the
/// destination included, so nothing is left in the export directory.
#[test]
fn an_abort_never_waits_on_a_stalled_write() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = SessionRoot::new(tmp.path().join("sessions"));
    let out = tmp.path().join("exports");
    let gate: Gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let captured = Captured::default();
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .finish(),
    );
    let session = tracing::dispatcher::with_default(&dispatch, || {
        exporting(
            &root,
            seal_export(
                &out,
                Duration::from_millis(200),
                Some(Hold {
                    persisting: Some(gate.clone()),
                    ..Hold::default()
                }),
            ),
        )
    });
    // `finish` on a thread of its own: a hang is a failed test, not a hung one.
    let (tx, rx) = std::sync::mpsc::channel();
    let finishing = dispatch.clone();
    std::thread::spawn(move || {
        let _dispatch = tracing::dispatcher::set_default(&finishing);
        session.finish(Some(0), false);
        tx.send(()).expect("the test waits for finish");
    });
    drop(dispatch);
    // The bound (200 ms) plus the abort's brief wait for its cleanup.
    let returned = rx.recv_timeout(Duration::from_secs(5));
    // Let the stalled write return either way, so nothing is left hanging.
    open_gate(&gate);
    returned.expect("finish did not return while the export was stuck in its write");
    let log = captured.settled_log();
    assert_eq!(entries(&out), Vec::<String>::new(), "{log}");
    assert!(entries(&store::tmp_dir(root.path())).is_empty());
    assert_eq!(log.matches("WARN").count(), 1, "{log}");
    assert!(log.contains("did not finish in time"), "{log}");
}

/// `f` under a fresh subscriber, and what it logged once settled.
fn logged<T>(f: impl FnOnce() -> T) -> (T, String) {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        .finish();
    let out = tracing::subscriber::with_default(subscriber, f);
    (out, captured.settled_log())
}

/// The local recording `id` under `root` ended with `exit_code`.
fn assert_ended(root: &SessionRoot, id: SessionId, exit_code: i32) {
    let rec =
        Recording::open_local(root.path(), &root.path().join(id.encode())).expect("open local");
    assert_eq!(
        rec.state().expect("state"),
        RecordingState::Finished {
            exit_code: Some(exit_code),
            panicked: false
        }
    );
}

/// A recorder that does not let go in time (`Finished::Late`: a hung disk
/// holds its `End`) is not exported: the export would read a recording
/// still being written. One warning naming the directory and why; nothing
/// in it, even once the disk comes back.
#[test]
fn a_late_recorder_is_not_exported() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = SessionRoot::new(tmp.path().join("sessions"));
    let out = tmp.path().join("exports");
    let gate: Gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let ((), log) = logged(|| {
        let session = Session::open_with(
            &root,
            SessionStart {
                export: Some(seal_export(&out, EXPORT_DIR_BOUND, None)),
                ..start()
            },
            cleanup::Retention::default(),
            recorder::Faults {
                gate: Some(Arc::clone(&gate)),
                ..recorder::Faults::default()
            },
        )
        .expect("open");
        session.finish(Some(0), false);
        assert_eq!(entries(&out), Vec::<String>::new());
        let (open, opened) = &*gate;
        *open.lock().expect("gate") = true;
        opened.notify_all();
    });
    assert_eq!(entries(&out), Vec::<String>::new());
    assert_eq!(log.matches("WARN").count(), 1, "{log}");
    assert!(log.contains("it is not exported"), "{log}");
    assert!(
        log.contains(&format!("dir={}", out.display())) && log.contains("reason="),
        "{log}"
    );
}

/// An export that fails at seal (the directory is a file, or cannot be
/// written) costs one warning naming the directory and the error, and
/// nothing else: the session's end is recorded as it was.
#[test]
fn a_seal_time_export_failure_is_one_warning() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = SessionRoot::new(tmp.path().join("sessions"));
    let a_file = tmp.path().join("a-file");
    std::fs::write(&a_file, "not a directory").expect("write");
    let read_only = tmp.path().join("read-only");
    std::fs::create_dir(&read_only).expect("mkdir");
    std::fs::set_permissions(&read_only, Permissions::from_mode(0o555)).expect("chmod");
    // Root writes anywhere: that case says nothing there.
    let probe = read_only.join("probe");
    let writable = std::fs::write(&probe, "").is_ok();
    let mut cases = vec![(a_file.clone(), "creating")];
    if writable {
        eprintln!("skipping the unwritable-directory case: it is writable here");
    } else {
        cases.push((read_only.clone(), "temp file"));
    }
    for (out, why) in cases {
        let (id, log) = logged(|| {
            let session = exporting(&root, seal_export(&out, EXPORT_DIR_BOUND, None));
            session.finish(Some(4), false);
            session.id()
        });
        assert_eq!(log.matches("WARN").count(), 1, "{}: {log}", out.display());
        assert!(log.contains("cannot export the session"), "{log}");
        assert!(
            log.contains(&format!("dir={}", out.display()))
                && log.contains("error=")
                && log.contains(why),
            "{log}"
        );
        assert_ended(&root, id, 4);
    }
    assert_eq!(std::fs::read(&a_file).expect("read"), b"not a directory");
    assert!(entries(&read_only).iter().all(|n| n == "probe"));
    std::fs::set_permissions(&read_only, Permissions::from_mode(0o755)).expect("chmod");
}

/// The seal-time export never replaces a file: one already at
/// `<dir>/<id>.hephsession` is kept, with one warning.
#[test]
fn a_seal_time_export_never_overwrites() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = SessionRoot::new(tmp.path().join("sessions"));
    let out = tmp.path().join("exports");
    std::fs::create_dir(&out).expect("mkdir");
    let (dest, log) = logged(|| {
        let session = exporting(&root, seal_export(&out, EXPORT_DIR_BOUND, None));
        let dest = out.join(file_name(session.id()));
        std::fs::write(&dest, "keep").expect("write");
        session.finish(Some(0), false);
        dest
    });
    assert_eq!(std::fs::read(&dest).expect("read"), b"keep");
    assert_eq!(entries(&out).len(), 1, "{:?}", entries(&out));
    assert_eq!(log.matches("WARN").count(), 1, "{log}");
    assert!(
        log.contains("cannot export the session") && log.contains(&dest.display().to_string()),
        "{log}"
    );
}

/// An export directory inside the workspace is warned about once, when the
/// session opens, naming both and what it costs; one beside the workspace
/// (a near-miss name included) is not, and neither is a private session.
/// The export still happens: the warning refuses nothing.
#[test]
fn an_export_dir_in_the_workspace_is_warned_about_once() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("mkdir");
    let warnings = |export: &Path, root: SessionRoot| {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .finish();
        let slot = crate::tui::session::SessionSlot::new();
        slot.configure(Ok(Some(root)), vec!["heph".to_owned()]);
        slot.set_workspace(&ws, &ws, &ws.join(".heph"));
        slot.configure_export(Some(export.as_os_str()), no_read);
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..2 {
                assert!(slot.open(SessionHeader::default(), false).is_some());
            }
            slot.finish(Some(0));
        });
        captured.settled_log()
    };
    let sessions = || SessionRoot::new(tmp.path().join("sessions"));
    for inside in [
        ws.join("out/sessions"),
        ws.clone(),
        // Not there yet, through `..`: still inside.
        ws.join("a/../b"),
    ] {
        let log = warnings(&inside, sessions());
        assert_eq!(
            log.matches("WARN").count(),
            1,
            "{}: {log}",
            inside.display()
        );
        assert!(
            log.contains("inside the workspace")
                && log.contains("misses the cache")
                && log.contains(&format!("workspace={}", ws.display())),
            "{log}"
        );
    }
    assert_eq!(
        entries(&ws.join("b")).len(),
        1,
        "warned, and exported all the same"
    );
    for outside in [tmp.path().join("ws2"), tmp.path().join("exports")] {
        let log = warnings(&outside, sessions());
        assert!(!log.contains("WARN"), "{}: {log}", outside.display());
    }
    let private = warnings(
        &ws.join("out"),
        SessionRoot::private(tmp.path().join("private")),
    );
    assert!(!private.contains("WARN"), "{private}");
}

/// `HEPH_SESSION_EXPORT_DIR`: unset or empty is off; only an absolute path
/// is a directory, a relative one is refused with a reason naming the
/// variable.
#[test]
fn export_dir_from_env_wants_an_absolute_path() {
    assert_eq!(export_dir_from_env(None), Ok(None));
    assert_eq!(export_dir_from_env(Some(OsStr::new(""))), Ok(None));
    assert_eq!(
        export_dir_from_env(Some(OsStr::new("/ci/sessions"))),
        Ok(Some(PathBuf::from("/ci/sessions")))
    );
    for relative in ["sessions", "./sessions", "../sessions", "~/sessions"] {
        let why = export_dir_from_env(Some(OsStr::new(relative))).expect_err(relative);
        assert!(
            why.contains(hcore::session_record::SESSION_EXPORT_DIR_ENV) && why.contains("absolute"),
            "{why}"
        );
    }
}

fn meta(id: SessionId) -> SessionMeta {
    SessionMeta {
        format: META_FORMAT,
        id,
        owner_pid: 1,
        started_unix_ms: id.started_unix_ms(),
        heph_version: "v0.0.0".to_owned(),
        argv: vec!["heph".to_owned(), "run".to_owned()],
        cwd: None,
        workspace_root: None,
        home_dir: None,
        label: "run".to_owned(),
        header: HeaderKind::Build,
        interactive: false,
        capabilities: Vec::new(),
    }
}

/// Spec row `export_of_a_live_session_is_a_snapshot` (E3): an export taken
/// while the owner records opens, reads `sealed=0`, and says it is a snapshot.
#[test]
fn export_of_a_live_session_is_a_snapshot() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let id = SessionId::new(1_763_000_000_000, 7);
    let dir = root.join(id.encode());
    let _rec = testing::Recording::create(&dir, &meta(id), &[], None).expect("recording");
    let _owner = lockfile::acquire_owner_lock(&dir.join(files::LOCK)).expect("hold the lock");

    let rec = Recording::open_local(&root, &dir).expect("open local");
    assert_eq!(rec.state().expect("state"), RecordingState::Live);
    let db = snapshot(&rec, None, &store::tmp_dir(&root), &Abort::default()).expect("snapshot");
    let out = tmp.path().join("out").join(file_name(id));
    std::fs::create_dir_all(out.parent().expect("parent")).expect("mkdir");
    write_file(&db, &out, false, &Abort::default()).expect("write");
    drop(db);

    let foreign = Recording::open_foreign(&out, &tmp.path().join("ftmp")).expect("open foreign");
    let sealed: bool = foreign
        .conn()
        .query_row("SELECT sealed FROM session", [], |r| r.get(0))
        .expect("sealed");
    assert!(!sealed, "a live session's copy is not sealed");
    let state = foreign.state().expect("state");
    assert_eq!(state, RecordingState::Snapshot);
    assert_eq!(state.caveat(), Some("snapshot of a running session"));
}

/// A foreign recording exports again, kept as it is or with its logs left
/// out, and its schema runs nothing while it does: the copy's `--no-logs`
/// `DELETE`s run with triggers off, so a trigger planted in the file (one
/// that would refuse the delete) never fires.
#[test]
fn a_foreign_recording_exports_again_without_running_its_triggers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("crafted.db");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/tui/session/testdata/format-1")
        .join(store::FILE);
    std::fs::copy(fixture, &path).expect("copy the fixture");
    let conn = Connection::open(&path).expect("open the copy");
    conn.execute_batch(
        "INSERT INTO notices VALUES ('//p:a', 1, 'plan', 'text');
         CREATE TRIGGER planted BEFORE DELETE ON notices
           BEGIN SELECT RAISE(ABORT, 'a planted trigger ran'); END;",
    )
    .expect("plant a trigger");
    drop(conn);

    let rec = Recording::open_foreign(&path, &tmp.path().join("ftmp")).expect("open foreign");
    for (logs, want) in [(Some(no_read as ReadNotice), 1), (None, 0)] {
        let db = snapshot(&rec, logs, &tmp.path().join("tmp"), &Abort::default())
            .expect("export a foreign recording again");
        let out = tmp.path().join("again.hephsession");
        write_file(&db, &out, false, &Abort::default()).expect("write");
        drop(db);
        let again = Recording::open_foreign(&out, &tmp.path().join("ftmp")).expect("reopen");
        let notices: i64 = again
            .conn()
            .query_row("SELECT count(*) FROM notices", [], |r| r.get(0))
            .expect("count");
        assert_eq!(notices, want, "logs kept: {}", logs.is_some());
    }
}

/// What the committed export fixture's approval notice says.
const FIXTURE_NOTICE: &str =
    "Migrates the production database: drops the column `users.legacy_id`.";
/// The root failure's log in the committed export fixture.
const FIXTURE_LOG: &str = "applying 0042_drop_legacy_id\nerror: column \"legacy_id\" is still read by view \"active_users\"\n";
/// When the committed export fixture's session started.
const FIXTURE_EXPORT_STARTED_MS: u64 = 1_763_000_000_000;

/// The notice reader the export fixture was written with: it stands in for
/// the engine's `read_notice_from_store`, which this crate cannot reach.
fn fixture_notice(_: &NoticeRoots, _: &[hcore::approval_notice::NoticeSource]) -> Option<String> {
    Some(FIXTURE_NOTICE.to_owned())
}

/// Format `n`'s committed export: what `HEPH_SESSION_EXPORT_DIR` leaves.
fn export_fixture(n: u32) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("src/tui/session/testdata/format-{n}"))
        .join(format!("example.{EXTENSION}"))
}

/// A finished `heph run` under `tmp` that asked for one approval (with a
/// notice) and failed at a root whose log is in its failure slot, exported
/// at seal as `HEPH_SESSION_EXPORT_DIR` exports it. Returns the export.
fn write_export_fixture(tmp: &Path) -> PathBuf {
    use crate::tui::session::{ApprovalPrompt, ApprovalPromptNotice, ApprovalQueued};
    use hcore::approval_notice::{NoticeOrigin, NoticeSource};
    use hcore::events::LeftBehindData;

    let t0 = FIXTURE_EXPORT_STARTED_MS;
    let root = tmp.join("sessions");
    let ws = tmp.join("ws");
    let home = ws.join(".heph");
    let log = home.join("failures").join("k1").join("log.txt");
    std::fs::create_dir_all(log.parent().expect("parent")).expect("mkdir");
    std::fs::write(&log, FIXTURE_LOG).expect("write the log");
    // Written during the session: an export keeps no log newer than its end.
    std::fs::File::options()
        .write(true)
        .open(&log)
        .and_then(|f| f.set_modified(std::time::UNIX_EPOCH + Duration::from_millis(t0 + 40)))
        .expect("set the log's mtime");

    let id = SessionId::new(t0, 1);
    let dir = root.join(id.encode());
    let mut m = meta(id);
    m.argv = ["heph", "run", "//app:deploy"].map(str::to_owned).into();
    m.cwd = Some(ws.display().to_string());
    m.workspace_root = Some(ws.display().to_string());
    m.home_dir = Some(home.display().to_string());
    let ev = |ms: u64, kind: BuildEventKind| SessionRecord {
        at_unix_ms: t0 + ms,
        kind: Some(kind),
        session: None,
    };
    let records = [
        ev(
            1,
            BuildEventKind::Matched {
                addrs: vec!["//app:deploy".to_owned()],
                complete: true,
            },
        ),
        ev(
            2,
            BuildEventKind::ResultStart {
                addr: "//app:deploy".to_owned(),
            },
        ),
        ev(
            3,
            BuildEventKind::ResultStart {
                addr: "//app:migrate".to_owned(),
            },
        ),
        SessionRecord::session(
            t0 + 10,
            SessionRecordKind::Approval {
                prompt: Some(Box::new(ApprovalPrompt {
                    queue: vec![ApprovalQueued {
                        seq: 1,
                        addr: "//app:migrate".to_owned(),
                        queued_at_unix_ms: t0 + 10,
                        notices: vec![ApprovalPromptNotice {
                            name: "plan".to_owned(),
                            sources: vec![NoticeSource {
                                addr: "//app:plan".to_owned(),
                                origin: NoticeOrigin::File {
                                    path: "PLAN".to_owned(),
                                    out_path: "PLAN".to_owned(),
                                    x: false,
                                    hashout: "0".to_owned(),
                                },
                            }],
                        }],
                    }],
                })),
                standing: None,
            },
        ),
        SessionRecord::session(
            t0 + 20,
            SessionRecordKind::Approval {
                prompt: None,
                standing: None,
            },
        ),
        ev(
            40,
            BuildEventKind::ResultEnd {
                addr: "//app:migrate".to_owned(),
                error: Some("exit status: 1".to_owned()),
                upstream_of: None,
                exit_status: Some("exit status: 1".to_owned()),
                log_tail: None,
                timed_out: None,
                cancelled: false,
                left_behind: Some(Box::new(LeftBehindData {
                    log_path: Some(log.display().to_string()),
                    ..LeftBehindData::default()
                })),
                spans: Vec::new(),
            },
        ),
        ev(
            41,
            BuildEventKind::ResultEnd {
                addr: "//app:deploy".to_owned(),
                error: Some("//app:migrate failed".to_owned()),
                upstream_of: Some("//app:migrate".to_owned()),
                exit_status: None,
                log_tail: None,
                timed_out: None,
                cancelled: false,
                left_behind: None,
                spans: Vec::new(),
            },
        ),
        SessionRecord::session(
            t0 + 50,
            SessionRecordKind::End {
                exit_code: Some(1),
                panicked: false,
            },
        ),
    ];
    testing::Recording::create(&dir, &m, &records, None)
        .expect("recording")
        .seal(t0 + 50, Some(1), false)
        .expect("seal");
    let out = tmp.join("exports");
    to_dir(&root, &dir, id, &out, fixture_notice, &Abort::default()).expect("export")
}

/// Writes the current format's export fixture. Run it only for a format
/// that has none yet: a released format's fixture is frozen. See
/// `testdata/README.md`.
#[test]
#[ignore = "writes the committed fixture; run by hand on a format bump"]
fn generate_export_fixture() {
    let target = export_fixture(store::FORMAT);
    assert!(
        !target.exists(),
        "{} exists: a released format's fixture is frozen; delete it first only if format {} was never released",
        target.display(),
        store::FORMAT
    );
    let tmp = tempfile::tempdir().expect("tempdir");
    let written = write_export_fixture(tmp.path());
    std::fs::create_dir_all(target.parent().expect("a parent")).expect("mkdir");
    std::fs::copy(written, &target).expect("copy the fixture");
}

/// Every released format's committed export (what a CI run's
/// `HEPH_SESSION_EXPORT_DIR` leaves) opens through the path `heph session
/// show`/`attach` take, and reads back the run it holds from itself alone:
/// its state, the approval's notice text, the root failure's log, and the
/// tables a reader queries, `notices` and `failure_logs` by their columns.
#[test]
fn opens_export_fixtures() {
    for n in 1..=store::FORMAT {
        let path = export_fixture(n);
        assert!(
            path.exists(),
            "format {n} has no committed export at {}: run `cargo test -p tui generate_export_fixture -- --ignored`",
            path.display()
        );
        let bytes = std::fs::read(&path).expect("read the export");
        assert_eq!(
            bytes.get(..4),
            Some(&[0x28, 0xB5, 0x2F, 0xFD][..]),
            "a zstd frame"
        );
        let tmp = tempfile::tempdir().expect("tempdir");
        let rec = Recording::open_foreign(&path, tmp.path()).expect("open the export");
        let meta = rec.meta().expect("meta");
        assert_eq!(
            meta.id,
            SessionId::new(FIXTURE_EXPORT_STARTED_MS, 1),
            "format {n}"
        );
        assert_eq!(meta.argv, ["heph", "run", "//app:deploy"]);
        assert_eq!(
            rec.state().expect("state"),
            RecordingState::Finished {
                exit_code: Some(1),
                panicked: false
            },
            "format {n}"
        );
        let conn = rec.conn();
        let row: (bool, bool, Option<i64>) = conn
            .query_row("SELECT sealed, snapshot, exit_code FROM session", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .expect("session row");
        assert_eq!(
            row,
            (true, false, Some(1)),
            "format {n}: sealed, not a snapshot"
        );
        let notices: Vec<(String, i64, String, String)> = conn
            .prepare("SELECT addr, seq, name, text FROM notices")
            .and_then(|mut s| {
                s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                    .collect()
            })
            .expect("notices");
        assert_eq!(
            notices,
            [(
                "//app:migrate".to_owned(),
                1,
                "plan".to_owned(),
                FIXTURE_NOTICE.to_owned()
            )],
            "format {n}"
        );
        let logs: Vec<(String, i64, Vec<u8>, i64)> = conn
            .prepare("SELECT addr, nth, log, cut_bytes FROM failure_logs")
            .and_then(|mut s| {
                s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                    .collect()
            })
            .expect("failure logs");
        assert_eq!(
            logs,
            [(
                "//app:migrate".to_owned(),
                0,
                FIXTURE_LOG.as_bytes().to_vec(),
                0
            )],
            "format {n}: the root failure's log, not the collateral one's"
        );
        let kinds: Vec<String> = conn
            .prepare("SELECT kind FROM records ORDER BY seq")
            .and_then(|mut s| s.query_map([], |r| r.get(0))?.collect())
            .expect("records");
        assert_eq!(kinds.last().map(String::as_str), Some("End"), "format {n}");
        assert_eq!(kinds.iter().filter(|k| *k == "Approval").count(), 2);

        let Recording::Foreign(f) = &rec else {
            panic!("foreign")
        };
        assert_eq!(
            f.recorded_notice("//app:migrate", 1, "plan")
                .expect("notice"),
            Some(FIXTURE_NOTICE.to_owned())
        );
        assert_eq!(
            f.failure_logs("//app:migrate").expect("logs"),
            [(FIXTURE_LOG.as_bytes().to_vec(), 0)]
        );
        assert!(f.failure_logs("//app:deploy").expect("logs").is_empty());

        // The viewers' source: everything above, read once at open.
        let source = rec.into_source().expect("a viewer's source");
        assert_eq!(
            source.entry().id,
            SessionId::new(FIXTURE_EXPORT_STARTED_MS, 1)
        );
        assert_eq!(
            source
                .notices()
                .get(&("//app:migrate".to_owned(), 1, "plan".to_owned())),
            Some(&FIXTURE_NOTICE.to_owned())
        );
        let logs: Vec<(&str, &[u8])> = source
            .failure_logs()
            .iter()
            .map(|l| (l.addr.as_str(), l.log.as_slice()))
            .collect();
        assert_eq!(logs, [("//app:migrate", FIXTURE_LOG.as_bytes())]);
    }
}
