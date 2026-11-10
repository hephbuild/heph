#![expect(
    clippy::panic_in_result_fn,
    clippy::panic,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! Target timeouts end to end: a BUILD file declares `timeout` on a bash
//! target, the driver moves it onto the def, and the engine — not the driver —
//! holds the run to it. The engine's half (cancel, grace, abandon) is unit
//! tested in `engine::run_timeout`; this proves the declaration reaches it and
//! what a timed-out target looks like to the caller: a failure, not a
//! cancellation, never cached, and a cache key the timeout does not move.

use heph::engine::event::{BuildEvent, BuildEventKind};
use heph::engine::{Config, Engine, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use heph::htmatcher::Matcher;
use heph::{pluginbuildfile, pluginexec};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// An engine over `root` with the buildfile provider and the bash driver.
/// Called again over the same root, it is the next `heph` invocation.
fn engine(root: &Path) -> anyhow::Result<Arc<Engine>> {
    engine_with(root, |e| {
        e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash().with_host_path()))
    })
}

/// [`engine`]'s provider, with the drivers (and settings) `setup` registers.
fn engine_with(
    root: &Path,
    setup: impl FnOnce(&mut Engine) -> anyhow::Result<()>,
) -> anyhow::Result<Arc<Engine>> {
    let mut e = Engine::new(Config::for_tests(root.to_path_buf()))?;
    let r = root.to_path_buf();
    e.register_provider(move |init| {
        Box::new(pluginbuildfile::Provider::new(
            r,
            init.runtime.clone(),
            Arc::clone(&init.functions),
        ))
    })?;
    setup(&mut e)?;
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

/// Resolve `addr` on its own request: the engine's error as it returned it
/// (not folded, so its chain can be inspected) and the recorded failures.
async fn build_raw(
    engine: &Arc<Engine>,
    addr: &str,
) -> anyhow::Result<(anyhow::Error, Vec<Arc<heph::engine::error::TargetFailure>>)> {
    let rs = engine.new_state();
    let res = Arc::clone(engine)
        .result_addr(
            rs.clone(),
            &parse_addr(addr)?,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await;
    let Err(err) = res else {
        anyhow::bail!("{addr} must fail");
    };
    Ok((err, rs.take_failures()))
}

/// The process the timeout kills is the run's whole process group, not just
/// the shell: a backgrounded grandchild the shell is waiting on is gone too.
///
/// A non-interactive shell starts a `&` job with SIGINT ignored, so the
/// group's SIGINT never reaches this `sleep`; only the SIGKILL to the group
/// once the shell has exited does.
#[tokio::test]
async fn a_timeout_kills_the_runs_grandchildren() -> anyhow::Result<()> {
    assert_timeout_kills_grandchild("sleep 300").await
}

/// The same, for a grandchild that holds none of the run's output pipes. One
/// that does is also killed by the post-exit drain's escalation on macOS,
/// which is how the test above passed there while the group was never killed
/// on Linux; this one passes only if the cancel path itself kills the group.
#[tokio::test]
async fn a_timeout_kills_a_grandchild_detached_from_the_runs_output() -> anyhow::Result<()> {
    assert_timeout_kills_grandchild("sleep 300 >/dev/null 2>&1").await
}

/// Run `<grandchild> & echo $! > pid; wait` under a 1s timeout, then wait for
/// the backgrounded grandchild to be gone.
async fn assert_timeout_kills_grandchild(grandchild: &str) -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let marks = tempfile::tempdir()?;
    let pid_file = marks.path().join("pid");
    write_build(
        ws.path(),
        &format!(
            r#"
target(
    name = "bg",
    driver = "bash",
    run = "{grandchild} & echo $! > '{pid}'; wait",
    out = "out.txt",
    timeout = 1,
)
"#,
            pid = pid_file.display(),
        ),
    );
    let engine = engine(ws.path())?;
    let err = build(&engine, "//p:bg").await.expect_err("timed out");
    assert!(
        format!("{err:#}").contains("//p:bg timed out after 1s"),
        "{err:#}"
    );

    let pid: libc::pid_t = std::fs::read_to_string(&pid_file)?.trim().parse()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !process_gone(pid) {
        assert!(
            Instant::now() < deadline,
            "the grandchild {pid} outlived its timed-out run (state {:?})",
            process_state(pid),
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

/// Whether `pid` no longer runs: reaped, or a zombie. A killed orphan is
/// reparented to whichever ancestor reaps orphans, and until that one gets to
/// it `kill(pid, 0)` still succeeds; the zombie runs nothing, so it counts.
fn process_gone(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only checks that the pid exists; nothing is sent.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        return true;
    }
    process_state(pid) == Some('Z')
}

/// The state letter `ps` shows for `pid` (`S`, `R`, `Z`, …), or `None` once
/// it is gone or `ps` cannot say.
fn process_state(pid: libc::pid_t) -> Option<char> {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().chars().next()
}

/// The last-failure slot (`<home>/failures/<addr-key>/log.txt`), when complete.
fn slot_log(engine: &Engine, addr: &str) -> Option<String> {
    let addr = parse_addr(addr).expect("addr");
    let slots = engine.failure_slots();
    slots.read(&addr)?;
    std::fs::read_to_string(slots.slot_dir(&addr).join("log.txt")).ok()
}

/// A timed-out target keeps what it printed before it was stopped: the
/// failure slot holds its log, as for any failure. The engine's error is a
/// timeout, and nothing in its chain reads as a cancellation (which is what
/// would make the CLI drop it as "cancelled" rather than show it).
#[tokio::test]
async fn a_timed_out_target_keeps_its_log() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(ws.path(), &slow("1"));
    let engine = engine(ws.path())?;
    let (err, failures) = build_raw(&engine, "//p:slow").await?;

    assert!(
        !hplugin::error::is_cancelled(&err),
        "a failure, not a cancellation: {err:#}"
    );
    let [failure] = failures.as_slice() else {
        panic!("one failure recorded, got {}", failures.len());
    };
    assert!(
        heph::engine::run_timeout::is_timed_out(&failure.source),
        "a timeout: {:#}",
        failure.source
    );
    assert!(
        !hplugin::error::is_cancelled(&failure.source),
        "{:#}",
        failure.source
    );
    let log = slot_log(&engine, "//p:slow").expect("a timed-out run fills its failure slot");
    assert!(log.contains("started"), "the log before the kill: {log:?}");
    assert!(!log.contains("done"), "{log:?}");
    Ok(())
}

/// A driver whose run never returns and ignores its token: what a timeout
/// does when cancelling the run does not stop it.
struct Unstoppable;

#[async_trait::async_trait]
impl hplugin::driver::Driver for Unstoppable {
    fn config(
        &self,
        _req: hplugin::driver::ConfigRequest,
    ) -> anyhow::Result<hplugin::driver::ConfigResponse> {
        Ok(hplugin::driver::ConfigResponse {
            name: "unstoppable".to_string(),
        })
    }
    fn schema(&self) -> hplugin::driver::DriverSchema {
        hplugin::driver::DriverSchema::default()
    }
    async fn parse(
        &self,
        req: hplugin::driver::ParseRequest,
        _ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ParseResponse> {
        // Fixed rather than read from the spec: this driver stands in for one
        // whose run ignores cancellation, not for how a spec declares it.
        Ok(hplugin::driver::ParseResponse {
            target_def: hplugin::driver::targetdef::TargetDef {
                addr: req.target_spec.addr.clone(),
                hash: b"unstoppable".to_vec(),
                timeout: Some(hplugin::driver::Timeout::from_millis(1000)?),
                ..Default::default()
            },
        })
    }
    async fn apply_transitive(
        &self,
        req: hplugin::driver::ApplyTransitiveRequest,
        _ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ApplyTransitiveResponse> {
        Ok(hplugin::driver::ApplyTransitiveResponse {
            target_def: req.target_def,
        })
    }
    async fn run<'a, 'io>(
        &self,
        _req: hplugin::driver::RunRequest<'a, 'io>,
        _ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::RunResponse> {
        std::future::pending().await
    }
    async fn run_shell<'a, 'io>(
        &self,
        _req: hplugin::driver::RunRequest<'a, 'io>,
        _ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::RunResponse> {
        anyhow::bail!("not a shell test")
    }
}

/// A run that ignores its cancellation is abandoned once the grace runs out:
/// the build does not hang on it, and the target fails saying it was
/// abandoned. The failure is recorded; an abandoned run returned nothing, so
/// there is no log and no failure slot to expect.
#[tokio::test]
async fn a_run_that_ignores_its_cancellation_is_abandoned() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        r#"target(name = "stuck", driver = "unstoppable")"#,
    );
    let engine = engine_with(ws.path(), |e| {
        e.set_timeout_grace(Duration::from_millis(200));
        e.register_driver(|_| Box::new(Unstoppable))
    })?;

    let started = Instant::now();
    let (err, failures) =
        tokio::time::timeout(Duration::from_secs(30), build_raw(&engine, "//p:stuck"))
            .await
            .expect("an abandoned run does not hang the build")?;
    let msg = format!("{err:#}");
    assert!(msg.contains("//p:stuck timed out after 1s"), "{msg}");
    assert!(msg.contains("was abandoned"), "{msg}");
    assert!(
        started.elapsed() < Duration::from_secs(9),
        "abandoned after the shortened grace, not the production one: {:?}",
        started.elapsed()
    );
    let [failure] = failures.as_slice() else {
        panic!("one failure recorded, got {}", failures.len());
    };
    assert!(
        format!("{:#}", failure.source).contains("was abandoned"),
        "{:#}",
        failure.source
    );
    Ok(())
}

/// A driver that does not carry `timeout` refuses it at parse, naming it,
/// rather than silently running unbounded.
#[tokio::test]
async fn a_driver_without_timeouts_refuses_one() -> anyhow::Result<()> {
    let ws = crate::common::Workspace::new();
    ws.write_build_file(
        "p",
        r#"
target(name = "a", driver = "bash", run = "echo a > $OUT", out = "a.txt")
target(name = "g", driver = "group", deps = [":a"], timeout = 5)
"#,
    );
    let Err(err) = ws.run("//p:g").await else {
        panic!("refused at parse");
    };
    let msg = format!("{err:#}");
    assert!(msg.contains("timeout"), "{msg}");
    assert!(!msg.contains("timed out"), "never ran: {msg}");
    Ok(())
}

/// The `ResultEnd` event of `addr`'s request.
fn result_end(events: &[BuildEvent], addr: &str) -> Option<hcore::events::TimedOutData> {
    let ends: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            BuildEventKind::ResultEnd {
                addr: a, timed_out, ..
            } if a == addr => Some(timed_out.clone()),
            _ => None,
        })
        .collect();
    let [end] = ends.as_slice() else {
        panic!("one ResultEnd for {addr}, got {}", ends.len());
    };
    end.clone()
}

/// A consumer tells a timeout from any other failure by the typed field on
/// `ResultEnd`, not by reading the message: set for a timed-out target, with
/// its timeout and whether it was abandoned; unset for a target that failed
/// on its own.
#[tokio::test]
async fn result_end_says_the_target_timed_out() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        &format!(
            r#"{}
target(name = "boom", driver = "bash", run = "exit 3", out = "out.txt", timeout = 60)
"#,
            slow("1")
        ),
    );
    let engine = engine(ws.path())?;

    let (res, events) =
        crate::common::run_drain_events(&engine, &Matcher::Addr(parse_addr("//p:slow")?)).await;
    assert!(res.is_err() || res.is_ok_and(|b| !b.errors.is_empty()));
    assert_eq!(
        result_end(&events, "//p:slow"),
        Some(hcore::events::TimedOutData {
            timeout_ms: 1000,
            abandoned: false,
        })
    );

    let (res, events) =
        crate::common::run_drain_events(&engine, &Matcher::Addr(parse_addr("//p:boom")?)).await;
    assert!(res.is_err() || res.is_ok_and(|b| !b.errors.is_empty()));
    assert_eq!(
        result_end(&events, "//p:boom"),
        None,
        "failed, not timed out"
    );
    Ok(())
}

/// `apply_transitive` re-derives the def; the timeout is restored from the
/// parse, so a driver that drops it there (as one rebuilding its def from
/// scratch would) does not leave a target with deps carrying transitive
/// settings unbounded.
#[tokio::test]
async fn a_timeout_survives_apply_transitive() -> anyhow::Result<()> {
    struct DropsTimeout(pluginexec::Driver);
    #[async_trait::async_trait]
    impl heph::engine::driver_managed::ManagedDriver for DropsTimeout {
        fn config(
            &self,
            _req: hplugin::driver::ConfigRequest,
        ) -> anyhow::Result<hplugin::driver::ConfigResponse> {
            Ok(hplugin::driver::ConfigResponse {
                name: "drops".to_string(),
            })
        }
        fn schema(&self) -> hplugin::driver::DriverSchema {
            heph::engine::driver_managed::ManagedDriver::schema(&self.0)
        }
        async fn parse(
            &self,
            req: hplugin::driver::ParseRequest,
            ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> anyhow::Result<hplugin::driver::ParseResponse> {
            heph::engine::driver_managed::ManagedDriver::parse(&self.0, req, ctoken).await
        }
        async fn apply_transitive(
            &self,
            req: hplugin::driver::ApplyTransitiveRequest,
            ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> anyhow::Result<hplugin::driver::ApplyTransitiveResponse> {
            let mut res =
                heph::engine::driver_managed::ManagedDriver::apply_transitive(&self.0, req, ctoken)
                    .await?;
            res.target_def.timeout = None;
            Ok(res)
        }
        async fn run<'a, 'io>(
            &self,
            req: heph::engine::driver_managed::ManagedRunRequest<'a, 'io>,
            ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> anyhow::Result<heph::engine::driver_managed::ManagedRunResponse> {
            heph::engine::driver_managed::ManagedDriver::run(&self.0, req, ctoken).await
        }
    }

    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        r#"
target(name = "tool", driver = "drops", run = "echo x > $OUT", out = "tool.txt",
       transitive = {"env": {"T": "t"}})
target(name = "c", driver = "drops", run = "echo $T; sleep 60; echo done > $OUT", out = "c.txt",
       deps = ["//p:tool"], timeout = 1)
"#,
    );
    let engine = engine_with(ws.path(), |e| {
        e.register_managed_driver(|_| {
            Box::new(DropsTimeout(
                pluginexec::Driver::new_bash().with_host_path(),
            ))
        })
    })?;
    let def = Arc::clone(&engine)
        .get_def(engine.new_state(), &parse_addr("//p:c")?)
        .await?;
    assert_eq!(
        def.target_def
            .timeout
            .map(hplugin::driver::Timeout::as_millis),
        Some(1000)
    );
    let started = Instant::now();
    let err = build(&engine, "//p:c").await.expect_err("timed out");
    assert!(
        format!("{err:#}").contains("//p:c timed out after 1s"),
        "{err:#}"
    );
    assert!(started.elapsed() < Duration::from_secs(9));
    Ok(())
}
