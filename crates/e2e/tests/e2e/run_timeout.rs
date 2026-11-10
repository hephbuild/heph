#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! Target timeouts end to end: a BUILD file declares `timeout` on a bash
//! target, the driver moves it onto the def, and the engine — not the driver —
//! holds the run to it. The engine's half (cancel, grace, abandon) is unit
//! tested in `engine::run_timeout`; this proves the declaration reaches it and
//! what a timed-out target looks like to the caller: a failure, not a
//! cancellation, never cached, and a cache key the timeout does not move.

use heph::engine::{Config, Engine, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use heph::{pluginbuildfile, pluginexec};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// An engine over `root` with the buildfile provider and the bash driver.
/// Called again over the same root, it is the next `heph` invocation.
fn engine(root: &Path) -> anyhow::Result<Arc<Engine>> {
    let mut e = Engine::new(Config::for_tests(root.to_path_buf()))?;
    let r = root.to_path_buf();
    e.register_provider(move |init| {
        Box::new(pluginbuildfile::Provider::new(
            r,
            init.runtime.clone(),
            Arc::clone(&init.functions),
        ))
    })?;
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash().with_host_path()))?;
    let e = Arc::new(e);
    e.install_exec_runner_host();
    Ok(e)
}

fn write_build(root: &Path, content: &str) {
    let dir = root.join("p");
    std::fs::create_dir_all(&dir).expect("create pkg dir");
    std::fs::write(dir.join("BUILD"), content).expect("write BUILD");
}

/// Build `addr`; the request's recorded failures are folded into the error, as
/// the CLI renders them. Returns the output's contents on success.
async fn build(engine: &Arc<Engine>, addr: &str) -> anyhow::Result<String> {
    let rs = engine.new_state();
    let res = Arc::clone(engine)
        .result_addr(
            rs.clone(),
            &parse_addr(addr)?,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await;
    match res {
        Ok(r) => Ok(crate::common::artifact_string(&r).trim().to_string()),
        Err(e) => {
            let mut msg = format!("{e:#}");
            for f in rs.take_failures() {
                msg.push_str(&format!("\nfailure {}: {:#}", f.addr.format(), f.source));
            }
            Err(anyhow::anyhow!(msg))
        }
    }
}

async fn hashin(engine: &Arc<Engine>, addr: &str) -> anyhow::Result<String> {
    Ok(Arc::clone(engine)
        .meta(engine.new_state(), &parse_addr(addr)?)
        .await?
        .hashin
        .clone())
}

fn slow(timeout: &str) -> String {
    format!(
        r#"
target(
    name = "slow",
    driver = "bash",
    run = "echo started; sleep 60; echo done > $OUT",
    out = "out.txt",
    timeout = {timeout},
)
"#
    )
}

/// The run is stopped at its timeout through the driver's own cancellation
/// (well inside the grace, so not abandoned), and the target fails as timed
/// out — recorded as a failure, which a cancellation never is. It is not
/// cached: the next build runs it, and times out, again.
#[tokio::test]
async fn a_run_past_its_timeout_fails_as_timed_out() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(ws.path(), &slow("1"));
    let engine = engine(ws.path())?;

    for attempt in 0..2 {
        let started = Instant::now();
        let err = build(&engine, "//p:slow")
            .await
            .expect_err("a 60s run under a 1s timeout fails");
        let elapsed = started.elapsed();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("//p:slow timed out after 1s"),
            "{attempt}: {msg}"
        );
        assert!(
            msg.contains("failure //p:slow"),
            "recorded as a failure: {msg}"
        );
        assert!(
            !msg.contains("abandoned"),
            "the driver stopped when cancelled: {msg}"
        );
        assert!(
            elapsed < Duration::from_secs(9),
            "{attempt}: stopped at the timeout, not after the sleep or the grace: {elapsed:?}"
        );
    }
    Ok(())
}

/// A run that finishes inside its timeout is an ordinary success, and the
/// timeout is in no hash: changing it, or removing it, keeps the cache key,
/// so the next invocation is a hit rather than a rebuild.
#[tokio::test]
async fn the_timeout_does_not_move_the_cache_key() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let fast = |timeout: &str| {
        format!(
            r#"
target(
    name = "fast",
    driver = "bash",
    run = "echo ok > $OUT",
    out = "out.txt",
    {timeout}
)
"#
        )
    };

    write_build(ws.path(), &fast(r#"timeout = "1h","#));
    let first = engine(ws.path())?;
    assert_eq!(build(&first, "//p:fast").await?, "ok");
    let key = hashin(&first, "//p:fast").await?;

    for other in [r#"timeout = "2h","#, "timeout = 30,", ""] {
        write_build(ws.path(), &fast(other));
        let next = engine(ws.path())?;
        assert_eq!(hashin(&next, "//p:fast").await?, key, "{other:?}");
    }
    Ok(())
}

/// A bad declaration fails the driver's parse, naming the target and the
/// attribute, before anything runs.
#[tokio::test]
async fn a_bad_timeout_is_refused_at_parse() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    for bad in [r#""0s""#, r#""soon""#, "-1"] {
        write_build(ws.path(), &slow(bad));
        let err = build(&engine(ws.path())?, "//p:slow")
            .await
            .expect_err("refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("timeout"), "{bad}: {msg}");
        assert!(!msg.contains("timed out"), "{bad}: never ran: {msg}");
    }
    Ok(())
}
