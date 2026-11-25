//! `heph session query` as the command runs it (spec "CLI", D20): the
//! sources it picks, its formats, its inputs and limits, and that whatever
//! the statement, the sessions directory is never written.

use clap::Parser as _;
use tokio::signal::unix::SignalKind;

use super::*;
use crate::tui::session::store;

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(clap::Parser)]
struct Cli {
    #[command(flatten)]
    args: QueryArgs,
}

fn args(argv: &[&str]) -> QueryArgs {
    Cli::try_parse_from(std::iter::once("query").chain(argv.iter().copied()))
        .unwrap_or_else(|e| panic!("{argv:?} parses: {e}"))
        .args
}

/// What one `heph session query` printed, and its stderr notes.
struct Ran {
    out: anyhow::Result<String>,
    notes: Vec<String>,
}

async fn run_query(root: &Path, argv: &[&str], stdin: &str) -> Ran {
    run_query_with(root, argv, stdin, Arc::new(NoDownload)).await
}

async fn run_query_with(
    root: &Path,
    argv: &[&str],
    stdin: &str,
    download: Arc<dyn reference::Download>,
) -> Ran {
    run_query_until(root, argv, stdin, download, Arc::default()).await
}

async fn run_query_until(
    root: &Path,
    argv: &[&str],
    stdin: &str,
    download: Arc<dyn reference::Download>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> Ran {
    let a = args(argv);
    let file = FileSource::new(root);
    let sources: [&dyn SessionSource; 1] = [&file];
    let opener = reference::Opener {
        tmp_dir: store::tmp_dir(root).into(),
        download,
    };
    let notes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let env = query::QueryEnv {
        root,
        sources: &sources,
        opts: opts(),
        opener: &opener,
        workspace: None,
        note: Arc::new(enclose::enclose!((notes) move |l: &str| {
            notes.lock().expect("notes").push(l.to_owned());
        })),
    };
    let mut out = Vec::new();
    let res = query::query_until(&a, env, &mut stdin.as_bytes(), &mut out, stop).await;
    let notes = notes.lock().expect("notes").clone();
    Ran {
        out: res.map(|()| String::from_utf8(out).expect("utf-8")),
        notes,
    }
}

/// An ended session of `//p:t` started `ago_ms` before `T0`.
fn session(root: &Path, ago_ms: u64, seq: u8) -> SessionId {
    let id = SessionId::new(T0 - ago_ms, seq);
    let start = id.started_unix_ms();
    write_ended(
        root,
        &meta(id, 100, &["heph", "run", "//p:t"]),
        &build_records("//p:t", start, Some(0)),
        &ended(Some(0), start + 1_000),
    );
    id
}

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("json ({e}):\n{text}"))
}

/// `e`'s exit code and the message main prints on stderr, as `execute`
/// reports them.
fn code_and_msg(e: anyhow::Error) -> (Option<u8>, String) {
    let msg = format!("{e:#}");
    let wrapped = wrap_exit(Err(e)).expect_err("still an error");
    (exit_code(&wrapped), msg)
}

/// Spec row `query_is_read_only`: `INSERT`, `ATTACH`, `load_extension` and
/// a writing pragma are refused with 125 over every kind of source (the
/// catalog, one recording in place, two copied), and the sessions directory
/// is unchanged byte for byte.
#[tokio::test]
async fn query_is_read_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let a = session(&root, 0, 1).encode();
    let b = session(&root, 60_000, 2).encode();
    let catalog: &[&str] = &[];
    let one: &[&str] = &["--from", &a];
    let two: &[&str] = &["--from", &a, "--from", &b];
    // Settle what a first query legitimately does: the catalog made and
    // healed, the recordings' indexes built, the temp dir created.
    for from in [catalog, one, two] {
        let argv: Vec<&str> = from
            .iter()
            .copied()
            .chain(["SELECT count(*) FROM sessions"])
            .collect();
        let ran = run_query(&root, &argv, "").await;
        ran.out.unwrap_or_else(|e| panic!("{argv:?}: {e:#}"));
    }
    let before = tree(&root);
    let attached = tmp.path().join("attached.db");
    let attach = format!("ATTACH '{}' AS x", attached.display());
    let refused = [
        "INSERT INTO sessions (session_id) VALUES ('x')",
        "DELETE FROM spans",
        attach.as_str(),
        "SELECT load_extension('libfoo')",
        "PRAGMA journal_mode = DELETE",
        "PRAGMA user_version = 7",
        "SELECT 1; DELETE FROM spans",
    ];
    for from in [catalog, one, two] {
        for sql in refused {
            let argv: Vec<&str> = from.iter().copied().chain([sql]).collect();
            let err = run_query(&root, &argv, "")
                .await
                .out
                .expect_err("a write is refused");
            let (code, msg) = code_and_msg(err);
            assert_eq!(code, Some(125), "{argv:?}: {msg}");
            assert!(!msg.is_empty(), "{argv:?}: says why");
        }
    }
    assert!(!attached.exists(), "ATTACH created {}", attached.display());
    // Every file and directory as it was. The root's own mtime is left out:
    // opening the catalog (as `list` does) makes its `-wal` and `-shm`, and
    // closing it removes them.
    let files = |t: BTreeMap<PathBuf, _>| {
        t.into_iter()
            .filter(|(p, _)| p != &root)
            .collect::<BTreeMap<_, _>>()
    };
    assert_eq!(
        files(tree(&root)),
        files(before),
        "a refused query wrote the sessions directory"
    );
}

/// `--format json` is `{version, columns, types, rows, coverage}`: INTEGER
/// a string, REAL a number (infinity null), BLOB base64, NULL null; csv
/// quotes what it must; the table aligns its columns.
#[tokio::test]
async fn query_formats() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let id = session(&root, 0, 1).encode();
    let sql = "SELECT session_id, 9007199254740993 AS big, 1.5 AS r, 1e999 AS inf, \
               NULL AS n, x'00ff' AS b, 'a,\"b' AS q FROM sessions";

    let ran = run_query(&root, &["--from", &id, "--format", "json", sql], "").await;
    let v = json(&ran.out.expect("json"));
    assert_eq!(v["version"], 1);
    assert_eq!(
        v["columns"],
        serde_json::json!(["session_id", "big", "r", "inf", "n", "b", "q"])
    );
    assert_eq!(
        v["types"],
        serde_json::json!(["TEXT", null, null, null, null, null, null])
    );
    assert_eq!(
        v["rows"],
        serde_json::json!([[id, "9007199254740993", 1.5, null, null, "AP8=", "a,\"b"]])
    );
    assert_eq!(
        v["coverage"],
        serde_json::json!({
            "catalog": [], "recordings": 1, "skipped": 0, "truncated": 0, "reaches_ms": null
        })
    );

    let ran = run_query(&root, &["--from", &id, "--format", "csv", sql], "").await;
    assert_eq!(
        ran.out.expect("csv"),
        format!("session_id,big,r,inf,n,b,q\n{id},9007199254740993,1.5,inf,,AP8=,\"a,\"\"b\"\n")
    );

    let ran = run_query(
        &root,
        &["--from", &id, "SELECT 'long value' AS a, 2 AS bb, 'x' AS c"],
        "",
    )
    .await;
    assert_eq!(
        ran.out.expect("table"),
        "a           bb  c\nlong value  2   x\n"
    );
}

/// A table cell is one line, its control bytes escaped: a foreign file's
/// text never reaches the terminal raw.
#[tokio::test]
async fn query_table_escapes_control_bytes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let ran = run_query(
        &root,
        &["SELECT char(27) || '[2J' || char(10) || 'x' AS v"],
        "",
    )
    .await;
    let out = ran.out.expect("table");
    assert!(!out.contains('\u{1b}'), "{out:?}");
    assert_eq!(out.lines().count(), 2, "{out:?}");
}

/// The SQL from the argument, `-` (stdin) or `-f FILE`; exactly one of
/// them or `--schema`. `--limit` caps the rows (more is a `sql` error), 0
/// lifts the cap; a SQL error carries SQLite's message and exits 125.
#[tokio::test]
async fn query_inputs_and_limits() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let three = "SELECT column1 AS value FROM (VALUES (1), (2), (3))";

    let ran = run_query(&root, &["--format", "csv", "-"], three).await;
    assert_eq!(ran.out.expect("stdin"), "value\n1\n2\n3\n");
    let file = tmp.path().join("q.sql");
    std::fs::write(&file, three).expect("write sql");
    let path = file.to_str().expect("utf-8 path");
    let ran = run_query(&root, &["--format", "csv", "-f", path], "").await;
    assert_eq!(ran.out.expect("file"), "value\n1\n2\n3\n");

    let missing = tmp.path().join("missing.sql");
    let err = run_query(&root, &["-f", missing.to_str().expect("utf-8")], "")
        .await
        .out
        .expect_err("a missing file");
    let (code, msg) = code_and_msg(err);
    assert_eq!(code, Some(125), "{msg}");
    assert!(msg.contains("reading the SQL in"), "{msg}");

    for both in [
        &["-f", path, three][..],
        &["--schema", three][..],
        &[][..],
        &["--schema", "--format", "json"][..],
        &["--from", "x", "--full", three][..],
        &["--from", "x", "--since", "1d", three][..],
    ] {
        let argv = std::iter::once("query").chain(both.iter().copied());
        assert!(Cli::try_parse_from(argv).is_err(), "{both:?} is refused");
    }

    let err = run_query(&root, &["--limit", "2", three], "")
        .await
        .out
        .expect_err("over the limit");
    let (code, msg) = code_and_msg(err);
    assert_eq!(code, Some(125), "{msg}");
    assert!(msg.contains("more than 2 rows"), "{msg}");
    let ran = run_query(&root, &["--limit", "0", "--format", "csv", three], "").await;
    assert_eq!(ran.out.expect("no cap"), "value\n1\n2\n3\n");

    let err = run_query(&root, &["SELECT nope FROM spans"], "")
        .await
        .out
        .expect_err("an unknown column");
    let (code, msg) = code_and_msg(err);
    assert_eq!(code, Some(125), "{msg}");
    assert!(msg.contains("nope") && msg.contains("duration_us"), "{msg}");

    assert!(Cli::try_parse_from(["query", "--timeout", "soon", three]).is_err());
}

/// `--schema` prints the six names, a line per column, and the version.
#[tokio::test]
async fn query_schema_lists_every_name() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = run_query(tmp.path(), &["--schema"], "")
        .await
        .out
        .expect("schema");
    assert!(
        out.starts_with(&format!(
            "explorer schema version {}\n",
            store::SCHEMA_VERSION
        )),
        "{out}"
    );
    for t in crate::tui::session::query::TABLES {
        assert!(
            out.contains(&format!("\n{}\n", t.name)),
            "{}: {out}",
            t.name
        );
        for (c, ty) in t.columns {
            assert!(
                out.lines()
                    .any(|l| l.split_whitespace().collect::<Vec<_>>() == [*c, *ty]),
                "{}.{c}: {out}",
                t.name
            );
        }
    }
    // The built-in recipes follow, each under its `-- name: purpose` line
    // (`recipes_run` runs them as printed).
    let recipes = out.split_once("\nrecipes\n").expect("a recipes section").1;
    for r in crate::tui::session::query::RECIPES {
        assert!(
            recipes.contains(&format!("\n-- {}: {}\n{};\n", r.name, r.purpose, r.sql)),
            "{}: {recipes}",
            r.name
        );
    }
}

/// The default source is the catalog scoped by the window; `--full` reads
/// every recording kept in the window instead, counting one in a format
/// this heph does not read as skipped (and saying so on stderr).
#[tokio::test]
async fn query_sources_follow_the_window() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    session(&root, 0, 1);
    session(&root, 60_000, 2);
    let oldest = session(&root, 10 * DAY_MS, 3);
    let newer = session(&root, 120_000, 4);
    testing::alter(&root.join(newer.encode()), "PRAGMA user_version=999").expect("newer");
    let count = "SELECT count(*) AS n FROM sessions";

    let ran = run_query(&root, &["--format", "json", count], "").await;
    let v = json(&ran.out.expect("catalog"));
    assert_eq!(v["rows"], serde_json::json!([["2"]]), "{v}");
    assert_eq!(v["coverage"]["catalog"], serde_json::json!(["sessions"]));
    // The catalog reaches its oldest session, outside the 7-day window.
    assert_eq!(
        v["coverage"]["reaches_ms"],
        oldest.started_unix_ms(),
        "{v}"
    );
    let ran = run_query(&root, &["--format", "json", "--since", "30d", count], "").await;
    assert_eq!(
        json(&ran.out.expect("30d"))["rows"],
        serde_json::json!([["3"]])
    );

    let ran = run_query(&root, &["--format", "json", "--full", count], "").await;
    let v = json(&ran.out.expect("full"));
    assert_eq!(v["rows"], serde_json::json!([["2"]]), "{v}");
    assert_eq!(v["coverage"]["recordings"], 2, "{v}");
    assert_eq!(v["coverage"]["skipped"], 1, "{v}");
    assert_eq!(v["coverage"]["reaches_ms"], serde_json::Value::Null, "{v}");
    assert!(
        ran.notes.iter().any(|n| n.contains("skipped 1 recording")),
        "{:?}",
        ran.notes
    );

    // No sessions directory at all: the empty schema answers.
    let none = tmp.path().join("none");
    let ran = run_query(&none, &["--format", "csv", count], "").await;
    assert_eq!(ran.out.expect("empty"), "n\n0\n");
    assert!(!none.exists(), "a query made the sessions directory");
}

/// The SIGINT listener is armed before its task runs, and sets the stop
/// flag the engine polls.
#[tokio::test]
async fn query_stop_listener_is_armed_before_its_task_runs() {
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // SIGURG: ignored by default, and nothing else in this binary listens.
    let _armed = query::StopOn::on(SignalKind::from_raw(libc::SIGURG), Arc::clone(&stop));
    // SAFETY: `raise` only delivers a signal to this thread; the handler was
    // installed by `StopOn::on` above.
    let rc = unsafe { libc::raise(libc::SIGURG) };
    assert_eq!(rc, 0, "raise(SIGURG)");
    tokio::time::timeout(Duration::from_secs(10), async {
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the signal raised before the task ran sets the stop flag");
}

/// A Ctrl-C while the sources open stops the query (`interrupted`, 130)
/// over every kind of source, though the statement is too short for the
/// engine's progress handler ever to look at the stop flag.
#[tokio::test]
async fn query_stops_when_interrupted_opening_its_sources() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let a = session(&root, 0, 1).encode();
    session(&root, 60_000, 2);
    let catalog: &[&str] = &[];
    let one: &[&str] = &["--from", &a];
    let full: &[&str] = &["--full"];
    for from in [catalog, one, full] {
        let argv: Vec<&str> = from.iter().copied().chain(["SELECT 1"]).collect();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let ran = run_query_until(&root, &argv, "", Arc::new(NoDownload), stop).await;
        let e = ran.out.expect_err(&format!("{argv:?}: answered"));
        let (kind, code, _) = kind_and_code(e);
        assert_eq!((kind, code), (ErrorKind::Interrupted, Some(130)), "{argv:?}");
    }
}

/// Serves `body` like [`super::refs::Serve`], counting the downloads.
struct CountedServe {
    body: Vec<u8>,
    downloads: std::sync::atomic::AtomicUsize,
}

impl reference::Download for CountedServe {
    fn get_to_temp(&self, url: &str, dir: &Path) -> anyhow::Result<tempfile::NamedTempFile> {
        self.downloads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        super::refs::Serve(self.body.clone()).get_to_temp(url, dir)
    }
}

/// `--from` naming one recording twice reads it once, with a note: the
/// same id (in full or by suffix), the same file by another spelling of its
/// path, the same URL (downloaded once). Distinct recordings are all read.
#[tokio::test]
async fn query_from_reads_each_recording_once() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let a = session(&root, 0, 1).encode();
    let b = session(&root, 60_000, 2).encode();
    let files = tmp.path().join("files");
    std::fs::create_dir_all(&files).expect("mkdir");
    let db = std::fs::read(root.join(&a).join(store::FILE)).expect("read the recording");
    std::fs::write(files.join("a.db"), &db).expect("copy the recording");
    let path = files.join("a.db").display().to_string();
    let dotted = files.join(".").join("a.db").display().to_string();
    let suffix = a.get(a.len() - 5..).expect("an id is 10 characters");
    const URL: &str = "https://bucket.invalid/run.hephsession";
    const URL_AGAIN: &str = "HTTPS://Bucket.INVALID/run.hephsession";

    let cases: [(&[&str], usize); 5] = [
        (&["--from", &a, "--from", &a], 1),
        (&["--from", &a, "--from", suffix, "--from", &b], 2),
        (&["--from", &path, "--from", &dotted], 1),
        (&["--from", URL, "--from", URL_AGAIN], 1),
        (&["--from", &a, "--from", &b], 2),
    ];
    for (from, recordings) in cases {
        let serve = Arc::new(CountedServe {
            body: db.clone(),
            downloads: Default::default(),
        });
        let argv: Vec<&str> = from
            .iter()
            .copied()
            .chain(["--format", "json", "SELECT count(*) AS n FROM sessions"])
            .collect();
        let ran = run_query_with(&root, &argv, "", serve.clone()).await;
        let v = json(&ran.out.unwrap_or_else(|e| panic!("{argv:?}: {e:#}")));
        assert_eq!(v["coverage"]["recordings"], recordings, "{argv:?}: {v}");
        assert_eq!(v["rows"][0][0], recordings.to_string(), "{argv:?}: {v}");
        let dropped = from.len() / 2 - recordings;
        let notes: Vec<_> = ran
            .notes
            .iter()
            .filter(|n| n.contains("names the same recording"))
            .collect();
        assert_eq!(notes.len(), dropped, "{argv:?}: {:?}", ran.notes);
        if from.contains(&URL) {
            assert_eq!(
                serve.downloads.load(std::sync::atomic::Ordering::Relaxed),
                1,
                "the duplicate URL is not downloaded"
            );
        }
    }
}

/// A query stopped by Ctrl-C exits 130, not 125 (the one `heph session`
/// error that does), with its message for stderr. A `--timeout` stays 125.
#[test]
fn query_interrupted_exits_130() {
    use crate::tui::session::query::QueryError;

    let (code, msg) = code_and_msg(query::query_error(QueryError::Interrupted));
    assert_eq!((code, msg.as_str()), (Some(130), "query interrupted"));

    let (code, msg) = code_and_msg(query::query_error(QueryError::Sql(
        "stopped after --timeout 1s".into(),
    )));
    assert_eq!(
        (code, msg.as_str()),
        (Some(125), "stopped after --timeout 1s")
    );
}

/// FQ M6, query side: `coverage.truncated` counts a truncated recording
/// (`truncated=1`) from each kind of source: the catalog, one recording in
/// place (`--from`), and recordings copied (`--full`); a recording in an
/// unknown format (`user_version=99`) is skipped, never a heal failure,
/// and the query still answers.
#[tokio::test]
async fn query_coverage_counts_truncated_sessions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let cut = session(&root, 0, 1);
    testing::alter(&root.join(cut.encode()), "UPDATE session SET truncated = 1")
        .expect("truncated");
    session(&root, 60_000, 2);
    let unknown = session(&root, 120_000, 3);
    testing::alter(&root.join(unknown.encode()), "PRAGMA user_version = 99")
        .expect("an unknown format");
    let count = "SELECT count(*) AS n FROM sessions";

    let ran = run_query(&root, &["--format", "json", count], "").await;
    let v = json(&ran.out.expect("catalog"));
    assert_eq!(v["rows"], serde_json::json!([["2"]]), "{v}");
    assert_eq!(v["coverage"]["catalog"], serde_json::json!(["sessions"]));
    assert_eq!(v["coverage"]["truncated"], 1, "{v}");

    let id = cut.encode();
    let ran = run_query(&root, &["--format", "json", "--from", &id, count], "").await;
    let v = json(&ran.out.expect("in place"));
    assert_eq!(v["coverage"]["recordings"], 1, "{v}");
    assert_eq!(v["coverage"]["truncated"], 1, "{v}");

    let ran = run_query(&root, &["--format", "json", "--full", count], "").await;
    let v = json(&ran.out.expect("full"));
    assert_eq!(v["coverage"]["recordings"], 2, "{v}");
    assert_eq!(v["coverage"]["skipped"], 1, "{v}");
    assert_eq!(v["coverage"]["truncated"], 1, "{v}");

    let failures: i64 = rusqlite::Connection::open_with_flags(
        root.join(crate::tui::session::catalog::FILE),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open the catalog")
    .query_row("SELECT count(*) FROM heal_failures", [], |r| r.get(0))
    .expect("heal_failures");
    assert_eq!(failures, 0, "an unknown format is skipped, not failed");
}
