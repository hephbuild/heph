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
    engine_configured(root, |_| {}, setup)
}

/// [`engine_with`], with `configure` adjusting the config first.
fn engine_configured(
    root: &Path,
    configure: impl FnOnce(&mut Config),
    setup: impl FnOnce(&mut Engine) -> anyhow::Result<()>,
) -> anyhow::Result<Arc<Engine>> {
    let mut cfg = Config::for_tests(root.to_path_buf());
    configure(&mut cfg);
    let mut e = Engine::new(cfg)?;
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
                msg.push_str(&format!(
                    "\nfailure {}: {:#}",
                    f.addr.format(),
                    crate::common::failure_text(&f)
                ));
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
pub(crate) fn process_gone(pid: libc::pid_t) -> bool {
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
        failure.record.timed_out.is_some(),
        "a timeout: {:?}",
        failure.record
    );
    assert!(!failure.record.cancelled, "{:?}", failure.record);
    let log = slot_log(&engine, "//p:slow").expect("a timed-out run fills its failure slot");
    assert!(log.contains("started"), "the log before the kill: {log:?}");
    assert!(!log.contains("done"), "{log:?}");
    // Its sandbox is removed like any failed run's, and the recorded tail,
    // read from the slot's copy, survives that.
    crate::failure_slot::removed(&crate::failure_slot::sandbox_dir(&engine, "p", "slow")).await;
    let tail = crate::common::failure_tail(failure).expect("the recorded tail");
    assert!(tail.text.contains("started"), "{tail:?}");
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
    let text = crate::common::failure_text(failure);
    assert!(text.contains("was abandoned"), "{text}");
    Ok(())
}

/// The managed bash driver, but its run is handed a token nothing cancels:
/// a sandboxed run that ignores its timeout's cancellation, so the engine
/// abandons it after the grace with its tree still on disk.
struct DeafBash(pluginexec::Driver);

#[async_trait::async_trait]
impl heph::engine::driver_managed::ManagedDriver for DeafBash {
    fn config(
        &self,
        _req: hplugin::driver::ConfigRequest,
    ) -> anyhow::Result<hplugin::driver::ConfigResponse> {
        Ok(hplugin::driver::ConfigResponse {
            name: "deaf".to_string(),
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
        heph::engine::driver_managed::ManagedDriver::apply_transitive(&self.0, req, ctoken).await
    }
    async fn run<'a, 'io>(
        &self,
        req: heph::engine::driver_managed::ManagedRunRequest<'a, 'io>,
        _ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<heph::engine::driver_managed::ManagedRunResponse> {
        let deaf = heph::hasync::StdCancellationToken::new();
        heph::engine::driver_managed::ManagedDriver::run(&self.0, req, &deaf).await
    }
}

/// An abandoned sandboxed run of `//p:deaf`, on an engine whose sandbox
/// removals are held by `gate` (closed) until the test opens it: a run whose
/// failure queued a removal still holds its `sandbox` slot when this returns.
pub(crate) struct Abandoned {
    pub(crate) engine: Arc<Engine>,
    pub(crate) failure: Arc<heph::engine::error::TargetFailure>,
    /// The request's background work, outliving the request.
    bg: heph::engine::sandbox_cleaner::PendingCounter,
    gate: heph::engine::sandbox_cleaner::RemovalGate,
}

impl Abandoned {
    pub(crate) async fn run(root: &Path, keep_failed_sandboxes: bool) -> anyhow::Result<Self> {
        write_build(
            root,
            r#"
target(
    name = "deaf",
    driver = "deaf",
    run = "echo started; sleep 3; echo done > $OUT",
    out = "out.txt",
    timeout = 1,
)
"#,
        );
        let gate = heph::engine::sandbox_cleaner::RemovalGate::closed();
        let engine = engine_configured(
            root,
            |cfg| cfg.keep_failed_sandboxes = keep_failed_sandboxes,
            |e| {
                e.set_timeout_grace(Duration::from_millis(200));
                e.set_sandbox_removal_gate(&gate);
                e.register_managed_driver(|_| {
                    Box::new(DeafBash(pluginexec::Driver::new_bash().with_host_path()))
                })
            },
        )?;
        let rs = engine.new_state();
        let res = tokio::time::timeout(
            Duration::from_secs(30),
            Arc::clone(&engine).result_addr(
                rs.clone(),
                &parse_addr("//p:deaf")?,
                OutputMatcher::All,
                &ResultOptions::default(),
            ),
        )
        .await
        .expect("an abandoned run does not hang the build");
        let Err(err) = res else {
            anyhow::bail!("//p:deaf must fail");
        };
        assert!(format!("{err:#}").contains("was abandoned"), "{err:#}");
        let [failure] = <[_; 1]>::try_from(rs.take_failures())
            .map_err(|f| anyhow::anyhow!("one failure recorded, got {}", f.len()))?;
        Ok(Self {
            engine,
            failure,
            bg: rs.bg_pending(),
            gate,
        })
    }

    /// What the `sandbox` pool holds right now.
    fn sandbox_slots_held(&self) -> u64 {
        self.engine.resource_snapshot().map_or(0, |s| {
            s.main
                .pools
                .iter()
                .find(|p| p.pool.as_str() == hplugin::provider::PoolName::SANDBOX)
                .map_or(0, |p| p.held)
        })
    }

    /// Open the gate and wait, bounded, for the request's background work.
    pub(crate) async fn drain(&self) {
        self.gate.open();
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.bg.load(std::sync::atomic::Ordering::Acquire) > 0 {
            assert!(Instant::now() < deadline, "background work never drained");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn sandbox_dir(&self) -> std::path::PathBuf {
        crate::failure_slot::sandbox_dir(&self.engine, "p", "deaf")
    }
}

/// A sandboxed run abandoned after its timeout's grace may still have a live
/// process writing to its tree, so the failed-sandbox removal does not race
/// it: nothing is queued, the run's `sandbox` slot is back at once (while
/// removals are held, so a queued one would still hold it), the tree is left
/// on disk for the next run, and the failure box says so.
#[tokio::test]
async fn an_abandoned_runs_sandbox_is_left_and_its_slot_returned() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let run = Abandoned::run(ws.path(), false).await?;
    assert_eq!(
        run.sandbox_slots_held(),
        0,
        "no removal holds the slot: {:?}",
        run.engine.resource_snapshot()
    );
    run.drain().await;
    let dir = run.sandbox_dir();
    assert!(
        dir.exists(),
        "an abandoned run's tree is not removed: {}",
        dir.display()
    );
    assert_eq!(
        run.failure.sandbox,
        Some(hplugin::error::SandboxFate::LeftAbandoned),
        "{}",
        crate::common::failure_text(&run.failure)
    );
    Ok(())
}

/// Under `--keep-failed-sandboxes` an abandoned run's tree is kept like any
/// failed run's, and the box names where it is.
#[tokio::test]
async fn an_abandoned_runs_sandbox_is_kept_under_the_flag() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let run = Abandoned::run(ws.path(), true).await?;
    assert_eq!(run.sandbox_slots_held(), 0);
    run.drain().await;
    let dir = run.sandbox_dir();
    assert!(dir.exists(), "{}", dir.display());
    assert_eq!(
        run.failure.sandbox,
        Some(hplugin::error::SandboxFate::Kept(dir)),
        "{}",
        crate::common::failure_text(&run.failure)
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

/// The managed bash driver with heph's own work around its process made
/// slow: `before` inside `run` ahead of the spawn (what slow staging or a
/// slow runner preparation costs) and `after` once the process has exited
/// (what slow output collection costs). It arms the run clock exactly as bash
/// does, so the timeout sees what it would see on a real slow tree.
struct PaddedBash {
    inner: pluginexec::Driver,
    before: Duration,
    after: Duration,
}

#[async_trait::async_trait]
impl heph::engine::driver_managed::ManagedDriver for PaddedBash {
    fn config(
        &self,
        _req: hplugin::driver::ConfigRequest,
    ) -> anyhow::Result<hplugin::driver::ConfigResponse> {
        Ok(hplugin::driver::ConfigResponse {
            name: "padded".to_string(),
        })
    }
    fn schema(&self) -> hplugin::driver::DriverSchema {
        heph::engine::driver_managed::ManagedDriver::schema(&self.inner)
    }
    async fn parse(
        &self,
        req: hplugin::driver::ParseRequest,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ParseResponse> {
        heph::engine::driver_managed::ManagedDriver::parse(&self.inner, req, ctoken).await
    }
    async fn apply_transitive(
        &self,
        req: hplugin::driver::ApplyTransitiveRequest,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ApplyTransitiveResponse> {
        heph::engine::driver_managed::ManagedDriver::apply_transitive(&self.inner, req, ctoken)
            .await
    }
    async fn run<'a, 'io>(
        &self,
        req: heph::engine::driver_managed::ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<heph::engine::driver_managed::ManagedRunResponse> {
        tokio::time::sleep(self.before).await;
        let out = heph::engine::driver_managed::ManagedDriver::run(&self.inner, req, ctoken).await;
        tokio::time::sleep(self.after).await;
        out
    }
    fn arms_run_clock(&self) -> bool {
        heph::engine::driver_managed::ManagedDriver::arms_run_clock(&self.inner)
    }
}

/// An engine over `root` whose `padded` driver spends `before` and `after`
/// around the target's process.
fn padded_engine(root: &Path, before: Duration, after: Duration) -> anyhow::Result<Arc<Engine>> {
    engine_with(root, |e| {
        e.register_managed_driver(move |_| {
            Box::new(PaddedBash {
                inner: pluginexec::Driver::new_bash().with_host_path(),
                before,
                after,
            })
        })
    })
}

fn padded(run: &str) -> String {
    format!(
        r#"
target(
    name = "padded",
    driver = "padded",
    run = "{run}",
    out = "out.txt",
    timeout = 1,
)
"#
    )
}

/// The timeout covers the target's process, not heph's preparation of it: 3s
/// spent before the spawn does not time out a 1s target whose process is
/// quick.
#[tokio::test]
async fn slow_preparation_does_not_count_against_the_timeout() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(ws.path(), &padded("echo ok > $OUT"));
    let engine = padded_engine(ws.path(), Duration::from_secs(3), Duration::ZERO)?;
    assert_eq!(build(&engine, "//p:padded").await?, "ok");
    Ok(())
}

/// Nor does what heph does once the process has exited: 3s after it does not
/// time out a 1s target.
#[tokio::test]
async fn slow_collection_does_not_count_against_the_timeout() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(ws.path(), &padded("echo ok > $OUT"));
    let engine = padded_engine(ws.path(), Duration::ZERO, Duration::from_secs(3))?;
    assert_eq!(build(&engine, "//p:padded").await?, "ok");
    Ok(())
}

/// A process that outlasts its timeout still times out after a slow
/// preparation, from its spawn, and fails exactly as any timed-out target:
/// a failure, recorded, stopped through its own cancellation (not abandoned).
#[tokio::test]
async fn a_process_past_its_timeout_times_out_after_a_slow_preparation() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        &padded("echo started; sleep 60; echo done > $OUT"),
    );
    let engine = padded_engine(ws.path(), Duration::from_secs(2), Duration::ZERO)?;
    let started = Instant::now();
    let err = build(&engine, "//p:padded")
        .await
        .expect_err("a 60s process under a 1s timeout fails");
    let elapsed = started.elapsed();
    let msg = format!("{err:#}");
    assert!(msg.contains("//p:padded timed out after 1s"), "{msg}");
    assert!(
        msg.contains("failure //p:padded"),
        "recorded as a failure: {msg}"
    );
    assert!(
        !msg.contains("abandoned"),
        "the driver stopped when cancelled: {msg}"
    );
    assert!(
        elapsed >= Duration::from_secs(3),
        "the 2s before the spawn were not on the clock: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(11),
        "stopped at the timeout, not after the sleep or the grace: {elapsed:?}"
    );
    Ok(())
}

/// The `ResultEnd` of `addr`'s timed-out run: its typed timeout and the log
/// tail it carries. It is a root failure, not a stop: the CI view prints
/// nothing live for a stop.
fn timed_out_end(
    events: &[BuildEvent],
    addr: &str,
) -> (
    Option<hcore::events::TimedOutData>,
    Option<hcore::events::LogTailData>,
) {
    let ends: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            BuildEventKind::ResultEnd {
                addr: a,
                timed_out,
                log_tail,
                upstream_of,
                cancelled,
                ..
            } if a == addr => {
                assert_eq!(upstream_of, &None, "{addr} is the root");
                assert!(!cancelled, "{addr} timed out, it was not stopped");
                Some((timed_out.clone(), log_tail.clone()))
            }
            _ => None,
        })
        .collect();
    let [end] = ends.as_slice() else {
        panic!("one ResultEnd for {addr}, got {}", ends.len());
    };
    end.clone()
}

/// What a run prints before it outlasts its timeout: the lines the CI view
/// shows under the failure's header.
const CHATTY: &str = "for i in $(seq 1 30); do echo line $i; done; sleep 60; echo done > $OUT";

/// A timed-out run's `ResultEnd` carries the last lines of what it printed,
/// as any failure's does: the line backend prints them live under the
/// "timed out" header. Stopped within its grace, the run fills its failure
/// slot, and the tail comes from there.
#[tokio::test]
async fn a_timed_out_result_end_carries_its_log_tail() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        &format!(
            r#"target(name = "chatty", driver = "bash", run = "{CHATTY}", out = "out.txt", timeout = 1)"#
        ),
    );
    let engine = engine(ws.path())?;

    let (res, events) =
        crate::common::run_drain_events(&engine, &Matcher::Addr(parse_addr("//p:chatty")?)).await;
    assert!(res.is_err() || res.is_ok_and(|b| !b.errors.is_empty()));
    let (timed_out, tail) = timed_out_end(&events, "//p:chatty");
    assert_eq!(
        timed_out,
        Some(hcore::events::TimedOutData {
            timeout_ms: 1000,
            abandoned: false,
        })
    );
    let tail = tail.expect("a timed-out run's tail");
    let lines: Vec<&str> = tail.text.lines().collect();
    assert_eq!(lines.last(), Some(&"line 30"), "{lines:?}");
    // 30 lines are under the default tail (1000): the whole log.
    assert_eq!(lines.first(), Some(&"line 1"), "{lines:?}");
    assert_eq!(lines.len(), 30, "{lines:?}");
    Ok(())
}

/// An abandoned run's `ResultEnd` carries the last lines it printed before it
/// was abandoned, read from its live log: the run never returned, so nothing
/// it handed back can hold them.
#[tokio::test]
async fn an_abandoned_result_end_carries_its_log_tail() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        &format!(
            r#"target(name = "deaf", driver = "deaf", run = "{CHATTY}", out = "out.txt", timeout = 1)"#
        ),
    );
    let engine = engine_with(ws.path(), |e| {
        e.set_timeout_grace(Duration::from_millis(200));
        e.register_managed_driver(|_| {
            Box::new(DeafBash(pluginexec::Driver::new_bash().with_host_path()))
        })
    })?;

    let (res, events) = tokio::time::timeout(
        Duration::from_secs(30),
        crate::common::run_drain_events(&engine, &Matcher::Addr(parse_addr("//p:deaf")?)),
    )
    .await
    .expect("an abandoned run does not hang the build");
    assert!(res.is_err() || res.is_ok_and(|b| !b.errors.is_empty()));
    let (timed_out, tail) = timed_out_end(&events, "//p:deaf");
    assert_eq!(
        timed_out,
        Some(hcore::events::TimedOutData {
            timeout_ms: 1000,
            abandoned: true,
        })
    );
    let tail = tail.expect("an abandoned run's tail");
    let lines: Vec<&str> = tail.text.lines().collect();
    assert_eq!(lines.last(), Some(&"line 30"), "{lines:?}");
    assert_eq!(lines.first(), Some(&"line 1"), "{lines:?}");
    assert_eq!(lines.len(), 30, "{lines:?}");
    Ok(())
}

/// Build a bash target running `script` under a 1s timeout, as the binary
/// does: on a multi-thread runtime. Returns how long the build took and the
/// `ResultEnd`'s timeout and tail.
///
/// The other tests here run on a current-thread runtime, where exec's output
/// tee awaits rather than parks, and so never saw that on macOS a run whose
/// pipes stay open used to be stopped only once it closed them: `sleep 60`
/// ran its minute, then failed as "timed out after 1s".
async fn timed_out_on_multi_thread(
    script: &str,
) -> anyhow::Result<(
    Duration,
    Option<hcore::events::TimedOutData>,
    Option<hcore::events::LogTailData>,
)> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        &format!(
            "target(name = \"t\", driver = \"bash\", run = '''{script}''', out = \"out.txt\", timeout = 1)"
        ),
    );
    let engine = engine(ws.path())?;
    let started = Instant::now();
    let (res, events) = tokio::time::timeout(
        Duration::from_secs(120),
        crate::common::run_drain_events(&engine, &Matcher::Addr(parse_addr("//p:t")?)),
    )
    .await
    .expect("the build ends");
    let took = started.elapsed();
    assert!(res.is_err() || res.is_ok_and(|b| !b.errors.is_empty()));
    let (timed_out, tail) = timed_out_end(&events, "//p:t");
    Ok((took, timed_out, tail))
}

/// Well inside the engine's grace, and far from the minute the run would take.
const STOPPED_WITHIN: Duration = Duration::from_secs(15);

/// A run that keeps its pipes open is stopped at its timeout on the binary's
/// runtime, not when it closes them, and keeps what it printed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_holding_its_pipes_is_stopped_at_its_timeout_on_a_multi_thread_runtime()
-> anyhow::Result<()> {
    let (took, timed_out, tail) =
        timed_out_on_multi_thread("for i in $(seq 1 30); do echo line $i; done; sleep 60").await?;
    assert!(
        took < STOPPED_WITHIN,
        "stopped at its timeout: took {took:?}"
    );
    assert_eq!(
        timed_out,
        Some(hcore::events::TimedOutData {
            timeout_ms: 1000,
            abandoned: false,
        })
    );
    let tail = tail.expect("its tail");
    assert_eq!(tail.text.lines().last(), Some("line 30"), "{tail:?}");
    Ok(())
}

/// The shape of a test runner: a parent waiting on workers that share its
/// output, and print on their way out when interrupted. Their lines, the
/// last ones printed after the cancel included, reach the tail.
///
/// The timeout's SIGINT lands 1s after the spawn, wherever the script has
/// got to. A runner stalled for that whole second can take it before it
/// is ready (once in CI, run 44826814881), so the script is built for
/// every early landing to end in one of two shapes that are retried:
///   - before the runner's `trap '' INT`: bash dies of it at once, with
///     no worker line.
///   - before the worker's `trap`: the worker ignores it, as bash starts
///     a `&` job with SIGINT ignored and the runner ignores it too. The
///     worker prints all its lines, and only the SIGKILL after the cancel
///     grace ends it.
///
/// The worker prints before it traps, so a SIGINT can never cut its lines
/// short or arrive between them and the trap. A tail that has every
/// worker line and no interrupt line, well inside the grace, is a worker
/// whose last line was lost: that still fails at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_runners_workers_output_survives_its_timeout_on_a_multi_thread_runtime()
-> anyhow::Result<()> {
    const ATTEMPTS: usize = 3;
    let missed_trap = Duration::from_secs(1) + heph::proc_exec::CANCEL_GRACE;
    let mut attempt = 1;
    let (took, timed_out, tail) = loop {
        let (took, timed_out, tail) = timed_out_on_multi_thread(
            "trap '' INT; ( for i in $(seq 1 30); do echo worker $i; done; \
             trap 'echo worker interrupted; exit 1' INT; sleep 60 & wait ) & wait",
        )
        .await?;
        let has = |line: &str| tail.as_ref().is_some_and(|t| t.text.contains(line));
        let early = if has("worker interrupted") {
            None
        } else if took >= missed_trap {
            Some("before the worker's trap")
        } else if !has("worker ") {
            Some("before the runner's trap")
        } else {
            None
        };
        let Some(early) = early.filter(|_| attempt < ATTEMPTS) else {
            break (took, timed_out, tail);
        };
        eprintln!("attempt {attempt}: the SIGINT landed {early} (took {took:?}); running again");
        attempt += 1;
    };
    assert!(
        took < STOPPED_WITHIN,
        "stopped at its timeout: took {took:?}"
    );
    assert!(
        timed_out.as_ref().is_some_and(|t| !t.abandoned),
        "{timed_out:?}"
    );
    let tail = tail.expect("its tail");
    assert!(tail.text.contains("worker 30"), "{tail:?}");
    assert_eq!(
        tail.text.lines().last(),
        Some("worker interrupted"),
        "{tail:?}"
    );
    Ok(())
}
