//! One failure, every renderer: the CI box, the interactive report,
//! `run --json` and `inspect failure --json` say the same thing about it, in
//! heph's words rather than its internal frames (F2, RUN-14/15/16/23,
//! CLI-13/14/20).
#![expect(
    clippy::panic,
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::common::Workspace;
use crate::run_json::{JsonRun, run_json};
use heph::commands::inspect::failure::{FailureView, inspect_view, print_to};
use heph::engine::Engine;
use heph::htaddr::parse_addr;
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;
use serde_json::Value;

fn addr(a: &str) -> Matcher {
    Matcher::Addr(parse_addr(a).expect("addr"))
}

fn package(p: &str) -> Matcher {
    Matcher::Package(PkgBuf::from(p))
}

/// `heph inspect failure --json <addr>`, parsed.
fn inspect_json(engine: &Engine, a: &str) -> anyhow::Result<Value> {
    let a = parse_addr(a)?;
    let view = inspect_view(engine, &a)?;
    let json = output::JsonArgs::<FailureView>::try_parse_from(["--json"])?;
    let mut buf = Vec::new();
    print_to(&json, &a, view.as_ref(), &mut buf, output::Style::Compact)?;
    Ok(serde_json::from_slice(&buf)?)
}

/// The sorted keys of a JSON object.
fn keys(v: &Value) -> Vec<String> {
    let mut k: Vec<String> = v
        .as_object()
        .unwrap_or_else(|| panic!("not an object: {v}"))
        .keys()
        .cloned()
        .collect();
    k.sort_unstable();
    k
}

/// The internal stage frames F2 names: none may open or sit inside a
/// rendered message.
const META_FRAMES: [&str; 7] = [
    "meta:",
    "link:",
    "get_def:",
    "get spec:",
    "driver run:",
    "execute //",
    "dependency failed (root:",
];

fn assert_no_meta_frames(what: &str, text: &str) {
    for f in META_FRAMES {
        assert!(!text.contains(f), "{what} has the frame {f:?}:\n{text}");
    }
}

/// F2 / STAGE-24: a run failure two deps deep, a driver option error and a
/// BUILD file that does not evaluate, each under a requester. No `error` in
/// `run --json` carries an internal frame; a skipped requester's names its
/// culprit as `blocked by`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn json_error_has_no_meta_frames() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "c",
        r#"target(name = "leaf", driver = "bash", out = "o.txt", run = "echo leaf about to fail >&2; exit 3")"#,
    );
    ws.write_build_file(
        "b",
        r#"
target(name = "mid", driver = "bash", out = "o.txt", deps = ["//c:leaf"], run = "true > o.txt")
target(name = "opt", driver = "bash", out = "o.txt", run = 1)
"#,
    );
    ws.write_build_file("e", "target(name = \"x\"\nbroken(\n");
    ws.write_build_file(
        "a",
        r#"
target(name = "top", driver = "bash", out = "o.txt", deps = ["//b:mid"], run = "true > o.txt")
target(name = "top_opt", driver = "bash", out = "o.txt", deps = ["//b:opt"], run = "true > o.txt")
target(name = "top_eval", driver = "bash", out = "o.txt", deps = ["//e:x"], run = "true > o.txt")
"#,
    );
    let run = run_json(&ws.engine, &package("a"), &["--json"], None).await;
    assert!(run.res.is_err());
    assert!(run.entries.len() >= 6, "{:?}", run.entries);
    for e in &run.entries {
        let error = e["error"].as_str().unwrap_or_default();
        assert_no_meta_frames(&format!("{}'s error", e["addr"]), error);
        // Contract 2: a skipped record names its culprit as `root` (or
        // `root_file`), and its `error` is null rather than a frame.
        if e["outcome"] == "skipped" {
            assert!(
                e["root"].is_string() || e["root_file"].is_string(),
                "a skipped record names its root: {e}"
            );
            assert_eq!(e["error"], Value::Null, "{e}");
        }
    }
    assert_no_meta_frames("the CI boxes", &run.boxes);
    assert_no_meta_frames("the report", &run.report);
    Ok(())
}

/// CLI-13/14/20: a dependency that fails in its run, in its options or in
/// its BUILD file gives its requester the same record: `blocked`, its root
/// named, the same keys; and the culprit its own record, again with the
/// same keys whichever stage it failed at.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn json_same_keys_for_run_eval_option_failures() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "c",
        r#"target(name = "leaf", driver = "bash", out = "o.txt", run = "exit 3")"#,
    );
    ws.write_build_file(
        "b",
        r#"target(name = "mid", driver = "bash", out = "o.txt", run = 1)"#,
    );
    ws.write_build_file("e", "target(name = \"x\"\nbroken(\n");
    ws.write_build_file(
        "a",
        r#"
target(name = "top_run", driver = "bash", out = "o.txt", deps = ["//c:leaf"], run = "true > o.txt")
target(name = "top_opt", driver = "bash", out = "o.txt", deps = ["//b:mid"], run = "true > o.txt")
target(name = "top_eval", driver = "bash", out = "o.txt", deps = ["//e:x"], run = "true > o.txt")
"#,
    );
    let run = run_json(&ws.engine, &package("a"), &["--json"], None).await;
    assert!(run.res.is_err());
    let tops = ["//a:top_run", "//a:top_opt", "//a:top_eval"];
    let want = keys(run.entry(tops[0]));
    for (top, root, stage) in [
        (tops[0], "//c:leaf", "run"),
        (tops[1], "//b:mid", "parse"),
        (tops[2], "e/BUILD", "eval"),
    ] {
        let e = run.entry(top);
        assert_eq!(keys(e), want, "{top}: {e}");
        assert_eq!(e["outcome"], "skipped", "{top}: {e}");
        let named = if root.contains('/') && !root.starts_with("//") {
            &e["root_file"]
        } else {
            &e["root"]
        };
        assert_eq!(named, root, "{top}: {e}");
        let culprit = run
            .entries
            .iter()
            .find(|c| c["outcome"] == "failed" && (c["addr"] == root || c["file"] == root))
            .unwrap_or_else(|| panic!("{root} has its own record: {:?}", run.entries));
        assert_eq!(keys(culprit), want, "{root}: {culprit}");
        assert_eq!(culprit["stage"], stage, "{root}: {culprit}");
        assert_eq!(culprit["selected"], false, "{root}: {culprit}");
    }
    Ok(())
}

/// What each renderer says about `culprit`: its CI box, its report block,
/// its `run --json` record and `inspect failure --json`.
struct Rendered {
    run: JsonRun,
    inspect: Value,
}

async fn render(ws: &Workspace, culprit: &str) -> anyhow::Result<Rendered> {
    let run = run_json(&ws.engine, &addr(culprit), &["--json"], None).await;
    assert!(run.res.is_err(), "{culprit} must fail");
    ws.engine.failure_slots().writes_drained().await;
    let inspect = inspect_json(&ws.engine, culprit)?;
    Ok(Rendered { run, inspect })
}

/// RUN-14: a run that outlasts its `timeout` reads as timed out in the box,
/// the report, `run --json` (`timed_out: true`) and `inspect failure`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timeout_named_in_every_renderer() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "a",
        r#"target(name = "to", driver = "bash", out = "o", timeout = "1s",
       run = "echo starting; sleep 30; echo never > o")"#,
    );
    let r = render(&ws, "//a:to").await?;
    let title = "//a:to · timed out after 1s";
    assert!(r.run.boxes.contains(title), "{}", r.run.boxes);
    assert!(r.run.report.contains(title), "{}", r.run.report);
    let e = r.run.entry("//a:to");
    assert_eq!(e["timed_out"], true, "{e}");
    // One wording everywhere, the box header's.
    let said = "timed out after 1s (its timeout)";
    assert_eq!(e["error"], said, "{e}");
    assert_eq!(r.inspect["message"], said, "{}", r.inspect);
    Ok(())
}

/// RUN-15: a process killed by a signal is named by number and name in the
/// box, the report, `run --json` and `inspect failure`; not as a bare
/// `signal: 9` anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signal_named_in_every_renderer() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "a",
        r#"target(name = "k9", driver = "bash", out = "o", run = "echo before-kill; kill -9 $$")"#,
    );
    let r = render(&ws, "//a:k9").await?;
    let named = "killed by signal 9 (SIGKILL)";
    assert!(r.run.boxes.contains(named), "{}", r.run.boxes);
    assert!(r.run.report.contains(named), "{}", r.run.report);
    let e = r.run.entry("//a:k9");
    assert_eq!(e["error"], named, "{e}");
    assert_eq!(e["exit_code"], Value::Null, "{e}");
    assert_eq!(r.inspect["message"], named, "{}", r.inspect);
    Ok(())
}

/// RUN-16: a chain that built green, then its leaf breaks. The rerun fails
/// the leaf, and its dependents are skipped because of it, not served from
/// the cache they hit last time; a sibling dependency that did not change
/// is still a hit. A skipped target keeps no record of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rerun_after_dep_breaks_is_skipped_not_stale() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let build = |leaf: &str| {
        format!(
            r#"
target(name = "leaf", driver = "bash", out = "leaf.txt", run = "{leaf}")
target(name = "other", driver = "bash", out = "other.txt", run = "echo other > other.txt")
target(name = "mid", driver = "bash", out = "mid.txt", deps = [":leaf"], run = "echo mid > mid.txt")
target(name = "top", driver = "bash", out = "top.txt", deps = [":mid", ":other"], run = "echo top > top.txt")
"#
        )
    };
    ws.write_build_file("a", &build("echo leaf > leaf.txt"));
    let green = run_json(&ws.engine, &package("a"), &["--json"], None).await;
    assert!(green.res.is_ok(), "{:?}", green.entries);

    ws.write_build_file("a", &build("echo broken >&2; exit 5"));
    let engine = ws.reopen()?;
    let run = run_json(&engine, &package("a"), &["--json"], None).await;
    assert!(run.res.is_err());
    assert_eq!(
        run.entry("//a:other")["outcome"],
        "cached",
        "{:?}",
        run.entries
    );
    assert_eq!(run.entry("//a:leaf")["outcome"], "failed");
    assert_eq!(run.entry("//a:leaf")["exit_code"], 5);
    for skipped in ["//a:mid", "//a:top"] {
        let e = run.entry(skipped);
        assert_eq!(e["outcome"], "skipped", "{skipped}: {e}");
        assert_eq!(e["root"], "//a:leaf", "{skipped}: {e}");
        assert_eq!(
            e["cache"],
            Value::Null,
            "{skipped} is not served stale: {e}"
        );
    }
    assert_eq!(run.box_count(), 1, "one box, the leaf's:\n{}", run.boxes);
    assert!(run.boxes.contains("✗ //a:leaf"), "{}", run.boxes);
    engine.failure_slots().writes_drained().await;
    for skipped in ["//a:mid", "//a:top"] {
        assert_eq!(inspect_json(&engine, skipped)?, Value::Null, "{skipped}");
    }
    Ok(())
}

/// RUN-23: a failing dependency with `cache = False` reads exactly as a
/// cached one does, in every renderer: the same box, the same report, the
/// same records, and a slot with its log. Its record's `cache` alone differs:
/// `none` where the cached one's is `miss`, never null (U22).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uncached_dep_failure_reads_as_cached_one_but_cache_none() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "a",
        r#"
target(name = "nc_fail", driver = "bash", out = "o", cache = False, run = "echo dep failing >&2; exit 2")
target(name = "nc_top", driver = "bash", out = "o", deps = [":nc_fail"], run = "true > o")
target(name = "ca_fail", driver = "bash", out = "o", run = "echo dep failing >&2; exit 2")
target(name = "ca_top", driver = "bash", out = "o", deps = [":ca_fail"], run = "true > o")
"#,
    );
    let mut seen = Vec::new();
    for prefix in ["nc", "ca"] {
        let top = format!("//a:{prefix}_top");
        let fail = format!("//a:{prefix}_fail");
        let run = run_json(&ws.engine, &addr(&top), &["--json"], None).await;
        assert!(run.res.is_err());
        ws.engine.failure_slots().writes_drained().await;
        let inspect = inspect_json(&ws.engine, &fail)?;
        assert_eq!(inspect["log_lines"], 1, "{fail} keeps its log: {inspect}");
        let want = if prefix == "nc" { "none" } else { "miss" };
        assert_eq!(run.entry(&fail)["cache"], want, "{fail}: {:?}", run.entries);
        // The slot's directory is keyed by the address, so its path differs
        // by name alone: the `full log:` row is left out.
        let neutral = |s: &str| {
            s.lines()
                .filter(|l| !l.contains("full log:"))
                .collect::<Vec<_>>()
                .join("\n")
                .replace(&format!("{prefix}_"), "X_")
        };
        let records: Vec<Value> = run
            .entries
            .iter()
            .map(|e| {
                let mut e = e.clone();
                e["duration_ms"] = Value::Null;
                // Asserted above, the one field that differs.
                e["cache"] = Value::Null;
                if e["failure"].is_object() {
                    e["failure"]["log"] = Value::Null;
                }
                serde_json::from_str(&neutral(&e.to_string())).expect("json")
            })
            .collect();
        let view = [
            "stage",
            "message",
            "location",
            "hints",
            "truncated",
            "log_lines",
        ]
        .map(|k| inspect[k].clone());
        seen.push((neutral(&run.boxes), neutral(&run.report), records, view));
    }
    let [uncached, cached] = <[_; 2]>::try_from(seen).expect("two runs");
    assert_eq!(uncached.0, cached.0, "the CI boxes");
    assert_eq!(uncached.1, cached.1, "the reports");
    assert_eq!(uncached.2, cached.2, "the run --json records");
    assert_eq!(uncached.3, cached.3, "inspect failure");
    Ok(())
}
