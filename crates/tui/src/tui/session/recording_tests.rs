use super::*;
use crate::tui::session::format::files;
use crate::tui::session::lockfile;

/// A session directory under `root` holding a copy of the format-1 fixture,
/// with `sql` run on the copy.
fn session_dir(root: &Path, sql: &str) -> PathBuf {
    let dir = root.join("01jfixture");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/tui/session/testdata/format-1")
        .join(store::FILE);
    std::fs::copy(fixture, dir.join(store::FILE)).expect("copy the fixture");
    let conn = Connection::open(dir.join(store::FILE)).expect("open the copy");
    conn.execute_batch(sql).expect("alter the copy");
    drop(conn);
    dir
}

fn roots(root: &Path) -> NoticeRoots {
    NoticeRoots {
        home: root.join("home"),
        root: root.join("ws"),
    }
}

fn store_read(_: &NoticeRoots, _: &[hcore::approval_notice::NoticeSource]) -> Option<String> {
    Some("from the store".to_string())
}

fn never_read(_: &NoticeRoots, _: &[hcore::approval_notice::NoticeSource]) -> Option<String> {
    panic!("a foreign recording read the local store")
}

/// Spec row `foreign_recording_touches_no_local_state` (I7): a recording
/// opened by path, even one sitting in a session directory whose lock is
/// held, takes its state from its row and its notices from its table, and
/// never reads the store.
#[test]
fn foreign_recording_touches_no_local_state() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = session_dir(
        tmp.path(),
        "UPDATE session SET sealed = 1, exit_code = 3, panicked = 0;
         INSERT INTO notices VALUES ('//p:deploy', 1, 'plan', 'the plan text');
         INSERT INTO failure_logs VALUES ('//p:t', 0, x'6f6f7073', 12);
         PRAGMA journal_mode = WAL;",
    );
    let _owner = lockfile::acquire_owner_lock(&dir.join(files::LOCK)).expect("hold the lock");

    // Opened as local, a held lock over a WAL header is a live owner.
    let local = Recording::open_local(tmp.path(), &dir).expect("open local");
    assert_eq!(local.state().expect("state"), RecordingState::Live);
    drop(local);

    let rec = Recording::open_foreign(&dir.join(store::FILE), &tmp.path().join(".tmp"))
        .expect("open foreign");
    assert!(rec.is_foreign());
    assert_eq!(
        rec.state().expect("state"),
        RecordingState::Finished {
            exit_code: Some(3),
            panicked: false
        }
    );
    let notice = ApprovalPromptNotice {
        name: "plan".to_string(),
        sources: vec![],
    };
    let r = roots(tmp.path());
    assert_eq!(
        rec.notice_text("//p:deploy", 1, &notice, &r, never_read),
        "the plan text"
    );
    // The near miss on each key reads as unavailable, not as another row.
    assert_eq!(
        rec.notice_text("//p:deploy", 2, &notice, &r, never_read),
        NOTICE_UNAVAILABLE
    );
    assert_eq!(
        rec.notice_text("//p:deploy2", 1, &notice, &r, never_read),
        NOTICE_UNAVAILABLE
    );
    let Recording::Foreign(f) = &rec else {
        panic!("foreign")
    };
    assert_eq!(
        f.failure_logs("//p:t").expect("logs"),
        [(b"oops".to_vec(), 12)]
    );
    assert!(f.failure_logs("//p:t2").expect("logs").is_empty());
}

/// A local recording reads its notices from the store.
#[test]
fn a_local_recording_reads_notices_from_the_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = session_dir(tmp.path(), "UPDATE session SET sealed = 1;");
    let rec = Recording::open_local(tmp.path(), &dir).expect("open local");
    let notice = ApprovalPromptNotice {
        name: "plan".to_string(),
        sources: vec![hcore::approval_notice::NoticeSource {
            addr: "//p:plan".to_string(),
            origin: hcore::approval_notice::NoticeOrigin::File {
                path: "plan.txt".to_string(),
                out_path: "plan.txt".to_string(),
                x: false,
                hashout: String::new(),
            },
        }],
    };
    assert_eq!(
        rec.notice_text("//p:deploy", 1, &notice, &roots(tmp.path()), store_read),
        "from the store"
    );
}

/// Sealed with no end (cleanup sealed a lost session), or not sealed (a
/// snapshot or a copied live file): incomplete. A panic without an exit
/// code is finished.
#[test]
fn a_foreign_state_comes_from_the_row() {
    let cases = [
        (
            "UPDATE session SET sealed = 1, exit_code = NULL, panicked = NULL;",
            RecordingState::Incomplete,
        ),
        (
            "UPDATE session SET sealed = 0, exit_code = 0;",
            RecordingState::Incomplete,
        ),
        (
            "UPDATE session SET sealed = 1, exit_code = NULL, panicked = 1;",
            RecordingState::Finished {
                exit_code: None,
                panicked: true,
            },
        ),
        (
            "UPDATE session SET sealed = 1, exit_code = 0, panicked = 0;",
            RecordingState::Finished {
                exit_code: Some(0),
                panicked: false,
            },
        ),
    ];
    for (sql, want) in cases {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = session_dir(tmp.path(), sql);
        let rec = Recording::open_foreign(&dir.join(store::FILE), &tmp.path().join(".tmp"))
            .expect("open foreign");
        assert_eq!(rec.state().expect("state"), want, "{sql}");
    }
}
