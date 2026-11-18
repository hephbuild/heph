//! `heph log`, in-process: [`heph::commands::log::show`] over a real engine.
//! Plain `heph log` only reads what heph holds (I10); `--current` is
//! `Engine::meta`, building the dependencies its hash needs and never the
//! target.
#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::log_api::{build, engine, write_build};
use heph::commands::log::{Request, Shown, show};
use heph::engine::fault_provider::{FaultProvider, Faults};
use heph::engine::{Engine, LogOptions, LogSource};
use heph::htaddr::parse_addr;
use heph::pluginexec;
use heph::pluginstatictarget::Target;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A writer the test reads back: what the command wrote to stdout or stderr.
#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Buf {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("buf")).into_owned()
    }
}

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_poisoned| std::io::Error::other("buf poisoned"))?
            .extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `heph log [--current] addr` on `engine`: what it returned, its stdout and
/// its stderr.
async fn log(
    engine: &Arc<Engine>,
    addr: &str,
    current: bool,
) -> anyhow::Result<(Shown, String, String)> {
    let (out, err) = (Buf::default(), Buf::default());
    let req = Request {
        current,
        ..Request::default()
    };
    let shown = show(
        engine,
        &engine.new_state(),
        &parse_addr(addr)?,
        req,
        LogOptions::default(),
        out.clone(),
        err.clone(),
    )
    .await?;
    Ok((shown, out.text(), err.text()))
}

/// The input hash of the run whose log was shown; `""` when none was.
fn hashin_of(shown: &Shown) -> &str {
    match shown {
        Shown::Log(info) => &info.hashin,
        Shown::Nothing => "",
    }
}

/// spec `log_cmd_never_builds` (I10): on a target with no log, `heph log`
/// says "no log" and shows nothing, with no provider `get`, no listing, no
/// execute and no lock taken: the target's exclusive locks are held by the
/// test throughout, so a command that took either would never return.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn log_cmd_never_builds() -> anyhow::Result<()> {
    let c = tempfile::tempdir()?;
    let marker = c.path().join("ran");
    let provider = FaultProvider::new(
        vec![Target {
            addr: "//p:t".to_string(),
            driver: "bash".to_string(),
            run: Some(format!("touch {}; printf x > out.txt", marker.display())),
            out: HashMap::from([(String::new(), vec!["out.txt".to_string()])]),
            ..Default::default()
        }],
        Faults::default(),
    )?;
    let (gets, lists, packages) = (
        provider.gets(),
        provider.list_calls(),
        provider.list_packages_calls(),
    );
    let ws = htestkit::WorkspaceBuilder::new()?
        .with_provider(move |_| Box::new(provider))
        .with_managed_driver(Box::new(pluginexec::Driver::new_bash()))
        .build()?;
    let addr = parse_addr("//p:t")?;
    let target = ws.engine.result_lock().try_lock_target(&addr)?;
    let execute = ws.engine.result_lock().try_lock_execute(&addr)?;
    assert!(target.is_some() && execute.is_some(), "the test holds both");

    let (shown, out, err) =
        tokio::time::timeout(Duration::from_secs(60), log(&ws.engine, "//p:t", false))
            .await
            .map_err(|_elapsed| anyhow::anyhow!("heph log waited on a lock"))??;
    assert!(matches!(shown, Shown::Nothing), "{shown:?}");
    assert_eq!(out, "", "nothing on stdout");
    assert!(err.contains("no log for //p:t"), "{err}");
    assert!(gets.addrs().is_empty(), "provider get: {:?}", gets.addrs());
    assert_eq!(lists.load(Ordering::SeqCst), 0, "provider list");
    assert_eq!(packages.load(Ordering::SeqCst), 0, "package listing");
    assert!(!marker.exists(), "the target ran");
    drop((target, execute));
    Ok(())
}

/// `//p:dep` writes `v` (its log and its output); `//p:t` depends on it and
/// touches `{c}/t-ran` each time it runs. Changing `v` changes only the
/// dependency, so `//p:t`'s input hash moves through its hashout. `gate`
/// makes the dependency wait for `{c}/gate` first.
fn current_build(root: &Path, c: &Path, v: u32, gate: bool) -> anyhow::Result<()> {
    let c = c.display();
    let wait = if gate {
        format!("while [ ! -e {c}/gate ]; do sleep 0.02; done; ")
    } else {
        String::new()
    };
    write_build(
        root,
        &format!(
            r#"target(name = "dep", driver = "bash", out = "d.txt", run = "touch {c}/dep-started; {wait}echo dep-{v}; printf {v} > d.txt")
target(name = "t", driver = "bash", out = "t.txt", deps = [":dep"], run = "echo t-run; touch {c}/t-ran; printf x > t.txt")
target(name = "bad", driver = "bash", out = "b.txt", deps = [":dep"], run = "echo bad-run; exit 3")
"#
        ),
    )
}

/// spec `log_cmd_current`: after a run on old inputs and an input change,
/// plain `heph log` shows the old run with its date; `--current` builds what
/// the hash needs, never executes the target, finds no log for the new hash
/// and says so; after a build, `--current` shows the new run. While it
/// builds, stderr says `building N dependencies to hash //x`, and a cancel
/// (ctrl-c) aborts it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn log_cmd_current() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let c = tempfile::tempdir()?;
    let (root, ran) = (ws.path(), c.path().join("t-ran"));

    current_build(root, c.path(), 1, false)?;
    let e = engine(root, None)?;
    build(&e, "//p:t").await?;
    let (old, out, _) = log(&e, "//p:t", false).await?;
    let old = hashin_of(&old).to_owned();
    assert!(!old.is_empty(), "the run's log is shown");
    assert_eq!(out, "t-run\n");
    drop(e);
    std::fs::remove_file(&ran)?;

    // The dependency changed: the latest log is still the old run's.
    current_build(root, c.path(), 2, false)?;
    let e = engine(root, None)?;
    let (shown, out, err) = log(&e, "//p:t", false).await?;
    assert_eq!(hashin_of(&shown), old);
    assert_eq!(out, "t-run\n");
    assert!(
        err.contains("//p:t · cached") && err.contains(" ago"),
        "the status line says when and from where: {err}"
    );

    // `--current` hashes the new inputs, building the dependency, and finds
    // no log for them.
    let (shown, out, err) = log(&e, "//p:t", true).await?;
    assert!(matches!(shown, Shown::Nothing), "{shown:?}");
    assert_eq!(out, "");
    assert!(
        err.contains("building 1 dependency to hash //p:t"),
        "the progress line: {err}"
    );
    assert!(
        err.contains("no log for //p:t at its current inputs"),
        "{err}"
    );
    assert!(!ran.exists(), "--current ran the target");
    assert!(
        c.path().join("dep-started").exists(),
        "the dependency built"
    );

    // Built at the new inputs: `--current` shows that run.
    build(&e, "//p:t").await?;
    let (shown, out, _) = log(&e, "//p:t", true).await?;
    assert!(matches!(shown, Shown::Log(_)), "{shown:?}");
    assert_ne!(hashin_of(&shown), old);
    assert_eq!(out, "t-run\n");
    drop(e);

    // A cancel while the dependency builds aborts `--current`.
    std::fs::remove_file(c.path().join("dep-started"))?;
    std::fs::remove_file(&ran)?;
    current_build(root, c.path(), 3, true)?;
    let e = engine(root, None)?;
    let rs = e.new_state();
    let err = Buf::default();
    let cmd = tokio::spawn({
        let (e, rs, err) = (Arc::clone(&e), rs.clone(), err.clone());
        async move {
            show(
                &e,
                &rs,
                &parse_addr("//p:t")?,
                Request {
                    current: true,
                    ..Request::default()
                },
                LogOptions::default(),
                Buf::default(),
                err,
            )
            .await
        }
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    while !(c.path().join("dep-started").exists() && err.text().contains("building 1 dependency")) {
        assert!(Instant::now() < deadline, "the dependency never started");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    rs.ctoken().cancel();
    let res = tokio::time::timeout(Duration::from_secs(30), cmd)
        .await
        .map_err(|_elapsed| anyhow::anyhow!("the cancel did not abort --current"))??;
    assert!(res.is_err(), "a cancelled --current fails: {res:?}");
    assert!(!ran.exists(), "--current ran the target");
    // Let the abandoned dependency finish before the workspace goes.
    std::fs::write(c.path().join("gate"), "")?;
    Ok(())
}

/// spec `log_cmd_current_is_meta`: the input hash `--current` uses is the
/// one the build computed for the same target: the cache entry a success
/// wrote, the failure slot a failure left, and `Engine::meta`'s.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn log_cmd_current_is_meta() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let c = tempfile::tempdir()?;
    let root = ws.path();
    current_build(root, c.path(), 1, false)?;
    let e = engine(root, None)?;
    build(&e, "//p:t").await?;
    assert!(build(&e, "//p:bad").await.is_err(), "//p:bad fails");

    for (addr, source) in [
        ("//p:t", LogSource::Cache { remote: false }),
        ("//p:bad", LogSource::FailureSlot),
    ] {
        let (latest, _, _) = log(&e, addr, false).await?;
        let (current, _, _) = log(&e, addr, true).await?;
        let Shown::Log(info) = &current else {
            panic!("{addr}: --current found no log");
        };
        assert_eq!(info.source, source, "{addr}");
        assert_eq!(info.hashin, hashin_of(&latest), "{addr}: the build's hash");
        let meta = Arc::clone(&e)
            .meta(e.new_state(), &parse_addr(addr)?)
            .await?
            .hashin;
        assert_eq!(info.hashin, meta, "{addr}: Engine::meta's hash");
    }
    let slot = e
        .failure_slots()
        .read(&parse_addr("//p:bad")?)
        .expect("a slot");
    let (current, _, _) = log(&e, "//p:bad", true).await?;
    assert_eq!(hashin_of(&current), slot.hashin, "the slot's hash");
    Ok(())
}
