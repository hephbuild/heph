//! The session catalog (spec "The catalog", I8, D14, D18, D19).

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hcore::events::{BuildEvent, BuildEventKind, PhaseSpan};
use rusqlite::Connection;

use super::cleanup::Retention;
use super::recorder::Faults;
use super::tests::{event_record, events, meta_for, start};
use super::*;

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis(),
    )
    .expect("ms")
}

fn phase(name: &'static str) -> PhaseSpan {
    PhaseSpan {
        name: name.into(),
        start_unix_us: 3_000,
        end_unix_us: 4_000,
    }
}

/// [`events`] (`//a:bad` failed, `//a:ok` started), plus `//a:ok` ending
/// after it ran and `//a:hit` a cache hit, each with a phase span.
fn build_events() -> Vec<BuildEvent> {
    let end = |addr: &str, spans| BuildEvent {
        at_unix_ms: 6,
        kind: BuildEventKind::ResultEnd {
            spans,
            left_behind: None,
            cancelled: false,
            addr: addr.into(),
            error: None,
            upstream_of: None,
            exit_status: None,
            log_tail: None,
            timed_out: None,
        },
    };
    let mut evs = events();
    evs.push(BuildEvent {
        at_unix_ms: 2,
        kind: BuildEventKind::ResultStart {
            addr: "//a:hit".into(),
        },
    });
    evs.push(end("//a:ok", vec![phase("heph.process")]));
    evs.push(end("//a:hit", vec![phase("heph.cache.read")]));
    evs
}

/// A sealed recording of [`build_events`] under `root`, ended `ended_ms`.
pub(super) fn sealed(root: &Path, id: SessionId) -> PathBuf {
    sealed_with(root, id, build_events())
}

/// `n` targets that each ran, with a phase span: a copy of many rows.
fn many_targets(n: usize) -> Vec<BuildEvent> {
    let mut evs = Vec::with_capacity(2 * n);
    for i in 0..n {
        let addr = format!("//many:t{i}");
        evs.push(BuildEvent {
            at_unix_ms: 2,
            kind: BuildEventKind::ResultStart { addr: addr.clone() },
        });
        evs.push(BuildEvent {
            at_unix_ms: 6,
            kind: BuildEventKind::ResultEnd {
                spans: vec![phase("heph.process")],
                left_behind: None,
                cancelled: false,
                addr,
                error: None,
                upstream_of: None,
                exit_status: None,
                log_tail: None,
                timed_out: None,
            },
        });
    }
    evs
}

/// A sealed recording of `evs` under `root`.
fn sealed_with(root: &Path, id: SessionId, evs: Vec<BuildEvent>) -> PathBuf {
    let dir = root.join(id.encode());
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join(files::LOCK), b"").expect("lock file");
    let (conn, _rw) =
        store::create(&dir, &meta_for(id), &store::Provenance::default(), false).expect("create");
    let mut writer = store::Writer::new(conn, id);
    for ev in evs {
        let r = event_record(ev);
        let body = serde_json::to_string(&r).expect("encode");
        writer.insert(&r, &body).expect("insert");
    }
    writer.commit().expect("commit");
    let conn = writer
        .finish(id.started_unix_ms() + 1_000, Some(0), false)
        .expect("finish");
    assert_eq!(
        store::seal(conn, store::SEAL_TIMEOUT).expect("seal"),
        store::Sealed::Done
    );
    dir
}

fn recent_id(random: u8, ago_ms: u64) -> SessionId {
    SessionId::new(now_ms() - ago_ms, random)
}

fn read(root: &Path) -> Connection {
    Connection::open_with_flags(
        root.join(catalog::FILE),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open the catalog")
}

/// Every catalog row but the `sid`s, in a stable order: what a rebuild must
/// reproduce.
fn snapshot(conn: &Connection) -> Vec<String> {
    let mut out = Vec::new();
    for sql in [
        "SELECT session_id, started_ms, ended_ms, exit_code, recorded, complete, derivation, \
           label, argv FROM sessions ORDER BY session_id",
        "SELECT session_id, addr, outcome, duration_us FROM targets ORDER BY session_id, addr",
        "SELECT session_id, span_id, parent_span_id, name, addr, outcome FROM spans \
           ORDER BY session_id, span_id",
        "SELECT session_id, span_id, name FROM span_events ORDER BY session_id, span_id, name",
        "SELECT session_id, name, value FROM metrics ORDER BY session_id, name, time_us, value",
    ] {
        let mut stmt = conn.prepare(sql).expect("prepare");
        let n = stmt.column_count();
        let rows = stmt
            .query_map([], |r| {
                (0..n)
                    .map(|i| {
                        r.get::<_, rusqlite::types::Value>(i)
                            .map(|v| format!("{v:?}"))
                    })
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map(|v| v.join("|"))
            })
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("rows");
        out.push(format!("{sql}\n{}", rows.join("\n")));
    }
    out
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).expect(sql)
}

/// The owner's two writes: a start row with `complete=0` that never
/// overwrites the seal's, then every row replaced at seal; the catalog keeps
/// every target (hits included) and the spans of what ran, not a hit's.
#[test]
fn catalog_owner_writes_start_then_seal() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let id = recent_id(1, 0);
    let dir = sealed(tmp.path(), id);
    catalog::record_start(tmp.path(), &dir);
    let conn = read(tmp.path());
    assert_eq!(count(&conn, "SELECT complete FROM sessions"), 0);
    catalog::record_seal(tmp.path(), &dir, true);
    catalog::record_start(tmp.path(), &dir);
    assert_eq!(
        count(&conn, "SELECT count(*) FROM sessions WHERE complete = 1"),
        1
    );
    let mut stmt = conn
        .prepare("SELECT addr, outcome FROM targets ORDER BY addr")
        .expect("prepare");
    let targets: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .collect::<rusqlite::Result<_>>()
        .expect("rows");
    assert_eq!(
        targets,
        [
            ("//a:bad".into(), "failed".into()),
            ("//a:hit".into(), "hit".into()),
            ("//a:ok".into(), "ran".into())
        ]
    );
    assert_eq!(
        count(&conn, "SELECT count(*) FROM spans WHERE addr = '//a:hit'"),
        0
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM spans WHERE name = 'heph.process'"
        ),
        1,
        "what ran keeps its phase spans"
    );
    assert!(count(&conn, "SELECT count(*) FROM spans WHERE addr IS NULL") > 0);
    let mode = std::fs::metadata(tmp.path().join(catalog::FILE))
        .expect("stat")
        .permissions();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
        0o600
    );
}

/// A seal a reader cut short leaves the recording in WAL mode, `End` and
/// `sealed=1` in the WAL. The owner's seal-time write reads through the WAL:
/// its `complete=1` row has the end and every target, not the last
/// checkpoint's empty session.
#[test]
fn a_cut_short_seal_still_writes_the_whole_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let id = recent_id(1, 0);
    let dir = tmp.path().join(id.encode());
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join(files::LOCK), b"").expect("lock file");
    let (conn, _rw) =
        store::create(&dir, &meta_for(id), &store::Provenance::default(), false).expect("create");
    // A viewer's read from before the build: the checkpoint cannot copy
    // anything written after it into the file.
    let reader = Connection::open_with_flags(
        dir.join(store::FILE),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("a reader");
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM records;")
        .expect("a read held open");
    let mut writer = store::Writer::new(conn, id);
    for ev in build_events() {
        let r = event_record(ev);
        let body = serde_json::to_string(&r).expect("encode");
        writer.insert(&r, &body).expect("insert");
    }
    writer.commit().expect("commit");
    let conn = writer
        .finish(id.started_unix_ms() + 1_000, Some(0), false)
        .expect("finish");
    assert_eq!(
        store::seal(conn, Duration::from_millis(50)).expect("seal"),
        store::Sealed::CutShort
    );
    catalog::record_seal(tmp.path(), &dir, true);
    drop(reader);

    let conn = read(tmp.path());
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM sessions WHERE complete = 1 AND ended_ms IS NOT NULL"
        ),
        1,
        "the seal-time row is complete and ended"
    );
    assert_eq!(count(&conn, "SELECT count(*) FROM targets"), 3);
}

/// `dropped_write_heals_without_watermark` (I8): A's write is dropped, a
/// later B's succeeds; the next command heals A. A heal racing an owner's
/// write doubles no row.
#[test]
fn dropped_write_heals_without_watermark() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let a = sealed(tmp.path(), recent_id(1, 2_000));
    let b = sealed(tmp.path(), recent_id(2, 1_000));
    catalog::record_seal(tmp.path(), &b, true);
    let opened = catalog::open(tmp.path(), now_ms()).expect("open");
    assert!(!opened.in_memory);
    assert_eq!(opened.heal.healed, 1, "A, written before B, is healed");
    assert_eq!(
        count(
            &opened.conn,
            "SELECT count(*) FROM sessions WHERE complete = 1"
        ),
        2
    );
    let once = snapshot(&opened.conn);
    drop(opened);

    // Both writers replace A's rows at once, many times over.
    let barrier = Arc::new(Barrier::new(2));
    let racers: Vec<_> = (0..2)
        .map(|i| {
            let (root, dir, barrier) = (tmp.path().to_path_buf(), a.clone(), Arc::clone(&barrier));
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..10 {
                    if i == 0 {
                        catalog::record_seal(&root, &dir, true);
                    } else {
                        catalog::replace(&root, &dir).expect("replace");
                    }
                }
            })
        })
        .collect();
    for r in racers {
        r.join().expect("join");
    }
    assert_eq!(snapshot(&read(tmp.path())), once);
}

/// `heal_failure_is_bounded` (I8): a recording whose heal fails is tried
/// again once a day, not on every command.
#[test]
fn heal_failure_is_bounded() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = sealed(tmp.path(), recent_id(1, 0));
    testing::alter(&dir, "DROP VIEW targets; DROP TABLE spans").expect("break it");
    let now = now_ms();
    let first = catalog::open(tmp.path(), now).expect("open").heal;
    assert_eq!((first.failed, first.deferred), (1, 0));
    let again = catalog::open(tmp.path(), now + 1_000).expect("open").heal;
    assert_eq!(
        (again.failed, again.deferred),
        (0, 1),
        "not retried within the day"
    );
    let tomorrow = catalog::open(tmp.path(), now + DAY_MS + 1_000)
        .expect("open")
        .heal;
    assert_eq!((tomorrow.failed, tomorrow.deferred), (1, 0));
}

/// A recording re-derived after its row was written, whose `replace` was
/// dropped, is healed by the next command: a complete row older than its
/// recording's derivation is copied again. A row as old as its recording is
/// left alone.
#[test]
fn heal_recopies_a_row_older_than_its_recording() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let dir = sealed(root, recent_id(1, 1_000));
    let fresh = catalog::open(root, now_ms()).expect("open");
    assert_eq!(fresh.heal.healed, 1);
    let once = snapshot(&fresh.conn);
    drop(fresh);

    // The row as an older heph wrote it, with a target the re-derivation
    // no longer has; the recording at this heph's derivation.
    let older = |derivation: u32| {
        let rw = Connection::open(root.join(catalog::FILE)).expect("open rw");
        rw.execute_batch(&format!(
            "UPDATE sessions SET derivation = {derivation};
             DELETE FROM target_rows;"
        ))
        .expect("age the row");
    };
    older(store::DERIVATION - 1);
    let healed = catalog::open(root, now_ms()).expect("open");
    assert_eq!(healed.heal.healed, 1, "copied again");
    assert_eq!(snapshot(&healed.conn), once);
    drop(healed);
    assert_eq!(
        catalog::open(root, now_ms()).expect("open").heal.healed,
        0,
        "current now: not copied again"
    );

    // Recording and row both at the older derivation (not re-derived yet):
    // nothing to copy.
    testing::alter(
        &dir,
        &format!("UPDATE session SET derivation = {}", store::DERIVATION - 1),
    )
    .expect("age the recording");
    older(store::DERIVATION - 1);
    let left = catalog::open(root, now_ms()).expect("open");
    assert_eq!(left.heal.healed, 0);
    assert_eq!(
        count(&left.conn, "SELECT count(*) FROM target_rows"),
        0,
        "left as it was"
    );
}

fn remove_catalog(root: &Path) {
    for name in [catalog::FILE, "catalog.db-wal", "catalog.db-shm"] {
        match std::fs::remove_file(root.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("remove {name}: {e}"),
        }
    }
}

fn corrupt_first_page(root: &Path) {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(root.join(catalog::FILE))
        .expect("open");
    f.write_all(&[0xA5; 100]).expect("overwrite the header");
}

fn corrupt_files(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .expect("readdir")
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with(catalog::CORRUPT_PREFIX))
        .collect();
    names.sort();
    names
}

/// `a_lost_catalog_is_rebuilt_on_open` (I4, I8): a deleted catalog, or one
/// whose first page is overwritten, is rebuilt by the next command equal to
/// the original for every kept recording; a corrupt one is moved aside
/// first; the command says how many days it covers.
#[test]
fn a_lost_catalog_is_rebuilt_on_open() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let oldest = recent_id(1, 3 * DAY_MS);
    for id in [oldest, recent_id(2, 1_000)] {
        let dir = sealed(root, id);
        catalog::record_seal(root, &dir, true);
    }
    let original = snapshot(&read(root));

    remove_catalog(root);
    let opened = catalog::open(root, now_ms()).expect("open");
    assert!(!opened.in_memory);
    assert_eq!(snapshot(&opened.conn), original);
    assert_eq!(opened.notes.len(), 1, "{:?}", opened.notes);
    assert!(opened.notes[0].contains("missing"), "{:?}", opened.notes);
    // Rounded down: 3 days and a second is "3 days", never a day it does
    // not hold.
    assert!(
        opened.notes[0].contains("covers the last 3 days"),
        "{:?}",
        opened.notes
    );
    assert_eq!(
        catalog::reaches_ms(&opened.conn).expect("reach"),
        Some(oldest.started_unix_ms()),
    );
    drop(opened);

    corrupt_first_page(root);
    let opened = catalog::open(root, now_ms()).expect("open");
    assert!(!opened.in_memory);
    assert_eq!(snapshot(&opened.conn), original);
    assert!(opened.notes[0].contains("corrupt"), "{:?}", opened.notes);
    assert_eq!(corrupt_files(root).len(), 1, "{:?}", corrupt_files(root));
    drop(opened);

    let opened = catalog::open(root, now_ms()).expect("open");
    assert!(
        opened.notes.is_empty(),
        "a sound catalog is quiet: {:?}",
        opened.notes
    );
}

/// A heph catalog in an older format (`user_version` below [`catalog::FORMAT`],
/// tables in it) is never stamped current over its old tables: an owner
/// leaves it as it is, and the next command moves it aside and rebuilds it.
#[test]
fn an_older_catalog_is_moved_aside_and_rebuilt() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let dir = sealed(root, recent_id(1, 1_000));
    catalog::record_seal(root, &dir, true);
    let original = snapshot(&read(root));
    let path = root.join(catalog::FILE);
    {
        let conn = Connection::open(&path).expect("open");
        conn.execute_batch("PRAGMA user_version = 0; PRAGMA wal_checkpoint(TRUNCATE);")
            .expect("stamp older");
    }
    let late = sealed(root, recent_id(2, 0));
    catalog::record_seal(root, &late, true);
    {
        let conn = read(root);
        let version: u32 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .expect("user_version");
        assert_eq!(version, 0, "an owner never stamps an older catalog");
        assert_eq!(
            count(&conn, "SELECT count(*) FROM sessions"),
            1,
            "an owner never writes an older catalog"
        );
    }
    std::fs::remove_dir_all(&late).expect("rm");

    let opened = catalog::open(root, now_ms()).expect("open");
    assert!(!opened.in_memory, "{:?}", opened.notes);
    assert_eq!(snapshot(&opened.conn), original);
    assert_eq!(opened.notes.len(), 1, "{:?}", opened.notes);
    assert!(
        opened.notes[0].contains("older format"),
        "{:?}",
        opened.notes
    );
    assert_eq!(corrupt_files(root).len(), 1, "{:?}", corrupt_files(root));
    let version: u32 = opened
        .conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .expect("user_version");
    assert_eq!(version, catalog::FORMAT);
}

/// A file with another application's id is named foreign, whatever its
/// `user_version`: never "written by a newer heph".
#[test]
fn a_foreign_catalog_is_foreign_before_newer() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let path = root.join(catalog::FILE);
    {
        let conn = Connection::open(&path).expect("open");
        conn.execute_batch(&format!(
            "CREATE TABLE t (x); PRAGMA application_id = 305419896; PRAGMA user_version = {}",
            catalog::FORMAT + 1
        ))
        .expect("a foreign file");
    }
    let before = std::fs::read(&path).expect("read");
    let opened = catalog::open(root, now_ms()).expect("open");
    assert!(opened.in_memory);
    assert!(
        opened.notes[0].contains("not a heph session catalog"),
        "{:?}",
        opened.notes
    );
    drop(opened);
    assert!(
        std::fs::read(&path).expect("read") == before,
        "never written"
    );
    assert!(corrupt_files(root).is_empty());
}

/// `catalog_repair_is_guarded` (I8): busy and permission failures answer
/// from the recordings and never move the catalog aside; a newer
/// `user_version` is never written and names its writer. (FULL takes the
/// same branch as BUSY: `catalog::is_corrupt` is false for every code but
/// CORRUPT and NOTADB.)
#[test]
fn catalog_repair_is_guarded() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let first = sealed(root, recent_id(1, 2_000));
    catalog::record_seal(root, &first, true);
    sealed(root, recent_id(2, 1_000)); // a row to heal: a write to make

    // BUSY: another connection holds the write lock past busy_timeout.
    let holder = Connection::open(root.join(catalog::FILE)).expect("open");
    holder.execute_batch("BEGIN IMMEDIATE").expect("hold");
    let opened = catalog::open(root, now_ms()).expect("open");
    assert!(opened.in_memory);
    assert_eq!(opened.heal.healed, 2, "answered from both recordings");
    assert!(opened.notes[0].contains("answering from the recordings"));
    // How far back that answer reaches: the oldest kept, 2 s ago.
    assert!(
        opened.notes[0].contains("it covers the last hour"),
        "{:?}",
        opened.notes
    );
    holder.execute_batch("ROLLBACK").expect("release");
    drop(holder);
    assert!(corrupt_files(root).is_empty());

    // A permission error.
    let path = root.join(catalog::FILE);
    let before = std::fs::read(&path).expect("read");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    let opened = catalog::open(root, now_ms()).expect("open");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    assert!(opened.in_memory);
    assert!(corrupt_files(root).is_empty());
    assert_eq!(std::fs::read(&path).expect("read"), before);

    // Newer: never written, never moved, its writer named.
    {
        let conn = Connection::open(&path).expect("open");
        conn.execute_batch(&format!(
            "UPDATE info SET heph_version = '99.0.0'; PRAGMA user_version = {}",
            catalog::FORMAT + 1
        ))
        .expect("stamp newer");
    }
    let before = std::fs::read(&path).expect("read");
    let opened = catalog::open(root, now_ms()).expect("open");
    assert!(opened.in_memory);
    assert!(
        opened.notes[0].contains("heph 99.0.0"),
        "{:?}",
        opened.notes
    );
    drop(opened);
    assert_eq!(std::fs::read(&path).expect("read"), before);
    assert!(corrupt_files(root).is_empty());
    let late = sealed(root, recent_id(3, 0));
    catalog::record_seal(root, &late, true);
    assert_eq!(
        std::fs::read(&path).expect("read"),
        before,
        "an owner never writes it"
    );
}

/// `catalog_lock_lifetime` (I8): a repair waits while another connection
/// holds `catalog.lock`; it checks again for corruption once it has the
/// lock; it moves `-wal` and `-shm` with the file, and removes an orphan
/// `-wal`.
#[test]
fn catalog_lock_lifetime() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let dir = sealed(&root, recent_id(1, 0));
    catalog::record_seal(&root, &dir, true);
    let path = root.join(catalog::FILE);
    let good = std::fs::read(&path).expect("read");

    // Another connection's shared lock: the repair waits for it, and finds
    // the catalog sound again once it may go on.
    let other = lockfile::open_shared_file(&root.join(catalog::LOCK)).expect("open lock");
    assert!(lockfile::lock_within(&other, false, Duration::ZERO).expect("lock"));
    corrupt_first_page(&root);
    let repair = std::thread::spawn({
        let root = root.clone();
        move || catalog::open(&root, now_ms()).expect("open")
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(corrupt_files(&root).is_empty(), "nothing moved while held");
    std::fs::write(&path, &good).expect("restore");
    drop(other);
    let opened = repair.join().expect("join");
    assert!(!opened.in_memory);
    assert!(opened.notes.is_empty(), "{:?}", opened.notes);
    assert!(corrupt_files(&root).is_empty(), "re-checked under the lock");
    drop(opened);

    // Corrupt with a -wal and -shm beside it: all three move.
    corrupt_first_page(&root);
    std::fs::write(root.join("catalog.db-wal"), b"stale wal").expect("wal");
    std::fs::write(root.join("catalog.db-shm"), b"stale shm").expect("shm");
    drop(catalog::open(&root, now_ms()).expect("open"));
    // SQLite itself may drop a -wal it cannot pair with the file before the
    // move gets to it; what matters is that none is left to pair with the
    // rebuilt catalog.
    assert_eq!(
        corrupt_files(&root)
            .iter()
            .filter(|n| !n.contains("-w") && !n.contains("-s"))
            .count(),
        1
    );
    for (name, stale) in [
        ("catalog.db-wal", &b"stale wal"[..]),
        ("catalog.db-shm", &b"stale shm"[..]),
    ] {
        assert!(
            std::fs::read(root.join(name)).map_or(true, |b| b != stale),
            "{name} left beside it"
        );
    }

    // An orphan -wal beside no catalog is removed before the rebuild.
    remove_catalog(&root);
    std::fs::write(root.join("catalog.db-wal"), b"orphan").expect("wal");
    let opened = catalog::open(&root, now_ms()).expect("open");
    assert_eq!(count(&opened.conn, "SELECT count(*) FROM sessions"), 1);
    drop(opened);
    assert!(
        std::fs::read(root.join("catalog.db-wal")).map_or(true, |b| b != b"orphan"),
        "the orphan is gone"
    );
}

/// `catalog_write_failure_never_fails_the_run` (I1, I8): a catalog locked
/// past `busy_timeout` drops the owner's write within its bound; the run's
/// recording still ends with its exit code.
#[test]
fn catalog_write_failure_never_fails_the_run() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let earlier = sealed(root, recent_id(1, 1_000));
    catalog::record_seal(root, &earlier, true);
    let holder = Connection::open(root.join(catalog::FILE)).expect("open");
    holder.execute_batch("BEGIN IMMEDIATE").expect("hold");

    let session = Session::open(&SessionRoot::new(root), start()).expect("open");
    let dir = session.dir().to_path_buf();
    let started = Instant::now();
    session.finish(Some(3), false);
    assert!(
        started.elapsed() < catalog::WRITE_BOUND + Duration::from_secs(1),
        "bounded: {:?}",
        started.elapsed()
    );
    holder.execute_batch("ROLLBACK").expect("release");
    let conn = store::open_sealed(&dir.join(store::FILE)).expect("sealed");
    let exit: Option<i64> = conn
        .query_row("SELECT exit_code FROM session", [], |r| r.get(0))
        .expect("exit");
    assert_eq!(exit, Some(3));
    let id = session.id().encode();
    assert_eq!(
        count(
            &read(root),
            &format!("SELECT count(*) FROM sessions WHERE session_id = '{id}' AND complete = 1")
        ),
        0,
        "the write was dropped"
    );
}

/// The owner's limits with `bound` in place of [`catalog::WRITE_BOUND`].
fn within(bound: Duration) -> catalog::OwnerLimits {
    catalog::OwnerLimits {
        bound,
        ..catalog::OwnerLimits::DEFAULT
    }
}

/// Over its cap, an owner's seal write keeps `sessions` and `targets` and
/// leaves the explorer rows out; under it, everything.
#[test]
fn an_owner_over_the_cap_writes_no_explorer_rows() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let first = sealed(root, recent_id(1, 2_000));
    catalog::record_seal(root, &first, true);
    let spans_of = |id: &str| {
        count(
            &read(root),
            &format!("SELECT count(*) FROM spans WHERE session_id = '{id}'"),
        )
    };
    let first_id = first.file_name().and_then(|n| n.to_str()).expect("id");
    assert!(spans_of(first_id) > 0, "under the cap: everything");

    let over = sealed(root, recent_id(2, 1_000));
    let limits = catalog::OwnerLimits {
        cap: 0,
        ..catalog::OwnerLimits::DEFAULT
    };
    catalog::record_seal_within(root, &over, true, limits);
    let over_id = over.file_name().and_then(|n| n.to_str()).expect("id");
    let conn = read(root);
    assert_eq!(
        count(
            &conn,
            &format!(
                "SELECT count(*) FROM sessions WHERE session_id = '{over_id}' AND complete = 1 AND recorded = 1"
            )
        ),
        1
    );
    assert_eq!(
        count(
            &conn,
            &format!("SELECT count(*) FROM targets WHERE session_id = '{over_id}'")
        ),
        3
    );
    assert_eq!(spans_of(over_id), 0, "over the cap: no spans");
    assert_eq!(
        count(&conn, "SELECT count(*) FROM sessions WHERE capped = 1"),
        1,
        "only the session over the cap is marked"
    );
    assert_eq!(
        count(
            &conn,
            &format!("SELECT count(*) FROM metrics WHERE session_id = '{over_id}'")
        ),
        0
    );
}

/// An owner write past its bound is interrupted and rolled back whole: the
/// start row stays `complete=0` with none of the session's other rows, and
/// the next command heals it.
#[test]
fn an_owner_write_past_its_bound_leaves_no_half_rows() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let id = recent_id(1, 1_000);
    // Big enough that copying its targets runs past the progress handler's
    // first check; the `sessions` upsert before it does not.
    let dir = sealed_with(root, id, many_targets(500));
    catalog::record_start(root, &dir);
    catalog::record_seal_within(root, &dir, true, within(Duration::ZERO));
    {
        let conn = read(root);
        assert_eq!(
            count(&conn, "SELECT count(*) FROM sessions WHERE complete = 0"),
            1,
            "the start row, as it was"
        );
        for table in ["targets", "spans", "span_events", "metrics"] {
            assert_eq!(
                count(&conn, &format!("SELECT count(*) FROM {table}")),
                0,
                "no half rows in {table}"
            );
        }
    }
    let opened = catalog::open(root, now_ms()).expect("open");
    assert!(!opened.in_memory);
    assert_eq!(opened.heal.healed, 1);
    assert_eq!(
        count(
            &opened.conn,
            "SELECT count(*) FROM sessions WHERE complete = 1"
        ),
        1
    );
    assert_eq!(count(&opened.conn, "SELECT count(*) FROM targets"), 500);
}

/// An owner waits on a locked catalog until its write's deadline, not for
/// a fixed busy timeout; and it leaves no checkpoint for its exit path: its
/// rows stay in the WAL for a command to fold in.
///
/// The bound is set a second past [`catalog::BUSY_TIMEOUT`], so the
/// deadline, not the fixed timeout, is what ends the wait (`took >= bound`,
/// which load cannot break), and it gives up then (a second of slack).
#[test]
fn an_owner_waits_on_a_locked_catalog_only_until_its_bound() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let earlier = sealed(root, recent_id(1, 2_000));
    catalog::record_seal(root, &earlier, true);
    let holder = Connection::open(root.join(catalog::FILE)).expect("open");
    holder.execute_batch("BEGIN IMMEDIATE").expect("hold");
    let dir = sealed(root, recent_id(2, 1_000));
    let bound = catalog::BUSY_TIMEOUT + Duration::from_secs(1);
    let started = Instant::now();
    catalog::record_seal_within(root, &dir, true, within(bound));
    let took = started.elapsed();
    assert!(
        took >= bound,
        "gave up before its bound, on a fixed timeout: {took:?}"
    );
    assert!(
        took < bound + Duration::from_secs(1),
        "waited past its bound: {took:?}"
    );
    holder.execute_batch("ROLLBACK").expect("release");
    drop(holder);

    // Alone on the catalog, its write is left in the WAL when it closes.
    catalog::record_seal(root, &dir, true);
    let wal = std::fs::metadata(root.join("catalog.db-wal")).map_or(0, |m| m.len());
    assert!(wal > 0, "the owner's close did not checkpoint");
    assert_eq!(
        count(
            &read(root),
            "SELECT count(*) FROM sessions WHERE complete = 1"
        ),
        2
    );
}

/// The `sessions` rows `catalog.db` holds on its own, without its WAL:
/// what a checkpoint folded in. Read from a copy, so the live catalog's WAL
/// is not checkpointed by the read.
fn folded_sessions(root: &Path) -> Vec<(String, i64)> {
    let copy = tempfile::tempdir().expect("tempdir");
    std::fs::copy(root.join(catalog::FILE), copy.path().join(catalog::FILE)).expect("copy");
    let conn = Connection::open(copy.path().join(catalog::FILE)).expect("open the copy");
    let Ok(mut stmt) =
        conn.prepare("SELECT session_id, complete FROM sessions ORDER BY session_id")
    else {
        return Vec::new();
    };
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .collect::<rusqlite::Result<_>>()
        .expect("rows")
}

/// Owners never checkpoint on the exit path, so the WAL would grow with
/// every session; the next session's start thread folds it in. The seal's
/// rows are in `catalog.db` itself after the next start, not only in the
/// WAL.
#[test]
fn the_wal_folds_in_at_the_next_sessions_start() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let first = sealed(root, recent_id(1, 2_000));
    catalog::record_start(root, &first);
    catalog::record_seal(root, &first, true);
    let first_id = first.file_name().and_then(|n| n.to_str()).expect("id");
    assert!(
        !folded_sessions(root)
            .iter()
            .any(|(id, complete)| id == first_id && *complete == 1),
        "the seal's rows are in the WAL only"
    );

    let next = sealed(root, recent_id(2, 1_000));
    catalog::record_start(root, &next);
    let next_id = next.file_name().and_then(|n| n.to_str()).expect("id");
    assert_eq!(
        folded_sessions(root),
        [(first_id.to_owned(), 1), (next_id.to_owned(), 0)],
        "the seal and the new start row, folded into catalog.db"
    );
}

/// Bytes of the catalog's `-wal`.
fn wal_len(root: &Path) -> u64 {
    std::fs::metadata(root.join("catalog.db-wal")).map_or(0, |m| m.len())
}

/// A WAL one large write grew is cut back once it is folded in, so one big
/// session does not leave a WAL its size behind for good. A passive fold
/// did not: each run's new connection counts every frame as unfolded, so
/// the WAL was never restarted, and only grew.
#[test]
fn a_folded_wal_is_cut_back_to_its_limit() {
    const LIMIT: u64 = catalog::WAL_KEEP.unsigned_abs();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let first = sealed(root, recent_id(1, 3_000));
    catalog::record_seal(root, &first, true);
    {
        // Left in the WAL, as an owner's write is.
        let conn = Connection::open(root.join(catalog::FILE)).expect("open");
        conn.pragma_update(None, "wal_autocheckpoint", 0)
            .expect("no autocheckpoint");
        conn.set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
            true,
        )
        .expect("no checkpoint on close");
        conn.execute_batch(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 16000)
             INSERT INTO metrics SELECT 'big', 'm', 'gauge', '1', i, i, randomblob(1000) FROM n;",
        )
        .expect("a large write");
    }
    assert!(wal_len(root) > 3 * LIMIT, "{} bytes", wal_len(root));
    // The next start folds it in; the one after restarts the WAL.
    catalog::record_start(root, &sealed(root, recent_id(2, 2_000)));
    catalog::record_start(root, &sealed(root, recent_id(3, 1_000)));
    assert!(
        wal_len(root) <= LIMIT,
        "the WAL kept {} bytes after a fold",
        wal_len(root)
    );
}

/// `a_build_never_repairs` (I1): a run with a missing catalog and a
/// recording of an older derivation writes only its own rows and
/// re-derives nothing.
#[test]
fn a_build_never_repairs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let old = sealed(root, recent_id(1, 1_000));
    testing::alter(
        &old,
        &format!(
            "UPDATE session SET derivation = {}",
            store::STALE_DERIVATION
        ),
    )
    .expect("age it");
    let before = std::fs::read(old.join(store::FILE)).expect("read");
    let session = Session::open_with(
        &SessionRoot::new(root),
        start(),
        Retention::default(),
        Faults {
            cleanup_interval: Some(Duration::ZERO),
            ..Faults::default()
        },
    )
    .expect("open");
    session.finish(Some(0), false);
    let conn = read(root);
    // `complete = 1`: the seal's own row, which a late start row never
    // stands in for.
    let ids: Vec<String> = conn
        .prepare("SELECT session_id FROM sessions WHERE complete = 1")
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<rusqlite::Result<_>>()
        .expect("rows");
    assert_eq!(ids, [session.id().encode()]);
    assert_eq!(std::fs::read(old.join(store::FILE)).expect("read"), before);
}

/// Under `HEPH_NO_SESSION` alone, the public catalog is still kept: each
/// private session's start folds in the WAL the seals before it left, and
/// its (hourly) cleanup expires the public catalog's old rows. Neither
/// happened once, so a private-only user's WAL grew with every run.
#[test]
fn private_sessions_fold_and_clean_the_public_catalog() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let public = tmp.path().join(SESSIONS_DIR);
    catalog::ensure_root(&public).expect("public root");
    let old = sealed(&public, recent_id(1, 100 * DAY_MS));
    catalog::record_seal(&public, &old, true);
    let old_id = old.file_name().and_then(|n| n.to_str()).expect("id");
    Connection::open(public.join(catalog::FILE))
        .expect("open")
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("fold the old row in");
    let private = SessionRoot::private(tmp.path().join(PRIVATE_SESSIONS_DIR));
    let folded = |id: &str| {
        folded_sessions(&public)
            .iter()
            .any(|(sid, complete)| sid == id && *complete == 1)
    };
    assert!(folded(old_id), "the expired row is in catalog.db");
    let mut seals: Vec<String> = Vec::new();
    // Only the first start's cleanup is due: the interval is the default.
    for _ in 0..3 {
        let session = Session::open(&private, start()).expect("open");
        if let Some(prev) = seals.last() {
            assert!(
                super::tests::eventually(|| folded(prev)),
                "a private start did not fold in the previous private seal"
            );
        }
        session.finish(Some(0), false);
        seals.push(session.id().encode());
    }
    assert!(
        super::tests::eventually(|| !folded_sessions(&public)
            .iter()
            .any(|(sid, _)| sid == old_id)),
        "a private session's cleanup did not expire the public catalog's old rows"
    );
}

/// `opt_outs_in_the_catalog` (D19): a private session leaves `sessions` and
/// `targets` rows with `recorded=0`, no spans, and no recording. (`--raw`
/// makes no session, so it has nothing to write.)
#[test]
fn opt_outs_in_the_catalog() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let private = tmp.path().join(PRIVATE_SESSIONS_DIR);
    let session = Session::open(&SessionRoot::private(&private), start()).expect("open");
    let dir = session.dir().to_path_buf();
    let tap = session.tap();
    for ev in build_events() {
        tap.event(ev);
    }
    session.finish(Some(0), false);
    assert!(!dir.exists(), "no recording is kept");
    let conn = read(&tmp.path().join(SESSIONS_DIR));
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM sessions WHERE recorded = 0 AND complete = 1"
        ),
        1
    );
    assert_eq!(count(&conn, "SELECT count(*) FROM targets"), 3);
    assert_eq!(count(&conn, "SELECT count(*) FROM spans"), 0);
    assert_eq!(count(&conn, "SELECT count(*) FROM metrics"), 0);
}

/// `retention_windows`, the catalog's half: a recording removed at 7 days
/// keeps its catalog row; rows go at 90 days; a moved-aside corrupt catalog
/// goes at 7 days.
#[test]
fn catalog_retention_windows() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let mut ids = Vec::new();
    for (i, ago) in [(1, 100 * DAY_MS), (2, 30 * DAY_MS), (3, 1_000)] {
        let id = recent_id(i, ago);
        let dir = sealed(root, id);
        catalog::record_seal(root, &dir, true);
        ids.push(id.encode());
    }
    let now = now_ms();
    std::fs::write(
        root.join(format!("{}{}", catalog::CORRUPT_PREFIX, now - 8 * DAY_MS)),
        b"x",
    )
    .expect("old");
    std::fs::write(
        root.join(format!(
            "{}{}-wal",
            catalog::CORRUPT_PREFIX,
            now - 8 * DAY_MS
        )),
        b"x",
    )
    .expect("old wal");
    let fresh = format!("{}{}", catalog::CORRUPT_PREFIX, now - DAY_MS);
    std::fs::write(root.join(&fresh), b"x").expect("fresh");

    // The recordings themselves go by mtime (cleanup's own tests); their
    // rows outlive them here, whatever cleanup did to them.
    for dir in std::fs::read_dir(root)
        .expect("readdir")
        .filter_map(Result::ok)
    {
        if dir.path().join(store::FILE).exists()
            && !dir.file_name().to_string_lossy().ends_with(&ids[2])
        {
            std::fs::remove_dir_all(dir.path()).expect("remove a recording");
        }
    }
    // An address only the expired session resolved.
    {
        let rw = Connection::open(root.join(catalog::FILE)).expect("open rw");
        rw.execute_batch(&format!(
            "INSERT INTO addrs (addr) VALUES ('//only:old');
             INSERT INTO target_rows (sid, addr_id, outcome, duration_us)
               SELECT sid, last_insert_rowid(), 1, 0 FROM sessions WHERE session_id = '{}';",
            ids[0]
        ))
        .expect("a target only the old session has");
    }
    let report = catalog::clean(root, SystemTime::now()).expect("clean");
    assert_eq!((report.expired, report.corrupt), (1, 2));
    assert_eq!(corrupt_files(root), [fresh]);
    let conn = read(root);
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM addrs WHERE addr = '//only:old'"
        ),
        0,
        "an address no target row uses is pruned"
    );
    assert_eq!(
        count(&conn, "SELECT count(*) FROM addrs"),
        count(&conn, "SELECT count(DISTINCT addr_id) FROM target_rows"),
        "every address still used is kept"
    );
    let kept: Vec<String> = conn
        .prepare("SELECT session_id FROM sessions ORDER BY started_ms")
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<rusqlite::Result<_>>()
        .expect("rows");
    assert_eq!(kept, ids[1..]);
    assert_eq!(
        count(&conn, "SELECT count(*) FROM target_rows"),
        6,
        "the expired session's targets went too"
    );
}

/// `catalog_targets_fit_the_cap` (D18): with addresses interned, 900 cold
/// 50k-target sessions (90 days of 10 a day) fit under 1 GiB — measured on
/// 20 of them and extrapolated, since 45M rows is minutes of a test. Past
/// the cap, spans and metrics go before targets, oldest first, then whole
/// sessions.
#[test]
fn catalog_targets_fit_the_cap() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let dir = sealed(root, recent_id(1, 0));
    catalog::record_seal(root, &dir, true);
    let conn = Connection::open(root.join(catalog::FILE)).expect("open");
    let used = |conn: &Connection| -> u64 {
        let page: i64 = conn
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .expect("size");
        let pages: i64 = conn
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .expect("pages");
        let free: i64 = conn
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))
            .expect("free");
        u64::try_from((pages - free) * page).expect("bytes")
    };
    conn.execute_batch(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 50000)
         INSERT INTO addrs (addr) SELECT '//some/package/path' || (i / 100) || ':target_' || i FROM n;",
    )
    .expect("addrs");
    // Enough explorer rows on the real session that dropping them frees pages.
    conn.execute_batch(&format!(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10000)
         INSERT INTO metrics SELECT '{}', 'synthetic', 'gauge', '1', i, i, NULL FROM n;",
        dir.file_name().expect("id").to_string_lossy()
    ))
    .expect("metrics");
    let base = used(&conn);
    const SAMPLE: u64 = 20;
    for s in 0..SAMPLE {
        conn.execute(
            "INSERT INTO sessions (session_id, started_ms, recorded, complete, derivation)
             VALUES (?1, ?2, 1, 1, 1)",
            rusqlite::params![
                format!("synthetic-{s}"),
                i64::try_from(now_ms() - 10_000 + s).expect("ms")
            ],
        )
        .expect("session");
        conn.execute(
            "INSERT INTO target_rows (sid, addr_id, outcome, duration_us)
             SELECT (SELECT sid FROM sessions WHERE session_id = ?1), id, 1,
               (id * 7919) % 10000000 FROM addrs WHERE addr LIKE '//some/%'",
            [format!("synthetic-{s}")],
        )
        .expect("targets");
    }
    let per_session = (used(&conn) - base) / SAMPLE;
    let projected = base + per_session * 900;
    assert!(
        projected < catalog::CAP,
        "900 sessions project to {projected} bytes ({per_session} per session)"
    );
    assert!(
        per_session / 50_000 <= 20,
        "a target row is about 16 bytes, not {}",
        per_session / 50_000
    );
    drop(conn);

    // Past the cap: explorer rows first, oldest first, targets kept.
    let conn = read(root);
    let total = used(&conn);
    drop(conn);
    let report = catalog::clean_with(root, SystemTime::now(), total - 1).expect("clean");
    assert!(
        report.capped_rows >= 1 && report.capped_sessions == 0,
        "{report:?}"
    );
    let conn = read(root);
    assert_eq!(count(&conn, "SELECT count(*) FROM target_rows"), 1_000_003);
    assert_eq!(
        usize::try_from(count(
            &conn,
            "SELECT count(*) FROM sessions WHERE capped = 1"
        ))
        .expect("count"),
        report.capped_rows,
        "each session whose explorer rows went is marked capped"
    );
    drop(conn);
    let report = catalog::clean_with(root, SystemTime::now(), 0).expect("clean");
    assert!(report.capped_sessions >= 1, "{report:?}");
    assert_eq!(count(&read(root), "SELECT count(*) FROM sessions"), 0);
}

/// Cleanup holds no write transaction between its steps, and gives the
/// free pages back a step at a time: an owner that comes in between two
/// lands its write within its bound.
#[test]
fn cleanup_yields_to_a_waiting_owner() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let first = sealed(root, recent_id(1, 2_000));
    catalog::record_seal(root, &first, true);
    {
        let conn = Connection::open(root.join(catalog::FILE)).expect("open");
        for s in 0..4 {
            conn.execute(
                "INSERT INTO sessions (session_id, started_ms, recorded, complete, derivation)
                 VALUES (?1, ?2, 1, 1, 1)",
                rusqlite::params![
                    format!("synthetic-{s}"),
                    i64::try_from(now_ms() - 10_000 + s).expect("ms")
                ],
            )
            .expect("session");
            // ~3 MiB of explorer rows each.
            conn.execute(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 3000)
                 INSERT INTO spans (session_id, span_id, name, attributes)
                 SELECT ?1, i, 'heph.result', hex(randomblob(512)) FROM n",
                [format!("synthetic-{s}")],
            )
            .expect("spans");
        }
    }
    let owner = sealed(root, recent_id(2, 1_000));
    let owner_id = owner
        .file_name()
        .and_then(|n| n.to_str())
        .expect("id")
        .to_owned();
    let watch = read(root);
    let free = |c: &Connection| count(c, "PRAGMA freelist_count");
    let (mut calls, mut landed, mut shrinking, mut last_free) = (0, None, 0, i64::MAX);
    let report = catalog::clean_between(root, SystemTime::now(), 0, &mut || {
        calls += 1;
        if calls == 1 {
            let started = Instant::now();
            // Its bound is generous: were cleanup holding a write
            // transaction across this call, the owner would wait it out
            // and its write would be dropped, whatever the bound.
            catalog::record_seal_within(root, &owner, true, within(Duration::from_secs(5)));
            landed = Some((
                count(
                    &watch,
                    &format!(
                        "SELECT count(*) FROM sessions WHERE session_id = '{owner_id}' AND complete = 1"
                    ),
                ),
                started.elapsed(),
            ));
        }
        let now = free(&watch);
        if now > 0 && now < last_free {
            shrinking += 1;
        }
        last_free = now;
    })
    .expect("clean");
    let (rows, took) = landed.expect("cleanup paused at least once");
    assert_eq!(
        rows, 1,
        "the owner's write landed between two steps (took {took:?})"
    );
    assert!(report.capped_rows >= 4, "{report:?}");
    assert!(
        shrinking >= 2,
        "the free pages went back in steps, a pause between two: {shrinking}"
    );
    assert_eq!(free(&watch), 0, "every free page given back");
    assert_eq!(
        count(&watch, "PRAGMA auto_vacuum"),
        2,
        "the catalog is made incremental"
    );
}

/// `an_older_derivation_is_rederived_on_open`, the command's half (I4,
/// D14): the first read of explorer rows re-derives the recording, says so
/// once on stderr, and replaces its catalog rows; a second read does no
/// work. `newer_derivation_is_left_alone`: a newer one is read as it is.
#[test]
fn show_rederives_an_older_derivation_once() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let dir = sealed(root, recent_id(1, 0));
    testing::alter(
        &dir,
        &format!(
            "DELETE FROM spans; UPDATE session SET derivation = {}",
            store::STALE_DERIVATION
        ),
    )
    .expect("age it");
    catalog::record_seal(root, &dir, true);
    let catalog_derivation = || count(&read(root), "SELECT derivation FROM sessions");
    assert_eq!(catalog_derivation(), i64::from(store::STALE_DERIVATION));
    let notes = Arc::new(Mutex::new(Vec::<String>::new()));
    let note: source::Note = {
        let notes = Arc::clone(&notes);
        Arc::new(move |s: &str| notes.lock().expect("notes").push(s.to_owned()))
    };
    let opened = source::open_repaired(root, &dir, note.as_ref()).expect("open");
    assert!(!opened.is_copy());
    assert!(count(&opened.conn, "SELECT count(*) FROM spans") > 0);
    drop(opened);
    let id = dir.file_name().expect("id").to_string_lossy().into_owned();
    assert_eq!(
        *notes.lock().expect("notes"),
        [format!(
            "heph: re-deriving session {id}: its session was lost, and cleanup sealed it\n"
        )],
        "names the session and why, once"
    );
    assert_eq!(catalog_derivation(), i64::from(store::DERIVATION));
    assert!(count(&read(root), "SELECT count(*) FROM spans") > 0);
    drop(source::open_repaired(root, &dir, note.as_ref()).expect("open"));
    assert_eq!(
        notes.lock().expect("notes").len(),
        1,
        "a second read does no work"
    );

    let newer = store::DERIVATION + 1;
    testing::alter(&dir, &format!("UPDATE session SET derivation = {newer}")).expect("newer");
    let before = std::fs::read(dir.join(store::FILE)).expect("read");
    drop(source::open_repaired(root, &dir, note.as_ref()).expect("open"));
    assert_eq!(notes.lock().expect("notes").len(), 1);
    assert_eq!(std::fs::read(dir.join(store::FILE)).expect("read"), before);
}

/// `rederive_is_durable_and_safe`, the rest (I4): a read-only session
/// directory leaves the original byte-identical and the command answers
/// from a temp copy; two concurrent re-derivations produce one.
#[test]
fn rederive_is_durable_and_safe_under_failure_and_race() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let dir = sealed(root, recent_id(1, 0));
    let age = || {
        testing::alter(
            &dir,
            &format!(
                "DELETE FROM spans; UPDATE session SET derivation = {}",
                store::STALE_DERIVATION
            ),
        )
        .expect("age it");
    };
    age();
    let before = std::fs::read(dir.join(store::FILE)).expect("read");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).expect("chmod");
    let failed = store::upgrade_local(&dir, store::Need::default());
    let notes = Mutex::new(Vec::<String>::new());
    let opened = source::open_repaired(root, &dir, &|n: &str| {
        notes.lock().expect("notes").push(n.to_owned());
    });
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    assert!(failed.is_err(), "{failed:?}");
    let opened = opened.expect("answered from a temp copy");
    assert!(opened.is_copy());
    let id = dir.file_name().expect("id").to_string_lossy().into_owned();
    let notes = notes.into_inner().expect("notes");
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(
        notes[0].starts_with(&format!("heph: re-deriving session {id}: ")),
        "{notes:?}"
    );
    assert!(
        notes[1].starts_with(&format!(
            "heph: session {id} cannot be re-derived in place ("
        )) && notes[1].ends_with("); reading a re-derived temp copy for this command\n")
            && notes[1].contains("Permission denied"),
        "the fallback is said, with its reason: {notes:?}"
    );
    assert!(count(&opened.conn, "SELECT count(*) FROM spans") > 0);
    drop(opened);
    assert_eq!(std::fs::read(dir.join(store::FILE)).expect("read"), before);

    let barrier = Arc::new(Barrier::new(2));
    let racers: Vec<_> = (0..2)
        .map(|_| {
            let (dir, barrier) = (dir.clone(), Arc::clone(&barrier));
            std::thread::spawn(move || {
                barrier.wait();
                store::upgrade_local(&dir, store::Need::default()).expect("upgrade")
            })
        })
        .collect();
    let mut outcomes: Vec<_> = racers
        .into_iter()
        .map(|r| r.join().expect("join"))
        .collect();
    outcomes.sort_by_key(|o| format!("{o:?}"));
    assert_eq!(outcomes, [store::Upgraded::Current, store::Upgraded::Done]);
}
