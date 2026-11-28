//! The interactive failure report (`ReportContext`) over real failures: an
//! engine runs targets that keep `failure_out` files, write `in_place` fixes,
//! are killed, time out or are abandoned, and the test reads the block each
//! one gets. The pure rendering rules are unit tests in
//! `src/commands/errors/report.rs`; this is what a real failure slot, fixes
//! list and sandbox fate make of them.
#![expect(
    clippy::panic,
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::common::Workspace;
use heph::commands::errors::ReportContext;
use heph::engine::{Engine, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use std::sync::Arc;

/// Resolve `addr`, which must fail, and render the report its request
/// would print: `took_down`, `was_cancelled` and the home all from the
/// request, as `finalize` gathers them.
async fn run_report(engine: &Arc<Engine>, addr: &str) -> anyhow::Result<String> {
    run_report_with(engine, addr, &ResultOptions::default()).await
}

/// [`run_report`] under `opts`.
async fn run_report_with(
    engine: &Arc<Engine>,
    addr: &str,
    opts: &ResultOptions,
) -> anyhow::Result<String> {
    let rs = engine.new_state();
    let res = Arc::clone(engine)
        .result_addr(rs.clone(), &parse_addr(addr)?, OutputMatcher::All, opts)
        .await;
    assert!(res.is_err(), "{addr} must fail");
    let failures = rs.take_failures_with_logs().await;
    let home = rs.heph_home();
    Ok(ReportContext::gather(
        &failures,
        |a| rs.was_cancelled(a),
        home.as_deref(),
        rs.log_tail_lines(),
    )
    .with_workspace(rs.workspace_root().as_deref())
    .with_width(WIDTH)
    .render(false))
}

/// The width every report here wraps at, whatever terminal ran the test.
const WIDTH: usize = 200;

/// The line of `report` whose row is `label` (`  label …`), or a panic
/// naming the report.
fn row<'a>(report: &'a str, label: &str) -> &'a str {
    report
        .lines()
        .find(|l| l.trim_start().starts_with(label))
        .unwrap_or_else(|| panic!("no {label:?} row:\n{report}"))
}

/// The lines listed under the row `label` (indented past it), trimmed.
fn listed<'a>(report: &'a str, label: &str) -> Vec<&'a str> {
    report
        .lines()
        .skip_while(|l| !l.trim_start().starts_with(label))
        .skip(1)
        .take_while(|l| l.starts_with("    "))
        .map(str::trim)
        .collect()
}

/// No line names the slot, its fixes list or a sandbox the report did not
/// keep: the only way into heph's home is a `heph` command.
fn names_no_home_path(engine: &Engine, addr: &str, report: &str) {
    let slot = engine
        .failure_slots()
        .slot_dir(&parse_addr(addr).expect("addr"));
    for needle in [
        "fixes.txt".to_owned(),
        slot.display().to_string(),
        engine.home.display().to_string(),
        "sandbox".to_owned(),
    ] {
        assert!(!report.contains(&needle), "{needle:?} in:\n{report}");
    }
}

/// The block ends with its log, named by `heph log <addr>`.
fn ends_with_log(report: &str, addr: &str) {
    let last = report.lines().last().unwrap_or_default();
    assert_eq!(last, format!("  log: heph log {addr}"), "{report}");
}

/// The pointer follows what `heph log` would find, not a path on disk: a
/// failure whose only log is a remote hit's (in the engine's index, not
/// fetched, no path anywhere) still ends with `heph log <addr>`, and one
/// with no log at all says "none kept" — in the report and in the CI box
/// alike, the box's other lines unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failure_report_points_at_any_log_the_engine_holds() -> anyhow::Result<()> {
    use crate::common::{build_engine, run_drain};
    use heph::commands::errors::FailedTargets;
    use heph::engine::error::TargetFailure;

    let remote = tempfile::tempdir()?;
    let uri = format!("file://{}", remote.path().display());
    let (producer, reader) = (tempfile::tempdir()?, tempfile::tempdir()?);
    let body = r#"target(name = "t", driver = "bash", out = "t.txt", run = "echo a log line; echo hi > t.txt")"#;
    for ws in [&producer, &reader] {
        crate::log_api::write_build(ws.path(), body)?;
    }
    run_drain(&build_engine(producer.path(), &uri), "//p:t").await;
    let engine = build_engine(reader.path(), &uri);
    run_drain(&engine, "//p:t").await;

    let rs = engine.new_state();
    for addr in ["//p:t", "//p:never"] {
        let a = parse_addr(addr)?;
        let failure = TargetFailure::new(a.clone());
        // The record the engine compacts from a run that failed: only a
        // failure in or after the run has a log to name, or none of.
        let mut record = failure.record.as_ref().clone();
        record.diagnostic.message = "boom".to_owned();
        record.failed_in = hcore::events::FailedIn::Exec;
        rs.record_failure(a, Arc::new(failure.with_record(Arc::new(record))));
    }
    let failures = rs.take_failures_with_logs().await;
    let readable: Vec<_> = failures
        .iter()
        .map(|f| (f.addr.format(), f.log_readable))
        .collect();
    assert_eq!(
        readable,
        vec![
            ("//p:t".to_owned(), Some(true)),
            ("//p:never".to_owned(), Some(false))
        ]
    );

    let report = ReportContext::gather(&failures, |_| false, None, rs.log_tail_lines())
        .with_width(WIDTH)
        .render(false);
    assert!(
        report.contains("\n✗ //p:t\n  boom\n  log: heph log //p:t\n"),
        "{report}"
    );
    assert!(
        report.contains("\n✗ //p:never\n  boom\n  log: none kept\n"),
        "{report}"
    );

    let mut boxes = Vec::new();
    drop(FailedTargets::reported(
        failures,
        None,
        &std::collections::HashSet::new(),
        &mut boxes,
        false,
    ));
    let boxes = String::from_utf8(boxes)?;
    assert!(
        boxes.starts_with("✗ //p:t\n  boom\n  log: heph log //p:t\n"),
        "{boxes}"
    );
    assert!(
        boxes.ends_with("\n✗ //p:never\n  boom\n  log: none kept\n"),
        "{boxes}"
    );
    Ok(())
}

/// A failed `in_place` target with `failure_out`: the block names the fixes
/// written, a restore command quoting a path with a space and a non-ASCII
/// one, and the kept files grouped with `heph inspect failure`; it ends with
/// `heph log`, and no line points into heph's home.
#[tokio::test]
async fn failure_report_failure_outputs() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "web",
        r#"
target(
    name = "lint",
    driver = "bash",
    deps = [file("a b.txt"), file("é.txt")],
    out = ["a b.txt", "é.txt"],
    codegen = "in_place",
    failure_out = ["report/*.txt", "summary.txt"],
    run = "for f in 'a b.txt' 'é.txt'; do up=$(tr a-z A-Z < \"$f\"); printf '%s' \"$up\" > \"$f\"; done; mkdir -p report; printf 1 > report/1.txt; printf 2 > report/2.txt; printf s > summary.txt; echo lint-marker; exit 1",
)
"#,
    );
    ws.write_file("web/a b.txt", "a");
    ws.write_file("web/é.txt", "e");

    let addr = "//web:lint";
    let report = run_report(&ws.engine, addr).await?;
    // The exit status read through the memoizer's shared wrapper, which
    // every recorded failure's source is.
    assert!(
        report.starts_with(&format!("\n✗ {addr} · exit status 1\n")),
        "{report}"
    );
    assert!(report.contains(" │ lint-marker\n"), "{report}");
    assert!(
        row(&report, "fixes written,").contains("target still failed (re-run to verify"),
        "{report}"
    );
    assert_eq!(
        listed(&report, "fixes written,"),
        ["web/a b.txt", "web/é.txt"],
        "{report}"
    );
    let restore = listed(&report, "restore with");
    assert!(
        matches!(restore.as_slice(), [git] if git
            .ends_with("--literal-pathspecs checkout -- 'web/a b.txt' 'web/é.txt'")),
        "{report}"
    );
    // Grouped from the package, not the workspace: `report/`, not `web/`.
    assert!(
        row(&report, "outputs:").ends_with(&format!(
            "report/ (2 files), summary.txt · heph inspect failure {addr}"
        )),
        "{report}"
    );
    ends_with_log(&report, addr);
    names_no_home_path(&ws.engine, addr, &report);
    Ok(())
}

/// An `in_place` target in `fix/` that uppercases `n` files and fails.
fn n_fixes_build_file(n: usize) -> String {
    let files: Vec<String> = (1..=n).map(|i| format!("f{i:02}.txt")).collect();
    let quoted = |f: &String| format!("{f:?}");
    format!(
        r#"
target(
    name = "upper",
    driver = "bash",
    deps = [{deps}],
    out = [{outs}],
    codegen = "in_place",
    run = "for f in {list}; do up=$(tr a-z A-Z < $f); printf '%s' \"$up\" > $f; done; echo fixed; exit 1",
)
"#,
        deps = files
            .iter()
            .map(|f| format!("file({})", quoted(f)))
            .collect::<Vec<_>>()
            .join(", "),
        outs = files.iter().map(quoted).collect::<Vec<_>>().join(", "),
        list = files.join(" "),
    )
}

/// A workspace whose `//fix:upper` writes `n` fixes and fails.
fn n_fixes(n: usize) -> Workspace {
    let ws = Workspace::new();
    ws.write_build_file("fix", &n_fixes_build_file(n));
    for i in 1..=n {
        ws.write_file(&format!("fix/f{i:02}.txt"), "x");
    }
    ws
}

/// Ten fixes are all named, and restored by name; an eleventh is `+1 more`,
/// and the restore goes through the list `heph inspect failure` names. A
/// target with no `failure_out` has no `outputs:` row.
#[tokio::test]
async fn failure_report_fix_counts() -> anyhow::Result<()> {
    let addr = "//fix:upper";

    let ws = n_fixes(10);
    let report = run_report(&ws.engine, addr).await?;
    let all: Vec<String> = (1..=10).map(|i| format!("fix/f{i:02}.txt")).collect();
    assert_eq!(listed(&report, "fixes written,"), all, "{report}");
    let restore = listed(&report, "restore with");
    assert!(
        matches!(restore.as_slice(), [git] if git
            .ends_with(&format!("checkout -- {}", all.join(" ")))),
        "{report}"
    );
    assert!(!report.contains("outputs:"), "{report}");
    ends_with_log(&report, addr);
    names_no_home_path(&ws.engine, addr, &report);

    let ws = n_fixes(11);
    let report = run_report(&ws.engine, addr).await?;
    let mut shown = all.clone();
    shown.push(format!("+1 more (full list: heph inspect failure {addr})"));
    assert_eq!(listed(&report, "fixes written,"), shown, "{report}");
    assert!(!report.contains("fix/f11.txt"), "{report}");
    let restore = listed(&report, "restore with");
    assert!(
        matches!(restore.as_slice(), [git] if git.ends_with(&format!(
            "--literal-pathspecs checkout \
             --pathspec-from-file=<the fixes file heph inspect failure {addr} names>"
        ))),
        "{report}"
    );
    assert!(!report.contains("outputs:"), "{report}");
    ends_with_log(&report, addr);
    names_no_home_path(&ws.engine, addr, &report);
    Ok(())
}

/// `//fix:upper`: uppercases `fix/src.txt` in place, then runs `tail`.
fn in_place_then(tail: &str) -> String {
    format!(
        r#"
target(
    name = "upper",
    driver = "bash",
    deps = [file("src.txt")],
    out = "src.txt",
    codegen = "in_place",
    failure_out = "report.txt",
    run = "up=$(tr a-z A-Z < src.txt); printf '%s' \"$up\" > $OUT; printf r > report.txt; echo fixed; {tail}",
)
"#
    )
}

/// A run killed by signal 9 wrote no fixes, and says why.
#[tokio::test]
async fn failure_report_killed_by_signal() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("fix", &in_place_then("kill -9 $$"));
    ws.write_file("fix/src.txt", "hello");

    let addr = "//fix:upper";
    let report = run_report(&ws.engine, addr).await?;
    assert!(
        row(&report, "fixes not written:").ends_with("fixes not written: killed by signal 9"),
        "{report}"
    );
    assert!(!report.contains("restore with"), "{report}");
    // A signal still keeps `failure_out`.
    assert!(
        row(&report, "outputs:").ends_with(&format!("report.txt · heph inspect failure {addr}")),
        "{report}"
    );
    ends_with_log(&report, addr);
    names_no_home_path(&ws.engine, addr, &report);
    Ok(())
}

/// U6, D6.1c "execution went fine, then a later step failed", through the
/// real bash driver: a run that streamed, exited 0 and never wrote its
/// declared `out`. The failure is past the run (`AfterRun`, not
/// `BeforeRun`: the process ran, and its log is the run's), so it is not
/// told in a one-liner and its box points at the log instead of repeating
/// it. The PTY form is bin-e2e `streamed_then_missing_output_box_without_log`.
#[tokio::test]
async fn streamed_then_missing_output_box_without_log() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        r#"target(name = "t", driver = "bash", cache = False, out = "never.txt", run = "echo streamed-line")"#,
    );
    let wrapper: heph::engine::InteractiveWrapper =
        Arc::new(move |inner: heph::engine::InteractiveInner| {
            Box::pin(async move {
                let (mut out, mut err) = (tokio::io::sink(), tokio::io::sink());
                inner(None, Some(&mut out), Some(&mut err), None).await?;
                Ok(heph::engine::TerminalUse { streamed: true })
            })
        });
    let opts = ResultOptions {
        interactive: Some(wrapper),
        ..ResultOptions::default()
    };
    let rs = ws.engine.new_state();
    let res = Arc::clone(&ws.engine)
        .result_addr(
            Arc::clone(&rs),
            &parse_addr("//p:t")?,
            OutputMatcher::All,
            &opts,
        )
        .await;
    assert!(res.is_err(), "the declared output is never written");
    let failures = rs.take_failures_with_logs().await;
    let [failure] = failures.as_slice() else {
        panic!("one failure: {failures:?}");
    };
    let rec = &failure.record;
    let msg = &rec.diagnostic.message;
    assert!(msg.contains("never.txt"), "the missing output: {msg}");
    assert_eq!(rec.failed_in, hcore::events::FailedIn::AfterRun, "{msg}");
    assert!(rec.streamed, "{msg}");
    assert!(
        !rec.told_streamed(),
        "a failure past the run gets its box: {msg}"
    );

    let report = ReportContext::gather(&failures, |_| false, None, rs.log_tail_lines())
        .with_width(WIDTH)
        .render(false);
    assert!(report.contains("✗ //p:t"), "{report}");
    assert!(
        report.contains("(output not shown here: 1 line · heph log //p:t)"),
        "{report}"
    );
    assert!(!report.contains("streamed-line"), "{report}");
    ends_with_log(&report, "//p:t");
    Ok(())
}

/// Options whose wrapper lends the run a terminal: its output streams, as
/// the sole selected target's does.
fn streaming() -> ResultOptions {
    let wrapper: heph::engine::InteractiveWrapper =
        Arc::new(move |inner: heph::engine::InteractiveInner| {
            Box::pin(async move {
                let (mut out, mut err) = (tokio::io::sink(), tokio::io::sink());
                inner(None, Some(&mut out), Some(&mut err), None).await?;
                Ok(heph::engine::TerminalUse { streamed: true })
            })
        });
    ResultOptions {
        interactive: Some(wrapper),
        ..ResultOptions::default()
    }
}

/// CQ r2: a streamed `in_place` run that wrote fixes and exited 1. Its
/// streamed line (`✗ //fix:upper · exit status 1 · heph log`) was told
/// before the write-back, so it cannot name the fixes: the failure gets its
/// box, with the fixes and the restore row, and without the streamed log.
#[tokio::test]
async fn streamed_in_place_fixes_get_their_box() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("fix", &in_place_then("exit 1"));
    ws.write_file("fix/src.txt", "hello");

    let addr = "//fix:upper";
    let report = run_report_with(&ws.engine, addr, &streaming()).await?;
    assert!(
        report.contains(&format!("✗ {addr} · exit status 1")),
        "{report}"
    );
    assert_eq!(
        listed(&report, "fixes written,"),
        ["fix/src.txt"],
        "{report}"
    );
    let restore = listed(&report, "restore with");
    assert!(
        matches!(restore.as_slice(), [git] if git.ends_with("checkout -- fix/src.txt")),
        "{report}"
    );
    assert!(
        !report.contains(" │ fixed"),
        "the streamed output is not repeated:\n{report}"
    );
    Ok(())
}

/// CQ r2: a streamed `in_place` run whose source moved while it ran. The
/// guard's refusal joins the error after the streamed line was told, so
/// the failure gets its box, saying why nothing was written.
#[tokio::test]
async fn streamed_in_place_moved_source_gets_its_box() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let src = ws.dir.path().join("fix/src.txt");
    ws.write_build_file(
        "fix",
        &in_place_then(&format!(
            "printf 'edited by someone else' > {}; exit 1",
            src.display()
        )),
    );
    ws.write_file("fix/src.txt", "hello");

    let addr = "//fix:upper";
    let report = run_report_with(&ws.engine, addr, &streaming()).await?;
    assert_eq!(std::fs::read_to_string(&src)?, "edited by someone else");
    assert!(report.contains(&format!("✗ {addr}")), "{report}");
    assert!(report.contains("changed while it ran"), "{report}");
    assert!(report.contains("fixes not written:"), "{report}");
    assert!(!report.contains("restore with"), "{report}");
    Ok(())
}

/// U6, D6.1c "failed before it ran", through the real bash driver: an
/// option the driver refuses fails the parse, under a wrapper that would
/// have streamed. The process never ran, so the record is `BeforeRun`, not
/// streamed, and the box is the full one: the refusal, no pointer and no
/// `log:` row. The PTY form is bin-e2e `streamed_target_bad_option_full_box`.
#[tokio::test]
async fn streamed_target_bad_option_full_box() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        r#"target(name = "t", driver = "bash", cache = False, timeout = "banana", run = "echo never-ran")"#,
    );
    let wrapper: heph::engine::InteractiveWrapper =
        Arc::new(move |inner: heph::engine::InteractiveInner| {
            Box::pin(async move {
                let (mut out, mut err) = (tokio::io::sink(), tokio::io::sink());
                inner(None, Some(&mut out), Some(&mut err), None).await?;
                Ok(heph::engine::TerminalUse { streamed: true })
            })
        });
    let opts = ResultOptions {
        interactive: Some(wrapper),
        ..ResultOptions::default()
    };
    let rs = ws.engine.new_state();
    let res = Arc::clone(&ws.engine)
        .result_addr(
            Arc::clone(&rs),
            &parse_addr("//p:t")?,
            OutputMatcher::All,
            &opts,
        )
        .await;
    assert!(res.is_err(), "the bad option is refused");
    let failures = rs.take_failures_with_logs().await;
    let [failure] = failures.as_slice() else {
        panic!("one failure: {failures:?}");
    };
    let rec = &failure.record;
    let msg = &rec.diagnostic.message;
    assert_eq!(rec.failed_in, hcore::events::FailedIn::BeforeRun, "{msg}");
    assert!(!rec.streamed, "nothing ran, so nothing streamed: {msg}");

    let report = ReportContext::gather(&failures, |_| false, None, rs.log_tail_lines())
        .with_width(WIDTH)
        .render(false);
    let mut lines = report.lines().skip_while(|l| l.is_empty());
    assert_eq!(lines.next(), Some("✗ //p:t"), "{report}");
    assert!(
        lines
            .next()
            .is_some_and(|l| l.contains("\"banana\" is not a duration")),
        "{report}"
    );
    for needle in ["output not shown here", "never-ran", "log:"] {
        assert!(!report.contains(needle), "{needle:?} in:\n{report}");
    }
    Ok(())
}

/// `--keep-failed-sandboxes` adds one `sandbox: <path> (kept)` row, and it
/// is the only line naming a sandbox, or anything else in heph's home.
#[tokio::test]
async fn failure_report_kept_sandbox() -> anyhow::Result<()> {
    let ws = Workspace::keeping_failed_sandboxes();
    ws.write_build_file("fix", &in_place_then("exit 1"));
    ws.write_file("fix/src.txt", "hello");

    let addr = "//fix:upper";
    let report = run_report(&ws.engine, addr).await?;
    let is_sandbox_row = |l: &str| l.trim_start().starts_with("sandbox:");
    let kept: Vec<&str> = report.lines().filter(|l| is_sandbox_row(l)).collect();
    let [kept] = kept.as_slice() else {
        panic!("one sandbox row:\n{report}");
    };
    let path = kept
        .trim_start()
        .strip_prefix("sandbox:")
        .and_then(|p| p.strip_suffix(" (kept)"))
        .map(str::trim)
        .unwrap_or_else(|| panic!("a kept sandbox row: {kept:?}"));
    assert!(
        std::path::Path::new(path).is_dir(),
        "the kept sandbox is on disk: {path}"
    );
    let rest: String = report
        .lines()
        .filter(|l| !is_sandbox_row(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        row(&report, "outputs:").contains("heph inspect failure"),
        "{report}"
    );
    ends_with_log(&report, addr);
    names_no_home_path(&ws.engine, addr, &rest);
    Ok(())
}

/// A run abandoned after its timeout's grace says so, and that its sandbox
/// was left behind, without naming it.
#[tokio::test]
async fn failure_report_abandoned_timeout_leaves_sandbox() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let run = crate::run_timeout::Abandoned::run(root.path(), false).await?;
    let report = ReportContext::gather(
        std::slice::from_ref(&run.failure),
        |_| false,
        Some(&run.engine.home),
        heph::engine::Engine::DEFAULT_LOG_TAIL_LINES,
    )
    .with_width(WIDTH)
    .render(false);
    run.drain().await;
    assert!(
        report.starts_with("\n✗ //p:deaf · timed out after 1s (its timeout)\n"),
        "{report}"
    );
    assert!(
        row(&report, "abandoned:").contains("heph stopped waiting for it"),
        "{report}"
    );
    assert_eq!(
        row(&report, "sandbox:"),
        "  sandbox: left (run abandoned after timeout)",
        "{report}"
    );
    assert!(!report.contains("(kept)"), "{report}");
    for needle in [run.engine.home.display().to_string(), "fixes.txt".into()] {
        assert!(!report.contains(&needle), "{needle:?} in:\n{report}");
    }
    Ok(())
}
