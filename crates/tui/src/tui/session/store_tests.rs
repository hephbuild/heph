//! The recording file itself (`session.db`): what it stores, how it is read
//! live, sealed and copied, and what cleanup does to it. The spec's test
//! table names each of these.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::{Duration, SystemTime};

use hcore::events::{
    AdmittedKind, BuildEvent, BuildEventKind, CacheHitKind, CacheOutcomeKind, FixesData,
    LeftBehindData, LogTailData, OutputGroupSummary, SelectionGap, SelectionGapExample,
    TimedOutData, YieldingToData,
};

use super::cleanup::{self, Retention};
use super::recorder::Faults;
use super::tests::{event_record, events, meta_for, read_records, recorder_with};
use super::*;

/// A session directory for `id` under `root` with its lock file present and
/// free, as a lost session leaves it.
fn session_dir(root: &Path, id: SessionId) -> PathBuf {
    let dir = root.join(id.encode());
    std::fs::create_dir(&dir).expect("mkdir");
    std::fs::write(dir.join(files::LOCK), b"").expect("lock file");
    dir
}

/// A fresh recording in `dir` and its writer: WAL mode, the session row
/// written, nothing else. (Its read-write registration ends here: these
/// tests have no other process to protect the writer's locks from.)
fn writer_in(dir: &Path, id: SessionId) -> store::Writer {
    let (conn, _rw) =
        store::create(dir, &meta_for(id), &store::Provenance::default(), false).expect("create");
    store::Writer::new(conn, id)
}

fn insert(writer: &mut store::Writer, record: &SessionRecord) -> String {
    let body = serde_json::to_string(record).expect("encode");
    writer.insert(record, &body).expect("insert");
    body
}

fn header(dir: &Path) -> store::Header {
    store::read_header(&dir.join(store::FILE))
        .expect("read header")
        .expect("a SQLite header")
}

fn now_id(random: u8) -> SessionId {
    SessionId::new(hcore::events::now_unix_ms(), random)
}

fn start_record(at: u64, addr: &str) -> SessionRecord {
    event_record(BuildEvent {
        at_unix_ms: at,
        kind: BuildEventKind::ResultStart { addr: addr.into() },
    })
}

fn end_record(at: u64, addr: &str) -> SessionRecord {
    event_record(BuildEvent {
        at_unix_ms: at,
        kind: BuildEventKind::ResultEnd {
            addr: addr.into(),
            error: None,
            upstream_of: None,
            exit_status: None,
            log_tail: None,
            timed_out: None,
            cancelled: false,
            left_behind: None,
        },
    })
}

fn session_record(at: u64, kind: SessionRecordKind) -> SessionRecord {
    SessionRecord {
        at_unix_ms: at,
        kind: None,
        session: Some(kind),
    }
}

/// The names of the files in `dir`, sorted.
fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn sealed_column(conn: &rusqlite::Connection) -> bool {
    store::read_row_end(conn).expect("row end").sealed
}

/// The position of `kind`'s variant in [`every_event_kind`]. Exhaustive, so
/// a new variant does not compile until it is added to the round trip.
fn event_variant(kind: &BuildEventKind) -> usize {
    use BuildEventKind as K;
    match kind {
        K::RequestConfig { .. } => 0,
        K::Matched { .. } => 1,
        K::SelectionIncomplete { .. } => 2,
        K::ResultStart { .. } => 3,
        K::ResultOutputs { .. } => 4,
        K::ResultEnd { .. } => 5,
        K::ExecuteStart { .. } => 6,
        K::ExecuteEnd { .. } => 7,
        K::LocalCacheHit { .. } => 8,
        K::LocalCacheMiss { .. } => 9,
        K::CacheOutcome { .. } => 10,
        K::CacheHitRebuilt { .. } => 11,
        K::LocalCacheWriteStart { .. } => 12,
        K::LocalCacheWriteEnd { .. } => 13,
        K::ResultLockWaitStart { .. } => 14,
        K::ResultLockWaitEnd { .. } => 15,
        K::AdmissionWaitStart { .. } => 16,
        K::AdmissionWaitEnd { .. } => 17,
        K::PoolUsage { .. } => 18,
        K::ScratchLockWaitStart { .. } => 19,
        K::ScratchLockWaitEnd { .. } => 20,
        K::ScratchPrepareStart { .. } => 21,
        K::ScratchPrepareEnd { .. } => 22,
        K::RemoteCacheHit { .. } => 23,
        K::RemoteCacheMiss { .. } => 24,
        K::RemoteCacheReadStart { .. } => 25,
        K::RemoteCacheReadEnd { .. } => 26,
        K::RemoteCacheWriteStart { .. } => 27,
        K::RemoteCacheWriteEnd { .. } => 28,
        K::GcTargetSwept { .. } => 29,
        K::GcFailureLogsSwept { .. } => 30,
        K::GcCacheDb { .. } => 31,
        K::CodegenExcluded { .. } => 32,
        K::CodegenImported { .. } => 33,
        K::Unknown => 34,
    }
}

/// One of every [`BuildEventKind`], every optional field filled.
#[expect(
    clippy::too_many_lines,
    reason = "one constructor per variant, in declaration order"
)]
fn every_event_kind() -> Vec<BuildEventKind> {
    use BuildEventKind as K;
    let a = || "//a:b".to_owned();
    let err = || Some("boom".to_owned());
    vec![
        K::RequestConfig {
            max_workers: 8,
            fail_fast: true,
            scratch_disabled: true,
            resources: BTreeMap::from([("cpu".into(), Some(8_000_000)), ("docker".into(), None)]),
            units: BTreeMap::from([("cpu".into(), "cpu".into())]),
        },
        K::Matched {
            addrs: vec![a(), "//a:c".into()],
            complete: false,
        },
        K::SelectionIncomplete {
            skipped: 2,
            groups: vec![SelectionGap {
                stage: "list".into(),
                provider: "buildfile".into(),
                count: 2,
                examples: vec![SelectionGapExample {
                    scope: "//x".into(),
                    cause: "broken".into(),
                }],
            }],
        },
        K::ResultStart { addr: a() },
        K::ResultOutputs {
            addr: a(),
            groups: vec![OutputGroupSummary {
                group: "out".into(),
                size: Some(3),
                hash: Some("abc".into()),
                artifacts: 1,
            }],
        },
        K::ResultEnd {
            addr: a(),
            error: err(),
            upstream_of: Some("//a:up".into()),
            exit_status: Some("exit 2".into()),
            log_tail: Some(LogTailData {
                text: "tail\n".into(),
                start_line: 9,
            }),
            timed_out: Some(TimedOutData {
                timeout_ms: 1000,
                abandoned: true,
            }),
            cancelled: true,
            left_behind: Some(Box::new(LeftBehindData {
                log_path: Some("log.txt".into()),
                sandbox: Some("sandbox".into()),
                outputs: Some("out".into()),
                fixes: Some(FixesData::Written {
                    root: "/w".into(),
                    paths: vec!["a.rs".into()],
                    list: Some("fixes.txt".into()),
                }),
                log_readable: Some(true),
            })),
        },
        K::ExecuteStart {
            addr: a(),
            driver: "exec".into(),
            cache: true,
            declared: Some(BTreeMap::from([("cpu".into(), 2_000_000)])),
            reserved: Some(BTreeMap::from([("cpu".into(), 1_000_000)])),
            queued_ms: Some(12),
            admitted: Some(AdmittedKind::Handoff),
            output_groups: vec!["out".into()],
            log_not_streamed: true,
        },
        K::ExecuteEnd {
            addr: a(),
            error: err(),
        },
        K::LocalCacheHit { addr: a() },
        K::LocalCacheMiss { addr: a() },
        K::CacheOutcome {
            addr: a(),
            outcome: CacheOutcomeKind::RemoteHit,
        },
        K::CacheHitRebuilt {
            addr: a(),
            was: CacheHitKind::Remote,
        },
        K::LocalCacheWriteStart { addr: a() },
        K::LocalCacheWriteEnd {
            addr: a(),
            error: err(),
        },
        K::ResultLockWaitStart {
            addr: a(),
            holder_pid: Some(77),
            in_use_by_readers: true,
        },
        K::ResultLockWaitEnd { addr: a() },
        K::AdmissionWaitStart {
            addr: a(),
            pool: "cpu".into(),
            need: 2,
            free: 1,
            ahead: 3,
            removals_pending: Some(4),
            yielding_to: Some(YieldingToData {
                pool: "docker".into(),
                need: 1,
            }),
        },
        K::AdmissionWaitEnd { addr: a() },
        K::PoolUsage {
            pool: "sandbox".into(),
            held: 3,
            capacity: 4,
            waiting: 1,
            removals_pending: 2,
        },
        K::ScratchLockWaitStart {
            addr: a(),
            scratch: "s".into(),
            access: "write".into(),
            holder_pid: Some(5),
        },
        K::ScratchLockWaitEnd {
            addr: a(),
            scratch: "s".into(),
        },
        K::ScratchPrepareStart {
            addr: a(),
            scratch: "s".into(),
        },
        K::ScratchPrepareEnd {
            addr: a(),
            scratch: "s".into(),
            outcome: "reused".into(),
            bytes: 10,
            path_mismatch: true,
            error: err(),
        },
        K::RemoteCacheHit { addr: a() },
        K::RemoteCacheMiss { addr: a() },
        K::RemoteCacheReadStart { addr: a() },
        K::RemoteCacheReadEnd {
            addr: a(),
            error: err(),
        },
        K::RemoteCacheWriteStart { addr: a() },
        K::RemoteCacheWriteEnd {
            addr: a(),
            error: err(),
        },
        K::GcTargetSwept {
            revisions_removed: 1,
            bytes_removed: 2,
        },
        K::GcFailureLogsSwept {
            removed: 3,
            bytes_removed: 4,
        },
        K::GcCacheDb {
            file_bytes: 1,
            wal_bytes: 2,
            live_bytes: 3,
            free_bytes: 4,
            mode: "incremental".into(),
            action: "vacuum".into(),
            freed_bytes: 5,
            needed_bytes: 6,
            available_bytes: 7,
            detail: Some("d".into()),
        },
        K::CodegenExcluded {
            path: "gen.rs".into(),
            status: "stale".into(),
            reason: Some("released".into()),
            addr: Some(a()),
        },
        K::CodegenImported {
            files: 2,
            skipped: 1,
        },
        K::Unknown,
    ]
}

/// The position of `kind`'s variant in [`every_session_kind`]; exhaustive.
fn session_variant(kind: &SessionRecordKind) -> usize {
    use SessionRecordKind as S;
    match kind {
        S::Log { .. } => 0,
        S::End { .. } => 1,
        S::Truncated => 2,
        S::InflightWritten => 3,
        S::Label { .. } => 4,
        S::Approval { .. } => 5,
        S::Finished { .. } => 6,
        S::Output { .. } => 7,
        S::RunLog(_) => 8,
        S::LockHolder { .. } => 9,
        S::Unknown => 10,
    }
}

fn every_session_kind() -> Vec<SessionRecordKind> {
    use SessionRecordKind as S;
    vec![
        S::Log {
            level: "warn".into(),
            msg: "careful".into(),
            seq: 3,
        },
        S::End {
            exit_code: Some(2),
            panicked: true,
        },
        S::Truncated,
        S::InflightWritten,
        S::Label {
            label: "run //a:...".into(),
        },
        S::Approval {
            prompt: Some(Box::new(ApprovalPrompt {
                queue: vec![ApprovalQueued {
                    seq: 1,
                    addr: "//a:gated".into(),
                    queued_at_unix_ms: 5,
                    notices: vec![ApprovalPromptNotice {
                        name: "terms".into(),
                        sources: Vec::new(),
                    }],
                }],
            })),
            standing: Some(ApprovalStanding::Reject),
        },
        S::Finished { stopped: true },
        S::Output {
            text: "out \u{1b}[1mbold\u{1b}[0m é\n".into(),
        },
        S::RunLog(Box::new(RunLogRecord {
            addr: "//a:b".into(),
            hashin: "h".into(),
            file: "runs/h/log.txt".into(),
        })),
        S::LockHolder {
            pid: 42,
            session: Some("0abc".into()),
        },
        S::Unknown,
    ]
}

/// Every build event kind and every session record kind is stored as its
/// JSON verbatim, with its kind name in `kind`, and reads back equal; a
/// record a newer heph writes reads as `Unknown`.
#[test]
fn records_round_trip_every_kind() {
    let events = every_event_kind();
    let indices: BTreeSet<usize> = events.iter().map(event_variant).collect();
    assert_eq!(
        indices,
        (0..events.len()).collect(),
        "one of each build event kind"
    );
    let sessions = every_session_kind();
    let indices: BTreeSet<usize> = sessions.iter().map(session_variant).collect();
    assert_eq!(
        indices,
        (0..sessions.len()).collect(),
        "one of each session kind"
    );

    let mut records: Vec<SessionRecord> = events
        .into_iter()
        .enumerate()
        .map(|(i, kind)| SessionRecord {
            at_unix_ms: 1 + i as u64,
            kind: Some(kind),
            session: None,
        })
        .collect();
    records.extend(
        sessions
            .into_iter()
            .enumerate()
            .map(|(i, kind)| session_record(100 + i as u64, kind)),
    );

    let root = tempfile::tempdir().expect("tempdir");
    let id = now_id(1);
    let dir = session_dir(root.path(), id);
    let mut writer = writer_in(&dir, id);
    let mut bodies: Vec<String> = records.iter().map(|r| insert(&mut writer, r)).collect();
    let future = [
        r#"{"at_unix_ms":200,"kind":{"type":"FromTheFuture","x":1}}"#,
        r#"{"at_unix_ms":201,"session":{"type":"Paused","why":"x"}}"#,
    ];
    for raw in future {
        let decoded: SessionRecord = serde_json::from_str(raw).expect("decode a future record");
        writer.insert(&decoded, raw).expect("insert");
        bodies.push(raw.to_owned());
    }
    let conn = writer.finish(300, Some(0), false).expect("finish");
    assert_eq!(
        store::seal(conn, store::SEAL_TIMEOUT).expect("seal"),
        store::Sealed::Done
    );

    let opened = tests::open(&dir);
    let stored: Vec<(String, String)> = opened
        .conn
        .prepare("SELECT kind, body FROM records ORDER BY seq")
        .expect("prepare")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    drop(opened);
    let (stored_kinds, stored_bodies): (Vec<String>, Vec<String>) = stored.into_iter().unzip();
    assert_eq!(stored_bodies, bodies, "bodies stored verbatim");
    let expected_kinds: Vec<&str> = records
        .iter()
        .map(|r| store::kind_and_addr(r).0)
        .chain(["Unknown", "Unknown"])
        .collect();
    assert_eq!(stored_kinds, expected_kinds);

    let (read, skipped) = read_records(&dir);
    assert_eq!(skipped, 0);
    assert_eq!(read.len(), records.len() + future.len());
    for (got, want) in read.iter().zip(&records) {
        assert_eq!(
            serde_json::to_value(got).expect("encode read"),
            serde_json::to_value(want).expect("encode written"),
        );
    }
    let tail: Vec<&SessionRecord> = read.iter().skip(records.len()).collect();
    assert!(
        matches!(
            tail.first().and_then(|r| r.kind.as_ref()),
            Some(BuildEventKind::Unknown)
        ),
        "{tail:?}"
    );
    assert_eq!(
        tail.get(1).and_then(|r| r.session.as_ref()),
        Some(&SessionRecordKind::Unknown)
    );
}

/// A live reader (the lock held, so a read-only WAL connection) that polls
/// sees each committed batch exactly once, in `seq` order, and nothing of a
/// batch before its commit — including a batch longer than one page.
#[test]
fn live_reader_sees_every_commit_in_order() {
    let root = tempfile::tempdir().expect("tempdir");
    let id = now_id(2);
    let dir = session_dir(root.path(), id);
    let _owner = lockfile::acquire_owner_lock(&dir.join(files::LOCK)).expect("owner lock");
    let mut writer = writer_in(&dir, id);
    let opened = tests::open(&dir);
    assert!(opened.live && !opened.is_copy(), "{opened:?}");
    let mut cursor = store::RecordCursor::new(opened);
    let drain = |cursor: &mut store::RecordCursor| -> Vec<u64> {
        std::iter::from_fn(|| cursor.next_record().expect("read"))
            .map(|r| r.at_unix_ms)
            .collect()
    };
    assert!(drain(&mut cursor).is_empty(), "nothing committed yet");

    let mut at = 0u64;
    for batch in [3u64, 1, 700, 5, 2] {
        let want: Vec<u64> = (at + 1..=at + batch).collect();
        for t in &want {
            insert(&mut writer, &start_record(*t, &format!("//a:t{t}")));
        }
        assert!(drain(&mut cursor).is_empty(), "uncommitted rows are unseen");
        writer.commit().expect("commit");
        assert_eq!(drain(&mut cursor), want, "batch of {batch}");
        assert!(drain(&mut cursor).is_empty(), "each record read once");
        at += batch;
    }
    assert_eq!(cursor.skipped(), 0);
}

/// After the owner's seal there is one file: no `-wal`, `-shm` or creation
/// leftover, a rollback-mode header, `sealed=1`, and it opens `immutable=1`
/// in place — or anywhere it is copied alone — with every record.
#[test]
fn seal_leaves_one_self_contained_file() {
    let root = tempfile::tempdir().expect("tempdir");
    let rec = recorder_with(root.path(), Faults::default());
    let tap = rec.tap();
    for ev in events() {
        tap.event(ev);
    }
    rec.recorder.finish(Some(1), false, Duration::from_secs(10));
    let dir = rec.dir().to_path_buf();

    assert_eq!(
        listing(&dir),
        vec![files::LOCK.to_owned(), store::FILE.to_owned()]
    );
    assert!(!header(&dir).wal, "rollback mode");
    let opened = tests::open(&dir);
    assert!(!opened.is_copy() && !opened.live, "{opened:?}");
    assert!(sealed_column(&opened.conn));
    drop(opened);
    let (records, skipped) = read_records(&dir);
    assert_eq!(skipped, 0);
    assert_eq!(records.len(), events().len() + 1, "every event and End");
    assert!(matches!(
        records.last().and_then(|r| r.session.as_ref()),
        Some(SessionRecordKind::End {
            exit_code: Some(1),
            ..
        })
    ));

    let elsewhere = tempfile::tempdir().expect("tempdir");
    let copy = elsewhere.path().join(store::FILE);
    std::fs::copy(dir.join(store::FILE), &copy).expect("copy");
    let conn = store::open_sealed(&copy).expect("open the copy");
    let n: i64 = conn
        .query_row("SELECT count(*) FROM records", [], |r| r.get(0))
        .expect("count");
    assert_eq!(usize::try_from(n).expect("count"), records.len());
    assert!(sealed_column(&conn));
}

/// Opening a lost, unsealed recording (its `-wal` still beside it) reads it
/// from a copy: the original's bytes and mtimes, and the directory's
/// listing, are unchanged.
#[test]
fn viewer_never_writes_the_original() {
    let root = tempfile::tempdir().expect("tempdir");
    let id = now_id(3);
    let dir = session_dir(root.path(), id);
    let mut writer = writer_in(&dir, id);
    let records: Vec<SessionRecord> = events().into_iter().map(event_record).collect();
    for r in &records {
        insert(&mut writer, r);
    }
    writer.commit().expect("commit");
    // The writer stays open, as a `kill -9` leaves the `-wal` behind.
    let db = dir.join(store::FILE);
    let wal = store::sibling(&db, "-wal");
    assert!(wal.exists(), "precondition: a -wal beside it");
    let snapshot = || {
        let state = |p: &Path| {
            let meta = std::fs::metadata(p).expect("metadata");
            (
                std::fs::read(p).expect("read"),
                meta.modified().expect("mtime"),
            )
        };
        (listing(&dir), state(&db), state(&wal))
    };
    let before = snapshot();

    let opened = tests::open(&dir);
    assert!(opened.is_copy(), "a lost recording reads from a copy");
    drop(opened);
    let (read, _) = read_records(&dir);
    assert_eq!(read.len(), records.len());
    assert_eq!(snapshot(), before, "the original is untouched");
    drop(writer);
}

/// A diamond — `//d:top` on `//d:a` and `//d:b`, both on `//d:shared`, which
/// both reach — gives one `heph.result` per target, every one parented by
/// the session span.
#[test]
fn spans_stay_flat() {
    let root = tempfile::tempdir().expect("tempdir");
    let id = now_id(4);
    let dir = session_dir(root.path(), id);
    let mut writer = writer_in(&dir, id);
    let mut at = id.started_unix_ms();
    let mut tick = || {
        at += 1;
        at
    };
    for addr in ["//d:top", "//d:a", "//d:b", "//d:shared", "//d:shared"] {
        insert(&mut writer, &start_record(tick(), addr));
    }
    insert(
        &mut writer,
        &event_record(BuildEvent {
            at_unix_ms: tick(),
            kind: BuildEventKind::LocalCacheHit {
                addr: "//d:shared".into(),
            },
        }),
    );
    // Two result scopes of `//d:shared` (one per output selection), each
    // with its own end: two flat spans, never one nested in another.
    for addr in ["//d:shared", "//d:shared", "//d:a", "//d:b", "//d:top"] {
        insert(&mut writer, &end_record(tick(), addr));
    }
    let conn = writer.finish(tick(), Some(0), false).expect("finish");

    let results: Vec<(String, Option<i64>)> = conn
        .prepare("SELECT addr, parent_span_id FROM spans WHERE name = 'heph.result' ORDER BY addr")
        .expect("prepare")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    let session = Some(derive::SESSION_SPAN);
    assert_eq!(
        results,
        vec![
            ("//d:a".to_owned(), session),
            ("//d:b".to_owned(), session),
            ("//d:shared".to_owned(), session),
            ("//d:shared".to_owned(), session),
            ("//d:top".to_owned(), session),
        ]
    );
    let root_parent: Option<i64> = conn
        .query_row(
            "SELECT parent_span_id FROM spans WHERE span_id = ?1",
            [derive::SESSION_SPAN],
            |r| r.get(0),
        )
        .expect("session span");
    assert_eq!(root_parent, None, "the session span is the root");
    let hit_on: i64 = conn
        .query_row(
            "SELECT e.span_id = s.span_id FROM span_events e, spans s
             WHERE e.name = 'heph.cache.local_hit' AND s.addr = '//d:shared'
               AND s.name = 'heph.result'",
            [],
            |r| r.get(0),
        )
        .expect("hit event");
    assert_eq!(hit_on, 1, "the hit is on the shared target's one span");
}

/// I3, against what the seal writes. The owner writes `sealed=1` in the
/// last WAL transaction, then seals; a viewer in the way leaves the file in
/// WAL mode, which sends every reader down the copy path, where it still
/// reads every committed row. Whatever the viewer does — mid-read, between
/// reads, gone partway through the seal — the file ends either in rollback
/// mode or with a WAL-mode header, never in between.
#[test]
fn seal_with_a_viewer_open() {
    enum Viewer {
        MidRead,
        BetweenReads,
        GoneMidSeal,
    }
    for (viewer, timeout) in [
        (Viewer::MidRead, Duration::from_millis(200)),
        (Viewer::BetweenReads, Duration::from_millis(500)),
        (Viewer::GoneMidSeal, Duration::from_secs(3)),
    ] {
        let root = tempfile::tempdir().expect("tempdir");
        let id = now_id(5);
        let dir = session_dir(root.path(), id);
        let owner = lockfile::acquire_owner_lock(&dir.join(files::LOCK)).expect("owner lock");
        let mut writer = writer_in(&dir, id);
        let records: Vec<SessionRecord> = events().into_iter().map(event_record).collect();
        for r in &records {
            insert(&mut writer, r);
        }
        writer.commit().expect("commit");

        let opened = tests::open(&dir);
        assert!(opened.live, "the owner holds the lock");
        let mut cursor = store::RecordCursor::new(opened);
        assert_eq!(
            std::iter::from_fn(|| cursor.next_record().expect("read")).count(),
            records.len()
        );
        if !matches!(viewer, Viewer::BetweenReads) {
            // A read transaction: its snapshot pins the WAL.
            cursor.conn().execute_batch("BEGIN").expect("begin");
            let _: i64 = cursor
                .conn()
                .query_row("SELECT count(*) FROM records", [], |r| r.get(0))
                .expect("read");
        }
        // Held through the seal and the reads after it, or dropped by a
        // thread partway through the seal.
        let (mut held, gone) = if matches!(viewer, Viewer::GoneMidSeal) {
            let t = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                drop(cursor);
            });
            (None, Some(t))
        } else {
            (Some(cursor), None)
        };
        let sealed = seal_end(writer, timeout);
        if let Some(t) = gone {
            t.join().expect("viewer thread");
            assert_eq!(sealed, store::Sealed::Done, "the viewer left in time");
        }
        // The viewer that stayed reads on after the seal: the next record
        // its cursor yields is the owner's `End`, and nothing after it.
        if let Some(cursor) = held.as_mut() {
            if matches!(viewer, Viewer::MidRead) {
                cursor.conn().execute_batch("COMMIT").expect("end the read");
            }
            let after: Vec<SessionRecord> =
                std::iter::from_fn(|| cursor.next_record().expect("read after the seal")).collect();
            assert_eq!(after.len(), 1, "only End was written after the first read");
            assert!(
                matches!(
                    after.first().and_then(|r| r.session.as_ref()),
                    Some(SessionRecordKind::End { .. })
                ),
                "the held cursor reads End after the seal: {after:?}"
            );
        }
        let mid_read = matches!(viewer, Viewer::MidRead);
        check_sealed(&dir, owner, sealed, records.len(), mid_read);
        drop(held);
        if mid_read {
            seal_lost_after_cut_short(&dir);
        }
    }
}

/// A cut-short seal leaves `sealed=1` under a WAL-mode header. The cleanup
/// then seals the file without rewriting the session row: `sealed=1` means
/// `End` (and the owner's end fields) was committed, so `seal_lost` keeps
/// the owner's `ended_ms` and `exit_code` and does not stamp the derivation
/// stale; it only finishes the checkpoint and the mode switch.
fn seal_lost_after_cut_short(dir: &Path) {
    let path = dir.join(store::FILE);
    let row = |conn: &rusqlite::Connection| -> (Option<i64>, Option<i64>, u32) {
        conn.query_row(
            "SELECT ended_ms, exit_code, derivation FROM session",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("session row")
    };
    let before = row(&tests::alter(dir));
    assert!(
        header(dir).wal,
        "precondition: the cut-short seal left WAL mode"
    );
    assert_eq!(before.1, Some(0), "precondition: the owner's exit code");
    assert_ne!(before.2, store::STALE_DERIVATION, "precondition: derived");

    assert_eq!(
        store::seal_lost(&path).expect("seal_lost"),
        store::Sealed::Done
    );
    assert!(!header(dir).wal, "seal_lost finished the mode switch");
    let opened = tests::open(dir);
    assert!(!opened.is_copy(), "sealed: read in place");
    assert_eq!(
        row(&opened.conn),
        before,
        "a session the owner sealed keeps its end time, exit code and derivation"
    );
}

/// The owner's end: `End`, the session row's end fields and `sealed=1` in
/// the last WAL transaction, then the seal.
fn seal_end(mut writer: store::Writer, timeout: Duration) -> store::Sealed {
    insert(
        &mut writer,
        &session_record(
            hcore::events::now_unix_ms(),
            SessionRecordKind::End {
                exit_code: Some(0),
                panicked: false,
            },
        ),
    );
    let conn = writer
        .finish(hcore::events::now_unix_ms(), Some(0), false)
        .expect("finish");
    store::seal(conn, timeout).unwrap_or_else(|e| panic!("seal within {timeout:?}: {e}"))
}

/// The owner releases its lock; then the recording reads as sealed in place
/// (rollback mode), or through a copy (WAL mode) with every row and `End`.
fn check_sealed(
    dir: &Path,
    owner: std::fs::File,
    sealed: store::Sealed,
    events: usize,
    expect_cut_short: bool,
) {
    drop(owner);
    let wal = header(dir).wal;
    match sealed {
        store::Sealed::Done => assert!(!wal, "a finished seal is in rollback mode"),
        store::Sealed::CutShort => assert!(wal, "a cut-short seal stays in WAL mode"),
    }
    if expect_cut_short {
        assert_eq!(
            sealed,
            store::Sealed::CutShort,
            "a reader mid-read blocks it"
        );
    }
    let opened = tests::open(dir);
    assert_eq!(opened.is_copy(), wal, "a WAL-mode header reads from a copy");
    assert!(sealed_column(&opened.conn));
    drop(opened);
    let (read, skipped) = read_records(dir);
    assert_eq!(skipped, 0);
    assert_eq!(read.len(), events + 1);
    assert!(matches!(
        read.last().and_then(|r| r.session.as_ref()),
        Some(SessionRecordKind::End { .. })
    ));
}

/// I3: only a rollback-mode header opens `immutable=1`. A file whose session
/// row says `sealed=1` under a WAL-mode header is read through a copy, with
/// every committed row.
#[test]
fn immutable_only_on_a_rollback_header() {
    let root = tempfile::tempdir().expect("tempdir");
    let id = now_id(6);
    let dir = session_dir(root.path(), id);
    let records: Vec<SessionRecord> = events().into_iter().map(event_record).collect();
    tests::unsealed_recording(&dir, id, &records);
    tests::alter(&dir)
        .execute("UPDATE session SET sealed = 1", [])
        .expect("mark sealed");
    assert!(header(&dir).wal, "precondition: still WAL mode");

    let opened = tests::open(&dir);
    assert!(
        opened.is_copy(),
        "a WAL-mode header is never opened immutable"
    );
    assert!(sealed_column(&opened.conn));
    drop(opened);
    let (read, skipped) = read_records(&dir);
    assert_eq!((read.len(), skipped), (records.len(), 0));
}

/// I3: a viewer copying a lost session holds its lock shared, so a cleanup
/// trying to seal it then leaves it alone (to the next cleanup: it does not
/// wait); once the viewer is done the copy held every committed row and the
/// next cleanup seals it. The viewer is parked inside `open_copy` itself,
/// after its lock and before its copy, so an `open_copy` that copied before
/// locking, or without locking, has the cleanup seal under it and fails
/// here. And a viewer arriving while a cleanup holds the lock to seal reads
/// in place, read-only, rather than copying a file being rewritten.
#[test]
fn copy_holds_the_lock_shared() {
    let root = tempfile::tempdir().expect("tempdir");
    let id = now_id(7);
    let dir = session_dir(root.path(), id);
    let records: Vec<SessionRecord> = events().into_iter().map(event_record).collect();
    tests::unsealed_recording(&dir, id, &records);
    let lock = dir.join(files::LOCK);
    let later = SystemTime::now() + 2 * cleanup::STARTING;

    let (locked_tx, locked_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let go_rx = std::sync::Mutex::new(go_rx);
    store::copy_gate::set(&lock, move || {
        // Later opens (after the test let go) find no one listening.
        locked_tx.send(()).unwrap_or_default();
        go_rx.lock().expect("gate").recv().unwrap_or_default();
    });
    let viewer = std::thread::spawn({
        let dir = dir.clone();
        move || {
            let opened = tests::open(&dir);
            assert!(opened.is_copy(), "{opened:?}");
            let mut cursor = store::RecordCursor::new(opened);
            std::iter::from_fn(|| cursor.next_record().expect("read")).count()
        }
    });
    locked_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the viewer reaches its copy");
    let report = cleanup::cleanup(root.path(), later, Retention::default()).expect("cleanup");
    assert!(report.sealed.is_empty(), "{report:?}");
    assert!(report.removed.is_empty(), "{report:?}");
    assert!(header(&dir).wal, "not sealed under the viewer");
    go_tx.send(()).expect("release the viewer");
    assert_eq!(viewer.join().expect("viewer"), records.len());
    store::copy_gate::set(&lock, || {});

    let sealing = lockfile::try_lock_free(&lock)
        .expect("lock exclusive")
        .expect("free");
    // The probe reads a held lock as an owner: read-only, in place, never a
    // copy of a file being rewritten.
    let opened = tests::open(&dir);
    assert!(opened.live && !opened.is_copy(), "{opened:?}");
    drop(opened);
    drop(sealing);

    let opened = tests::open(&dir);
    assert!(opened.is_copy());
    drop(opened);
    let (read, skipped) = read_records(&dir);
    assert_eq!((read.len(), skipped), (records.len(), 0));

    let report = cleanup::cleanup(root.path(), later, Retention::default()).expect("cleanup");
    assert_eq!(report.sealed, vec![id]);
    assert!(!header(&dir).wal);
}

/// I3, raced for real: a cleanup sealing a lost session and a viewer opening
/// it start together, many times over. Whichever wins, the viewer reads every
/// committed record (never a short or failed read, and never "try again": a
/// viewer that meets the seal waits for it), the cleanup seals it or leaves
/// it for the next one, and the next cleanup always ends with it sealed and
/// whole.
#[test]
fn seal_lost_races_open_local() {
    let records: Vec<SessionRecord> = events().into_iter().map(event_record).collect();
    let later = SystemTime::now() + 2 * cleanup::STARTING;
    let mut outcomes = BTreeMap::<&str, usize>::new();
    for round in 0..40u8 {
        let root = tempfile::tempdir().expect("tempdir");
        let id = now_id(round);
        let dir = session_dir(root.path(), id);
        tests::unsealed_recording(&dir, id, &records);
        let start = Arc::new(Barrier::new(2));
        let viewer = std::thread::spawn({
            let (root, dir, start) = (root.path().to_path_buf(), dir.clone(), Arc::clone(&start));
            move || {
                start.wait();
                match store::open_local(&root, &dir) {
                    Ok(opened) => {
                        let how = if opened.is_copy() {
                            "copy"
                        } else if opened.live {
                            "in place"
                        } else {
                            "sealed"
                        };
                        let mut cursor = store::RecordCursor::new(opened);
                        let n = std::iter::from_fn(|| cursor.next_record().expect("read")).count();
                        (how, n)
                    }
                    Err(e) => panic!("round {round}: {e:?}"),
                }
            }
        });
        start.wait();
        let first = cleanup::cleanup(root.path(), later, Retention::default()).expect("cleanup");
        let (how, n) = viewer.join().expect("viewer");
        assert_eq!(n, records.len(), "round {round}: read {how}");
        *outcomes.entry(how).or_default() += 1;
        assert!(first.removed.is_empty(), "round {round}: {first:?}");
        if first.sealed.is_empty() {
            let again =
                cleanup::cleanup(root.path(), later, Retention::default()).expect("cleanup");
            assert_eq!(again.sealed, vec![id], "round {round}");
        }
        assert!(!header(&dir).wal, "round {round}: sealed");
        let (read, skipped) = read_records(&dir);
        assert_eq!((read.len(), skipped), (records.len(), 0), "round {round}");
    }
    eprintln!("seal_lost_races_open_local: {outcomes:?}");
}

/// A header read of a recording this process has open read-write goes
/// through the registry's fd, never a plain fd of its own, whose close would
/// drop every `fcntl` lock the writer holds (`store::rw_open`): the
/// recorder's own recording is registered from before its write connection
/// opens until after it closes, and is read (header, `open_local`, the
/// holder scan's `read_meta_in`) without one. Once the recorder is gone, a
/// read opens its own fd again.
#[test]
fn header_reads_never_close_an_fd_on_a_recording_open_read_write() {
    let root = tempfile::tempdir().expect("tempdir");
    let rec = recorder_with(root.path(), Faults::default());
    let path = rec.dir().join(store::FILE);
    assert!(store::rw_open::is_open(&path).expect("stat"));
    store::rw_open::plain_reads::take(&path);

    let h = store::read_header(&path).expect("read").expect("sqlite");
    assert!(h.is_known() && h.wal, "{h:?}");
    let meta = store::read_meta_in(root.path(), rec.dir()).expect("meta");
    assert_eq!(meta.id, rec.id());
    assert_eq!(store::rw_open::plain_reads::take(&path), 0);

    rec.finish(Some(0), false);
    // The thread drops its registration as it exits, just after the seal.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while store::rw_open::is_open(&path).expect("stat") {
        assert!(std::time::Instant::now() < deadline, "still registered");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !store::read_header(&path)
            .expect("read")
            .expect("sqlite")
            .wal
    );
    assert_eq!(store::rw_open::plain_reads::take(&path), 1);
}

/// Registrations count: the kept fd stays until the last one drops.
#[test]
fn rw_registrations_nest() {
    let root = tempfile::tempdir().expect("tempdir");
    let id = now_id(41);
    let dir = session_dir(root.path(), id);
    tests::unsealed_recording(&dir, id, &[start_record(1, "//p:a")]);
    let path = dir.join(store::FILE);
    let outer = store::rw_open::register(&path).expect("register");
    let inner = store::rw_open::register(&path).expect("register");
    drop(inner);
    assert!(store::rw_open::is_open(&path).expect("stat"));
    drop(outer);
    assert!(!store::rw_open::is_open(&path).expect("stat"));
}

/// A viewer never copies a recording this process still has open read-write
/// with its lock free (a recorder that stopped before its thread closed the
/// connection): the copy's plain fds would drop the writer's locks. It
/// waits, as for a seal, and copies once the registration is gone.
#[test]
fn open_local_waits_out_a_write_connection_of_this_process() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let root = tempfile::tempdir().expect("tempdir");
    let id = now_id(42);
    let dir = session_dir(root.path(), id);
    tests::unsealed_recording(&dir, id, &[start_record(1, "//p:a")]);
    let path = dir.join(store::FILE);
    let rw = store::rw_open::register(&path).expect("register");
    let released = Arc::new(AtomicBool::new(false));
    let releaser = std::thread::spawn({
        let released = Arc::clone(&released);
        move || {
            std::thread::sleep(Duration::from_millis(200));
            released.store(true, Ordering::SeqCst);
            drop(rw);
        }
    });
    let opened = store::open_local(root.path(), &dir).expect("open");
    assert!(released.load(Ordering::SeqCst), "copied while registered");
    assert!(opened.is_copy());
    releaser.join().expect("releaser");
}

/// A cursor reads past more undecodable bodies than one page holds: a page
/// that decodes to nothing is not the end of the recording. (1100 bodies:
/// the first page also holds the first record, so the second is all
/// undecodable.)
#[test]
fn cursor_reads_past_a_page_of_undecodable_bodies() {
    let root = tempfile::tempdir().expect("tempdir");
    let id = now_id(10);
    let dir = session_dir(root.path(), id);
    tests::unsealed_recording(&dir, id, &[start_record(1, "//p:a")]);
    let last = start_record(3, "//p:b");
    let body = serde_json::to_string(&last).expect("encode");
    let conn = tests::alter(&dir);
    conn.execute_batch(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1100)
         INSERT INTO records (at_ms, kind, body) SELECT 2, 'Event', 'not json' FROM n",
    )
    .expect("undecodable bodies");
    conn.execute(
        "INSERT INTO records (at_ms, kind, body) VALUES (3, 'Event', ?1)",
        [&body],
    )
    .expect("last record");
    drop(conn);

    let (read, skipped) = read_records(&dir);
    assert_eq!(skipped, 1100);
    let bodies: Vec<String> = read
        .iter()
        .map(|r| serde_json::to_string(r).expect("encode"))
        .collect();
    let first = serde_json::to_string(&start_record(1, "//p:a")).expect("encode");
    assert_eq!(bodies, vec![first, body]);
}

/// Creating a recording is one transaction: when a statement of the schema
/// fails, nothing before it is left behind (no session table, no format
/// stamp) and the file is never renamed into place.
#[test]
fn create_is_one_transaction() {
    let root = tempfile::tempdir().expect("tempdir");
    let id = now_id(9);
    let dir = session_dir(root.path(), id);
    let tmp = dir.join(store::CREATING);
    // The schema's last table, already there: its CREATE fails.
    rusqlite::Connection::open(&tmp)
        .expect("open")
        .execute_batch("CREATE TABLE metrics (x)")
        .expect("pre-create");

    let err = store::create(&dir, &meta_for(id), &store::Provenance::default(), false)
        .expect_err("the schema cannot be created");
    assert!(format!("{err:#}").contains("schema"), "{err:#}");
    assert!(!dir.join(store::FILE).exists());
    let conn = rusqlite::Connection::open(&tmp).expect("reopen");
    let tables: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'session'",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(tables, 0, "a failed schema left its first tables behind");
    let format: u32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .expect("user_version");
    assert_eq!(format, 0);
}

/// A viewer arriving while the lock holder seals (the session row already
/// says `sealed=1`, the header is still WAL) reads the session as ended, not
/// live, and holds nothing open meanwhile: the seal finishes and the viewer
/// reads it sealed in place. If the holder lets go without sealing, the
/// viewer reads a copy.
#[test]
fn viewer_during_a_seal_reads_it_ended() {
    for seal_it in [true, false] {
        let root = tempfile::tempdir().expect("tempdir");
        let id = now_id(8);
        let dir = session_dir(root.path(), id);
        let records: Vec<SessionRecord> = events().into_iter().map(event_record).collect();
        tests::unsealed_recording(&dir, id, &records);
        let sealing = lockfile::try_lock_free(&dir.join(files::LOCK))
            .expect("lock exclusive")
            .expect("free");
        // What seal_lost commits before its checkpoint and mode switch.
        let conn = tests::alter(&dir);
        conn.execute("UPDATE session SET sealed = 1", [])
            .expect("mark sealed");

        let viewer = std::thread::spawn({
            let dir = dir.clone();
            move || tests::open(&dir)
        });
        std::thread::sleep(Duration::from_millis(100));
        if seal_it {
            let sealed = store::seal(conn, Duration::from_millis(500)).expect("seal");
            assert_eq!(sealed, store::Sealed::Done, "the viewer cut the seal short");
        } else {
            drop(conn);
        }
        drop(sealing);
        let opened = viewer.join().expect("viewer thread");
        assert!(!opened.live, "a session being sealed read as live");
        assert_eq!(opened.is_copy(), !seal_it, "{opened:?}");
        assert!(sealed_column(&opened.conn));
        drop(opened);
        let (read, skipped) = read_records(&dir);
        assert_eq!((read.len(), skipped), (records.len(), 0));
    }
}

/// I2: a session still starting (a free lock file younger than a minute,
/// with or without a recording) is neither sealed nor given a recording;
/// a lock file with no recording never gets one; and two cleanups sealing
/// one lost session at once leave one sealed file.
#[test]
fn cleanup_spares_a_starting_session() {
    let root = tempfile::tempdir().expect("tempdir");
    let now = SystemTime::now();

    // Its owner has created the lock file but not yet the recording.
    let empty = session_dir(root.path(), now_id(8));
    // Its owner has written the recording but not yet taken the lock.
    let id = now_id(9);
    let fresh = session_dir(root.path(), id);
    let records: Vec<SessionRecord> = events().into_iter().map(event_record).collect();
    tests::unsealed_recording(&fresh, id, &records);
    for at in [now, now + Duration::from_secs(30)] {
        let report = cleanup::cleanup(root.path(), at, Retention::default()).expect("cleanup");
        assert!(
            report.sealed.is_empty() && report.removed.is_empty(),
            "{report:?}"
        );
        assert_eq!(listing(&empty), vec![files::LOCK.to_owned()]);
        assert!(header(&fresh).wal, "not sealed while starting");
    }

    let later = now + 2 * cleanup::STARTING;
    let barrier = Arc::new(Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let root = root.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                cleanup::cleanup(&root, later, Retention::default())
            })
        })
        .collect();
    let sealed: Vec<SessionId> = threads
        .into_iter()
        .flat_map(|t| t.join().expect("join").expect("cleanup").sealed)
        .collect();
    assert!(
        !sealed.is_empty() && sealed.iter().all(|s| *s == id),
        "{sealed:?}"
    );
    assert_eq!(
        listing(&empty),
        vec![files::LOCK.to_owned()],
        "never created"
    );
    assert_eq!(
        listing(&fresh),
        vec![files::LOCK.to_owned(), store::FILE.to_owned()],
        "one self-contained file"
    );
    assert!(!header(&fresh).wal);
    let opened = tests::open(&fresh);
    assert!(sealed_column(&opened.conn));
    drop(opened);
    let (read, skipped) = read_records(&fresh);
    assert_eq!((read.len(), skipped), (records.len(), 0));
}

/// Where format `n`'s committed fixture lives.
fn fixture(n: u32) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/tui/session/testdata")
        .join(format!("format-{n}"))
        .join(store::FILE)
}

/// The fixture's session: fixed, so the fixture's rows are too.
const FIXTURE_STARTED_MS: u64 = 1_700_000_000_000;

/// Write a small sealed recording in the current format into `dir`: two
/// targets (one hit, one ran with phase spans and declared resources), two
/// pools, a log line, a lock holder and the end.
fn write_fixture(dir: &Path) {
    let id = SessionId::new(FIXTURE_STARTED_MS, 1);
    let meta = SessionMeta {
        owner_pid: 4242,
        heph_version: "0.0.0-fixture".into(),
        cwd: Some("/work".into()),
        workspace_root: Some("/work".into()),
        label: "run //p:...".into(),
        ..meta_for(id)
    };
    let provenance = store::Provenance {
        host: Some("fixture-host".into()),
        user: Some("fixture".into()),
        actor: store::Actor {
            kind: "human",
            name: None,
            ci_run_url: None,
        },
    };
    let (conn, _rw) = store::create(dir, &meta, &provenance, false).expect("create");
    let mut writer = store::Writer::new(conn, id);
    let s = FIXTURE_STARTED_MS;
    let ev = |at: u64, kind| {
        event_record(BuildEvent {
            at_unix_ms: s + at,
            kind,
        })
    };
    let pools = |at: u64, cpu: u64, docker: u64, waiting: u64| {
        [("cpu", cpu, 4_000_000), ("docker", docker, 2)].map(|(pool, held, capacity)| {
            ev(
                at,
                BuildEventKind::PoolUsage {
                    pool: pool.into(),
                    held,
                    capacity,
                    waiting,
                    removals_pending: 0,
                },
            )
        })
    };
    let mut records = vec![
        ev(
            1,
            BuildEventKind::RequestConfig {
                max_workers: 4,
                fail_fast: false,
                scratch_disabled: false,
                resources: BTreeMap::from([
                    ("cpu".into(), Some(4_000_000)),
                    ("docker".into(), Some(2)),
                ]),
                units: BTreeMap::from([
                    ("cpu".into(), "cpu".into()),
                    ("docker".into(), "count".into()),
                ]),
            },
        ),
        ev(
            2,
            BuildEventKind::Matched {
                addrs: vec!["//p:t".into(), "//p:dep".into()],
                complete: true,
            },
        ),
        start_record(s + 3, "//p:t"),
        start_record(s + 3, "//p:dep"),
        ev(
            4,
            BuildEventKind::LocalCacheHit {
                addr: "//p:dep".into(),
            },
        ),
        ev(
            4,
            BuildEventKind::CacheOutcome {
                addr: "//p:dep".into(),
                outcome: CacheOutcomeKind::LocalHit,
            },
        ),
        end_record(s + 5, "//p:dep"),
        ev(
            5,
            BuildEventKind::LocalCacheMiss {
                addr: "//p:t".into(),
            },
        ),
    ];
    records.extend(pools(6, 1_000_000, 1, 1));
    records.push(ev(
        6,
        BuildEventKind::ExecuteStart {
            addr: "//p:t".into(),
            driver: "exec".into(),
            cache: true,
            declared: Some(BTreeMap::from([("cpu".into(), 1_000_000)])),
            reserved: Some(BTreeMap::from([("cpu".into(), 1_000_000)])),
            queued_ms: Some(1),
            admitted: Some(AdmittedKind::Arrival),
            output_groups: Vec::new(),
            log_not_streamed: false,
        },
    ));
    records.push(session_record(
        s + 7,
        SessionRecordKind::Log {
            level: "info".into(),
            msg: "hello".into(),
            seq: 1,
        },
    ));
    records.push(session_record(
        s + 8,
        SessionRecordKind::LockHolder {
            pid: 99,
            session: None,
        },
    ));
    records.extend(pools(9, 0, 0, 0));
    for r in &records {
        insert(&mut writer, r);
    }
    // ResultEnd with phase spans, as a host that sends them writes it.
    let end = end_record(s + 10, "//p:t");
    let mut body = serde_json::to_value(&end).expect("encode");
    let us = |ms: u64| (s + ms) * 1000;
    let spans: Vec<serde_json::Value> = [
        ("heph.resolve", 3, 4),
        ("heph.admission", 5, 6),
        ("heph.process", 6, 9),
        ("heph.collect", 9, 10),
    ]
    .into_iter()
    .map(|(name, from, to)| {
        serde_json::json!({"name": name, "start_unix_us": us(from), "end_unix_us": us(to)})
    })
    .collect();
    body.get_mut("kind")
        .and_then(serde_json::Value::as_object_mut)
        .expect("a build event")
        .insert("spans".into(), spans.into());
    writer
        .insert(&end, &body.to_string())
        .expect("insert ResultEnd");
    insert(
        &mut writer,
        &session_record(
            s + 11,
            SessionRecordKind::End {
                exit_code: Some(0),
                panicked: false,
            },
        ),
    );
    let conn = writer.finish(s + 12, Some(0), false).expect("finish");
    assert_eq!(
        store::seal(conn, store::SEAL_TIMEOUT).expect("seal"),
        store::Sealed::Done
    );
    // Small pages and no free ones: a few KiB to commit. Both keep the
    // header's application id and format.
    tests::alter(dir)
        .execute_batch("PRAGMA page_size = 1024; VACUUM;")
        .expect("compact");
}

/// Writes the current format's fixture. Run it only for a format that has
/// none yet: a released format's fixture is frozen. See `testdata/README.md`.
#[test]
#[ignore = "writes the committed fixture; run by hand on a format bump"]
fn generate_format_fixture() {
    let target = fixture(store::FORMAT);
    assert!(
        !target.exists(),
        "{} exists: a released format's fixture is frozen; delete it first only if format {} was never released",
        target.display(),
        store::FORMAT
    );
    let root = tempfile::tempdir().expect("tempdir");
    let dir = session_dir(root.path(), SessionId::new(FIXTURE_STARTED_MS, 1));
    write_fixture(&dir);
    std::fs::create_dir_all(target.parent().expect("a parent")).expect("mkdir");
    std::fs::copy(dir.join(store::FILE), &target).expect("copy the fixture");
}

/// The recipes `docs/SESSIONS.md` gives, one statement each, from its first
/// `sql` block.
fn recipes() -> Vec<String> {
    let doc = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/SESSIONS.md");
    let text = std::fs::read_to_string(&doc).expect("read docs/SESSIONS.md");
    let block = text
        .split("```sql\n")
        .nth(1)
        .and_then(|rest| rest.split("```").next())
        .expect("a sql block in docs/SESSIONS.md");
    block
        .split(';')
        .map(|stmt| {
            stmt.lines()
                .filter(|l| !l.trim_start().starts_with("--"))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|stmt| !stmt.trim().is_empty())
        .collect()
}

/// I5: every released format's committed fixture opens in place as a sealed
/// recording, reads back every record with none skipped, and answers the
/// documented recipes and the pool summary. A format bump that adds no
/// fixture fails here.
#[test]
fn opens_format_1_fixture() {
    let recipes = recipes();
    assert_eq!(recipes.len(), 2, "{recipes:?}");
    for n in 1..=store::FORMAT {
        let path = fixture(n);
        assert!(
            path.exists(),
            "format {n} has no committed fixture at {}: run `cargo test -p tui generate_format_fixture -- --ignored`",
            path.display()
        );
        let root = tempfile::tempdir().expect("tempdir");
        let id = store::read_meta(&store::open_sealed(&path).expect("open the fixture"))
            .expect("the fixture's session row")
            .id;
        let dir = session_dir(root.path(), id);
        std::fs::copy(&path, dir.join(store::FILE)).expect("copy the fixture");

        let header = header(&dir);
        assert_eq!((header.format, header.wal), (n, false), "format {n}");
        let opened = tests::open(&dir);
        assert!(!opened.is_copy() && !opened.live, "format {n}: sealed");
        assert!(sealed_column(&opened.conn));
        if n == store::FORMAT {
            // Within a format the schema only grows (I5, I9): every object of
            // the fixture is still there, with its columns in their order.
            // The public explorer names' columns are the contract and do not
            // move at all; growth there is a TEMP view of a later heph (D20).
            let fresh_root = tempfile::tempdir().expect("tempdir");
            let fresh_dir = session_dir(fresh_root.path(), SessionId::new(FIXTURE_STARTED_MS, 1));
            write_fixture(&fresh_dir);
            let fresh = store::open_sealed(&fresh_dir.join(store::FILE)).expect("open fresh");
            assert_schema_grew_only(n, &opened.conn, &fresh);
        }
        for sql in &recipes {
            let rows = count_rows(&opened.conn, sql);
            assert!(rows > 0, "format {n}: no rows from {sql}");
        }
        assert!(
            !store::read_pools(&opened.conn)
                .expect("read pools")
                .is_empty(),
            "format {n}: pool summaries"
        );
        drop(opened);
        let (read, skipped) = read_records(&dir);
        assert_eq!(skipped, 0, "format {n}");
        assert!(
            matches!(
                read.last().and_then(|r| r.session.as_ref()),
                Some(SessionRecordKind::End { .. })
            ),
            "format {n}: ends with End"
        );
    }
}

/// The explorer's public names (I9): their columns are frozen within a
/// format.
const PUBLIC_NAMES: [&str; 6] = [
    "sessions",
    "spans",
    "span_events",
    "metrics",
    "logs",
    "targets",
];

/// Every schema object of a recording but SQLite's own, as `(type, name)`.
fn schema_objects(conn: &rusqlite::Connection) -> Vec<(String, String)> {
    let mut stmt = conn
        .prepare(
            "SELECT type, name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
        )
        .expect("prepare");
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .collect::<rusqlite::Result<_>>()
        .expect("schema objects")
}

/// The columns of a table or view (`table_info`) or an index (`index_info`),
/// in order.
fn object_columns(conn: &rusqlite::Connection, kind: &str, name: &str) -> Vec<String> {
    let sql = match kind {
        "index" => "SELECT coalesce(name, '<expr>') FROM pragma_index_info(?1) ORDER BY seqno",
        _ => "SELECT name FROM pragma_table_info(?1) ORDER BY cid",
    };
    let mut stmt = conn.prepare(sql).expect("prepare");
    stmt.query_map([name], |r| r.get(0))
        .expect("query")
        .collect::<rusqlite::Result<_>>()
        .expect("columns")
}

/// `part` appears in `whole` in the same order (not necessarily adjacent).
fn in_order(part: &[String], whole: &[String]) -> bool {
    let mut rest = whole.iter();
    part.iter().all(|p| rest.any(|w| w == p))
}

/// Format `n`'s fixture schema against a fresh recording's: every fixture
/// object still exists with the same type and its columns in their order,
/// and the public names' columns are exactly the fixture's.
fn assert_schema_grew_only(n: u32, fixture: &rusqlite::Connection, fresh: &rusqlite::Connection) {
    let fresh_objects = schema_objects(fresh);
    let objects = schema_objects(fixture);
    for public in PUBLIC_NAMES {
        assert!(
            objects.iter().any(|(_, name)| name == public),
            "format {n}: the fixture has no {public}"
        );
    }
    for (kind, name) in objects {
        assert!(
            fresh_objects.iter().any(|(k, m)| *k == kind && *m == name),
            "format {n}: {kind} {name} is in the format's fixture but no longer in its schema. \
             Within a format the schema only grows (I5): keep it, or make the change a new format \
             (bump store::FORMAT and run `cargo test -p tui generate_format_fixture -- --ignored`)"
        );
        if kind == "trigger" {
            continue;
        }
        let was = object_columns(fixture, &kind, &name);
        let now = object_columns(fresh, &kind, &name);
        assert!(
            !was.is_empty(),
            "format {n}: no columns read for {kind} {name}"
        );
        if PUBLIC_NAMES.contains(&name.as_str()) {
            assert_eq!(
                was, now,
                "format {n}: the columns of {name}, a public explorer name, moved. Stored views in \
                 a released recording never change (I9): grow the contract with heph's TEMP views \
                 (D20), or make it a new format"
            );
        } else {
            assert!(
                in_order(&was, &now),
                "format {n}: {kind} {name} lost or reordered columns ({was:?} -> {now:?}). Within a \
                 format the schema only grows (I5): add columns, or make it a new format"
            );
        }
    }
}

/// How many rows `sql` returns.
fn count_rows(conn: &rusqlite::Connection, sql: &str) -> usize {
    let mut stmt = conn.prepare(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let mut rows = stmt.query([]).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let mut n = 0;
    while rows
        .next()
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .is_some()
    {
        n += 1;
    }
    n
}

/// Every row `sql` returns, its columns joined by `|`: `NULL`, an integer, a
/// real as `{:?}` (`4.0`), text as is.
fn dump(conn: &rusqlite::Connection, sql: &str) -> Vec<String> {
    use rusqlite::types::Value;
    let mut stmt = conn.prepare(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let n = stmt.column_count();
    stmt.query_map([], |r| {
        (0..n)
            .map(|i| {
                Ok(match r.get::<_, Value>(i)? {
                    Value::Null => "NULL".to_owned(),
                    Value::Integer(v) => v.to_string(),
                    Value::Real(v) => format!("{v:?}"),
                    Value::Text(v) => v,
                    Value::Blob(v) => format!("<{} bytes>", v.len()),
                })
            })
            .collect::<rusqlite::Result<Vec<_>>>()
            .map(|cols| cols.join("|"))
    })
    .unwrap_or_else(|e| panic!("{sql}: {e}"))
    .collect::<rusqlite::Result<_>>()
    .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn ev(at: u64, kind: BuildEventKind) -> SessionRecord {
    event_record(BuildEvent {
        at_unix_ms: at,
        kind,
    })
}

/// A `RequestConfig` naming `units` (pool → unit name).
fn request_config(units: &[(&str, &str)]) -> BuildEventKind {
    BuildEventKind::RequestConfig {
        max_workers: 4,
        fail_fast: false,
        scratch_disabled: false,
        resources: BTreeMap::new(),
        units: units
            .iter()
            .map(|(p, u)| ((*p).to_owned(), (*u).to_owned()))
            .collect(),
    }
}

fn pool_usage(pool: &str, held: u64, capacity: u64, waiting: u64) -> BuildEventKind {
    BuildEventKind::PoolUsage {
        pool: pool.into(),
        held,
        capacity,
        waiting,
        removals_pending: 0,
    }
}

fn execute_start(
    addr: &str,
    declared: Option<&[(&str, u64)]>,
    reserved: Option<&[(&str, u64)]>,
) -> BuildEventKind {
    let map = |m: &[(&str, u64)]| -> BTreeMap<String, u64> {
        m.iter().map(|(p, v)| ((*p).to_owned(), *v)).collect()
    };
    BuildEventKind::ExecuteStart {
        addr: addr.into(),
        driver: "exec".into(),
        cache: true,
        declared: declared.map(map),
        reserved: reserved.map(map),
        queued_ms: None,
        admitted: None,
        output_groups: Vec::new(),
        log_not_streamed: false,
    }
}

/// `records` written and finished at `ended_ms` in a fresh recording; the
/// writer's connection, to query the explorer rows.
fn derived(
    id: SessionId,
    records: &[SessionRecord],
    ended_ms: u64,
) -> (tempfile::TempDir, rusqlite::Connection) {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = session_dir(root.path(), id);
    let mut writer = writer_in(&dir, id);
    for r in records {
        insert(&mut writer, r);
    }
    let conn = writer.finish(ended_ms, Some(0), false).expect("finish");
    (root, conn)
}

const SPANS: &str = "SELECT span_id, parent_span_id, name, class, addr, driver, outcome, \
     start_us - 1700000000000000, end_us - 1700000000000000, duration_us, status, attributes \
     FROM spans ORDER BY span_id";
const EVENTS: &str = "SELECT span_id, name, time_us - 1700000000000000, attributes \
     FROM span_events ORDER BY time_us, rowid";
const METRICS: &str = "SELECT name, kind, unit, time_us - 1700000000000000, value, attributes \
     FROM metrics ORDER BY rowid";

/// I9: the explorer rows of a scripted stream, frozen. The fixture's stream
/// (two targets, phase spans, two pools, a lock holder) gives the root span,
/// the trace id, startup and teardown, each phase with its class, the pool
/// gauges and summaries and the result counters — derived now and as the
/// committed format-1 fixture holds them. A second stream gives every other
/// span-event kind. A change to any of these is a released name changing.
#[test]
fn explorer_golden_rows() {
    let id = SessionId::new(FIXTURE_STARTED_MS, 1);
    assert_eq!(id.encode(), "cbszjpg001");
    // `printf 'heph.trace.v1:%s' cbszjpg001 | shasum -a 256 | cut -c1-32`
    assert_eq!(
        derive::trace_id(&id.encode()),
        "295f593bc1e97948392ff731bc7bd461"
    );
    assert_eq!(
        derive::span_id_hex(derive::SESSION_SPAN),
        "0000000000000001"
    );
    assert_eq!(derive::span_id_hex(300), "000000000000012c");

    let spans = [
        "1|NULL|heph.session|NULL|NULL|NULL|NULL|0|12000|12000|unset|NULL",
        r#"2|1|heph.result|NULL|//p:t|exec|ran|3000|10000|7000|ok|{"heph.admission.queued_ms":1,"heph.resources.declared.cpu":1.0,"heph.resources.reserved.cpu":1.0}"#,
        r#"3|1|heph.result|NULL|//p:dep|NULL|hit|3000|5000|2000|ok|{"heph.cache":"local_hit"}"#,
        "4|2|heph.resolve|heph|//p:t|exec|NULL|3000|4000|1000|unset|NULL",
        "5|2|heph.admission|waiting|//p:t|exec|NULL|5000|6000|1000|unset|NULL",
        "6|2|heph.process|target|//p:t|exec|NULL|6000|9000|3000|unset|NULL",
        "7|2|heph.collect|heph|//p:t|exec|NULL|9000|10000|1000|unset|NULL",
        "8|1|heph.startup|NULL|NULL|NULL|NULL|0|3000|3000|unset|NULL",
        "9|1|heph.teardown|NULL|NULL|NULL|NULL|10000|12000|2000|unset|NULL",
    ];
    let events = [
        "3|heph.cache.local_hit|4000|NULL",
        "2|heph.cache.local_miss|5000|NULL",
        r#"1|heph.lock.holder_seen|8000|{"heph.lock.holder_pid":99}"#,
    ];
    let cpu = r#"{"heph.pool":"cpu"}"#;
    let docker = r#"{"heph.pool":"docker"}"#;
    let metrics = [
        format!("heph.pool.capacity|gauge|{{cpu}}|6000|4.0|{cpu}"),
        format!("heph.pool.held|gauge|{{cpu}}|6000|1.0|{cpu}"),
        format!("heph.pool.waiting|gauge|1|6000|1.0|{cpu}"),
        format!("heph.pool.capacity|gauge|1|6000|2.0|{docker}"),
        format!("heph.pool.held|gauge|1|6000|1.0|{docker}"),
        format!("heph.pool.waiting|gauge|1|6000|1.0|{docker}"),
        format!("heph.pool.capacity|gauge|{{cpu}}|9000|4.0|{cpu}"),
        format!("heph.pool.held|gauge|{{cpu}}|9000|0.0|{cpu}"),
        format!("heph.pool.waiting|gauge|1|9000|0.0|{cpu}"),
        format!("heph.pool.capacity|gauge|1|9000|2.0|{docker}"),
        format!("heph.pool.held|gauge|1|9000|0.0|{docker}"),
        format!("heph.pool.waiting|gauge|1|9000|0.0|{docker}"),
        // Held 1 from 6 ms to 9 ms, then 0 until the end at 12 ms.
        format!("heph.session.pool.held.mean|gauge|{{cpu}}|12000|0.5|{cpu}"),
        format!("heph.session.pool.held.peak|gauge|{{cpu}}|12000|1.0|{cpu}"),
        format!("heph.session.pool.waiting_us|counter|us|12000|3000.0|{cpu}"),
        format!("heph.session.pool.held.mean|gauge|1|12000|0.5|{docker}"),
        format!("heph.session.pool.held.peak|gauge|1|12000|1.0|{docker}"),
        format!("heph.session.pool.waiting_us|counter|us|12000|3000.0|{docker}"),
        r#"heph.session.results|counter|1|12000|1.0|{"heph.outcome":"hit"}"#.to_owned(),
        r#"heph.session.results|counter|1|12000|1.0|{"heph.outcome":"ran"}"#.to_owned(),
    ];

    let root = tempfile::tempdir().expect("tempdir");
    let fresh = session_dir(root.path(), id);
    write_fixture(&fresh);
    let committed_root = tempfile::tempdir().expect("tempdir");
    let committed = session_dir(committed_root.path(), id);
    std::fs::copy(fixture(1), committed.join(store::FILE)).expect("copy the fixture");
    for dir in [&fresh, &committed] {
        let opened = tests::open(dir);
        assert_eq!(dump(&opened.conn, SPANS), spans, "{}", dir.display());
        assert_eq!(dump(&opened.conn, EVENTS), events, "{}", dir.display());
        assert_eq!(dump(&opened.conn, METRICS), metrics, "{}", dir.display());
    }

    // Every other span-event kind, on the target's span or the session's.
    let s = FIXTURE_STARTED_MS;
    let answered = SessionRecordKind::Approval {
        prompt: None,
        standing: None,
    };
    let asked = every_session_kind()
        .into_iter()
        .find(|k| {
            matches!(
                k,
                SessionRecordKind::Approval {
                    prompt: Some(_),
                    ..
                }
            )
        })
        .expect("an asked approval");
    let records = [
        start_record(s + 1, "//q:a"),
        ev(
            s + 2,
            BuildEventKind::RemoteCacheMiss {
                addr: "//q:a".into(),
            },
        ),
        ev(
            s + 3,
            BuildEventKind::RemoteCacheHit {
                addr: "//q:a".into(),
            },
        ),
        ev(
            s + 4,
            BuildEventKind::CacheHitRebuilt {
                addr: "//q:a".into(),
                was: CacheHitKind::Remote,
            },
        ),
        ev(
            s + 5,
            BuildEventKind::CodegenExcluded {
                path: "gen.rs".into(),
                status: "stale".into(),
                reason: Some("released".into()),
                addr: Some("//q:a".into()),
            },
        ),
        ev(
            s + 6,
            BuildEventKind::CodegenExcluded {
                path: "old.rs".into(),
                status: "unknown".into(),
                reason: None,
                addr: None,
            },
        ),
        session_record(s + 7, asked),
        session_record(s + 8, answered),
        session_record(
            s + 9,
            SessionRecordKind::LockHolder {
                pid: 7,
                session: Some("cbszjpg002".into()),
            },
        ),
        end_record(s + 10, "//q:a"),
        session_record(s + 11, SessionRecordKind::Truncated),
        start_record(s + 11, "//q:b"),
        ev(
            s + 11,
            BuildEventKind::ResultEnd {
                addr: "//q:b".into(),
                error: Some("dep failed".into()),
                upstream_of: Some("//q:a".into()),
                exit_status: None,
                log_tail: None,
                timed_out: Some(TimedOutData {
                    timeout_ms: 1000,
                    abandoned: false,
                }),
                cancelled: false,
                left_behind: None,
            },
        ),
    ];
    let (_root, conn) = derived(id, &records, s + 12);
    assert_eq!(
        dump(&conn, EVENTS),
        [
            "2|heph.cache.remote_miss|2000|NULL",
            "2|heph.cache.remote_hit|3000|NULL",
            r#"2|heph.cache.hit_rebuilt|4000|{"heph.cache.was":"remote"}"#,
            r#"2|heph.codegen.excluded|5000|{"heph.codegen.path":"gen.rs","heph.codegen.reason":"released","heph.codegen.status":"stale"}"#,
            r#"1|heph.codegen.excluded|6000|{"heph.codegen.path":"old.rs","heph.codegen.status":"unknown"}"#,
            "1|heph.approval.prompt|7000|NULL",
            "1|heph.approval.cleared|8000|NULL",
            r#"1|heph.lock.holder_seen|9000|{"heph.lock.holder_pid":7,"heph.lock.holder_session":"cbszjpg002"}"#,
            "1|heph.truncated|11000|NULL",
        ]
    );

    // The attribute keys the two streams write are exactly the documented
    // ones: a key the deriver starts writing without a row in the table, or
    // a documented key nothing writes, fails here.
    let committed = tests::open(&committed);
    let mut written: BTreeSet<String> = attribute_keys(&committed.conn);
    written.extend(attribute_keys(&conn));
    assert_eq!(written, documented_attribute_keys());
}

/// Every attribute key in `spans`, `span_events` and `metrics`, with a
/// per-pool suffix written as `<pool>`, as the documentation spells it.
fn attribute_keys(conn: &rusqlite::Connection) -> BTreeSet<String> {
    let sql = "SELECT DISTINCT j.key FROM spans, json_each(spans.attributes) j \
         UNION SELECT DISTINCT j.key FROM span_events, json_each(span_events.attributes) j \
         UNION SELECT DISTINCT j.key FROM metrics, json_each(metrics.attributes) j";
    dump(conn, sql)
        .into_iter()
        .map(|key| {
            ["heph.resources.declared.", "heph.resources.reserved."]
                .into_iter()
                .find(|p| key.starts_with(p))
                .map_or(key.clone(), |p| format!("{p}<pool>"))
        })
        .collect()
}

/// The keys in the first column of docs/SESSIONS.md's "Attribute keys" table.
fn documented_attribute_keys() -> BTreeSet<String> {
    let doc = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/SESSIONS.md");
    let text = std::fs::read_to_string(&doc).expect("read docs/SESSIONS.md");
    let table = text
        .split("#### Attribute keys\n")
        .nth(1)
        .expect("an Attribute keys section in docs/SESSIONS.md");
    let keys: BTreeSet<String> = table
        .lines()
        .skip_while(|l| !l.starts_with('|'))
        .take_while(|l| l.starts_with('|'))
        .skip(2)
        .filter_map(|row| row.split('|').nth(1))
        .flat_map(|cell| cell.split(','))
        .map(|key| key.trim().trim_matches('`').to_owned())
        .collect();
    assert!(keys.len() > 10, "{keys:?}");
    keys
}

/// I5, the store's half: a SQLite file that is not a heph recording, an
/// empty or garbage file, and a heph recording in a newer format each open
/// as what they are, never as a recording. (`list` and cleanup skipping
/// them: `list_shows_newer_format_placeholder`,
/// `cleanup_leaves_unknown_recordings_to_age`.)
#[test]
fn unknown_format_is_skipped() {
    let root = tempfile::tempdir().expect("tempdir");
    let open = |dir: &Path| store::open_local(root.path(), dir);

    let foreign = session_dir(root.path(), now_id(10));
    rusqlite::Connection::open(foreign.join(store::FILE))
        .expect("open")
        .execute_batch("CREATE TABLE t (x); PRAGMA application_id = 7; PRAGMA user_version = 1;")
        .expect("a foreign SQLite file");
    assert!(matches!(open(&foreign), Err(store::OpenError::NotASession)));

    for (random, bytes) in [(11, &b""[..]), (12, &b"not a database at all"[..])] {
        let dir = session_dir(root.path(), now_id(random));
        std::fs::write(dir.join(store::FILE), bytes).expect("write");
        assert!(
            matches!(open(&dir), Err(store::OpenError::NotASession)),
            "{bytes:?}"
        );
    }

    let id = now_id(13);
    let newer = session_dir(root.path(), id);
    let conn = writer_in(&newer, id)
        .finish(hcore::events::now_unix_ms(), Some(0), false)
        .expect("finish");
    assert_eq!(
        store::seal(conn, store::SEAL_TIMEOUT).expect("seal"),
        store::Sealed::Done
    );
    tests::alter(&newer)
        .execute_batch("PRAGMA user_version = 99;")
        .expect("a newer format");
    assert!(
        matches!(open(&newer), Err(store::OpenError::TooNew { found: 99 })),
        "{:?}",
        open(&newer).map(|o| o.is_copy())
    );
    assert!(matches!(
        store::read_meta_in(root.path(), &newer),
        Err(store::OpenError::TooNew { found: 99 })
    ));
}

/// I3: the owner's connection is reopened on the final name, so a viewer
/// attached as soon as the session exists reads each commit, and no `-wal`
/// named after the creation's temp file is ever left beside it.
#[test]
fn viewer_sees_writes_after_rename() {
    let root = tempfile::tempdir().expect("tempdir");
    let rec = recorder_with(root.path(), Faults::default());
    let dir = rec.dir().to_path_buf();
    let tap = rec.tap();
    let opened = tests::open(&dir);
    assert!(opened.live && !opened.is_copy(), "{opened:?}");
    let mut cursor = store::RecordCursor::new(opened);
    let starts = |cursor: &mut store::RecordCursor| -> Vec<String> {
        std::iter::from_fn(|| cursor.next_record().expect("read"))
            .filter_map(|r| match r.kind {
                Some(BuildEventKind::ResultStart { addr }) => Some(addr),
                _ => None,
            })
            .collect()
    };
    for round in 0..4 {
        let want: Vec<String> = (0..=round).map(|i| format!("//v:r{round}_{i}")).collect();
        for addr in &want {
            tap.event(BuildEvent {
                at_unix_ms: hcore::events::now_unix_ms(),
                kind: BuildEventKind::ResultStart { addr: addr.clone() },
            });
        }
        tap.flush(Duration::from_secs(10)).expect("flush");
        assert_eq!(starts(&mut cursor), want, "round {round}");
        let names = listing(&dir);
        assert!(
            !names.iter().any(|n| n.starts_with(".session.db.creating")),
            "{names:?}"
        );
        assert!(names.iter().any(|n| n == "session.db-wal"), "{names:?}");
    }
    rec.recorder.finish(Some(0), false, Duration::from_secs(10));
    let rest: Vec<SessionRecord> =
        std::iter::from_fn(|| cursor.next_record().expect("read")).collect();
    assert!(
        rest.iter()
            .any(|r| matches!(r.session, Some(SessionRecordKind::End { .. }))),
        "the viewer reads End"
    );
    assert_eq!(cursor.skipped(), 0);
}

/// I3: `sealed=1` is in the last WAL transaction, before the mode switch,
/// and nothing writes the file after it: no `-journal` once sealed, even
/// when the process dies right after the switch. A lost-session seal (the
/// re-derivation cleanup does) refuses beside a `-journal`, and over a
/// rollback-mode file with a stray `-wal` writes nothing.
#[test]
fn seal_writes_nothing_after_the_switch() {
    let root = tempfile::tempdir().expect("tempdir");
    let end = |at| {
        session_record(
            at,
            SessionRecordKind::End {
                exit_code: Some(0),
                panicked: false,
            },
        )
    };
    let finished = |random: u8| {
        let id = now_id(random);
        let dir = session_dir(root.path(), id);
        let mut writer = writer_in(&dir, id);
        let at = id.started_unix_ms();
        insert(&mut writer, &start_record(at + 1, "//s:a"));
        insert(&mut writer, &end_record(at + 2, "//s:a"));
        insert(&mut writer, &end(at + 3));
        let conn = writer.finish(at + 4, Some(0), false).expect("finish");
        (dir, conn)
    };

    let (dir, conn) = finished(20);
    assert!(header(&dir).wal, "not switched yet");
    {
        let copy = tests::open(&dir);
        assert!(copy.is_copy(), "a WAL-mode header is read through a copy");
        assert!(
            sealed_column(&copy.conn),
            "sealed=1 is committed in the WAL"
        );
    }
    assert_eq!(
        store::seal(conn, store::SEAL_TIMEOUT).expect("seal"),
        store::Sealed::Done
    );
    assert_eq!(listing(&dir), [files::LOCK, store::FILE]);
    assert!(!header(&dir).wal);

    // The seal's two steps, then a death before the connection closes.
    let (died, conn) = finished(21);
    let busy: i64 = conn
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))
        .expect("checkpoint");
    assert_eq!(busy, 0);
    let mode: String = conn
        .query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))
        .expect("switch");
    assert_eq!(mode, "delete");
    // Never closed, as a process killed here leaves it.
    let _died = std::mem::ManuallyDrop::new(conn);
    let names = listing(&died);
    assert!(!names.iter().any(|n| n.ends_with("-journal")), "{names:?}");
    let opened = tests::open(&died);
    assert!(!opened.is_copy() && sealed_column(&opened.conn));
    drop(opened);

    // Beside a rollback journal, a lost-session seal refuses and writes
    // nothing.
    let id = now_id(22);
    let lost = session_dir(root.path(), id);
    let mut writer = writer_in(&lost, id);
    insert(
        &mut writer,
        &start_record(id.started_unix_ms() + 1, "//s:b"),
    );
    writer.commit().expect("commit");
    drop(writer);
    let path = lost.join(store::FILE);
    std::fs::write(store::sibling(&path, "-journal"), b"").expect("journal");
    let before = std::fs::read(&path).expect("read");
    let err = store::seal_lost(&path).expect_err("refused beside a -journal");
    assert!(format!("{err:#}").contains("journal"), "{err:#}");
    assert_eq!(std::fs::read(&path).expect("read"), before);

    // A sealed file with a stray `-wal`: nothing to do, nothing written.
    let wal = store::sibling(&dir.join(store::FILE), "-wal");
    std::fs::write(&wal, b"stray").expect("stray wal");
    let before = std::fs::read(dir.join(store::FILE)).expect("read");
    assert_eq!(
        store::seal_lost(&dir.join(store::FILE)).expect("seal_lost"),
        store::Sealed::Done
    );
    assert_eq!(std::fs::read(dir.join(store::FILE)).expect("read"), before);
    assert_eq!(std::fs::read(&wal).expect("read"), b"stray");
}

/// D30: pool usage is named `heph.pool.*` with a `heph.pool` attribute, in
/// each pool's unit (micro-CPUs read as CPUs, bytes as `By`, a count as
/// `1`); a pool the `RequestConfig` does not name has its builtin unit;
/// `removals_pending` only on the sandbox pool; an all-hit stream, which
/// admits nothing, has no pool rows at all.
#[test]
fn pool_metrics_named() {
    let id = now_id(30);
    let s = id.started_unix_ms();
    let mut sandbox = pool_usage("sandbox", 1, 4, 0);
    if let BuildEventKind::PoolUsage {
        removals_pending, ..
    } = &mut sandbox
    {
        *removals_pending = 3;
    }
    let records = [
        // `cpu` and `memory` unnamed: an older host.
        ev(
            s + 1,
            request_config(&[("docker", "count"), ("gpu", "cpu")]),
        ),
        ev(s + 2, pool_usage("cpu", 2_500_000, 8_000_000, 1)),
        ev(s + 2, pool_usage("memory", 1 << 30, 4 << 30, 0)),
        ev(s + 2, pool_usage("docker", 1, 2, 0)),
        ev(s + 2, pool_usage("gpu", 500_000, 1_000_000, 0)),
        ev(s + 2, sandbox),
    ];
    let (_root, conn) = derived(id, &records, s + 3);
    let rows = dump(
        &conn,
        "SELECT json_extract(attributes, '$.\"heph.pool\"'), name, kind, unit, value FROM metrics \
         WHERE name LIKE 'heph.pool.%' ORDER BY rowid",
    );
    assert_eq!(
        rows,
        [
            "cpu|heph.pool.capacity|gauge|{cpu}|8.0",
            "cpu|heph.pool.held|gauge|{cpu}|2.5",
            "cpu|heph.pool.waiting|gauge|1|1.0",
            "memory|heph.pool.capacity|gauge|By|4294967296.0",
            "memory|heph.pool.held|gauge|By|1073741824.0",
            "memory|heph.pool.waiting|gauge|1|0.0",
            "docker|heph.pool.capacity|gauge|1|2.0",
            "docker|heph.pool.held|gauge|1|1.0",
            "docker|heph.pool.waiting|gauge|1|0.0",
            "gpu|heph.pool.capacity|gauge|{cpu}|1.0",
            "gpu|heph.pool.held|gauge|{cpu}|0.5",
            "gpu|heph.pool.waiting|gauge|1|0.0",
            "sandbox|heph.pool.capacity|gauge|1|4.0",
            "sandbox|heph.pool.held|gauge|1|1.0",
            "sandbox|heph.pool.waiting|gauge|1|0.0",
            "sandbox|heph.pool.removals_pending|gauge|1|3.0",
        ]
    );

    let all_hit = [
        ev(s + 1, request_config(&[("cpu", "cpu")])),
        start_record(s + 2, "//h:a"),
        ev(
            s + 3,
            BuildEventKind::LocalCacheHit {
                addr: "//h:a".into(),
            },
        ),
        end_record(s + 4, "//h:a"),
    ];
    let (_root, conn) = derived(id, &all_hit, s + 5);
    assert_eq!(
        count_rows(
            &conn,
            "SELECT 1 FROM metrics WHERE name LIKE 'heph.pool.%' OR name LIKE 'heph.session.pool.%'"
        ),
        0
    );
    assert!(store::read_pools(&conn).expect("read pools").is_empty());
}

/// D30: a target that ran carries what it declared and what it was granted
/// as `heph.resources.declared.*` / `heph.resources.reserved.*`, in each
/// pool's unit; a grant clamped below the declaration shows both. An older
/// host's `ExecuteStart` (no resources) and a cache hit carry none.
#[test]
fn result_span_carries_declared_and_reserved() {
    let id = now_id(31);
    let s = id.started_unix_ms();
    let gib = 1u64 << 30;
    let records = [
        ev(
            s + 1,
            request_config(&[("cpu", "cpu"), ("memory", "bytes")]),
        ),
        start_record(s + 2, "//r:granted"),
        ev(
            s + 3,
            execute_start(
                "//r:granted",
                Some(&[("cpu", 4_000_000)]),
                Some(&[("cpu", 2_000_000), ("memory", gib)]),
            ),
        ),
        end_record(s + 4, "//r:granted"),
        start_record(s + 2, "//r:old"),
        ev(s + 3, execute_start("//r:old", None, None)),
        end_record(s + 4, "//r:old"),
        start_record(s + 2, "//r:hit"),
        ev(
            s + 3,
            BuildEventKind::LocalCacheHit {
                addr: "//r:hit".into(),
            },
        ),
        end_record(s + 4, "//r:hit"),
    ];
    let (_root, conn) = derived(id, &records, s + 5);
    let attrs = |addr: &str| -> BTreeMap<String, serde_json::Value> {
        let text: Option<String> = conn
            .query_row(
                "SELECT attributes FROM spans WHERE name = 'heph.result' AND addr = ?1",
                [addr],
                |r| r.get(0),
            )
            .expect(addr);
        let all: BTreeMap<String, serde_json::Value> =
            text.map_or_else(BTreeMap::new, |t| serde_json::from_str(&t).expect("json"));
        all.into_iter()
            .filter(|(k, _)| k.starts_with("heph.resources."))
            .collect()
    };
    assert_eq!(
        attrs("//r:granted"),
        BTreeMap::from([
            ("heph.resources.declared.cpu".to_owned(), 4.0.into()),
            ("heph.resources.reserved.cpu".to_owned(), 2.0.into()),
            (
                "heph.resources.reserved.memory".to_owned(),
                (gib as f64).into()
            ),
        ])
    );
    assert_eq!(attrs("//r:old"), BTreeMap::new());
    assert_eq!(attrs("//r:hit"), BTreeMap::new());
}

/// Two overlapping results of one addr (two scopes resolving `//p:x` at
/// once, one run between them), frozen: each `ResultEnd` closes the oldest
/// open result (FIFO); the run's attributes, its cache events and its phase
/// spans go on the oldest; the waiter reads `ran` with the driver and no
/// resource attributes; `heph.session.results` counts the target once; and
/// `targets` gives one row.
#[test]
fn overlapping_results_of_one_addr() {
    let id = SessionId::new(FIXTURE_STARTED_MS, 2);
    let s = FIXTURE_STARTED_MS;
    let mut records = vec![
        ev(s, request_config(&[("cpu", "cpu")])),
        start_record(s + 1, "//p:x"),
        start_record(s + 2, "//p:x"),
        ev(
            s + 3,
            BuildEventKind::LocalCacheMiss {
                addr: "//p:x".into(),
            },
        ),
        ev(
            s + 4,
            execute_start(
                "//p:x",
                Some(&[("cpu", 1_000_000)]),
                Some(&[("cpu", 1_000_000)]),
            ),
        ),
    ];
    let root = tempfile::tempdir().expect("tempdir");
    let dir = session_dir(root.path(), id);
    let mut writer = writer_in(&dir, id);
    for r in records.drain(..) {
        insert(&mut writer, &r);
    }
    // The first ResultEnd carries the run's phase spans.
    let end = end_record(s + 7, "//p:x");
    let mut body = serde_json::to_value(&end).expect("encode");
    let spans = serde_json::json!([
        {"name": "heph.process", "start_unix_us": (s + 4) * 1000, "end_unix_us": (s + 6) * 1000},
    ]);
    body.get_mut("kind")
        .and_then(serde_json::Value::as_object_mut)
        .expect("a build event")
        .insert("spans".into(), spans);
    writer
        .insert(&end, &body.to_string())
        .expect("insert ResultEnd");
    insert(&mut writer, &end_record(s + 8, "//p:x"));
    let conn = writer.finish(s + 9, Some(0), false).expect("finish");

    let result_spans = "SELECT span_id, parent_span_id, name, addr, driver, outcome, \
         start_us - 1700000000000000, end_us - 1700000000000000, attributes \
         FROM spans WHERE name NOT IN ('heph.session', 'heph.startup', 'heph.teardown') \
         ORDER BY span_id";
    assert_eq!(
        dump(&conn, result_spans),
        [
            r#"2|1|heph.result|//p:x|exec|ran|1000|7000|{"heph.resources.declared.cpu":1.0,"heph.resources.reserved.cpu":1.0}"#,
            "3|1|heph.result|//p:x|exec|ran|2000|8000|NULL",
            "4|2|heph.process|//p:x|exec|NULL|4000|6000|NULL",
        ]
    );
    assert_eq!(
        dump(
            &conn,
            "SELECT span_id, name FROM span_events ORDER BY time_us, rowid"
        ),
        ["2|heph.cache.local_miss"]
    );
    assert_eq!(
        dump(
            &conn,
            "SELECT value, attributes FROM metrics WHERE name = 'heph.session.results'"
        ),
        [r#"1.0|{"heph.outcome":"ran"}"#]
    );
    assert_eq!(
        dump(&conn, "SELECT addr, outcome, duration_us FROM targets"),
        ["//p:x|ran|6000"]
    );
}

/// D30, the recording's half: per pool, a time-weighted held mean, the held
/// peak and the time anything waited, written at the end and read back by
/// [`store::read_pools`]. A value holds until the next row, and the last one
/// until the end. (The catalog's copy is PR 3.)
#[test]
fn pool_summaries() {
    let id = now_id(32);
    let s = id.started_unix_ms();
    let records = [
        ev(
            s + 1,
            request_config(&[("cpu", "cpu"), ("docker", "count")]),
        ),
        // cpu: 2 for 10 ms (one waiting), 4 for 20 ms, 1 for 60 ms.
        ev(s + 10, pool_usage("cpu", 2_000_000, 8_000_000, 1)),
        ev(s + 20, pool_usage("cpu", 4_000_000, 8_000_000, 0)),
        ev(s + 40, pool_usage("cpu", 1_000_000, 8_000_000, 0)),
        // docker: one row, holding until the end.
        ev(s + 50, pool_usage("docker", 2, 3, 2)),
    ];
    let (_root, conn) = derived(id, &records, s + 100);
    let pools = store::read_pools(&conn).expect("read pools");
    let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
    let [cpu, docker] = pools.as_slice() else {
        panic!("two pools: {pools:?}");
    };
    assert_eq!((cpu.pool.as_str(), cpu.unit.as_str()), ("cpu", "{cpu}"));
    assert_eq!(cpu.capacity, Some(8.0));
    assert!(
        close(cpu.held_mean, (2.0 * 10.0 + 4.0 * 20.0 + 60.0) / 90.0),
        "{cpu:?}"
    );
    assert!(close(cpu.held_peak, 4.0), "{cpu:?}");
    assert_eq!(cpu.waiting_us, 10_000, "{cpu:?}");
    assert_eq!(
        (docker.pool.as_str(), docker.unit.as_str()),
        ("docker", "1")
    );
    assert_eq!(docker.capacity, Some(3.0));
    assert!(close(docker.held_mean, 2.0), "{docker:?}");
    assert!(close(docker.held_peak, 2.0), "{docker:?}");
    assert_eq!(docker.waiting_us, 50_000, "{docker:?}");
}

/// O5: who ran the session, from the environment alone. An agent first
/// (it may run in CI), then CI, then a human; only the exact values the
/// tools set count, and a near-miss name counts for nothing.
#[test]
fn actor_from_env() {
    let gh_url = [
        ("GITHUB_SERVER_URL", "https://github.com"),
        ("GITHUB_REPOSITORY", "o/r"),
        ("GITHUB_RUN_ID", "42"),
    ];
    let url = Some("https://github.com/o/r/actions/runs/42");
    /// The environment, then the expected kind, name and run URL.
    type Case<'a> = (
        &'a [(&'a str, &'a str)],
        &'a str,
        Option<&'a str>,
        Option<&'a str>,
    );
    let cases: &[Case<'_>] = &[
        (&[], "human", None, None),
        (&[("CLAUDECODE", "1")], "agent", Some("claude-code"), None),
        (
            &[("GITHUB_ACTIONS", "true")],
            "ci",
            Some("github-actions"),
            None,
        ),
        (&[("CI", "1")], "ci", Some("unknown"), None),
        (&[("CI", "true")], "ci", Some("unknown"), None),
        (&[("CI", "false")], "human", None, None),
        (&[("CI", "FALSE")], "human", None, None),
        (&[("CI", "0")], "human", None, None),
        (&[("CI", " 0 ")], "human", None, None),
        (&[("CI", "")], "human", None, None),
        (&[("CI", "no")], "human", None, None),
        // GitHub Actions also sets CI: the named CI wins.
        (
            &[("GITHUB_ACTIONS", "true"), ("CI", "true")],
            "ci",
            Some("github-actions"),
            None,
        ),
        // Near misses.
        (&[("CLAUDECODE", "0")], "human", None, None),
        (&[("CLAUDECODE", "")], "human", None, None),
        (&[("CLAUDECODE_X", "1")], "human", None, None),
        (&[("claudecode", "1")], "human", None, None),
        (&[("GITHUB_ACTIONS", "1")], "human", None, None),
        (&[("GITHUB_ACTIONS", "false")], "human", None, None),
        (&[("GITHUB_ACTIONSX", "true")], "human", None, None),
        // The run URL only on GitHub Actions, and only when whole.
        (&gh_url, "human", None, None),
        (&gh_url[..2], "human", None, None),
    ];
    for (env, kind, name, ci_run_url) in cases {
        let actor = store::Actor::from_env(env.iter().map(|(k, v)| ((*k).into(), (*v).into())));
        assert_eq!(
            (actor.kind, actor.name, actor.ci_run_url.as_deref()),
            (*kind, *name, *ci_run_url),
            "{env:?}"
        );
    }

    let with = |extra: &[(&str, &str)]| {
        store::Actor::from_env(
            gh_url
                .iter()
                .chain(extra)
                .map(|(k, v)| ((*k).into(), (*v).into())),
        )
    };
    let gh = with(&[("GITHUB_ACTIONS", "true")]);
    assert_eq!(
        (gh.kind, gh.name, gh.ci_run_url.as_deref()),
        ("ci", Some("github-actions"), url)
    );
    let agent_in_ci = with(&[("GITHUB_ACTIONS", "true"), ("CLAUDECODE", "1")]);
    assert_eq!(
        (
            agent_in_ci.kind,
            agent_in_ci.name,
            agent_in_ci.ci_run_url.as_deref()
        ),
        ("agent", Some("claude-code"), url)
    );
    let partial = store::Actor::from_env(
        gh_url[..2]
            .iter()
            .chain(&[("GITHUB_ACTIONS", "true")])
            .map(|(k, v)| ((*k).into(), (*v).into())),
    );
    assert_eq!((partial.kind, partial.ci_run_url), ("ci", None));
    let unknown_ci = with(&[("CI", "1")]);
    assert_eq!(unknown_ci.ci_run_url, None, "no URL off GitHub Actions");

    // Only empty, `0`, `false` and `no` (trimmed, any case) say "not CI";
    // every other value is CI, including ones that read as "no" to a human.
    for v in ["off", "n", "OFF", "yes", "\t", "\u{1b}[0m", "\u{0}"] {
        let actor = store::Actor::from_env([("CI".into(), v.into())]);
        let expect = if v.trim().is_empty() { "human" } else { "ci" };
        assert_eq!(actor.kind, expect, "CI={v:?}");
    }

    // The run URL is whole or nothing: a missing, empty or control-byte
    // part drops it; a long part is kept whole.
    let gh_with = |server: &str, repo: &str, run: &str| {
        store::Actor::from_env(
            [
                ("GITHUB_ACTIONS", "true"),
                ("GITHUB_SERVER_URL", server),
                ("GITHUB_REPOSITORY", repo),
                ("GITHUB_RUN_ID", run),
            ]
            .map(|(k, v)| (k.to_owned(), v.to_owned())),
        )
    };
    for (server, repo, run) in [
        ("", "o/r", "42"),
        ("https://github.com", "", "42"),
        ("https://github.com", "o/r", ""),
        ("https://github.com\u{1b}]8;;x\u{7}", "o/r", "42"),
        ("https://github.com", "o/r\n", "42"),
        ("https://github.com", "o/r", "4\u{0}2"),
        ("https://github.com", "o/r", "42\u{7f}"),
    ] {
        let actor = gh_with(server, repo, run);
        assert_eq!(
            (actor.kind, actor.name, actor.ci_run_url),
            ("ci", Some("github-actions"), None),
            "{server:?} {repo:?} {run:?}"
        );
    }
    let long_repo = format!("o/{}", "r".repeat(64 * 1024));
    let long = gh_with("https://github.com", &long_repo, "42");
    assert_eq!(
        long.ci_run_url,
        Some(format!("https://github.com/{long_repo}/actions/runs/42")),
        "a long part is kept whole"
    );
    let unicode = gh_with("https://gïthub.example", "ö/r", "42");
    assert_eq!(
        unicode.ci_run_url.as_deref(),
        Some("https://gïthub.example/ö/r/actions/runs/42"),
        "non-ASCII is not a control byte"
    );
}

/// O5, `Provenance::detect`'s filter: a variable whose name or value is not
/// UTF-8 is skipped whole, never decoded lossily into a match.
#[test]
fn actor_env_skips_non_utf8() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let bad = |b: &[u8]| OsString::from_vec(b.to_vec());
    let env = [
        (OsString::from("CLAUDECODE"), bad(b"1\xff")),
        (bad(b"GITHUB_ACTIONS\xff"), OsString::from("true")),
        (OsString::from("CI"), bad(b"\xfe")),
        (OsString::from("USER_OK"), OsString::from("x")),
    ];
    let kept: Vec<(String, String)> = store::utf8_env(env.clone()).collect();
    assert_eq!(kept, [("USER_OK".to_owned(), "x".to_owned())]);
    let actor = store::Actor::from_env(store::utf8_env(env));
    assert_eq!((actor.kind, actor.name), ("human", None));
}
