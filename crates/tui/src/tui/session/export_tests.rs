use super::*;
use crate::tui::session::format::files;
use crate::tui::session::recorder::Gate;
use crate::tui::session::recording::RecordingState;
use crate::tui::session::tests::{Captured, start};
use crate::tui::session::{
    HeaderKind, META_FORMAT, SealExport, Session, SessionMeta, SessionRoot, SessionStart, lockfile,
    testing,
};

fn no_read(_: &NoticeRoots, _: &[hcore::approval_notice::NoticeSource]) -> Option<String> {
    None
}

fn seal_export(dir: &Path, bound: Duration, gate: Option<Gate>) -> SealExport {
    SealExport {
        dir: dir.to_path_buf(),
        read: no_read,
        bound,
        gate,
    }
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
            seal_export(&out, Duration::from_millis(200), Some(gate.clone())),
        );
        slow.finish(Some(3), false);
        // Held past its bound, with its temp file already made beside the
        // destination: stopped, and the temp file gone.
        assert_eq!(entries(&out), [file_name(fast.id())]);
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
