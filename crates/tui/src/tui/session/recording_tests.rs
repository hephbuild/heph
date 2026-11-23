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
        "UPDATE session SET sealed = 0, exit_code = NULL, panicked = 0;
         INSERT INTO notices VALUES ('//p:deploy', 1, 'plan', 'the plan text');
         INSERT INTO failure_logs VALUES ('//p:t', 0, x'6f6f7073', 12);
         PRAGMA journal_mode = WAL;",
    );
    let _owner = lockfile::acquire_owner_lock(&dir.join(files::LOCK)).expect("hold the lock");
    let file = dir.join(store::FILE);
    let tmp_dir = tmp.path().join(".tmp");

    // Opened as local, a held lock over an unsealed WAL file is a live owner.
    // (Over a `sealed=1` row, a held lock is the owner sealing: not live.)
    let local = Recording::open_local(tmp.path(), &dir).expect("open local");
    assert_eq!(local.state().expect("state"), RecordingState::Live);
    drop(local);
    // Opened by path, the same file under the same held lock is read from its
    // row alone: unsealed is incomplete, never live.
    let rec = Recording::open_foreign(&file, &tmp_dir).expect("open foreign");
    assert!(rec.is_foreign());
    assert_eq!(rec.state().expect("state"), RecordingState::Incomplete);
    drop(rec);

    // Sealed with an exit code, the row is all a foreign reader goes by.
    let conn = Connection::open(&file).expect("open the copy");
    conn.execute_batch("UPDATE session SET sealed = 1, exit_code = 3;")
        .expect("seal the copy");
    drop(conn);
    let rec = Recording::open_foreign(&file, &tmp_dir).expect("open foreign");
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

/// A foreign recording's notices and failure logs are cut as an export cuts
/// them, whichever way they are read: a 10 MiB notice to `NOTICE_READ_CAP`,
/// a 5 MiB log to its last `FAILURE_LOG_CAP` with the rest counted as cut.
#[test]
fn foreign_notices_and_logs_are_cut() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = session_dir(
        tmp.path(),
        "INSERT INTO notices VALUES ('//p:a', 1, 'plan', hex(zeroblob(5 * 1024 * 1024)));
         INSERT INTO failure_logs VALUES ('//p:a', 0, zeroblob(5 * 1024 * 1024), 7);",
    );
    let rec = Recording::open_foreign(&dir.join(store::FILE), &tmp.path().join(".tmp"))
        .expect("open foreign");
    let Recording::Foreign(f) = &rec else {
        panic!("foreign")
    };
    let notice = f
        .recorded_notice("//p:a", 1, "plan")
        .expect("notice")
        .expect("a row");
    assert_eq!(notice.len(), NOTICE_READ_CAP);
    let cut = u64::try_from(5 << 20).expect("small") - FAILURE_LOG_CAP + 7;
    let logs = f.failure_logs("//p:a").expect("logs");
    assert_eq!(logs.len(), 1);
    assert_eq!(logs.first().map(|(l, c)| (l.len() as u64, *c)), Some((FAILURE_LOG_CAP, cut)));

    let source = rec.into_source().expect("source");
    let notices = source.notices();
    let text = notices
        .get(&("//p:a".to_owned(), 1, "plan".to_owned()))
        .expect("the notice");
    assert_eq!(text.len(), NOTICE_READ_CAP);
    let [log] = source.failure_logs() else {
        panic!("one log")
    };
    assert_eq!((log.log.len() as u64, log.cut_bytes), (FAILURE_LOG_CAP, cut));
}

/// Every row costs the budget its addr and name too, and a constant: rows
/// of empty text (or no text at all) are not free, so a file of many rows
/// loads only what the budget holds.
#[test]
fn foreign_rows_are_charged_in_full() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = session_dir(
        tmp.path(),
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000)
         INSERT INTO notices SELECT printf('//p:%0100d', i), 1, 'plan', '' FROM n;
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000)
         INSERT INTO failure_logs SELECT printf('//p:%0100d', i), 0, x'', 0 FROM n;",
    );
    let rec = Recording::open_foreign(&dir.join(store::FILE), &tmp.path().join(".tmp"))
        .expect("open foreign");
    let budget = 10_000;
    let notices = read_notices(rec.conn(), budget).expect("notices");
    let charged: usize = notices
        .iter()
        .map(|((addr, _, name), text)| addr.len() + name.len() + text.len() + ROW_COST)
        .sum();
    assert!(!notices.is_empty() && charged <= budget, "{} rows, {charged} bytes", notices.len());
    let logs = read_failure_logs(rec.conn(), None, budget as u64).expect("logs");
    let charged: usize = logs.iter().map(|l| l.addr.len() + l.log.len() + ROW_COST).sum();
    assert!(!logs.is_empty() && charged <= budget, "{} rows, {charged} bytes", logs.len());
}

/// A value past the foreign length limit is refused with an error, not read
/// into memory.
#[test]
fn an_oversized_foreign_value_is_refused() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let over = hephsession::VALUE_CAP + 1;
    let dir = session_dir(
        tmp.path(),
        &format!("INSERT INTO notices VALUES ('//p:a', 1, 'plan', zeroblob({over}));"),
    );
    let rec = Recording::open_foreign(&dir.join(store::FILE), &tmp.path().join(".tmp"))
        .expect("open foreign");
    let Err(err) = rec.into_source() else {
        panic!("a value past the limit was read")
    };
    assert!(format!("{err:#}").contains("too big"), "{err:#}");
}
