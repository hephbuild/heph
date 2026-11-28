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
