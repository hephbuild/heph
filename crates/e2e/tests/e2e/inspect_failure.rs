//! `heph inspect failure <addr>` (D24): reads a target's failure slot, builds
//! nothing. The command is a thin print over
//! `heph::commands::inspect::failure::failure_view`, so the tests drive that
//! against a real engine's slots, filled by real runs.

#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::common::Workspace;
use heph::commands::inspect::failure::{FailureView, inspect_view};
use heph::engine::{Engine, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use std::sync::Arc;

/// Resolve `addr`, which must fail.
async fn fail(engine: &Arc<Engine>, addr: &str) -> anyhow::Result<()> {
    let rs = engine.new_state();
    let res = engine
        .clone()
        .result_addr(
            rs,
            &parse_addr(addr)?,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await;
    assert!(res.is_err(), "{addr} must fail");
    Ok(())
}

fn view(engine: &Engine, addr: &str) -> anyhow::Result<Option<FailureView>> {
    inspect_view(engine, &parse_addr(addr)?)
}

/// A BUILD file that does not evaluate: `inspect failure //d:t` returns the
/// file's record (no addr, its file, eval, no run facts); the fix and a
/// green run clear it.
#[tokio::test]
async fn inspect_failure_of_target_in_broken_file_returns_file_record() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("d", "target(name = \"t\"\nbroken(\n");
    fail(&ws.engine, "//d:t").await?;
    ws.engine.failure_slots().writes_drained().await;
    let v = view(&ws.engine, "//d:t")?.expect("the file's record");
    assert_eq!(v.addr, None);
    assert_eq!(v.file.as_deref(), Some("d/BUILD"));
    assert_eq!(v.stage, hcore::diagnostic::Stage::Eval);
    assert_eq!(
        (v.hashin, v.failed_at, v.log, v.log_lines),
        (None, None, None, None)
    );

    // slot_cleared_after_fix_and_green_run, for the file's slot.
    ws.write_build_file(
        "d",
        r#"target(name = "t", driver = "bash", out = "o.txt", run = "echo ok > o.txt")"#,
    );
    // A new heph, as the next command is: it lists the file's slot.
    let engine = ws.reopen()?;
    Arc::clone(&engine)
        .result_addr(
            engine.new_state(),
            &parse_addr("//d:t")?,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await?;
    assert_eq!(view(&engine, "//d:t")?, None);
    Ok(())
}

/// Assert `v` is a record of a failure before the run: no run facts.
fn assert_before_run(v: &FailureView) {
    assert!(v.before_run(), "{v:?}");
    assert_eq!(
        (&v.hashin, v.failed_at, &v.log, v.log_lines),
        (&None, None, &None, None),
        "{v:?}"
    );
}

/// STAGE-09: a BUILD file that raises while it evaluates (a runtime error,
/// not a syntax one): the record is the file's, at the eval stage.
#[tokio::test]
async fn inspect_failure_after_eval_failure() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "d",
        "target(name = \"t\", driver = \"bash\", run = \"true\")\nx = 1 + \"a\"\n",
    );
    fail(&ws.engine, "//d:t").await?;
    ws.engine.failure_slots().writes_drained().await;
    let v = view(&ws.engine, "//d:t")?.expect("the file's record");
    assert_eq!(v.stage, hcore::diagnostic::Stage::Eval, "{v:?}");
    assert_eq!(v.file.as_deref(), Some("d/BUILD"), "{v:?}");
    assert!(!v.message.is_empty(), "{v:?}");
    assert_before_run(&v);
    Ok(())
}

/// STAGE-09: the driver refuses the target's options. The record is the
/// target's own, at the parse stage, with nothing about a run.
#[tokio::test]
async fn inspect_failure_after_parse_failure() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "d",
        r#"target(name = "t", driver = "bash", run = "true", timeout = "not-a-duration")"#,
    );
    fail(&ws.engine, "//d:t").await?;
    ws.engine.failure_slots().writes_drained().await;
    let v = view(&ws.engine, "//d:t")?.expect("a recorded failure");
    assert_eq!(v.addr.as_deref(), Some("//d:t"), "{v:?}");
    assert_eq!(v.stage, hcore::diagnostic::Stage::Parse, "{v:?}");
    assert_before_run(&v);
    Ok(())
}

/// STAGE-09: a dependency that does not exist. The record is the consumer's,
/// at the link stage, with nothing about a run.
#[tokio::test]
async fn inspect_failure_after_link_failure() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "d",
        r#"target(name = "t", driver = "bash", run = "true", deps = ["//d:missing"])"#,
    );
    fail(&ws.engine, "//d:t").await?;
    ws.engine.failure_slots().writes_drained().await;
    let v = view(&ws.engine, "//d:t")?.expect("a recorded failure");
    assert_eq!(v.addr.as_deref(), Some("//d:t"), "{v:?}");
    assert_eq!(v.stage, hcore::diagnostic::Stage::Link, "{v:?}");
    assert_before_run(&v);
    Ok(())
}

/// RUN-19 / CLI-28: a run that printed nothing and failed still gets its
/// slot: the run facts are there, and the log is empty, not missing.
#[tokio::test]
async fn inspect_failure_after_empty_log_failure() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "d",
        r#"target(name = "t", driver = "bash", run = "exit 3", cache = False)"#,
    );
    fail(&ws.engine, "//d:t").await?;
    ws.engine.failure_slots().writes_drained().await;
    let v = view(&ws.engine, "//d:t")?.expect("a recorded failure");
    assert_eq!(v.stage, hcore::diagnostic::Stage::Run, "{v:?}");
    assert!(!v.before_run(), "{v:?}");
    assert!(v.hashin.as_deref().is_some_and(|h| !h.is_empty()), "{v:?}");
    assert!(v.failed_at.is_some(), "{v:?}");
    assert_eq!(v.log_lines, Some(0), "{v:?}");
    let log = v.log.as_ref().expect("an empty log, not none");
    assert_eq!(std::fs::read_to_string(log)?, "");
    Ok(())
}

/// A dependency failed and took its consumer down.
async fn skipped_consumer() -> anyhow::Result<Workspace> {
    let ws = Workspace::new();
    ws.write_build_file(
        "d",
        r#"
target(name = "dep", driver = "bash", out = "o.txt", run = "exit 3", cache = False)
target(name = "c", driver = "bash", out = "c.txt", deps = [":dep"], run = "cp $SRC c.txt")
"#,
    );
    fail(&ws.engine, "//d:c").await?;
    ws.engine.failure_slots().writes_drained().await;
    Ok(ws)
}

/// U16: a skipped target keeps no record, so `inspect failure` answers
/// `null` for it; its culprit has one.
#[tokio::test]
async fn inspect_failure_of_skipped_target_is_null() -> anyhow::Result<()> {
    let ws = skipped_consumer().await?;
    assert_eq!(view(&ws.engine, "//d:c")?, None);
    let culprit = view(&ws.engine, "//d:dep")?.expect("the culprit's record");
    assert_eq!(culprit.addr.as_deref(), Some("//d:dep"));
    Ok(())
}

/// U16: in text, the skipped target's answer is an error that says a skipped
/// target has no record and points at the run's `--json`; with `--json` it is
/// `null`, exit 0.
#[tokio::test]
async fn inspect_failure_of_skipped_target_text_points_at_json() -> anyhow::Result<()> {
    use heph::commands::inspect::failure::{no_record_message, print_to};
    let ws = skipped_consumer().await?;
    let addr = parse_addr("//d:c")?;
    let v = inspect_view(&ws.engine, &addr)?;
    assert_eq!(v, None);

    let mut buf = Vec::new();
    let text = output::JsonArgs::<FailureView>::default();
    let err = print_to(&text, &addr, v.as_ref(), &mut buf, output::Style::Compact)
        .expect_err("text mode errors on no record");
    let msg = format!("{err:#}");
    assert_eq!(msg, no_record_message(&addr));
    assert!(
        msg.contains("skipped targets keep no record") && msg.contains("--json"),
        "{msg}"
    );
    assert!(buf.is_empty(), "nothing on stdout: {buf:?}");

    let json = output::JsonArgs::<FailureView>::try_parse_from(["--json"])?;
    print_to(&json, &addr, v.as_ref(), &mut buf, output::Style::Compact)?;
    assert_eq!(String::from_utf8(buf)?.trim(), "null");
    Ok(())
}

/// D5: an `in_place` target that wrote its fixes back while failing: the
/// view points at the slot's `fixes.txt` as well as its log.
#[tokio::test]
async fn inspect_failure_keeps_slot_out_fixes() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "fix",
        r#"
target(
    name = "upper",
    driver = "bash",
    deps = [file("src.txt")],
    out = "src.txt",
    codegen = "in_place",
    failure_out = "r/",
    run = "mkdir -p r; echo trace > r/t.txt; up=$(tr a-z A-Z < src.txt); printf '%s' \"$up\" > $OUT; exit 1",
)
"#,
    );
    ws.write_file("fix/src.txt", "fixme");
    fail(&ws.engine, "//fix:upper").await?;
    ws.engine.failure_slots().writes_drained().await;
    let v = view(&ws.engine, "//fix:upper")?.expect("a recorded failure");
    let fixes = v.fixes.as_ref().expect("the fixes list");
    assert_eq!(fixes, &v.slot.join("fixes.txt"));
    assert!(
        std::fs::read_to_string(fixes)?.contains("fix/src.txt"),
        "{}",
        fixes.display()
    );
    assert!(
        v.log.as_ref().is_some_and(|l| l.starts_with(&v.slot)),
        "{v:?}"
    );
    let out = v.out.as_ref().expect("an out/");
    assert_eq!(std::fs::read_to_string(out.join("fix/r/t.txt"))?, "trace\n");
    Ok(())
}

/// D5: a target's own slot (here a link failure) is gone once the fix
/// resolves and runs green, in a new heph as the next command is. The file
/// slot's half is in
/// [`inspect_failure_of_target_in_broken_file_returns_file_record`].
#[tokio::test]
async fn slot_cleared_after_fix_and_green_run() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "d",
        r#"target(name = "t", driver = "bash", out = "o.txt", run = "echo ok > o.txt", deps = ["//d:missing"])"#,
    );
    fail(&ws.engine, "//d:t").await?;
    ws.engine.failure_slots().writes_drained().await;
    assert!(view(&ws.engine, "//d:t")?.is_some(), "a recorded failure");

    ws.write_build_file(
        "d",
        r#"target(name = "t", driver = "bash", out = "o.txt", run = "echo ok > o.txt")"#,
    );
    let engine = ws.reopen()?;
    Arc::clone(&engine)
        .result_addr(
            engine.new_state(),
            &parse_addr("//d:t")?,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await?;
    engine.failure_slots().writes_drained().await;
    assert_eq!(view(&engine, "//d:t")?, None);
    Ok(())
}

/// D5: `heph query <addr>` on a target whose BUILD file does not evaluate
/// records the file as the request's culprit (what gives `query` its one box
/// and exit 1); a target that is merely missing records nothing. The
/// command's exit and stdout are in bin-e2e `query_on_broken_file_*`.
#[tokio::test]
async fn query_records_the_broken_file_as_culprit() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("d", "target(name = \"t\"\nbroken(\n");
    ws.write_build_file("ok", r#"target(name = "t", driver = "bash", run = "true")"#);

    let rs = ws.engine.new_state();
    assert!(
        Arc::clone(&ws.engine)
            .record_file_culprit(rs.clone(), &parse_addr("//d:t")?)
            .await
    );
    let failures = rs.take_failures();
    assert_eq!(failures.len(), 1, "one culprit: {failures:?}");
    let f = failures.first().expect("one failure");
    assert_eq!(f.file.as_deref(), Some("d/BUILD"), "{f:?}");

    for addr in ["//ok:missing", "//ok:t"] {
        let rs = ws.engine.new_state();
        assert!(
            !Arc::clone(&ws.engine)
                .record_file_culprit(rs.clone(), &parse_addr(addr)?)
                .await,
            "{addr}"
        );
        assert!(rs.take_failures().is_empty(), "{addr}");
    }
    Ok(())
}

#[tokio::test]
async fn inspect_failure_finds_the_slot() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let ctl = tempfile::tempdir()?;
    let c = ctl.path().display();
    // Fails while `<ctl>/fail` exists, leaving a trace under `r/`; passes
    // once it is gone. The switch sits outside the workspace, where no hash
    // sees it.
    ws.write_build_file(
        "p",
        &format!(
            r#"target(name = "t", driver = "bash", out = "o.txt", cache = False,
       failure_out = "r/",
       run = "mkdir -p r; echo trace > r/t.txt; if [ -f {c}/fail ]; then echo boom; exit 3; fi; echo ok > o.txt")"#
        ),
    );
    // A credential producer that fails, and the consumer that runs it.
    ws.write_build_file(
        "auth",
        r#"
target(name = "fetch", driver = "bash", out = {"credential": "cred.json"}, cache = False,
       failure_out = "r/",
       run = ["mkdir -p r", "echo trace > r/t.txt", "echo producer-marker", "exit 4"])
target(name = "t", driver = "auth.credential", sources = ["//auth:fetch"],
       present = {"env": {"T": "${token}"}})
"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "a", driver = "bash", out = "o.txt", cache = False,
       credentials = ["//auth:t"], run = ["printf '%s' \"$T\" > o.txt"])"#,
    );

    // Never failed: no slot, nothing to report.
    assert_eq!(view(&ws.engine, "//p:t")?, None);

    // A failing run: the slot, its log and its `out/`, every path absolute
    // and inside the slot; no write-back, so no fixes.
    std::fs::write(ctl.path().join("fail"), "")?;
    fail(&ws.engine, "//p:t").await?;
    let v = view(&ws.engine, "//p:t")?.expect("a recorded failure");
    assert_eq!(v.addr.as_deref(), Some("//p:t"));
    assert!(v.slot.is_absolute(), "{}", v.slot.display());
    assert_eq!(
        v.slot,
        std::path::absolute(ws.engine.failure_slots().slot_dir(&parse_addr("//p:t")?))?
    );
    assert!(v.hashin.as_deref().is_some_and(|h| !h.is_empty()));
    assert!(v.failed_at.is_some_and(|t| t > 0));
    let log = v.log.as_ref().expect("a log");
    assert!(log.starts_with(&v.slot));
    assert!(std::fs::read_to_string(log)?.contains("boom"));
    let out = v.out.as_ref().expect("an out/");
    assert!(out.starts_with(&v.slot));
    assert_eq!(std::fs::read_to_string(out.join("p/r/t.txt"))?, "trace\n");
    assert_eq!(v.fixes, None);
    // The `--json` shape: every key present, an absent part `null`.
    let json = serde_json::to_value(&v)?;
    let keys: Vec<&str> = json
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys.len(),
        15,
        "addr, file, slot, stage, message, location, hints, truncated, hashin, \
         failed_at, log, log_lines, log_replaced, out, fixes: {json}"
    );
    assert_eq!(json["stage"], "run", "{json}");
    assert_eq!(json["file"], serde_json::Value::Null, "{json}");
    assert!(json["fixes"].is_null(), "{json}");

    // A passing run clears the slot: nothing recorded any more.
    std::fs::remove_file(ctl.path().join("fail"))?;
    ws.run("//p:t").await?;
    assert_eq!(view(&ws.engine, "//p:t")?, None);

    // A credential producer's failure keeps no log (its output is material),
    // but its `failure_out` is the author's to declare (I6) and is kept.
    fail(&ws.engine, "//app:a").await?;
    let v = view(&ws.engine, "//auth:fetch")?.expect("a recorded failure");
    assert_eq!(v.log, None, "a material target keeps no log");
    let out = v.out.as_ref().expect("an out/");
    assert_eq!(
        std::fs::read_to_string(out.join("auth/r/t.txt"))?,
        "trace\n"
    );
    Ok(())
}
