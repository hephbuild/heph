//! `heph session query`'s engine (D20): read-only by construction, the same
//! SQL over every source, and a copy of only what a statement reads.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use rusqlite::types::Value;

use super::catalog_tests::sealed;
use super::export::{Abort, write_file};
use super::id::SessionId;
use super::query::{self, Answer, Limits, QueryError, Scope, Source};
use super::recording::Recording;
use super::{catalog, store, testing};

const INDEXES: [&str; 4] = [
    "records_kind",
    "spans_name",
    "span_events_span",
    "metrics_name",
];

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis(),
    )
    .expect("ms")
}

/// A sealed recording under `root`, started `ago_ms` before now.
fn recording(root: &Path, n: u8, ago_ms: u64) -> PathBuf {
    sealed(root, SessionId::new(now_ms() - ago_ms, n))
}

fn local(root: &Path, dir: &Path) -> Source {
    Source::Recording(query::RecordingFile::local(root, dir, &|_| {}).expect("local"))
}

fn foreign(path: &Path, tmp: &Path) -> Source {
    let rec = Recording::open_foreign(path, tmp).expect("open foreign");
    Source::Recording(query::RecordingFile::foreign(rec).expect("foreign"))
}

fn catalog_of(root: &Path, scope: Scope) -> Source {
    Source::Catalog {
        catalog: catalog::open(root, now_ms()).expect("catalog"),
        scope,
    }
}

fn run(sql: &str, sources: Vec<Source>, tmp: &Path) -> Result<Answer, QueryError> {
    query::run(sql, sources, tmp, &Limits::default())
}

fn rows(sql: &str, sources: Vec<Source>, tmp: &Path) -> Vec<Vec<Value>> {
    run(sql, sources, tmp)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Integer(n) => *n,
        other => panic!("not an integer: {other:?}"),
    }
}

/// Every file under `dir`, with its bytes and mtime.
fn files(dir: &Path) -> BTreeMap<PathBuf, (Vec<u8>, SystemTime)> {
    let mut out = BTreeMap::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).expect("read dir") {
            let path = e.expect("entry").path();
            let meta = std::fs::symlink_metadata(&path).expect("stat");
            if meta.is_dir() {
                todo.push(path);
            } else {
                let bytes = std::fs::read(&path).expect("read");
                out.insert(path, (bytes, meta.modified().expect("mtime")));
            }
        }
    }
    out
}

fn indexes(file: &Path) -> Vec<String> {
    let conn = Connection::open_with_flags(file, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open");
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'index' AND sql IS NOT NULL")
        .expect("prepare");
    let mut names: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<rusqlite::Result<_>>()
        .expect("rows");
    names.sort();
    names
}

/// A copy of `dir`'s recording at `dest`, as a file someone sent.
fn copy_out(dir: &Path, dest: &Path) -> PathBuf {
    std::fs::copy(dir.join(store::FILE), dest).expect("copy");
    dest.to_path_buf()
}

fn assert_sql(res: Result<Answer, QueryError>, what: &str, contains: &str) {
    match res {
        Err(QueryError::Sql(msg)) => assert!(
            msg.contains(contains),
            "{what}: {msg:?} does not say {contains:?}"
        ),
        Err(e) => panic!("{what}: not a sql error: {e}"),
        Ok(a) => panic!("{what}: answered {:?}", a.rows),
    }
}

/// `query_stays_read_only` (I3): every row of the spec's authorizer table,
/// in place (one source), copied (two) and over the empty schema; no
/// source changes bytes or mtime, and no file is created.
#[test]
fn query_stays_read_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let qtmp = tmp.path().join("qtmp");
    let a = recording(&root, 1, 2_000);
    let b = recording(&root, 2, 1_000);
    let vacuum = tmp.path().join("x.db");
    let refused: &[(&str, &str)] = &[
        ("INSERT INTO spans (name) VALUES ('x')", "only reads"),
        ("SELECT 1; DELETE FROM spans", "more than one statement"),
        ("", "no statement"),
        ("   ", "no statement"),
        ("-- only a comment", "no statement"),
        ("/* a comment */", "no statement"),
        ("ATTACH 'x.db' AS x", "not authorized"),
        ("DETACH main", "not authorized"),
        ("PRAGMA query_only=0", "not authorized"),
        ("PRAGMA writable_schema=1", "not authorized"),
        ("PRAGMA journal_mode=DELETE", "not authorized"),
        ("CREATE TEMP TABLE t(x)", "not authorized"),
        ("CREATE TEMP VIEW spans AS SELECT 1", "not authorized"),
        ("DROP VIEW spans", "not authorized"),
        ("BEGIN", "not authorized"),
        ("SAVEPOINT a", "not authorized"),
        ("SELECT load_extension('x')", "not authorized"),
        ("SELECT readfile('/etc/passwd')", ""),
        ("SELECT * FROM main.session", ""),
        ("SELECT * FROM sqlite_schema", ""),
        // A CTE named like one of heph's views is not one.
        (
            "WITH spans AS (SELECT * FROM main.spans) SELECT * FROM spans",
            "",
        ),
        ("SELECT randomblob(1000000000)", "too big"),
        (
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r) SELECT * FROM r",
            "more than 100000 rows",
        ),
    ];
    let vacuum_sql = format!("VACUUM INTO '{}'", vacuum.display());
    let before = files(&root);
    for (mode, sources) in [("empty", 0), ("in place", 1), ("copied", 2)] {
        let mut checks: Vec<(&str, &str)> = refused.to_vec();
        checks.push((&vacuum_sql, ""));
        for (sql, says) in checks {
            // Empty and copied, `main.spans` is the explorer's own table.
            if sql.starts_with("WITH spans") && sources != 1 {
                continue;
            }
            let srcs: Vec<Source> = [&a, &b]
                .into_iter()
                .take(sources)
                .map(|d| foreign(&d.join(store::FILE), &qtmp))
                .collect();
            let res = run(sql, srcs, &qtmp);
            assert!(res.is_err(), "{mode}: {sql:?} answered");
            if !says.is_empty() {
                assert_sql(res, &format!("{mode}: {sql:?}"), says);
            }
        }
        let info = run(
            "PRAGMA table_info(spans)",
            [&a, &b]
                .into_iter()
                .take(sources)
                .map(|d| foreign(&d.join(store::FILE), &qtmp))
                .collect(),
            &qtmp,
        )
        .unwrap_or_else(|e| panic!("{mode}: table_info: {e}"));
        assert!(!info.rows.is_empty(), "{mode}: table_info lists columns");
        let trailing = run(
            "SELECT count(*) FROM spans; -- done",
            [&a, &b]
                .into_iter()
                .take(sources)
                .map(|d| foreign(&d.join(store::FILE), &qtmp))
                .collect(),
            &qtmp,
        );
        assert!(
            trailing.is_ok(),
            "{mode}: a trailing comment is one statement"
        );
    }
    assert!(!vacuum.exists(), "VACUUM INTO created a file");
    assert_eq!(files(&root), before, "a source changed");
    assert!(
        std::fs::read_dir(&qtmp).map_or(true, |mut d| d.next().is_none()),
        "the query left a temp file"
    );
    // The recursive CTE with no row cap runs to the timeout instead.
    let limits = Limits {
        rows: None,
        timeout: Duration::from_millis(200),
        ..Limits::default()
    };
    let res = query::run(
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r) SELECT * FROM r",
        vec![foreign(&a.join(store::FILE), &qtmp)],
        &qtmp,
        &limits,
    );
    assert_sql(res, "--limit 0", "--timeout");
}

/// A foreign file whose stored `logs` view is replaced: heph's own view
/// answers, and the stored one is never read.
#[test]
fn query_never_runs_a_stored_view() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let dir = recording(&root, 1, 1_000);
    let honest = rows(
        "SELECT * FROM logs ORDER BY seq",
        vec![local(&root, &dir)],
        tmp.path(),
    );
    let evil = tmp.path().join("evil");
    std::fs::create_dir_all(&evil).expect("mkdir");
    copy_out(&dir, &evil.join(store::FILE));
    testing::alter(
        &evil,
        "DROP VIEW logs;
         CREATE VIEW logs AS SELECT 'evil' AS session_id, 0 AS seq, 0 AS time_us,
           'info' AS severity, hex(randomblob(8)) AS body;",
    )
    .expect("replace the stored view");
    let file = evil.join(store::FILE);
    let before = std::fs::read(&file).expect("read");
    let qtmp = tmp.path().join("qtmp");
    assert_eq!(
        rows(
            "SELECT * FROM logs ORDER BY seq",
            vec![foreign(&file, &qtmp)],
            &qtmp
        ),
        honest,
        "heph's view answers"
    );
    assert!(
        run(
            "SELECT * FROM main.logs",
            vec![foreign(&file, &qtmp)],
            &qtmp
        )
        .is_err(),
        "the stored view is refused"
    );
    assert_eq!(std::fs::read(&file).expect("read"), before);
}

/// Heph's in-place view of each name answers what the recording's stored
/// views (heph's own `store::SCHEMA`) answer: the two derivations of the
/// recording never drift apart.
#[test]
fn query_views_match_the_stored_views() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let dir = recording(&root, 1, 1_000);
    // A log line and an output line, so `logs` (json_extract and lower in
    // a TEMP view, trusted_schema off) has rows.
    testing::alter(
        &dir,
        r#"INSERT INTO records (seq, at_ms, kind, body) VALUES
             (100000, 7, 'Log', '{"session":{"level":"WARN","msg":"careful"}}'),
             (100001, 8, 'Output', '{"session":{"text":"made it"}}');"#,
    )
    .expect("add logs");
    let stored = Connection::open_with_flags(
        dir.join(store::FILE),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open");
    for t in query::TABLES {
        let cols: Vec<&str> = t.columns.iter().map(|(c, _)| *c).collect();
        let sql = format!(
            "SELECT {} FROM {} ORDER BY {}",
            cols.join(", "),
            t.name,
            (1..=cols.len())
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut stmt = stored.prepare(&sql).expect(&sql);
        let want: Vec<Vec<Value>> = stmt
            .query_map([], |r| (0..cols.len()).map(|i| r.get(i)).collect())
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("rows");
        let got = rows(&sql, vec![local(&root, &dir)], tmp.path());
        assert_eq!(got, want, "{}", t.name);
        if matches!(t.name, "sessions" | "spans" | "targets" | "logs") {
            assert!(!got.is_empty(), "{} has rows to compare", t.name);
        }
    }
}

/// `query_same_sql_every_source` (I9): one statement answers the same over
/// a recording, its `.hephsession` and the catalog, for the spans of the
/// targets that ran.
#[test]
fn query_same_sql_every_source() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let qtmp = tmp.path().join("qtmp");
    let dir = recording(&root, 1, 1_000);
    let exported = tmp.path().join("run.hephsession");
    write_file(&dir.join(store::FILE), &exported, false, &Abort::default()).expect("export");
    let sqls = [
        "SELECT session_id, started_ms, ended_ms, exit_code, recorded FROM sessions",
        "SELECT addr, outcome, duration_us FROM targets ORDER BY addr",
        "SELECT s.name, s.addr, s.outcome, s.duration_us FROM spans s
         JOIN targets t ON t.addr = s.addr AND t.outcome <> 'hit'
         WHERE s.name = 'heph.result' ORDER BY s.span_id",
        "SELECT c.name, c.start_us, c.end_us FROM spans c JOIN spans p ON c.parent_span_id = p.span_id
         WHERE p.name = 'heph.result' AND p.outcome <> 'hit' ORDER BY c.span_id",
    ];
    for sql in sqls {
        let want = rows(sql, vec![local(&root, &dir)], &qtmp);
        assert!(!want.is_empty(), "{sql}: rows to compare");
        assert_eq!(
            rows(sql, vec![foreign(&exported, &qtmp)], &qtmp),
            want,
            ".hephsession: {sql}"
        );
        assert_eq!(
            rows(sql, vec![catalog_of(&root, Scope::default())], &qtmp),
            want,
            "catalog: {sql}"
        );
    }
}

/// `query_over_many_sources` (D20): one statement over 50 recordings
/// answers every row, columns in the statement's order, coverage counted;
/// the catalog in place answers the same count.
#[test]
fn query_over_many_sources() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let qtmp = tmp.path().join("qtmp");
    let dirs: Vec<PathBuf> = (0..50u8)
        .map(|n| recording(&root, n, 10_000 + u64::from(n) * 1_000))
        .collect();
    let one = rows(
        "SELECT count(*) FROM spans",
        vec![local(&root, &dirs[0])],
        &qtmp,
    );
    let per = int(&one[0][0]);
    let sources = || dirs.iter().map(|d| local(&root, d)).collect::<Vec<_>>();
    let a = run(
        "SELECT name, session_id, span_id FROM spans ORDER BY session_id, span_id",
        sources(),
        &qtmp,
    )
    .expect("query");
    assert_eq!(a.columns, ["name", "session_id", "span_id"]);
    assert_eq!(a.types.len(), 3);
    assert_eq!(a.rows.len(), usize::try_from(per * 50).expect("count"));
    assert_eq!(a.coverage.recordings, 50);
    assert!(!a.in_place);
    let sessions = "SELECT count(DISTINCT session_id) FROM targets";
    assert_eq!(int(&rows(sessions, sources(), &qtmp)[0][0]), 50);
    assert_eq!(
        int(&rows(sessions, vec![catalog_of(&root, Scope::default())], &qtmp)[0][0]),
        50
    );
}

/// `query_copies_only_what_it_reads` (D20): a statement reading only
/// `spans` copies only `spans`; the catalog answers only inside its window;
/// one source is read in place; a stopped query leaves no temp database.
#[test]
fn query_copies_only_what_it_reads() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let qtmp = tmp.path().join("qtmp");
    let old = recording(&root, 1, 60_000);
    let new = recording(&root, 2, 1_000);
    let a = run(
        "SELECT count(*) FROM spans",
        vec![local(&root, &old), local(&root, &new)],
        &qtmp,
    )
    .expect("copied");
    assert_eq!(a.copied, ["spans"]);
    assert!(!a.in_place);
    let one = run(
        "SELECT count(*) FROM spans",
        vec![local(&root, &new)],
        &qtmp,
    )
    .expect("one");
    assert!(one.in_place);
    assert!(one.copied.is_empty());

    let new_id = rows(
        "SELECT session_id FROM sessions",
        vec![local(&root, &new)],
        &qtmp,
    )
    .remove(0)
    .remove(0);
    let by_session = "SELECT session_id, count(*) FROM spans GROUP BY 1 ORDER BY 1";
    let all = rows(by_session, vec![catalog_of(&root, Scope::default())], &qtmp);
    assert_eq!(all.len(), 2);
    let window = Scope {
        since_ms: Some(now_ms() - 30_000),
        ..Scope::default()
    };
    let within = run(by_session, vec![catalog_of(&root, window)], &qtmp).expect("window");
    assert_eq!(within.rows.len(), 1);
    assert_eq!(within.rows[0][0], new_id);
    assert_eq!(
        Some(&within.rows[0]),
        all.iter().find(|r| r[0] == new_id),
        "every row of a session in the window"
    );
    assert_eq!(within.coverage.catalog, ["spans"]);

    // SIGINT sets `stop`: the query ends Interrupted and its temp database
    // is gone.
    let limits = Limits {
        rows: None,
        ..Limits::default()
    };
    let stop = Arc::clone(&limits.stop);
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        stop.store(true, Ordering::Relaxed);
    });
    let res = query::run(
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r) \
         SELECT count(*) FROM r, spans",
        vec![local(&root, &old), local(&root, &new)],
        &qtmp,
        &limits,
    );
    stopper.join().expect("stopper");
    assert!(
        matches!(res, Err(QueryError::Interrupted)),
        "{:?}",
        res.map(|a| a.rows)
    );
    assert_eq!(
        std::fs::read_dir(&qtmp).expect("tmp").count(),
        0,
        "the temp database is gone"
    );
}

/// `indexes_built_on_first_need`, the query half: a seal builds no index;
/// the first query on a local sealed recording builds them (under its
/// lock); a foreign file is queried without, and never written.
#[test]
fn indexes_built_on_first_query() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let qtmp = tmp.path().join("qtmp");
    let dir = recording(&root, 1, 1_000);
    let file = dir.join(store::FILE);
    assert_eq!(indexes(&file), Vec::<String>::new(), "a seal builds none");

    let sent = copy_out(&dir, &tmp.path().join("sent.db"));
    let before = std::fs::read(&sent).expect("read");
    rows(
        "SELECT count(*) FROM spans",
        vec![foreign(&sent, &qtmp)],
        &qtmp,
    );
    assert_eq!(std::fs::read(&sent).expect("read"), before);
    assert_eq!(
        indexes(&sent),
        Vec::<String>::new(),
        "a foreign file gets none"
    );

    rows(
        "SELECT count(*) FROM spans",
        vec![local(&root, &dir)],
        &qtmp,
    );
    let mut want: Vec<String> = INDEXES.iter().map(|s| (*s).to_owned()).collect();
    want.sort();
    assert_eq!(indexes(&file), want, "the first query builds them");
}

/// A foreign recording an older heph derived is re-derived into a temp
/// copy: the query reads the re-derived spans, the file is unchanged.
#[test]
fn query_rederives_an_older_foreign_recording() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let qtmp = tmp.path().join("qtmp");
    let dir = recording(&root, 1, 1_000);
    let spans = "SELECT name, addr, outcome, start_us, end_us FROM spans ORDER BY span_id";
    let want = rows(spans, vec![local(&root, &dir)], &qtmp);
    assert!(!want.is_empty());
    let old = tmp.path().join("old");
    std::fs::create_dir_all(&old).expect("mkdir");
    let file = copy_out(&dir, &old.join(store::FILE));
    testing::alter(
        &old,
        &format!(
            "UPDATE session SET derivation = {}; DELETE FROM span_events; DELETE FROM spans;",
            store::STALE_DERIVATION
        ),
    )
    .expect("age it");
    let before = std::fs::read(&file).expect("read");
    assert_eq!(rows(spans, vec![foreign(&file, &qtmp)], &qtmp), want);
    assert_eq!(std::fs::read(&file).expect("read"), before);
}

/// A refused pragma never takes effect. `soft_heap_limit` acts when its
/// statement is prepared, process-wide; a refused statement is prepared a
/// second time to tell "more than one statement" apart, alone or as the
/// second statement, and that prepare must not run it either.
#[test]
fn a_refused_pragma_takes_no_effect_when_prepared() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let qtmp = tmp.path().join("qtmp");
    let probe = Connection::open_in_memory().expect("probe");
    let limit = || -> i64 {
        probe
            .query_row("PRAGMA soft_heap_limit", [], |r| r.get(0))
            .expect("soft_heap_limit")
    };
    let before = limit();
    // 1 TiB and change: distinctive, and harmless if it does take effect.
    let set = (1_i64 << 40) + 12_345;
    for sql in [
        format!("PRAGMA soft_heap_limit = {set}"),
        format!("SELECT 1; PRAGMA soft_heap_limit = {set}"),
    ] {
        let res = run(&sql, Vec::new(), &qtmp);
        let after = limit();
        probe
            .pragma_update(None, "soft_heap_limit", before)
            .expect("restore");
        assert!(res.is_err(), "{sql}: answered");
        assert_eq!(after, before, "{sql}: the refused pragma took effect");
    }
}

/// `json_each` and `json_tree` over an `attributes` column read like the
/// explorer tables they walk, the same over every kind of source.
#[test]
fn query_walks_attributes_with_json_each() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let qtmp = tmp.path().join("qtmp");
    let dir = recording(&root, 1, 2_000);
    let other = recording(&root, 2, 1_000);
    for sql in [
        "SELECT j.key, j.value FROM sessions s, json_each(json_array(s.session_id, s.started_ms)) j \
         ORDER BY s.started_ms, j.key",
        "SELECT count(*), group_concat(j.key) FROM spans s, json_each(s.attributes) j",
        "SELECT count(*) FROM spans s, main.json_tree(s.attributes) j",
    ] {
        let one = rows(sql, vec![local(&root, &dir)], &qtmp);
        assert!(!one.is_empty(), "{sql}: rows");
        let both = rows(sql, vec![local(&root, &dir), local(&root, &other)], &qtmp);
        assert!(!both.is_empty(), "{sql}: rows copied");
        assert_eq!(
            rows(sql, vec![catalog_of(&root, Scope::default())], &qtmp),
            both,
            "catalog: {sql}"
        );
    }
    // A recording holding its own `json_each` would be read in its place:
    // there, a walk is refused like any other name.
    let shadowed = tmp.path().join("shadowed");
    std::fs::create_dir_all(&shadowed).expect("mkdir");
    let file = copy_out(&dir, &shadowed.join(store::FILE));
    testing::alter(
        &shadowed,
        "CREATE TABLE json_each (key, value); INSERT INTO json_each VALUES ('secret', 1);",
    )
    .expect("shadow json_each");
    assert_sql(
        run(
            "SELECT key FROM json_each",
            vec![foreign(&file, &qtmp)],
            &qtmp,
        ),
        "a shadowed json_each",
        "prohibited",
    );
}

/// A session an owner wrote over the catalog's cap has no spans, span
/// events or metrics there: a query over the catalog that reads them says
/// so in `coverage.partial`, rather than answering a silent empty.
#[test]
fn a_query_over_a_capped_session_says_it_is_partial() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let qtmp = tmp.path().join("qtmp");
    let kept = recording(&root, 1, 2_000);
    catalog::record_seal(&root, &kept, true);
    let over = recording(&root, 2, 1_000);
    let limits = catalog::OwnerLimits {
        cap: 0,
        ..catalog::OwnerLimits::DEFAULT
    };
    catalog::record_seal_within(&root, &over, true, limits);
    let partial = |sql: &str, scope: Scope| {
        let coverage = run(sql, vec![catalog_of(&root, scope)], &qtmp)
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
            .coverage;
        assert_eq!(
            coverage.capped,
            usize::from(coverage.partial.values().any(|why| why.contains("cap"))),
            "{sql}: {coverage:?}"
        );
        coverage.partial
    };
    let capped = "none for 1 session(s) over the catalog's cap";

    let samples = "machine samples not kept; summaries only";
    let metrics = partial("SELECT count(*) FROM metrics", Scope::default());
    assert_eq!(
        metrics.get("metrics").map(String::as_str),
        Some(&*format!("{samples}; {capped}")),
        "{metrics:?}"
    );
    let spans = partial("SELECT count(*) FROM spans", Scope::default());
    assert_eq!(
        spans.get("spans").map(String::as_str),
        Some(&*format!("ran targets only; {capped}")),
        "{spans:?}"
    );
    // `targets` lost nothing to the cap.
    assert!(partial("SELECT count(*) FROM targets", Scope::default()).is_empty());
    // Out of scope, the capped session is not counted.
    let before_over = Scope {
        until_ms: Some(now_ms() - 1_500),
        ..Scope::default()
    };
    let metrics = partial("SELECT count(*) FROM metrics", before_over);
    assert_eq!(
        metrics.get("metrics").map(String::as_str),
        Some(samples),
        "{metrics:?}"
    );
}
