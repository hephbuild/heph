//! `heph run --force` reaches the driver as the run request's `force` bit
//! (spec "nix store liveness", O7 and O9).
//!
//! The bit tells a driver to drop state that freezes a resolution (the nix
//! driver's lock record) for a target `--force` forced. It is the target's
//! effective `force`, never the request's flag: a dependency reached only as
//! one, a stale-cache re-run, `--no-scratch` and `--shell` all see it unset,
//! and a forced exec/bash target's process sees nothing of it.
//!
//! [`ForceRec`] wraps the bash driver and records the bit of every run. It
//! stands in for the nix driver, which reads the same bit: the real one needs
//! a nix store and network access to resolve a nixpkgs ref.

#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::common::{artifact_string, drain};
use hcore::events::CacheOutcomeKind;
use heph::engine::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use heph::engine::event::BuildEventKind;
use heph::engine::fault_provider::{FaultProvider, Faults};
use heph::engine::{
    Config, EResult, Engine, InteractiveInner, InteractiveWrapper, OutputMatcher, ResultOptions,
};
use heph::htaddr::parse_addr;
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;
use heph::pluginstatictarget::Target;
use heph::{pluginbuildfile, pluginexec};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// What every [`ForceRec`] run saw, shared with the test.
#[derive(Default)]
struct Rec {
    /// `(target name, force bit)` of each `run`, in order.
    runs: Mutex<Vec<(String, bool)>>,
    /// `(target name, force bit)` of each `run_shell`, in order.
    shells: Mutex<Vec<(String, bool)>>,
    /// A host path every run creates and declares, so deleting it makes the
    /// entry stale.
    declare: Mutex<Option<PathBuf>>,
}

impl Rec {
    /// The force bit of each run of `name`, in order.
    fn bits(&self, name: &str) -> Vec<bool> {
        Self::of(&self.runs, name)
    }

    fn shell_bits(&self, name: &str) -> Vec<bool> {
        Self::of(&self.shells, name)
    }

    fn of(list: &Mutex<Vec<(String, bool)>>, name: &str) -> Vec<bool> {
        list.lock()
            .expect("rec")
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, force)| *force)
            .collect()
    }

    fn clear(&self) {
        self.runs.lock().expect("runs").clear();
        self.shells.lock().expect("shells").clear();
    }

    fn record(list: &Mutex<Vec<(String, bool)>>, req: &ManagedRunRequest<'_, '_>) {
        list.lock()
            .expect("rec")
            .push((req.request.target.addr.name.to_string(), req.request.force));
    }
}

/// The bash driver, registered as `forcerec`, recording each run's force bit.
struct ForceRec {
    bash: pluginexec::Driver,
    rec: Arc<Rec>,
}

#[async_trait::async_trait]
impl ManagedDriver for ForceRec {
    fn config(
        &self,
        _req: hplugin::driver::ConfigRequest,
    ) -> anyhow::Result<hplugin::driver::ConfigResponse> {
        Ok(hplugin::driver::ConfigResponse {
            name: "forcerec".to_string(),
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
        Rec::record(&self.rec.runs, &req);
        let declare = self.rec.declare.lock().expect("declare").clone();
        let host_paths = match &declare {
            Some(p) => {
                std::fs::write(p, b"")
                    .map_err(|e| anyhow::anyhow!("creating host path {}: {e}", p.display()))?;
                vec![hplugin::driver::HostPath::from_path(p)?]
            }
            None => Vec::new(),
        };
        let mut res = ManagedDriver::run(&self.bash, req, ctoken).await?;
        res.host_paths = host_paths;
        Ok(res)
    }
    fn supports_shell(&self) -> bool {
        true
    }
    async fn run_shell<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        Rec::record(&self.rec.shells, &req);
        ManagedDriver::run_shell(&self.bash, req, ctoken).await
    }
}

/// An engine over `root` with the buildfile provider, `bash`, `exec` and
/// `forcerec` recording into `rec`. Each call is a fresh `heph` invocation
/// over the same on-disk cache.
fn engine(root: &Path, rec: &Arc<Rec>) -> anyhow::Result<Arc<Engine>> {
    let mut e = Engine::new(Config::for_tests(root))?;
    e.register_provider(|init| {
        Box::new(pluginbuildfile::Provider::new(
            init.root.to_path_buf(),
            init.runtime.clone(),
            Arc::clone(&init.functions),
        ))
    })?;
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash().with_host_path()))?;
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_exec().with_host_path()))?;
    let rec = Arc::clone(rec);
    e.register_managed_driver(move |_| {
        Box::new(ForceRec {
            bash: pluginexec::Driver::new_bash().with_host_path(),
            rec,
        })
    })?;
    Ok(Arc::new(e))
}

fn write_pkg(root: &Path, build: &str) {
    std::fs::create_dir_all(root.join("pkg")).expect("mkdir pkg");
    std::fs::write(root.join("pkg").join("BUILD"), build).expect("write BUILD");
}

/// `//pkg:top` consumes `//pkg:dep`; both record their force bit. `top`
/// carries the label `app`, so a selector can force it alone.
const CHAIN: &str = r#"
target(name = "dep", driver = "forcerec", run = "echo dep > $OUT", out = "d.txt",
       cache = {"remote": False})
target(name = "top", driver = "forcerec", deps = {"in": ["//pkg:dep"]},
       run = "cat $SRC_IN > $OUT", out = "t.txt", labels = ["app"],
       cache = {"remote": False})
"#;

/// A fresh workspace holding [`CHAIN`].
fn chain() -> anyhow::Result<(tempfile::TempDir, Arc<Engine>, Arc<Rec>)> {
    let ws = tempfile::tempdir()?;
    write_pkg(ws.path(), CHAIN);
    let rec = Arc::new(Rec::default());
    let engine = engine(ws.path(), &rec)?;
    Ok((ws, engine, rec))
}

/// `heph run <addr>` with `opts`, its background work drained.
async fn run(
    engine: &Arc<Engine>,
    addr: &str,
    opts: &ResultOptions,
) -> anyhow::Result<Arc<EResult>> {
    let addr = parse_addr(addr)?;
    let rs = engine.new_state();
    let res = Arc::clone(engine)
        .result_addr(rs.clone(), &addr, OutputMatcher::All, opts)
        .await;
    drain(rs).await;
    res
}

/// [`run`], with every event the request emitted.
async fn run_events(
    engine: &Arc<Engine>,
    addr: &str,
    opts: &ResultOptions,
) -> (anyhow::Result<Arc<EResult>>, Vec<BuildEventKind>) {
    let addr = parse_addr(addr).expect("parse addr");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = engine.new_state_with_events(false, Some(tx));
    let res = Arc::clone(engine)
        .result_addr(rs.clone(), &addr, OutputMatcher::All, opts)
        .await;
    drain(rs).await;
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev.kind);
    }
    (res, events)
}

/// `heph run <selector>` with `opts`: the walk installs the `Selection` that
/// decides each target's effective `force`.
async fn run_matcher(
    engine: &Arc<Engine>,
    matcher: &Matcher,
    opts: &ResultOptions,
) -> anyhow::Result<()> {
    let rs = engine.new_state();
    let batch = Arc::clone(engine)
        .result(rs.clone(), matcher, OutputMatcher::All, opts)
        .await;
    drain(rs).await;
    let batch = batch?;
    let errors: Vec<String> = batch
        .errors
        .iter()
        .map(|(addr, err)| format!("{}: {err:#}", addr.format()))
        .collect();
    anyhow::ensure!(errors.is_empty(), "{}", errors.join("\n"));
    Ok(())
}

fn plain() -> ResultOptions {
    ResultOptions::default()
}

fn forced() -> ResultOptions {
    ResultOptions {
        force: true,
        ..Default::default()
    }
}

/// Every target in the workspace whose label is `label`.
fn label(label: &str) -> Matcher {
    Matcher::And(vec![
        Matcher::Label(label.to_string()),
        Matcher::PackagePrefix(PkgBuf::from("")),
    ])
}

/// `heph run --force` sets the bit for the target it forces and for nothing
/// else: a plain run, a stale-miss re-run and the forced target's dependency
/// (reached only as one, and built in the same request) all see it unset.
#[tokio::test]
async fn force_reaches_driver() -> anyhow::Result<()> {
    let (ws, engine, rec) = chain()?;
    let store = tempfile::tempdir()?;
    let path = store.path().join("env");
    *rec.declare.lock().expect("declare") = Some(path.clone());

    // A plain run: both build, neither forced.
    run(&engine, "//pkg:top", &plain()).await?;
    assert_eq!(rec.bits("dep"), [false]);
    assert_eq!(rec.bits("top"), [false]);

    // A forced run: only the target it names runs, with the bit; its
    // dependency stays a hit.
    rec.clear();
    run(&engine, "//pkg:top", &forced()).await?;
    assert_eq!(rec.bits("top"), [true]);
    assert_eq!(rec.bits("dep"), [] as [bool; 0], "force is not deep");

    // Warm: a plain run runs nothing.
    rec.clear();
    run(&engine, "//pkg:top", &plain()).await?;
    assert_eq!(rec.bits("top"), [] as [bool; 0]);

    // The declared path goes: `dep`'s entry is stale, and its re-run is a
    // plain one. The re-run creates the path again, as a nix rebuild brings
    // the store path back, so `top` (same path, same inputs) is a hit.
    std::fs::remove_file(&path)?;
    let (res, events) = run_events(&engine, "//pkg:top", &plain()).await;
    res?;
    let stale = format!("stale: host path {} is gone", path.display());
    assert!(
        events.iter().any(|e| matches!(
            e,
            BuildEventKind::CacheOutcome { addr, outcome: CacheOutcomeKind::Miss, reason }
                if addr == "//pkg:dep" && reason.as_deref() == Some(stale.as_str())
        )),
        "{events:?}"
    );
    assert_eq!(
        rec.bits("dep"),
        [false],
        "a stale-miss re-run is not forced"
    );
    assert_eq!(rec.bits("top"), [] as [bool; 0]);
    drop(ws);

    // Cold, forced: the dependency builds in the forced request, unforced.
    let (_ws, engine, rec) = chain()?;
    run(&engine, "//pkg:top", &forced()).await?;
    assert_eq!(rec.bits("top"), [true]);
    assert_eq!(
        rec.bits("dep"),
        [false],
        "a dependency of a forced target is not forced"
    );
    Ok(())
}

fn rec_target(addr: &str, run: &str, deps: &[&str], labels: &[&str]) -> Target {
    Target {
        addr: addr.to_string(),
        driver: "forcerec".to_string(),
        run: Some(run.to_string()),
        out: HashMap::from([(String::new(), vec!["out.txt".to_string()])]),
        deps: if deps.is_empty() {
            HashMap::new()
        } else {
            HashMap::from([(
                "in".to_string(),
                deps.iter().map(|d| (*d).to_string()).collect(),
            )])
        },
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
        ..Default::default()
    }
}

/// `heph r x --force`, where the walk reaches the selected `//x:d` first as
/// the dependency of an unforced `//c:c` (resolving `//a:probe`'s spec builds
/// `//c:c`, the go provider's shape). `//x:d` still runs once, forced, and
/// `//c:c` runs unforced: the bit follows the selection, not the call path.
#[tokio::test]
async fn force_bit_follows_selection_when_reached_as_dep() -> anyhow::Result<()> {
    let rec = Arc::new(Rec::default());
    let provider = FaultProvider::new(
        vec![
            Target {
                addr: "//a:probe".to_string(),
                driver: "bash".to_string(),
                run: Some("echo probe > $OUT".to_string()),
                out: HashMap::from([(String::new(), vec!["out.txt".to_string()])]),
                ..Default::default()
            },
            rec_target("//c:c", "cat $SRC_IN > $OUT", &["//x:d"], &[]),
            rec_target("//x:d", "echo d > $OUT", &[], &["x"]),
        ],
        Faults {
            builds: vec![(parse_addr("//a:probe")?, parse_addr("//c:c")?)],
            // A label walk reaches a building `get` only when the listing
            // leaves labels unknown.
            facts_unknown: true,
            ..Default::default()
        },
    )?;
    let ws = htestkit::WorkspaceBuilder::new()?
        .with_provider(move |_| Box::new(provider))
        .with_managed_driver(Box::new(pluginexec::Driver::new_bash().with_host_path()))
        .with_managed_driver(Box::new(ForceRec {
            bash: pluginexec::Driver::new_bash().with_host_path(),
            rec: Arc::clone(&rec),
        }))
        .build()?;

    let results = ws.run_matcher_with(&label("x"), &forced()).await?;
    assert_eq!(results.len(), 1, "only //x:d carries the label");
    assert_eq!(
        rec.bits("d"),
        [true],
        "first reached as an unforced consumer's dependency, still forced, once"
    );
    assert_eq!(rec.bits("c"), [false], "//c:c is not selected");
    Ok(())
}

/// Forcing a consumer leaves its nix dependency's bit unset: the dependency's
/// recorded nixpkgs lock is not re-resolved. `//pkg:dep` stands in for the nix
/// target. Checked through an address (cold, then warm, where the dependency
/// stays a hit) and through a selector matching the consumer alone.
#[tokio::test]
async fn force_on_consumer_does_not_reresolve_nix_dep() -> anyhow::Result<()> {
    let (_ws, engine, rec) = chain()?;
    run(&engine, "//pkg:top", &forced()).await?;
    assert_eq!(rec.bits("top"), [true]);
    assert_eq!(rec.bits("dep"), [false], "built, but not forced");

    rec.clear();
    run(&engine, "//pkg:top", &forced()).await?;
    assert_eq!(rec.bits("top"), [true]);
    assert_eq!(rec.bits("dep"), [] as [bool; 0], "still a hit");

    let (_ws, engine, rec) = chain()?;
    run_matcher(&engine, &label("app"), &forced()).await?;
    assert_eq!(rec.bits("top"), [true]);
    assert_eq!(rec.bits("dep"), [false], "not selected, so not forced");
    Ok(())
}

/// Prints what the target's process sees: its argv, its environment, and the
/// files in its sandbox.
const DUMP: &str = r#"{ echo "argv: $0 $*"; env | sort; find . | sort; } > "$OUT""#;

/// A forced exec or bash target's process sees the same argv, environment
/// and files as an unforced one: the bit is the driver's, never the target's.
#[tokio::test]
async fn force_never_reaches_process() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let exec_script = DUMP.replace("\"$OUT\"", "o.txt");
    write_pkg(
        ws.path(),
        &format!(
            r#"
target(name = "b", driver = "bash", run = {DUMP:?}, out = "o.txt")
target(name = "e", driver = "exec", out = "o.txt",
       run = ["sh", "-c", {exec_script:?}, "argv0", "one", "two"])
"#
        ),
    );
    let rec = Arc::new(Rec::default());
    let engine = engine(ws.path(), &rec)?;
    for addr in ["//pkg:b", "//pkg:e"] {
        let unforced = artifact_string(&*run(&engine, addr, &plain()).await?);
        let forced = artifact_string(&*run(&engine, addr, &forced()).await?);
        assert!(unforced.contains("argv: "), "{addr}: {unforced}");
        assert!(
            !unforced.to_ascii_lowercase().contains("force"),
            "{addr}: {unforced}"
        );
        assert_eq!(forced, unforced, "{addr}: --force reached the process");
    }
    Ok(())
}

/// A `--shell` session that types `exit 0`.
fn shell(force: bool) -> ResultOptions {
    let wrapper: InteractiveWrapper = Arc::new(move |inner: InteractiveInner| {
        Box::pin(async move {
            let mut typed: &[u8] = b"exit 0\n";
            let (mut out, mut err) = (tokio::io::sink(), tokio::io::sink());
            inner(Some(&mut typed), Some(&mut out), Some(&mut err), None).await
        })
    });
    ResultOptions {
        shell: true,
        force,
        interactive: Some(wrapper),
        ..Default::default()
    }
}

/// `--no-scratch` forces a rebuild but not the bit, and a `--shell`, even
/// with `--force`, leaves it unset: neither re-resolves a recorded lock (O9).
#[tokio::test]
async fn no_scratch_keeps_recorded_lock() -> anyhow::Result<()> {
    let (_ws, engine, rec) = chain()?;
    run(&engine, "//pkg:top", &plain()).await?;

    rec.clear();
    let no_scratch = ResultOptions {
        no_scratch: true,
        ..Default::default()
    };
    run(&engine, "//pkg:top", &no_scratch).await?;
    assert_eq!(rec.bits("top"), [false], "rebuilt, with the bit unset");

    for force in [false, true] {
        rec.clear();
        // The shell's own exit status is not what this test is about.
        let _shell = run(&engine, "//pkg:top", &shell(force)).await;
        assert_eq!(
            rec.shell_bits("top"),
            [false],
            "--shell (force={force}) leaves the bit unset"
        );
        assert_eq!(rec.bits("top"), [] as [bool; 0], "force={force}");
    }
    Ok(())
}
