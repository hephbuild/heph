//! A run's live log (`<home>/runs/<engine-id>/<n>.log`) as the engine keeps
//! it: what an older plugin's returned log does to it, and that it reaches no
//! hash. The writer, scrubber and file life are unit-tested in
//! `crates/engine/src/engine/run_log.rs`.

#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use hcore::events::CacheOutcomeKind;
use heph::engine::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use heph::engine::event::BuildEventKind;
use heph::engine::{Engine, OutputMatcher, ResultOptions};
use heph::hartifactcontent::WalkEntryKind;
use heph::htaddr::parse_addr;
use heph::{pluginbuildfile, pluginexec};
use hplugin::driver::outputartifact::{self, OutputArtifact};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The credential every `legacy` target is handed: what a leaky older plugin
/// writes into the log it returns.
const SECRET: &str = "legacy-plugin-s3cr3t-value";

const RETURNED: &str = "returned log with ";

/// The bytes of the cache entry's `Log` for `addr` at its current `hashin`.
async fn cached_log(engine: &Arc<Engine>, addr: &str) -> anyhow::Result<Option<String>> {
    let addr = parse_addr(addr)?;
    let hashin = Arc::clone(engine)
        .meta(engine.new_state(), &addr)
        .await?
        .hashin;
    let Some(log) = engine.cached_log(&addr, &hashin)? else {
        return Ok(None);
    };
    let mut out = Vec::new();
    for entry in log.walk()? {
        if let WalkEntryKind::File { mut data, .. } = entry?.kind {
            std::io::Read::read_to_end(&mut data, &mut out)?;
        }
    }
    Ok(Some(String::from_utf8(out)?))
}

/// How a `legacy` target returns its log, from its name's prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Returns {
    /// A regular file inside the sandbox: imported.
    Inside,
    /// An inline `Content::Raw`: imported.
    Raw,
    /// A file outside the sandbox: "log unavailable".
    Outside,
    /// A symlink inside the sandbox to a file inside it: "log unavailable".
    Symlink,
    /// A file inside the sandbox nobody may read: "log unavailable".
    Unreadable,
}

impl Returns {
    const ALL: [Self; 5] = [
        Self::Inside,
        Self::Raw,
        Self::Outside,
        Self::Symlink,
        Self::Unreadable,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Inside => "inside",
            Self::Raw => "raw",
            Self::Outside => "outside",
            Self::Symlink => "symlink",
            Self::Unreadable => "unreadable",
        }
    }

    fn imported(self) -> bool {
        matches!(self, Self::Inside | Self::Raw)
    }
}

/// An older plugin: the bash driver, which streams to the writers it is
/// handed, and then returns a log of its own as well, as a plugin from before
/// `log_capture` did.
struct Legacy {
    bash: pluginexec::Driver,
    /// Where the `outside` variant's file lives.
    outside: PathBuf,
}

/// A file named `name` under `dir` holding the returned log.
fn returned_file(dir: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let path = dir.join(name);
    std::fs::write(&path, format!("{RETURNED}{SECRET}\n"))?;
    Ok(path)
}

fn log_artifact(content: outputartifact::Content) -> OutputArtifact {
    OutputArtifact {
        group: String::new(),
        name: "log.txt".to_string(),
        r#type: outputartifact::Type::Log,
        content,
        hashout: String::new(),
    }
}

fn file_content(path: &Path) -> anyhow::Result<outputartifact::Content> {
    Ok(outputartifact::Content::File(outputartifact::ContentFile {
        source_path: path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("not UTF-8"))?
            .to_string(),
        out_path: "log.txt".to_string(),
        x: false,
        passthrough: false,
    }))
}

impl Legacy {
    /// The log this run returns, made once the run is over and while its
    /// sandbox exists.
    fn returned(&self, returns: Returns, sandbox: &Path) -> anyhow::Result<OutputArtifact> {
        let content = match returns {
            Returns::Inside => file_content(&returned_file(sandbox, "legacy.log")?)?,
            Returns::Raw => outputartifact::Content::Raw(outputartifact::ContentRaw {
                data: format!("{RETURNED}{SECRET}\n").into_bytes(),
                path: "log.txt".to_string(),
                x: false,
            }),
            Returns::Outside => {
                std::fs::create_dir_all(&self.outside)?;
                file_content(&returned_file(&self.outside, "legacy.log")?)?
            }
            Returns::Symlink => {
                let target = returned_file(sandbox, "legacy-target.log")?;
                let link = sandbox.join("legacy.log");
                std::os::unix::fs::symlink(&target, &link)?;
                file_content(&link)?
            }
            Returns::Unreadable => {
                let path = returned_file(sandbox, "legacy.log")?;
                std::fs::set_permissions(
                    &path,
                    std::os::unix::fs::PermissionsExt::from_mode(0o000),
                )?;
                file_content(&path)?
            }
        };
        Ok(log_artifact(content))
    }
}

#[async_trait::async_trait]
impl ManagedDriver for Legacy {
    fn config(
        &self,
        _req: hplugin::driver::ConfigRequest,
    ) -> anyhow::Result<hplugin::driver::ConfigResponse> {
        Ok(hplugin::driver::ConfigResponse {
            name: "legacy".to_string(),
        })
    }
    fn schema(&self) -> hplugin::driver::DriverSchema {
        ManagedDriver::schema(&self.bash)
    }
    async fn parse(
        &self,
        req: hplugin::driver::ParseRequest,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ParseResponse> {
        ManagedDriver::parse(&self.bash, req, ctoken).await
    }
    async fn apply_transitive(
        &self,
        req: hplugin::driver::ApplyTransitiveRequest,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ApplyTransitiveResponse> {
        ManagedDriver::apply_transitive(&self.bash, req, ctoken).await
    }
    async fn run<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        assert!(
            req.request.log_capture,
            "the engine hands a captured run its writers"
        );
        let name = req.request.target.addr.name.to_string();
        let returns = Returns::ALL
            .into_iter()
            .find(|r| name.starts_with(r.name()))
            .ok_or_else(|| anyhow::anyhow!("no variant in {name}"))?;
        let sandbox = req.sandbox_dir.clone();
        let ran = ManagedDriver::run(&self.bash, req, ctoken).await;
        let returned = self.returned(returns, &sandbox)?;
        match ran {
            Ok(mut res) => {
                res.artifacts.push(returned);
                Ok(res)
            }
            Err(e) => Err(hplugin::error::with_failure_artifacts(e, vec![returned])),
        }
    }
}

/// A failure slot's `log.txt`, when the slot is complete.
fn slot_log(engine: &Engine, addr: &str) -> Option<String> {
    let addr = parse_addr(addr).expect("addr");
    let slots = engine.failure_slots();
    slots.read(&addr)?;
    std::fs::read_to_string(slots.slot_dir(&addr).join("log.txt")).ok()
}

/// An older plugin streams *and* returns its own log. A returned regular file
/// inside the sandbox, or an inline log, is imported, scrubbed, and replaces
/// what it streamed, in the cache entry on success and the failure slot on
/// failure. A returned path outside the sandbox, a symlink, or an unreadable
/// file is "log unavailable": the streamed bytes are kept and nothing of the
/// returned log is.
#[tokio::test]
async fn legacy_plugin_log_imported() -> anyhow::Result<()> {
    let outside = tempfile::tempdir()?;
    let ws = htestkit::WorkspaceBuilder::new()?
        .with_provider(|init| {
            Box::new(pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
                init.runtime.clone(),
                Arc::clone(&init.functions),
            ))
        })
        .with_managed_driver(Box::new(pluginexec::Driver::new_bash().with_host_path()))
        .with_managed_driver(Box::new(Legacy {
            bash: pluginexec::Driver::new_bash().with_host_path(),
            outside: outside.path().join("logs"),
        }))
        .build()?;
    ws.write_build_file(
        "auth",
        &format!(
            r#"target(name = "t", driver = "auth.credential",
       sources = [heph.auth.exec(["sh", "-c", "printf '{{\"token\":\"{SECRET}\"}}'"],
           fields = {{"token": "token"}})],
       present = {{"env": {{"TOKEN": "${{token}}"}}}})
"#
        ),
    );
    let mut build = String::new();
    for returns in Returns::ALL {
        for (suffix, exit) in [("ok", "0"), ("fail", "3")] {
            build.push_str(&format!(
                r#"target(name = "{name}_{suffix}", driver = "legacy", out = "o.txt",
       credentials = ["//auth:t"],
       run = ["echo \"streamed-out $TOKEN\"", "echo streamed-err >&2",
              "printf ok > o.txt", "exit {exit}"])
"#,
                name = returns.name(),
            ));
        }
    }
    ws.write_build_file("app", &build);

    for returns in Returns::ALL {
        if returns == Returns::Unreadable && running_privileged() {
            // An unreadable file is readable to a privileged user.
            continue;
        }
        let ok = format!("//app:{}_ok", returns.name());
        ws.run(&ok).await?;
        let log = cached_log(&ws.engine, &ok)
            .await?
            .unwrap_or_else(|| panic!("{ok}: the entry keeps a log"));
        let fail = format!("//app:{}_fail", returns.name());
        assert!(ws.run(&fail).await.is_err(), "{fail} exits 3");
        let failed =
            slot_log(&ws.engine, &fail).unwrap_or_else(|| panic!("{fail}: the slot keeps a log"));
        for (addr, log) in [(&ok, &log), (&fail, &failed)] {
            assert!(!log.contains(SECRET), "{addr}: scrubbed: {log:?}");
            if returns.imported() {
                assert_eq!(
                    log,
                    &format!("{RETURNED}[redacted]\n"),
                    "{addr}: the returned log wins over the streamed bytes"
                );
            } else {
                assert!(
                    log.contains("streamed-out [redacted]") && log.contains("streamed-err"),
                    "{addr}: the streamed bytes are kept: {log:?}"
                );
                assert!(
                    !log.contains(RETURNED),
                    "{addr}: nothing of a refused log is imported: {log:?}"
                );
            }
        }
    }
    Ok(())
}

/// Whether this process reads files whatever their mode (root).
fn running_privileged() -> bool {
    let probe = tempfile::NamedTempFile::new().expect("probe");
    std::fs::set_permissions(
        probe.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o000),
    )
    .expect("chmod");
    std::fs::File::open(probe.path()).is_ok()
}

/// The `bash`-only engine over `root`.
fn engine(root: &Path) -> anyhow::Result<Arc<Engine>> {
    let mut e = Engine::new(heph::engine::Config::for_tests(root.to_path_buf()))?;
    let r = root.to_path_buf();
    e.register_provider(move |init| {
        Box::new(pluginbuildfile::Provider::new(
            r,
            init.runtime.clone(),
            Arc::clone(&init.functions),
        ))
    })?;
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash().with_host_path()))?;
    Ok(Arc::new(e))
}

async fn result(
    engine: &Arc<Engine>,
    addr: &str,
    opts: &ResultOptions,
) -> anyhow::Result<Arc<hplugin::eresult::EResult>> {
    Arc::clone(engine)
        .result_addr(
            engine.new_state(),
            &parse_addr(addr)?,
            OutputMatcher::All,
            opts,
        )
        .await
}

/// [`result`] in a request with an event stream, and every event of it,
/// through its end: the stream closes when the request's data drops.
async fn result_with_events(
    engine: &Arc<Engine>,
    addr: &str,
) -> anyhow::Result<Vec<BuildEventKind>> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = engine.new_state_with_events(false, Some(tx));
    Arc::clone(engine)
        .result_addr(
            rs,
            &parse_addr(addr)?,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await?;
    let mut out = Vec::new();
    let collect = async {
        while let Some(ev) = rx.recv().await {
            out.push(ev.kind);
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(60), collect)
        .await
        .map_err(|_elapsed| anyhow::anyhow!("the request's stream never ended"))?;
    Ok(out)
}

fn pool_usage(events: &[BuildEventKind]) -> Vec<&BuildEventKind> {
    events
        .iter()
        .filter(|e| matches!(e, BuildEventKind::PoolUsage { .. }))
        .collect()
}

/// Every `*.log` under `<home>/runs/`.
fn live_files(engine: &Engine) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(dirs) = std::fs::read_dir(engine.home.join("runs")) else {
        return out;
    };
    for dir in dirs.flatten() {
        for f in std::fs::read_dir(dir.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            if f.path().extension().is_some_and(|e| e == "log") {
                out.push(f.path());
            }
        }
    }
    out
}

/// I6: a log is diagnostics only. Two forced runs whose logs differ give the
/// same output hash, and a dependent stays a cache hit; nothing the target
/// sees (env, argv, its tree) names `runs/`. A warm, all-hit build creates no
/// live file.
#[tokio::test]
async fn log_never_reaches_hashin() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    std::fs::create_dir_all(ws.path().join("p"))?;
    std::fs::write(
        ws.path().join("p").join("BUILD"),
        r#"
target(name = "t", driver = "bash", out = "t.txt",
       run = ["echo \"noise $RANDOM$RANDOM $$\"", "echo \"noise $RANDOM\" >&2",
              "{ env; echo \"argv: $0 $*\"; find .. ../.. 2>/dev/null; } > t.txt"])
target(name = "d", driver = "bash", out = "d.txt", deps = [":t"],
       run = "cat $SRC > d.txt")
"#,
    )?;
    let engine = engine(ws.path())?;
    let forced = ResultOptions {
        force: true,
        ..Default::default()
    };

    // A result holds a read lock on its entry: read what is needed and drop
    // it before the next forced run of the same target.
    let hashouts = |r: Arc<hplugin::eresult::EResult>| -> Vec<String> {
        r.artifacts_meta.iter().map(|m| m.hashout.clone()).collect()
    };
    let first = hashouts(result(&engine, "//p:t", &forced).await?);
    let first_log = cached_log(&engine, "//p:t").await?.expect("first log");
    drop(result(&engine, "//p:d", &ResultOptions::default()).await?);
    let second_res = result(&engine, "//p:t", &forced).await?;
    let seen = htestkit::artifact_string(&second_res);
    let second = hashouts(second_res);
    let second_log = cached_log(&engine, "//p:t").await?.expect("second log");
    assert_ne!(first_log, second_log, "the two runs' logs differ");
    assert_eq!(first, second, "a log never reaches an output hash");

    // What the target saw names no live log.
    let runs = engine.home.join("runs");
    assert!(
        !seen.contains(&*runs.to_string_lossy()) && !seen.contains("/runs/"),
        "a runs/ path reached the target: {seen}"
    );
    assert!(
        !seen.lines().any(|l| l.ends_with(".log")),
        "a log file in the target's tree: {seen}"
    );

    // The dependent is a hit after the second run.
    let (res, events) = crate::common::run_drain_events(
        &engine,
        &heph::htmatcher::Matcher::Addr(parse_addr("//p:d")?),
    )
    .await;
    assert!(res?.errors.is_empty());
    let outcome: Vec<CacheOutcomeKind> = events
        .iter()
        .filter_map(|e| match &e.kind {
            BuildEventKind::CacheOutcome { addr, outcome } if addr == "//p:d" => Some(*outcome),
            _ => None,
        })
        .collect();
    assert_eq!(outcome, [CacheOutcomeKind::LocalHit], "{outcome:?}");

    // A warm, all-hit build on a fresh engine writes no live file and makes
    // no runs/ directory of its own.
    let before = runs_entries(&engine);
    let warm = self::engine(ws.path())?;
    result(&warm, "//p:d", &ResultOptions::default()).await?;
    result(&warm, "//p:t", &ResultOptions::default()).await?;
    assert_eq!(
        runs_entries(&warm),
        before,
        "an all-hit build touched runs/"
    );
    assert!(live_files(&warm).is_empty(), "{:?}", live_files(&warm));
    Ok(())
}

/// The `RunLog` records an engine announced.
#[derive(Debug, Default)]
struct Announced(std::sync::Mutex<Vec<hcore::session_record::RunLogRecord>>);

impl hcore::session_record::RunLogAnnouncer for Announced {
    fn run_log_created(&self, record: hcore::session_record::RunLogRecord) {
        self.0.lock().expect("announced").push(record);
    }
}

/// The `bash`-only engine over `root`, announcing its live logs to `to`.
fn announcing_engine(root: &Path, to: &Arc<Announced>) -> anyhow::Result<Arc<Engine>> {
    let mut e = Engine::new(heph::engine::Config::for_tests(root.to_path_buf()))?;
    e.set_run_log_announcer(Arc::clone(to) as _);
    let r = root.to_path_buf();
    e.register_provider(move |init| {
        Box::new(pluginbuildfile::Provider::new(
            r,
            init.runtime.clone(),
            Arc::clone(&init.functions),
        ))
    })?;
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash().with_host_path()))?;
    Ok(Arc::new(e))
}

/// A cold run announces its live log once (addr, hashin, the file relative
/// to the home); a warm, all-hit build writes no live file, announces no
/// `RunLog` record and sends no `PoolUsage`.
#[tokio::test]
async fn warm_build_pays_nothing_for_logs() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    std::fs::create_dir_all(ws.path().join("p"))?;
    std::fs::write(
        ws.path().join("p").join("BUILD"),
        r#"target(name = "t", driver = "bash", out = "t.txt", run = ["echo hi", "printf t > t.txt"])"#,
    )?;
    let cold_seen = Arc::new(Announced::default());
    let cold = announcing_engine(ws.path(), &cold_seen)?;
    let cold_events = result_with_events(&cold, "//p:t").await?;
    // The run was admitted, so the pool's use was reported: the warm
    // build's silence below is not a stream that never carries it.
    assert!(
        !pool_usage(&cold_events).is_empty(),
        "a cold build reported no PoolUsage"
    );
    let hashin = Arc::clone(&cold)
        .meta(cold.new_state(), &parse_addr("//p:t")?)
        .await?
        .hashin;
    let records = cold_seen.0.lock().expect("announced").clone();
    assert_eq!(records.len(), 1, "{records:?}");
    let rec = records.first().expect("one record");
    assert_eq!(
        (rec.addr.as_str(), rec.hashin.as_str()),
        ("//p:t", &*hashin)
    );
    assert!(rec.relative_file().is_some(), "{rec:?}");
    drop(cold);

    let warm_seen = Arc::new(Announced::default());
    let warm = announcing_engine(ws.path(), &warm_seen)?;
    let before = runs_entries(&warm);
    let warm_events = result_with_events(&warm, "//p:t").await?;
    assert!(
        warm_seen.0.lock().expect("announced").is_empty(),
        "a hit announced a RunLog record"
    );
    assert!(live_files(&warm).is_empty(), "{:?}", live_files(&warm));
    assert_eq!(
        runs_entries(&warm),
        before,
        "an all-hit build touched runs/"
    );
    // It admitted nothing, so it reports no pool use either.
    assert!(
        pool_usage(&warm_events).is_empty(),
        "{:?}",
        pool_usage(&warm_events)
    );
    Ok(())
}

/// The names under `<home>/runs/`.
fn runs_entries(engine: &Engine) -> std::collections::BTreeSet<std::ffi::OsString> {
    std::fs::read_dir(engine.home.join("runs"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name())
        .collect()
}

/// A run that writes no byte creates no live file, and no `Log` reaches its
/// cache entry.
#[tokio::test]
async fn a_quiet_run_creates_no_live_file() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    std::fs::create_dir_all(ws.path().join("p"))?;
    std::fs::write(
        ws.path().join("p").join("BUILD"),
        r#"target(name = "q", driver = "bash", out = "q.txt", run = "printf q > q.txt")"#,
    )?;
    let engine = engine(ws.path())?;
    result(&engine, "//p:q", &ResultOptions::default()).await?;
    assert!(
        !engine.home.join("runs").exists(),
        "a quiet run made no runs/ directory"
    );
    assert_eq!(cached_log(&engine, "//p:q").await?, None);
    Ok(())
}

/// Older than a sweep's ten-minute grace for `.new-` and tomb directories.
const STALE: std::time::Duration = std::time::Duration::from_secs(20 * 60);

/// What a dead heph leaves in `runs/`, every one of which a sweep removes: a
/// directory whose lock nobody holds, one with no lock file at all (a removal
/// cut short), and a stale `.new-` and tomb. Plus a fresh `.new-`, which is
/// another engine setting up its directory right now and is kept.
fn seed_dead(runs: &Path) -> anyhow::Result<()> {
    for (name, lock) in [("released", true), ("lockless", false)] {
        let d = runs.join(name);
        std::fs::create_dir_all(&d)?;
        std::fs::write(d.join("0.log"), "dead")?;
        if lock {
            std::fs::File::create(d.join("lock"))?;
        }
    }
    for name in [".new-stale-1", ".tomb-stale-1"] {
        let d = runs.join(name);
        std::fs::create_dir_all(&d)?;
        std::fs::write(d.join("0.log"), "dead")?;
        std::fs::File::open(&d)?.set_modified(std::time::SystemTime::now() - STALE)?;
    }
    std::fs::create_dir_all(runs.join(".new-fresh-2"))?;
    Ok(())
}

fn names<'a>(
    it: impl IntoIterator<Item = &'a str>,
) -> std::collections::BTreeSet<std::ffi::OsString> {
    it.into_iter().map(std::ffi::OsString::from).collect()
}

/// gc, clean and an engine's start sweep remove a `runs/<id>/` whose own
/// lock is free (a killed heph's, its `cache = false` live files with it) and
/// keep it while the lock is held, a live engine's own directory included.
/// The owning engine's drop removes its own directory.
#[tokio::test]
async fn runs_dir_gc_respects_its_lock() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    std::fs::create_dir_all(ws.path().join("p"))?;
    std::fs::write(
        ws.path().join("p").join("BUILD"),
        r#"target(name = "t", driver = "bash", cache = False, out = "t.txt",
       run = "echo live; printf t > t.txt")"#,
    )?;
    let first = engine(ws.path())?;
    assert_eq!(
        first.run_logs_start_sweep().await,
        0,
        "nothing to sweep yet"
    );
    let runs = first.home.join("runs");

    // Another heph, alive: its directory's lock is held.
    let held = runs.join("held");
    std::fs::create_dir_all(&held)?;
    std::fs::write(held.join("0.log"), "alive")?;
    let held_lock = std::fs::File::create(held.join("lock"))?;
    held_lock.try_lock()?;
    seed_dead(&runs)?;

    // A second engine's start sweep.
    let second = engine(ws.path())?;
    assert_eq!(second.run_logs_start_sweep().await, 4);
    assert_eq!(runs_entries(&second), names(["held", ".new-fresh-2"]));

    // A cache = false success keeps its live file in the engine's own
    // directory, held for the engine's life.
    drop(result(&second, "//p:t", &ResultOptions::default()).await?);
    let own: Vec<_> = runs_entries(&second)
        .into_iter()
        .filter(|n| n != "held" && n != ".new-fresh-2")
        .collect();
    let [own] = own.as_slice() else {
        panic!("one directory of the second engine's own: {own:?}");
    };
    let own_dir = runs.join(own);
    let retained: Vec<_> = live_files(&second)
        .into_iter()
        .filter(|f| f.starts_with(&own_dir))
        .collect();
    assert_eq!(retained.len(), 1, "the retained live file: {retained:?}");
    let alive = |at: &str| {
        let mut want = names(["held", ".new-fresh-2"]);
        want.insert(own.clone());
        assert_eq!(runs_entries(&second), want, "after {at}");
    };

    seed_dead(&runs)?;
    let gc = Arc::clone(&second).gc_all(second.new_state()).await?;
    assert_eq!(gc.run_log_dirs_removed, 4, "{gc:?}");
    alive("gc");

    seed_dead(&runs)?;
    let cleaned = Arc::clone(&second)
        .clean(
            second.new_state(),
            &heph::htmatcher::Matcher::Addr(parse_addr("//p:t")?),
            heph::engine::Discovery::Complete,
        )
        .await?;
    assert_eq!(cleaned.run_log_dirs_removed, 4, "{cleaned:?}");
    alive("clean");

    seed_dead(&runs)?;
    let third = engine(ws.path())?;
    assert_eq!(third.run_logs_start_sweep().await, 4);
    alive("a third engine's start sweep");

    // Released, the other heph's directory goes at the next sweep.
    drop(held_lock);
    let gc = Arc::clone(&third).gc_all(third.new_state()).await?;
    assert_eq!(gc.run_log_dirs_removed, 1, "{gc:?}");
    assert!(!held.exists());
    assert!(own_dir.exists(), "the second engine is alive");

    // The second engine's own drop removes its directory.
    let second = Arc::try_unwrap(second)
        .map_err(|_shared| anyhow::anyhow!("the second engine is still shared"))?;
    drop(second);
    assert!(!own_dir.exists(), "an engine removes its own directory");
    assert_eq!(runs_entries(&third), names([".new-fresh-2"]));
    Ok(())
}
