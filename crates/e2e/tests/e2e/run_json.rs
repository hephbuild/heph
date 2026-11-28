//! `heph run --json` over a real engine and BUILD files: the entries the
//! request's own events fold into, and when the list is closed.
//!
//! The command wraps exactly this: it hands the request a channel of its own,
//! runs the build as `run_json::collect`'s future, and closes the list when
//! `Collected::may_close` says so. Each test drives that seam directly, with
//! the build future spelled as `RunApp` spells it; the process boundary (the
//! exit code, Ctrl-C, the entrypoint refusal) is `crates/bin-e2e`'s.

use crate::common::{build_engine, build_engine_with_remote, drain, run_drain_events};
use anyhow::Context as _;
use heph::commands::run::{RunArgs, build, request_events};
use heph::commands::run_json::{Sources, collect};
use heph::engine::event::BuildEventKind;
use heph::engine::{Discovery, Engine, Gaps, HomeDir, ResultOptions};
use heph::htaddr::parse_addr;
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

/// A success, a failure (exit 3), and a target blocked by the failure.
const BUILD: &str = r#"
target(name = "ok", driver = "bash", run = "echo ok > $OUT", out = "ok.txt")
target(name = "bad", driver = "bash", run = "echo oops; exit 3", out = "bad.txt")
target(name = "blk", driver = "bash", run = "echo b > $OUT", out = "blk.txt", deps = [":bad"])
"#;

/// The default fields: every `free` one but `outputs`.
const DEFAULT_KEYS: [&str; 15] = [
    "addr",
    "cache",
    "duration_ms",
    "error",
    "exit_code",
    "failure",
    "file",
    "more",
    "outcome",
    "path",
    "remote",
    "root",
    "root_file",
    "selected",
    "timed_out",
];

fn write_pkg(root: &Path, pkg: &str, build: &str) {
    std::fs::create_dir_all(root.join(pkg)).expect("mkdir pkg");
    std::fs::write(root.join(pkg).join("BUILD"), build).expect("write BUILD");
}

fn addr(a: &str) -> Matcher {
    Matcher::Addr(parse_addr(a).expect("parse addr"))
}

/// What a `heph run --json` printed.
pub(crate) struct JsonRun {
    pub(crate) entries: Vec<Value>,
    /// The list got its `]`.
    pub(crate) closed: bool,
    /// The run's own result, which `finalize` turns into the exit code.
    res: anyhow::Result<()>,
    /// Every event the display saw.
    events: Vec<heph::engine::event::BuildEvent>,
    /// What `finalize` makes of the walk's skips: an error when it skipped
    /// part of the selection.
    complete: anyhow::Result<()>,
    /// The failure boxes a text run of the same request prints.
    pub(crate) boxes: String,
}

impl JsonRun {
    /// How many failure boxes the text run prints.
    pub(crate) fn box_count(&self) -> usize {
        self.boxes.matches("× target failed:").count()
    }

    pub(crate) fn entry(&self, addr: &str) -> &Value {
        self.entries
            .iter()
            .find(|e| e["addr"] == addr)
            .with_context(|| format!("no entry for {addr}: {:?}", self.entries))
            .expect("an entry")
    }
}

/// `heph run <m> <flags>`, as `RunApp` runs it under an output flag. With
/// `cancel_at`, the request is cancelled (Ctrl-C) once that target starts
/// executing.
pub(crate) async fn run_json(
    engine: &Arc<Engine>,
    m: &Matcher,
    flags: &[&str],
    cancel_at: Option<&str>,
) -> JsonRun {
    run_json_with(engine, m, flags, cancel_at, false).await
}

/// [`run_json`], under `--fail-fast` when `fail_fast`.
pub(crate) async fn run_json_with(
    engine: &Arc<Engine>,
    m: &Matcher,
    flags: &[&str],
    cancel_at: Option<&str>,
    fail_fast: bool,
) -> JsonRun {
    run_json_full(engine, m, flags, cancel_at, fail_fast, false).await
}

/// [`run_json_with`]; with `cancel_first`, the request is cancelled before
/// the build starts, so the walk never runs.
async fn run_json_full(
    engine: &Arc<Engine>,
    m: &Matcher,
    flags: &[&str],
    cancel_at: Option<&str>,
    fail_fast: bool,
    cancel_first: bool,
) -> JsonRun {
    let args = RunArgs::try_parse_from(flags).expect("run flags");
    let out = &args.out;
    assert!(out.enabled(), "{flags:?}");
    // The display's end of the fold: it sees every event, after the fold.
    let (forward, mut shown) =
        tokio::sync::mpsc::unbounded_channel::<heph::engine::event::BuildEvent>();
    let (tx, rx) = request_events(out.enabled(), Some(forward.clone()));
    let rx = rx.expect("under an output flag the request gets its own channel");
    let rs = engine.new_state_with_events(fail_fast, tx);
    if cancel_first {
        rs.ctoken().cancel();
    }
    let gaps = Gaps::new("test");
    let opts = ResultOptions {
        discovery: Discovery::keep_going_unless(fail_fast, &gaps),
        ..ResultOptions::default()
    };
    let outputs = args.output_matcher();
    let run = build(
        Arc::clone(engine),
        Arc::clone(&rs),
        m.clone(),
        outputs.clone(),
        &opts,
        &gaps,
    );
    let canceller = async {
        let mut seen = Vec::new();
        while let Some(ev) = shown.recv().await {
            if let BuildEventKind::ExecuteStart { addr, .. } = &ev.kind
                && cancel_at == Some(addr.as_str())
            {
                rs.ctoken().cancel();
            }
            seen.push(ev);
        }
        seen
    };
    let mut buf = Vec::new();
    let (closed, res, events) = {
        let src = Sources {
            engine,
            rs: &rs,
            out,
            outputs: &outputs,
            opts: &opts,
        };
        let mut list = out
            .printer_to(&mut buf, output::Style::Compact)
            .start_list()
            .expect("start the list");
        let (c, events) = tokio::join!(collect(run, rx, Some(forward), &src, &mut list), canceller);
        assert!(!c.stopped, "stdout is a buffer; it cannot close");
        assert!(c.write_error.is_none(), "{:?}", c.write_error);
        assert!(c.field_error.is_none(), "{:?}", c.field_error);
        let closed = c.may_close(&gaps);
        if closed {
            list.finish(&output::never_cancelled)
                .expect("close the list");
        }
        (closed, c.res.map(drop), events)
    };
    // The failure boxes the text run prints, from the same request, and
    // what `--fail-fast` stopped after them.
    let mut printed = Vec::new();
    drop(heph::commands::errors::FailedTargets::reported_in(
        rs.failures(),
        None,
        &std::collections::HashSet::new(),
        &mut printed,
        false,
        heph::commands::errors::BoxRequest {
            root: None,
            stopped: rs.fail_fast_stopped(),
        },
    ));
    let boxes = String::from_utf8(printed).expect("utf-8 boxes");
    drain(rs).await;
    let text = String::from_utf8(buf).expect("utf-8");
    let entries = if closed {
        serde_json::from_str(&text)
    } else {
        assert!(
            !text.trim_end().ends_with(']'),
            "an incomplete list is left open: {text}"
        );
        serde_json::from_str(&format!("{text}\n]"))
    }
    .with_context(|| format!("not a JSON list: {text}"))
    .expect("a JSON list");
    JsonRun {
        entries,
        closed,
        res,
        events,
        complete: heph::commands::errors::require_complete_selection(&gaps),
        boxes,
    }
}

fn keys(v: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = v
        .as_object()
        .with_context(|| format!("not an object: {v}"))
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// D7: ran (a miss), cached (local), failed, blocked; the default set and the
/// opt-in `outputs` and `hashin`; the list closed although targets failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_json_entries() {
    let root = tempfile::tempdir().expect("root tempdir");
    write_pkg(root.path(), "p", BUILD);
    let engine = build_engine_with_remote(root.path(), None);

    let ran = run_json(&engine, &addr("//p:ok"), &["--json"], None).await;
    assert!(ran.res.is_ok(), "{:?}", ran.res);
    assert!(ran.closed);
    assert_eq!(ran.entries.len(), 1, "{:?}", ran.entries);
    let e = ran.entry("//p:ok");
    assert_eq!(keys(e), DEFAULT_KEYS, "{e}");
    assert_eq!(e["outcome"], "ran", "{e}");
    assert_eq!(e["cache"], "miss", "{e}");
    assert_eq!(e["exit_code"], 0, "{e}");
    assert_eq!(e["timed_out"], false, "{e}");
    assert_eq!(e["selected"], true, "{e}");
    for null in [
        "error",
        "failure",
        "remote",
        "file",
        "root",
        "root_file",
        "more",
        "path",
    ] {
        assert!(e[null].is_null(), "{null}: {e}");
    }

    let all = run_json(
        &engine,
        &Matcher::Package(PkgBuf::from("p")),
        &["--fields", "+outputs,+hashin"],
        None,
    )
    .await;
    assert!(all.res.is_err(), "a target failed: the run fails");
    assert!(
        all.closed,
        "every matched target has its entry: closed, failures and all"
    );
    assert_eq!(all.entries.len(), 3, "{:?}", all.entries);

    let ok = all.entry("//p:ok");
    assert_eq!(ok["outcome"], "cached", "{ok}");
    assert_eq!(ok["cache"], "local", "{ok}");
    assert!(ok["exit_code"].is_null(), "nothing ran: {ok}");
    let outputs = ok["outputs"].as_array().expect("outputs selected");
    assert!(
        outputs
            .iter()
            .any(|p| p.as_str().is_some_and(|p| p.ends_with("ok.txt"))),
        "{ok}"
    );
    assert!(ok["hashin"].is_string(), "{ok}");

    let bad = all.entry("//p:bad");
    assert_eq!(bad["outcome"], "failed", "{bad}");
    assert_eq!(bad["exit_code"], 3, "{bad}");
    assert!(
        bad["error"].as_str().is_some_and(|e| !e.contains('\n')),
        "one line: {bad}"
    );
    assert!(bad["failure"]["log"].is_string(), "{bad}");
    assert!(bad["outputs"].is_null(), "a failure has no outputs: {bad}");

    let blk = all.entry("//p:blk");
    assert_eq!(bad["root"], "//p:bad", "a culprit is its own root: {bad}");
    assert_eq!(blk["outcome"], "skipped", "{blk}");
    assert_eq!(blk["root"], "//p:bad", "{blk}");
    assert_eq!(
        blk["path"],
        serde_json::json!(["//p:blk", "//p:bad"]),
        "{blk}"
    );
    assert_eq!(blk["more"], 0, "{blk}");
    assert!(blk["error"].is_null(), "{blk}");
    assert!(blk["exit_code"].is_null(), "{blk}");
    assert!(blk["failure"].is_null(), "{blk}");

    let pos = |a: &str| all.entries.iter().position(|e| e["addr"] == a);
    assert!(pos("//p:bad") < pos("//p:blk"), "completion order");
}

/// `cache: remote`, naming the cache the hit came from.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_json_entries_remote_hit() {
    let root = tempfile::tempdir().expect("root tempdir");
    let remote = tempfile::tempdir().expect("remote tempdir");
    let remote_uri = format!("file://{}", remote.path().display());
    write_pkg(root.path(), "p", BUILD);

    let seeder = build_engine(root.path(), &remote_uri);
    let seeded = run_json(&seeder, &addr("//p:ok"), &["--json"], None).await;
    assert!(seeded.res.is_ok(), "{:?}", seeded.res);
    drop(seeder);
    let rm_local = || {
        std::fs::remove_dir_all(HomeDir::for_tests(root.path()).join("cache"))
            .expect("rm local cache");
    };
    rm_local();

    // One remote read span is the hit's presence probe; a second is its
    // blobs being pulled.
    let remote_reads = |run: &JsonRun| {
        run.events
            .iter()
            .filter(|e| {
                matches!(&e.kind,
                    BuildEventKind::RemoteCacheReadStart { addr } if addr == "//p:ok")
            })
            .count()
    };

    let engine = build_engine(root.path(), &remote_uri);
    let hit = run_json(&engine, &addr("//p:ok"), &["--json"], None).await;
    assert!(hit.res.is_ok(), "{:?}", hit.res);
    let e = hit.entry("//p:ok");
    assert_eq!(e["outcome"], "cached", "{e}");
    assert_eq!(e["cache"], "remote", "{e}");
    assert_eq!(e["remote"], "shared", "{e}");
    assert_eq!(
        remote_reads(&hit),
        1,
        "the default fields fetch no outputs: {:?}",
        hit.events
    );
    drop(engine);
    rm_local();

    // `outputs` is what pulls them: its cost, off by default.
    let engine = build_engine(root.path(), &remote_uri);
    let pulled = run_json(&engine, &addr("//p:ok"), &["--fields", "+outputs"], None).await;
    assert!(pulled.res.is_ok(), "{:?}", pulled.res);
    let e = pulled.entry("//p:ok");
    assert_eq!(e["cache"], "remote", "{e}");
    assert!(
        e["outputs"].as_array().is_some_and(|o| !o.is_empty()),
        "{e}"
    );
    assert_eq!(
        remote_reads(&pulled),
        2,
        "`outputs` pulls the hit's blobs: {:?}",
        pulled.events
    );
}

/// Ctrl-C: the running target's entry says `cancelled`, and the list is left
/// without its `]`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_json_entries_cancelled() {
    let root = tempfile::tempdir().expect("root tempdir");
    write_pkg(
        root.path(),
        "p",
        r#"target(name = "slow", driver = "bash", run = "sleep 60; echo > $OUT", out = "o.txt")"#,
    );
    let engine = build_engine_with_remote(root.path(), None);

    let stopped = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run_json(&engine, &addr("//p:slow"), &["--json"], Some("//p:slow")),
    )
    .await
    .expect("the cancelled run ends");
    let err = stopped.res.as_ref().expect_err("cancelled");
    assert!(heph::commands::errors::is_cancelled(err), "{err:#}");
    assert!(!stopped.closed, "a cancelled run's list is not whole");
    let e = stopped.entry("//p:slow");
    assert_eq!(e["outcome"], "cancelled", "{e}");
    assert!(e["exit_code"].is_null(), "{e}");
}

/// RUN-24's graph: `bad` fails (exit 3) after `slow` has started, and `slow`
/// would run for a minute.
const FAIL_FAST: &str = r#"
target(name = "bad", driver = "bash", run = "sleep 1; echo oops; exit 3", out = "bad.txt")
target(name = "slow", driver = "bash", run = "sleep 60; echo > $OUT", out = "slow.txt")
"#;

/// `heph run --fail-fast --json //p:` over [`FAIL_FAST`], bounded so a
/// sibling fail-fast never stopped fails the test rather than hanging it.
async fn fail_fast_run() -> JsonRun {
    let root = tempfile::tempdir().expect("root tempdir");
    write_pkg(root.path(), "p", FAIL_FAST);
    let engine = build_engine_with_remote(root.path(), None);
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run_json_with(
            &engine,
            &Matcher::Package(PkgBuf::from("p")),
            &["--json"],
            None,
            true,
        ),
    )
    .await
    .expect("fail-fast stops the sibling")
}

/// RUN-24: `--fail-fast` stops the running sibling once `bad` fails. The
/// sibling ends cancelled, in its event and its record, and is neither
/// blamed nor boxed: the one failure is `bad`'s. One line after the box
/// says it was stopped while running.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fail_fast_siblings_are_cancelled_and_listed() {
    let run = fail_fast_run().await;
    assert!(run.res.is_err(), "a target failed: the run fails");
    let bad = run.entry("//p:bad");
    assert_eq!(bad["outcome"], "failed", "{bad}");
    assert_eq!(bad["exit_code"], 3, "{bad}");
    let slow = run.entry("//p:slow");
    assert_eq!(slow["outcome"], "cancelled", "{slow}");
    assert!(slow["root"].is_null(), "a stop blames no one: {slow}");
    assert!(slow["root_file"].is_null(), "{slow}");
    assert!(slow["exit_code"].is_null(), "{slow}");
    let ends: Vec<_> = run
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            BuildEventKind::ResultEnd {
                addr,
                cancelled,
                upstream_of,
                ..
            } if addr == "//p:slow" => Some((*cancelled, upstream_of.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(ends, [(true, None)], "one cancelled end, naming no root");
    assert_eq!(run.box_count(), 1, "{}", run.boxes);
    assert!(run.boxes.contains("//p:bad"), "{}", run.boxes);
    let (boxes, stopped) = run
        .boxes
        .split_once("\nstopped by --fail-fast")
        .unwrap_or_else(|| panic!("a fail-fast line:\n{}", run.boxes));
    assert!(!boxes.contains("//p:slow"), "{}", run.boxes);
    assert_eq!(
        format!("stopped by --fail-fast{stopped}"),
        "stopped by --fail-fast: //p:slow (was running)\n",
        "{}",
        run.boxes
    );
}

/// RUN-24 / JSON_OUTPUT.md: under `--fail-fast --json` the failure's record
/// and the stopped sibling's `cancelled` record are written, then the list
/// is left without its `]`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fail_fast_json_cancelled_records_then_unterminated() {
    let run = fail_fast_run().await;
    assert!(!run.closed, "a fail-fast stop leaves the list open");
    let outcomes: Vec<(&str, &str)> = run
        .entries
        .iter()
        .map(|e| {
            (
                e["addr"].as_str().unwrap_or_default(),
                e["outcome"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        outcomes,
        [("//p:bad", "failed"), ("//p:slow", "cancelled")],
        "every record, in completion order, before the open end"
    );
}

/// A run past its `timeout` fails, and says it timed out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_json_entries_timeout() {
    let root = tempfile::tempdir().expect("root tempdir");
    write_pkg(
        root.path(),
        "p",
        r#"target(name = "slow", driver = "bash", run = "sleep 60; echo > $OUT", out = "o.txt", timeout = 1)"#,
    );
    let engine = build_engine_with_remote(root.path(), None);

    let late = run_json(&engine, &addr("//p:slow"), &["--json"], None).await;
    assert!(late.res.is_err(), "a timeout fails the run");
    assert!(late.closed, "a timeout is a failure, not a cancellation");
    let e = late.entry("//p:slow");
    assert_eq!(e["outcome"], "failed", "{e}");
    assert_eq!(e["timed_out"], true, "{e}");
    assert!(
        e["error"].as_str().is_some_and(|m| m.contains("timed out")),
        "{e}"
    );
}

/// A selector that matches nothing prints a whole `[]`, and still fails the
/// run as it does without `--json`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_json_entries_zero_matches() {
    let root = tempfile::tempdir().expect("root tempdir");
    write_pkg(root.path(), "p", BUILD);
    write_pkg(root.path(), "empty", "");
    let engine = build_engine_with_remote(root.path(), None);

    let none = run_json(
        &engine,
        &Matcher::Package(PkgBuf::from("empty")),
        &["--json"],
        None,
    )
    .await;
    assert!(none.closed);
    assert!(none.entries.is_empty(), "{:?}", none.entries);
    assert!(none.res.is_err(), "a selector that chose nothing fails");
}

/// D10: a package whose BUILD does not evaluate is skipped under keep-going,
/// so the list is not whole: the healthy package's entries are printed, the
/// `]` is not, and the run fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_json_entries_broken_package() {
    let root = tempfile::tempdir().expect("root tempdir");
    write_pkg(
        root.path(),
        "w/p",
        r#"target(name = "ok", driver = "bash", run = "echo ok > $OUT", out = "ok.txt")"#,
    );
    write_pkg(root.path(), "w/broken", "target(name = \n");
    let engine = build_engine_with_remote(root.path(), None);

    let run = run_json(
        &engine,
        &Matcher::PackagePrefix("w".into()),
        &["--json"],
        None,
    )
    .await;
    assert!(!run.closed, "a skipped package leaves the list open");
    assert_eq!(run.entry("//w/p:ok")["outcome"], "ran");
    let skipped = run
        .complete
        .expect_err("a run that skipped part of its selection fails");
    assert!(
        skipped
            .downcast_ref::<heph::commands::errors::IncompleteSelection>()
            .is_some(),
        "{skipped:#}"
    );
}

/// Two requested targets of one broken `d/BUILD`, under the default
/// keep-going walk: the walk's probe of `d` fails, and the file is blamed
/// rather than left a gap (D5, U2): the file's record (`addr: null`), then
/// both targets skipped because of it, each reaching the file through
/// itself, and the list closed. One requested target is the file's entry:
/// the same file record, then the target, closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_json_broken_build_two_requested_targets() {
    let root = tempfile::tempdir().expect("root tempdir");
    let t = |n: &str| {
        format!(r#"target(name = "{n}", driver = "bash", run = "echo ok > $OUT", out = "o.txt")"#)
    };
    write_pkg(
        root.path(),
        "d",
        &format!("{}\n{}\nfail(\"boom\")\n", t("t1"), t("t2")),
    );
    let engine = build_engine_with_remote(root.path(), None);
    let m = Matcher::Or(vec![addr("//d:t1"), addr("//d:t2")]);
    let two = run_json(&engine, &m, &["--json"], None).await;
    assert!(
        two.closed,
        "every requested target is accounted for: {:?}",
        two.entries
    );
    assert!(
        two.complete.is_ok(),
        "the file is blamed, not skipped: {:?}",
        two.complete
    );
    assert!(two.res.is_err(), "the run fails");
    assert_eq!(two.box_count(), 1, "one box, the file's: {}", two.boxes);
    assert_eq!(two.entries.len(), 3, "{:?}", two.entries);
    let file = &two.entries[0];
    assert!(file["addr"].is_null(), "{file}");
    assert_eq!(file["file"], "d/BUILD", "{file}");
    assert_eq!(file["root_file"], "d/BUILD", "{file}");
    assert!(
        file["root"].is_null(),
        "a file culprit has no `root`: {file}"
    );
    assert_eq!(file["selected"], false, "{file}");
    assert_eq!(file["path"], serde_json::json!(["//d:t1"]), "{file}");
    for t in ["//d:t1", "//d:t2"] {
        let e = two.entry(t);
        assert_eq!(e["outcome"], "skipped", "{e}");
        assert_eq!(e["root_file"], "d/BUILD", "{e}");
        assert!(e["root"].is_null(), "{e}");
        assert_eq!(e["selected"], true, "{e}");
        assert_eq!(e["path"], serde_json::json!([t]), "{e}");
        assert!(e["error"].is_null(), "{e}");
    }

    let one = run_json(&engine, &addr("//d:t1"), &["--json"], None).await;
    assert!(one.closed, "{:?}", one.entries);
    assert_eq!(one.entries.len(), 2, "{:?}", one.entries);
    let file = &one.entries[0];
    assert!(file["addr"].is_null(), "{file}");
    assert_eq!(file["file"], "d/BUILD", "{file}");
    assert_eq!(file["root_file"], "d/BUILD", "{file}");
    assert!(
        file["root"].is_null(),
        "a file culprit has no `root`: {file}"
    );
    assert_eq!(file["outcome"], "failed", "{file}");
    assert_eq!(file["selected"], false, "{file}");
    assert_eq!(file["path"], serde_json::json!(["//d:t1"]), "{file}");
    assert!(
        file["error"].as_str().is_some_and(|e| !e.contains('\n')),
        "{file}"
    );
    let t1 = &one.entries[1];
    assert_eq!(t1["addr"], "//d:t1", "{t1}");
    assert_eq!(t1["outcome"], "skipped", "{t1}");
    assert_eq!(t1["root_file"], "d/BUILD", "{t1}");
    assert!(t1["root"].is_null(), "{t1}");
    assert_eq!(t1["selected"], true, "{t1}");
    assert_eq!(t1["path"], serde_json::json!(["//d:t1"]), "{t1}");
}

/// I6: `--json` reaches no cache key, so the plain run after it is a local
/// hit and runs nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_flag_keeps_cache_hit() {
    let root = tempfile::tempdir().expect("root tempdir");
    write_pkg(root.path(), "p", BUILD);
    let engine = build_engine_with_remote(root.path(), None);

    let first = run_json(
        &engine,
        &addr("//p:ok"),
        &["--fields", "+outputs,+hashin"],
        None,
    )
    .await;
    assert!(first.res.is_ok(), "{:?}", first.res);
    assert_eq!(first.entry("//p:ok")["outcome"], "ran");

    let (res, events) = run_drain_events(&engine, &Matcher::Package(PkgBuf::from("p"))).await;
    drop(res);
    let ok_outcomes: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            BuildEventKind::CacheOutcome { addr, outcome, .. } if addr == "//p:ok" => {
                Some(*outcome)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        ok_outcomes,
        [hcore::events::CacheOutcomeKind::LocalHit],
        "{events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(
            &e.kind,
            BuildEventKind::ExecuteStart { addr, .. } if addr == "//p:ok"
        )),
        "the plain run re-ran //p:ok"
    );
}

/// A bash target named `name`, with `extra` keyword arguments.
fn bash(name: &str, extra: &str) -> String {
    format!(
        r#"target(name = "{name}", driver = "bash", run = "echo ok > $OUT", out = "o.txt"{extra})"#
    )
}

/// D5: a culprit the selection did not name still has its record, with
/// `selected: false`, and the selected target it took down names it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unselected_culprit_emitted_selected_false() {
    let root = tempfile::tempdir().expect("root tempdir");
    write_pkg(root.path(), "p", BUILD);
    let engine = build_engine_with_remote(root.path(), None);

    let run = run_json(&engine, &addr("//p:blk"), &["--json"], None).await;
    assert!(run.res.is_err(), "a dependency failed: the run fails");
    assert!(run.closed, "{:?}", run.entries);
    assert_eq!(run.entries.len(), 2, "{:?}", run.entries);
    let bad = run.entry("//p:bad");
    assert_eq!(keys(bad), DEFAULT_KEYS, "{bad}");
    assert_eq!(bad["outcome"], "failed", "{bad}");
    assert_eq!(bad["selected"], false, "not named by the selection: {bad}");
    assert_eq!(bad["root"], "//p:bad", "{bad}");
    assert_eq!(
        bad["path"],
        serde_json::json!(["//p:blk", "//p:bad"]),
        "{bad}"
    );
    assert_eq!(bad["exit_code"], 3, "{bad}");
    assert!(bad["failure"]["log"].is_string(), "{bad}");
    let blk = run.entry("//p:blk");
    assert_eq!(blk["outcome"], "skipped", "{blk}");
    assert_eq!(blk["selected"], true, "{blk}");
    assert_eq!(blk["root"], "//p:bad", "{blk}");
}

/// D5 / U2: `//a:t1` and `//a:t2` both reach `//d:t` through `//b:t` and
/// `//c:t`, and `d/BUILD` does not parse. One record for the file:
/// `addr: null`, `file` set, its own root, never selected, with the path
/// from the smaller requested address to the address that led into it. Each
/// requested target is skipped because of the file, with its full path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_culprit_record_shape() {
    let root = tempfile::tempdir().expect("root tempdir");
    let dep = |d: &str| format!(r#", deps = ["{d}"]"#);
    write_pkg(
        root.path(),
        "a",
        &format!(
            "{}\n{}\n",
            bash("t1", &dep("//b:t")),
            bash("t2", &dep("//b:t"))
        ),
    );
    write_pkg(root.path(), "b", &bash("t", &dep("//c:t")));
    write_pkg(root.path(), "c", &bash("t", &dep("//d:t")));
    write_pkg(root.path(), "d", "target(name = \"t\"\nbroken(\n");
    let engine = build_engine_with_remote(root.path(), None);

    let m = Matcher::Or(vec![addr("//a:t1"), addr("//a:t2")]);
    let run = run_json(&engine, &m, &["--json"], None).await;
    assert!(run.res.is_err(), "the run fails");
    assert!(run.closed, "{:?}", run.entries);
    assert_eq!(
        run.entries.len(),
        3,
        "the file and the two requested: {:?}",
        run.entries
    );
    let file = &run.entries[0];
    assert_eq!(keys(file), DEFAULT_KEYS, "{file}");
    assert!(
        file["addr"].is_null(),
        "a path never goes in `addr`: {file}"
    );
    assert_eq!(file["file"], "d/BUILD", "{file}");
    assert_eq!(file["outcome"], "failed", "{file}");
    assert_eq!(
        file["root_file"], "d/BUILD",
        "a culprit is its own root: {file}"
    );
    assert!(file["root"].is_null(), "{file}");
    assert_eq!(file["selected"], false, "a file is never selected: {file}");
    assert_eq!(
        file["path"],
        serde_json::json!(["//a:t1", "//b:t", "//c:t", "//d:t"]),
        "{file}"
    );
    assert!(
        file["error"]
            .as_str()
            .is_some_and(|e| !e.is_empty() && !e.contains('\n')),
        "one line: {file}"
    );
    for null in ["exit_code", "cache", "remote", "more", "failure"] {
        assert!(file[null].is_null(), "{null}: {file}");
    }
    assert_eq!(run.box_count(), 1, "{}", run.boxes);
    for t in ["//a:t1", "//a:t2"] {
        let e = run.entry(t);
        assert_eq!(keys(e), DEFAULT_KEYS, "{e}");
        assert_eq!(e["outcome"], "skipped", "{e}");
        assert_eq!(e["selected"], true, "{e}");
        assert_eq!(e["root_file"], "d/BUILD", "{e}");
        assert!(e["root"].is_null(), "{e}");
        assert_eq!(
            e["path"],
            serde_json::json!([t, "//b:t", "//c:t", "//d:t"]),
            "{e}"
        );
        assert_eq!(e["more"], 0, "{e}");
        assert!(e["file"].is_null(), "{e}");
        assert!(e["error"].is_null(), "{e}");
    }
}

/// D5 under a package matcher: `//q:a` and `//q:b` both depend on `//d:t`,
/// and `d/BUILD` does not parse. Their defs fail before a result scope
/// opens, on another culprit. The file's record, then both selected targets
/// skipped because of it, and the list closed: every requested target is
/// accounted for. The text run prints one box, the file's. The same
/// selection by address takes the same path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn package_matcher_broken_dep_file() {
    let root = tempfile::tempdir().expect("root tempdir");
    let dep = r#", deps = ["//d:t"]"#;
    write_pkg(
        root.path(),
        "q",
        &format!("{}\n{}\n", bash("a", dep), bash("b", dep)),
    );
    write_pkg(root.path(), "d", "target(name = \"t\"\nbroken(\n");
    let engine = build_engine_with_remote(root.path(), None);

    for m in [
        Matcher::Package(PkgBuf::from("q")),
        Matcher::Or(vec![addr("//q:a"), addr("//q:b")]),
    ] {
        let run = run_json(&engine, &m, &["--json"], None).await;
        let shown: Vec<String> = run.events.iter().map(|e| format!("{:?}", e.kind)).collect();
        assert!(run.res.is_err(), "{m:?}: the run fails");
        assert!(
            run.closed,
            "{m:?}: every requested target is accounted for: {:?}\nevents: {shown:#?}",
            run.entries
        );
        assert_eq!(run.entries.len(), 3, "{m:?}: {:?}", run.entries);
        let file = &run.entries[0];
        assert!(file["addr"].is_null(), "{m:?}: {file}");
        assert_eq!(file["file"], "d/BUILD", "{m:?}: {file}");
        assert_eq!(file["root_file"], "d/BUILD", "{m:?}: {file}");
        assert!(file["root"].is_null(), "{m:?}: {file}");
        assert_eq!(file["outcome"], "failed", "{m:?}: {file}");
        assert_eq!(file["selected"], false, "{m:?}: {file}");
        for t in ["//q:a", "//q:b"] {
            let e = run.entry(t);
            assert_eq!(e["outcome"], "skipped", "{m:?}: {e}");
            assert_eq!(e["selected"], true, "{m:?}: {e}");
            assert_eq!(e["root_file"], "d/BUILD", "{m:?}: {e}");
            assert!(e["root"].is_null(), "{m:?}: {e}");
            assert_eq!(e["path"], serde_json::json!([t, "//d:t"]), "{m:?}: {e}");
            assert_eq!(e["more"], 0, "{m:?}: {e}");
            assert!(e["error"].is_null(), "{m:?}: {e}");
        }
        assert!(run.complete.is_ok(), "{m:?}: {:?}", run.complete);
        assert_eq!(
            run.box_count(),
            1,
            "{m:?}: one box, the file's: {}",
            run.boxes
        );
    }
}

/// A BUILD file that does not parse, for the keep-going blame rows below.
const BROKEN_BUILD: &str = "target(name = \"t\"\nbroken(\n";

/// Whether any `FileFailed` was announced for `file`.
fn file_failed(run: &JsonRun, file: &str) -> usize {
    run.events
        .iter()
        .filter(|e| matches!(&e.kind, BuildEventKind::FileFailed { file: f, .. } if f == file))
        .count()
}

/// U2 under a selection that does not name its targets only by address:
/// `//d:t` beside a package or a label. What else `d` held is unknown, so
/// its gap is kept (the list stays open, the selection is incomplete) and
/// nothing is recorded for the named address, which never had a scope. The
/// healthy part of the selection still runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broken_named_package_beside_a_package_or_label_keeps_its_gap() {
    let root = tempfile::tempdir().expect("root tempdir");
    write_pkg(root.path(), "q", &bash("ok", r#", labels = ["x"]"#));
    write_pkg(root.path(), "d", BROKEN_BUILD);
    let engine = build_engine_with_remote(root.path(), None);

    for m in [
        Matcher::Or(vec![addr("//d:t"), Matcher::Package(PkgBuf::from("q"))]),
        Matcher::Or(vec![addr("//d:t"), Matcher::Label("x".into())]),
    ] {
        let run = run_json(&engine, &m, &["--json"], None).await;
        assert!(!run.closed, "{m:?}: the gap leaves the list open");
        let skipped = run
            .complete
            .as_ref()
            .expect_err("the selection is incomplete");
        assert!(
            skipped
                .downcast_ref::<heph::commands::errors::IncompleteSelection>()
                .is_some(),
            "{m:?}: {skipped:#}"
        );
        // The second selection finds the first one's output in the cache.
        let ok = &run.entry("//q:ok")["outcome"];
        assert!(ok == "ran" || ok == "cached", "{m:?}: {ok}");
        assert!(
            run.entries
                .iter()
                .all(|e| e["addr"] != "//d:t" && e["file"].is_null()),
            "{m:?}: nothing recorded for //d:t: {:?}",
            run.entries
        );
        assert_eq!(file_failed(&run, "d/BUILD"), 0, "{m:?}");
    }
}

/// U2 with a healthy named sibling: `//d:t` in a broken package and
/// `//q:ok`. The sibling runs, the file is recorded once and takes `//d:t`
/// down, and every named address is accounted for: the list closes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broken_named_package_beside_a_healthy_named_sibling() {
    let root = tempfile::tempdir().expect("root tempdir");
    write_pkg(root.path(), "q", &bash("ok", ""));
    write_pkg(root.path(), "d", BROKEN_BUILD);
    let engine = build_engine_with_remote(root.path(), None);

    let m = Matcher::Or(vec![addr("//d:t"), addr("//q:ok")]);
    let run = run_json(&engine, &m, &["--json"], None).await;
    assert!(run.closed, "{:?}", run.entries);
    assert!(run.complete.is_ok(), "{:?}", run.complete);
    assert!(run.res.is_err(), "the run fails");
    assert_eq!(run.entry("//q:ok")["outcome"], "ran");
    let files: Vec<&Value> = run
        .entries
        .iter()
        .filter(|e| e["file"] == "d/BUILD")
        .collect();
    assert_eq!(files.len(), 1, "one file record: {:?}", run.entries);
    assert_eq!(file_failed(&run, "d/BUILD"), 1);
    let t = run.entry("//d:t");
    assert_eq!(t["outcome"], "skipped", "{t}");
    assert_eq!(t["root_file"], "d/BUILD", "{t}");
    assert_eq!(run.entries.len(), 3, "{:?}", run.entries);
    assert_eq!(run.box_count(), 1, "{}", run.boxes);
}

/// U2 for an address the broken package may not even declare: whether it
/// exists is unknowable while the file does not evaluate, so it is the
/// file's, as a declared one would be, and the list closes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broken_named_package_missing_address_is_the_files() {
    let root = tempfile::tempdir().expect("root tempdir");
    write_pkg(root.path(), "d", BROKEN_BUILD);
    let engine = build_engine_with_remote(root.path(), None);

    let run = run_json(&engine, &addr("//d:nope"), &["--json"], None).await;
    assert!(run.closed, "{:?}", run.entries);
    assert!(run.complete.is_ok(), "{:?}", run.complete);
    assert!(run.res.is_err(), "the run fails");
    assert_eq!(run.entries.len(), 2, "{:?}", run.entries);
    assert_eq!(run.entries[0]["file"], "d/BUILD", "{:?}", run.entries);
    let t = run.entry("//d:nope");
    assert_eq!(t["outcome"], "skipped", "{t}");
    assert_eq!(t["root_file"], "d/BUILD", "{t}");
    assert_eq!(file_failed(&run, "d/BUILD"), 1);
}

/// A request cancelled before its walk blames no file: the run is a
/// cancel, nothing is recorded, nothing announced, and the list is open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broken_named_package_cancelled_before_the_walk() {
    let root = tempfile::tempdir().expect("root tempdir");
    write_pkg(root.path(), "d", BROKEN_BUILD);
    let engine = build_engine_with_remote(root.path(), None);

    let run = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run_json_full(&engine, &addr("//d:t"), &["--json"], None, false, true),
    )
    .await
    .expect("the cancelled run ends");
    let err = run.res.as_ref().expect_err("cancelled");
    assert!(heph::commands::errors::is_cancelled(err), "{err:#}");
    assert!(!run.closed, "a cancelled run's list is not whole");
    assert!(run.entries.is_empty(), "{:?}", run.entries);
    assert_eq!(file_failed(&run, "d/BUILD"), 0);
    assert_eq!(run.box_count(), 0, "{}", run.boxes);
}

/// D5: whichever order the selection names them in, a culprit's record is
/// written before the records of the targets it took down: a reader that
/// meets a `root` has already read it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn culprit_record_precedes_its_skipped_records() {
    let root = tempfile::tempdir().expect("root tempdir");
    write_pkg(root.path(), "p", BUILD);
    let engine = build_engine_with_remote(root.path(), None);

    for m in [
        Matcher::Or(vec![addr("//p:blk"), addr("//p:bad")]),
        Matcher::Or(vec![addr("//p:bad"), addr("//p:blk")]),
        addr("//p:blk"),
    ] {
        let run = run_json(&engine, &m, &["--json"], None).await;
        assert!(run.closed, "{m:?}: {:?}", run.entries);
        let id = |e: &Value| {
            e["addr"]
                .as_str()
                .or_else(|| e["file"].as_str())
                .map(str::to_owned)
        };
        let seen: Vec<_> = run.entries.iter().filter_map(id).collect();
        for e in &run.entries {
            if e["outcome"] != "skipped" {
                continue;
            }
            let root = e["root"]
                .as_str()
                .or_else(|| e["root_file"].as_str())
                .expect("a skipped record names its root");
            let me = id(e).expect("an id");
            let at = |x: &str| seen.iter().position(|s| s == x);
            assert!(
                at(root).is_some_and(|r| Some(r) < at(&me)),
                "{m:?}: {root} before {me}: {seen:?}"
            );
        }
        assert!(
            run.entries.iter().any(|e| e["outcome"] == "skipped"),
            "{m:?}: {:?}",
            run.entries
        );
    }
}

/// U3: a 500-deep chain to a culprit. The text box elides the path; the
/// JSON record carries all of it: 501 entries, from the requested target
/// to the culprit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_chain_elided_in_text_full_in_json() {
    const DEPTH: usize = 500;
    let root = tempfile::tempdir().expect("root tempdir");
    let mut build = bash("leaf", r#", deps = [":missing"]"#);
    for i in 0..DEPTH {
        let dep = if i + 1 == DEPTH {
            "leaf".to_string()
        } else {
            format!("n{:03}", i + 1)
        };
        build.push('\n');
        build.push_str(&bash(
            &format!("n{i:03}"),
            &format!(r#", deps = [":{dep}"]"#),
        ));
    }
    write_pkg(root.path(), "a", &build);
    let engine = build_engine_with_remote(root.path(), None);

    let run = run_json(&engine, &addr("//a:n000"), &["--json"], None).await;
    assert!(run.closed, "{:?}", run.entries);
    let leaf = run.entry("//a:leaf");
    let path = leaf["path"].as_array().expect("a path");
    assert_eq!(path.len(), DEPTH + 1, "{leaf}");
    assert_eq!(path.first().and_then(Value::as_str), Some("//a:n000"));
    assert_eq!(path.last().and_then(Value::as_str), Some("//a:leaf"));
    let top = run.entry("//a:n000");
    assert_eq!(top["path"], leaf["path"], "{top}");

    assert_eq!(run.box_count(), 1, "{}", run.boxes);
    assert!(
        run.boxes.contains(
            "reached by: //a:n000 → //a:n001 → … 497 more (--json has the full path) … → //a:n499 → //a:leaf"
        ),
        "{}",
        run.boxes
    );
    assert!(!run.boxes.contains("//a:n250"), "elided: {}", run.boxes);
}

/// GRAPH-07: a target that depends on itself is a failure, never a green
/// run. Its end carries the cycle as its own error (not a stop, not
/// collateral), the TUI's fold counts it as one root, and the text run
/// prints one box for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn self_cycle_counts_as_failed() {
    let root = tempfile::tempdir().expect("root tempdir");
    write_pkg(root.path(), "c", &bash("me", r#", deps = [":me"]"#));
    let engine = build_engine_with_remote(root.path(), None);

    let run = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run_json(&engine, &addr("//c:me"), &["--json"], None),
    )
    .await
    .expect("a self-cycle is detected, not a hang");
    assert!(run.res.is_err(), "a cycle fails the run");
    let ends: Vec<_> = run
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            BuildEventKind::ResultEnd {
                addr,
                error,
                cancelled,
                upstream_of,
                duplicate,
                ..
            } if addr == "//c:me" => {
                Some((error.is_some(), *cancelled, upstream_of.clone(), *duplicate))
            }
            _ => None,
        })
        .collect();
    assert!(
        ends.contains(&(true, false, None, false)),
        "an own failure's end: {ends:?}"
    );
    let mut fold = heph::tui::progress::BuildState::new();
    for ev in &run.events {
        fold.apply(ev);
    }
    assert!(fold.unsuccessful(), "never drawn green");
    assert_eq!(
        fold.failure_split(),
        (1, 0),
        "one root, nothing it took down"
    );
    assert_eq!(run.box_count(), 1, "{}", run.boxes);
    assert!(run.boxes.contains("//c:me"), "{}", run.boxes);
}
