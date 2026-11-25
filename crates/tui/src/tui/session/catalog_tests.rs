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
    let dir = root.join(id.encode());
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join(files::LOCK), b"").expect("lock file");
    let (conn, _rw) =
        store::create(&dir, &meta_for(id), &store::Provenance::default(), false).expect("create");
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
    for (i, ago) in [(1, 3 * DAY_MS), (2, 1_000)] {
        let dir = sealed(root, recent_id(i, ago));
        catalog::record_seal(root, &dir, true);
    }
    let original = snapshot(&read(root));

    remove_catalog(root);
    let opened = catalog::open(root, now_ms()).expect("open");
    assert!(!opened.in_memory);
    assert_eq!(snapshot(&opened.conn), original);
    assert_eq!(opened.notes.len(), 1, "{:?}", opened.notes);
    assert!(opened.notes[0].contains("missing"), "{:?}", opened.notes);
    assert!(
        opened.notes[0].contains("covers the last 4 days"),
        "{:?}",
        opened.notes
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
        started.elapsed() < Duration::from_secs(4),
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
    drop(conn);
    let report = catalog::clean_with(root, SystemTime::now(), 0).expect("clean");
    assert!(report.capped_sessions >= 1, "{report:?}");
    assert_eq!(count(&read(root), "SELECT count(*) FROM sessions"), 0);
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
    assert_eq!(
        *notes.lock().expect("notes"),
        ["heph: re-deriving 1 recording\n"]
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
    let opened = source::open_repaired(root, &dir, &|_: &str| {});
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    assert!(failed.is_err(), "{failed:?}");
    let opened = opened.expect("answered from a temp copy");
    assert!(opened.is_copy());
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
