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
    let a = args(argv);
    let file = FileSource::new(root);
    let sources: [&dyn SessionSource; 1] = [&file];
    let opener = reference::Opener {
        tmp_dir: store::tmp_dir(root).into(),
        download: Arc::new(NoDownload),
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
    let res = query::query(&a, env, &mut stdin.as_bytes(), &mut out).await;
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
        serde_json::json!({"catalog": [], "recordings": 1, "skipped": 0, "truncated": 0})
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
    session(&root, 10 * DAY_MS, 3);
    let newer = session(&root, 120_000, 4);
    testing::alter(&root.join(newer.encode()), "PRAGMA user_version=999").expect("newer");
    let count = "SELECT count(*) AS n FROM sessions";

    let ran = run_query(&root, &["--format", "json", count], "").await;
    let v = json(&ran.out.expect("catalog"));
    assert_eq!(v["rows"], serde_json::json!([["2"]]), "{v}");
    assert_eq!(v["coverage"]["catalog"], serde_json::json!(["sessions"]));
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
