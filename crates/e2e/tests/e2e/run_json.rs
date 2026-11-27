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
const DEFAULT_KEYS: [&str; 14] = [
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
struct JsonRun {
    entries: Vec<Value>,
    /// The list got its `]`.
    closed: bool,
    /// The run's own result, which `finalize` turns into the exit code.
    res: anyhow::Result<()>,
    /// Every event the display saw.
    events: Vec<heph::engine::event::BuildEvent>,
    /// What `finalize` makes of the walk's skips: an error when it skipped
    /// part of the selection.
    complete: anyhow::Result<()>,
}

impl JsonRun {
    fn entry(&self, addr: &str) -> &Value {
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
async fn run_json(
    engine: &Arc<Engine>,
    m: &Matcher,
    flags: &[&str],
    cancel_at: Option<&str>,
) -> JsonRun {
    let args = RunArgs::try_parse_from(flags).expect("run flags");
    let out = &args.out;
    assert!(out.enabled(), "{flags:?}");
    // The display's end of the fold: it sees every event, after the fold.
    let (forward, mut shown) =
        tokio::sync::mpsc::unbounded_channel::<heph::engine::event::BuildEvent>();
    let (tx, rx) = request_events(out.enabled(), Some(forward.clone()));
    let rx = rx.expect("under an output flag the request gets its own channel");
    let rs = engine.new_state_with_events(false, tx);
    let gaps = Gaps::new("test");
    let opts = ResultOptions {
        discovery: Discovery::keep_going_unless(false, &gaps),
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
    for null in ["error", "failure", "remote", "file", "root", "more", "path"] {
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

/// The duplicate-end risk: two requested targets of one broken `d/BUILD`.
/// The walk probes `d` and, keep-going, records a gap: no end and no record
/// for either, and the list is left open (D10), so no duplicate end can keep
/// it pending. One requested target is the file's entry: the file's record
/// (`addr: null`), then the target skipped because of it, closed.
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
    assert!(!two.closed, "a gap leaves the list open: {:?}", two.entries);
    assert!(two.entries.is_empty(), "{:?}", two.entries);
    assert!(two.complete.is_err(), "the walk skipped `d`");

    let one = run_json(&engine, &addr("//d:t1"), &["--json"], None).await;
    assert!(one.closed, "{:?}", one.entries);
    assert_eq!(one.entries.len(), 2, "{:?}", one.entries);
    let file = &one.entries[0];
    assert!(file["addr"].is_null(), "{file}");
    assert_eq!(file["file"], "d/BUILD", "{file}");
    assert_eq!(file["root"], "d/BUILD", "{file}");
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
    assert_eq!(t1["root"], "d/BUILD", "{t1}");
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
