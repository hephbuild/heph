#![expect(
    clippy::panic_in_result_fn,
    clippy::panic,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! `exec.entrypoint` end to end, in-process: `Engine::invoke` over a real
//! workspace, its closure built, staged and held, and the command it prepares
//! run the way the binary would run it.
//!
//! What only a real process can show — exit codes, stdin, signal deaths — is
//! in `crates/bin-e2e/tests/entrypoint.rs`.

use anyhow::Context as _;
use heph::engine::driver::entrypoint::{Caller, PreparedCommand};
use heph::engine::{
    Config, Engine, InvokeRequest, OutputMatcher, PluginParts, PreparedInvocation, RemoteCacheDef,
    ResultOptions,
};
use heph::htaddr::parse_addr;
use heph::{pluginbuildfile, pluginexec};
use htestkit::WorkspaceBuilder;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn exec_plugin() -> anyhow::Result<PluginParts> {
    Ok(PluginParts::default()
        .with_managed_driver(Box::new(pluginexec::Driver::new_exec().with_host_path()))
        .with_entrypoint_driver(Arc::new(pluginexec::entrypoint::Entrypoint::new())))
}

fn workspace() -> htestkit::Workspace {
    WorkspaceBuilder::new()
        .expect("workspace tempdir")
        .with_provider(|init| {
            Box::new(pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
                init.runtime.clone(),
                Arc::clone(&init.functions),
            ))
        })
        .with_plugin("exec", |_| exec_plugin())
        .with_managed_driver(Box::new(pluginexec::Driver::new_bash().with_host_path()))
        .build()
        .expect("build workspace")
}

/// An engine over `root` with a `file://` remote cache, built the way the
/// workspace builder builds one.
fn engine_with_remote(root: &Path, remote: &Path) -> anyhow::Result<Arc<Engine>> {
    let mut e = Engine::new(Config {
        remote_caches: vec![RemoteCacheDef {
            name: "shared".to_string(),
            uri: format!("file://{}", remote.display()),
            read: true,
            write: true,
            concurrency: 4,
            endpoint: None,
            region: None,
        }],
        ..Config::for_tests(root)
    })?;
    e.register_provider(|init| {
        Box::new(pluginbuildfile::Provider::new(
            init.root.to_path_buf(),
            init.runtime.clone(),
            Arc::clone(&init.functions),
        ))
    })?;
    e.register_plugin("exec", |_| exec_plugin())?;
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash().with_host_path()))?;
    let e = Arc::new(e);
    e.install_exec_runner_host();
    Ok(e)
}

fn caller(cwd: &Path, args: &[&str]) -> Caller {
    Caller {
        cwd: cwd.to_path_buf(),
        env: vec![
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
            (
                OsString::from("HEPH_TEST_CALLER"),
                OsString::from("from-caller"),
            ),
        ],
        args: args.iter().map(OsString::from).collect(),
    }
}

async fn invoke(
    engine: &Arc<Engine>,
    addr: &str,
    caller: Caller,
) -> anyhow::Result<PreparedInvocation> {
    Arc::clone(engine)
        .invoke(
            engine.new_state(),
            InvokeRequest {
                addr: parse_addr(addr)?,
                caller,
                shell: false,
            },
        )
        .await
}

/// Run a prepared command as the binary does: exactly its env, in its cwd.
fn run(cmd: &PreparedCommand) -> anyhow::Result<std::process::Output> {
    let (program, args) = cmd.argv.split_first().context("empty argv")?;
    std::process::Command::new(program)
        .args(args)
        .env_clear()
        .envs(cmd.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&cmd.cwd)
        .output()
        .context("run the prepared command")
}

/// The error of a result whose `Ok` side has no `Debug`.
fn err_of<T>(r: anyhow::Result<T>, what: &str) -> anyhow::Error {
    match r {
        Ok(_) => panic!("{what}: expected an error"),
        Err(e) => e,
    }
}

fn env_of<'c>(cmd: &'c PreparedCommand, key: &str) -> Option<&'c OsString> {
    cmd.env.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

const TOOLS_BUILD: &str = r#"
target(name = "data", driver = "bash", run = "printf payload > $OUT", out = "data.txt")
target(name = "tool", driver = "bash",
       run = "printf '#!/bin/sh\necho tool-ran \"$@\"\n' > $OUT && chmod +x $OUT",
       out = "mytool")
"#;

/// The closure (a dep at its staged path, a tool first on `PATH` through the
/// toolbin), the caller's env and the args, in order and byte for byte, with
/// the program running in the caller's cwd (I9, D18, D29).
#[tokio::test]
async fn entrypoint_runs_in_caller_cwd_and_env() -> anyhow::Result<()> {
    let ws = workspace();
    ws.write_build_file("tools", TOOLS_BUILD);
    ws.write_build_file(
        "app",
        r#"
target(name = "ep", driver = "exec.entrypoint",
       deps = {"d": "//tools:data"}, tools = {"t": "//tools:tool"},
       env = {"FROM_TARGET": "$SRC_D"},
       run = ["/bin/sh", "-c",
              "cat $SRC_D; echo; mytool x; echo \"cwd=$(pwd)\"; echo \"caller=$HEPH_TEST_CALLER\"; echo \"target=$FROM_TARGET\"; printf '<%s>' \"$@\"",
              "sh"],
       args = True)
"#,
    );
    let nested = ws.dir.path().join("app/sub");
    std::fs::create_dir_all(&nested)?;
    let args = ["", "-x", "--quiet", "a b", "é"];
    let prepared = invoke(&ws.engine, "//app:ep", caller(&nested, &args)).await?;
    let cmd = &prepared.command;

    let src_d = PathBuf::from(env_of(cmd, "SRC_D").context("SRC_D set")?);
    assert!(
        src_d.is_absolute() && src_d.ends_with("tools/data.txt"),
        "{src_d:?}"
    );
    let path = env_of(cmd, "PATH")
        .context("PATH set")?
        .to_string_lossy()
        .into_owned();
    let (first, rest) = path.split_once(':').context("composed PATH")?;
    assert!(
        Path::new(first).join("mytool").exists(),
        "the toolbin leads PATH: {path}"
    );
    assert_eq!(rest, "/usr/bin:/bin", "then the caller's PATH");

    let out = run(cmd)?;
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout)?;
    let want = format!(
        "payload\ntool-ran x\ncwd={}\ncaller=from-caller\ntarget={}\n<><-x><--quiet><a b><é>",
        nested.canonicalize()?.display(),
        src_d.display(),
    );
    assert_eq!(stdout, want);
    Ok(())
}

/// Undeclared args are refused after the spec resolves and before any dep
/// builds; with `args = True` they are appended in order (D18, D21).
#[tokio::test]
async fn entrypoint_args_must_be_declared() -> anyhow::Result<()> {
    let ws = workspace();
    ws.write_build_file(
        "app",
        r#"
target(name = "boom", driver = "bash", run = "exit 1", out = "x")
target(name = "noargs", driver = "exec.entrypoint", deps = {"b": ":boom"},
       run = ["/bin/echo", "hi"])
target(name = "withargs", driver = "exec.entrypoint", run = ["/bin/echo"], args = True)
"#,
    );
    let cwd = ws.dir.path().to_path_buf();
    // The dep would fail if it built: the error being the args refusal is what
    // shows nothing built first.
    let err = invoke(&ws.engine, "//app:noargs", caller(&cwd, &["x"]))
        .await
        .expect_err("undeclared args must be refused");
    let msg = format!("{err:#}");
    assert!(msg.contains("//app:noargs"), "{msg}");
    assert!(msg.contains("args = True"), "{msg}");
    assert!(
        err.downcast_ref::<heph::engine::invoke::UndeclaredArgs>()
            .is_some(),
        "refused before the failing dep built: {msg}"
    );

    // No args at all is fine — the dep then builds, and fails, as it should.
    let err = invoke(&ws.engine, "//app:noargs", caller(&cwd, &[]))
        .await
        .expect_err("the dep fails");
    assert!(
        err.downcast_ref::<heph::engine::invoke::UndeclaredArgs>()
            .is_none()
    );

    let prepared = invoke(
        &ws.engine,
        "//app:withargs",
        caller(&cwd, &["", "-n", "--quiet", "x"]),
    )
    .await?;
    assert_eq!(
        prepared.command.argv,
        ["/bin/echo", "", "-n", "--quiet", "x"].map(OsString::from)
    );
    Ok(())
}

/// `workdir = "caller"` is the caller's cwd, nested or outside the workspace;
/// `"root"` is the workspace root. The caller's env reaches both.
#[tokio::test]
async fn workdir_modes() -> anyhow::Result<()> {
    let ws = workspace();
    ws.write_build_file(
        "app",
        r#"
target(name = "caller", driver = "exec.entrypoint",
       run = ["/bin/sh", "-c", "pwd; echo $HEPH_TEST_CALLER"])
target(name = "root", driver = "exec.entrypoint", workdir = "root",
       run = ["/bin/sh", "-c", "pwd; echo $HEPH_TEST_CALLER"])
"#,
    );
    let nested = ws.dir.path().join("app/deep/er");
    std::fs::create_dir_all(&nested)?;
    let outside = tempfile::tempdir()?;
    for (addr, cwd, want) in [
        ("//app:caller", nested.clone(), nested.clone()),
        (
            "//app:caller",
            outside.path().to_path_buf(),
            outside.path().to_path_buf(),
        ),
        ("//app:root", nested.clone(), ws.dir.path().to_path_buf()),
        (
            "//app:root",
            outside.path().to_path_buf(),
            ws.dir.path().to_path_buf(),
        ),
    ] {
        let prepared = invoke(&ws.engine, addr, caller(&cwd, &[])).await?;
        assert_eq!(prepared.command.cwd, want, "{addr} from {cwd:?}");
        let out = run(&prepared.command)?;
        assert_eq!(
            String::from_utf8(out.stdout)?,
            format!("{}\nfrom-caller\n", want.canonicalize()?.display()),
            "{addr} from {cwd:?}"
        );
    }
    Ok(())
}

/// Every place an entrypoint could be pulled in as an input refuses it, naming
/// it (I8); so does a selection, and a direct build.
#[tokio::test]
async fn entrypoint_refused_as_input() -> anyhow::Result<()> {
    let ws = workspace();
    ws.write_build_file(
        "ep",
        r#"
target(name = "tool", driver = "exec.entrypoint", run = ["/bin/true"], labels = ["shim"])
target(name = "carrier", driver = "bash", run = "printf c > $OUT", out = "c",
       transitive = {"deps": {"t": ["//ep:tool"]}})
target(name = "tcarrier", driver = "bash", run = "printf c > $OUT", out = "c",
       transitive = {"tools": {"t": ["//ep:tool"]}})
"#,
    );
    ws.write_build_file(
        "use",
        r#"
target(name = "dep", driver = "bash", run = "true", out = "o", deps = ["//ep:tool"])
target(name = "rtdep", driver = "exec", run = ["true"], runtime_deps = ["//ep:tool"])
target(name = "tooldep", driver = "bash", run = "true", out = "o", tools = ["//ep:tool"])
target(name = "hashdep", driver = "exec", run = ["true"], hash_deps = ["//ep:tool"])
target(name = "runner", driver = "exec", run = ["true"], runner = "//ep:tool")
target(name = "read", driver = "exec", run = ["echo", "${read://ep:tool}"])
target(name = "transitive_dep", driver = "bash", run = "true", out = "o", deps = ["//ep:carrier"])
target(name = "transitive_tool", driver = "bash", run = "true", out = "o", deps = ["//ep:tcarrier"])
target(name = "ep_dep", driver = "exec.entrypoint", run = ["/bin/true"], deps = ["//ep:tool"])
"#,
    );
    for consumer in [
        "//use:dep",
        "//use:rtdep",
        "//use:tooldep",
        "//use:hashdep",
        "//use:runner",
        "//use:read",
        "//use:transitive_dep",
        "//use:transitive_tool",
    ] {
        let err = err_of(ws.run(consumer).await, consumer);
        let msg = format!("{err:#}");
        assert!(
            msg.contains("//ep:tool") && msg.contains("cannot be an input"),
            "{consumer}: {msg}"
        );
    }
    // An entrypoint's own deps go through the same check.
    let err = invoke(&ws.engine, "//use:ep_dep", caller(ws.dir.path(), &[]))
        .await
        .expect_err("an entrypoint as an entrypoint's input");
    assert!(format!("{err:#}").contains("cannot be an input"), "{err:#}");

    // Selections: a label, a package pattern.
    for matcher in [
        heph::htmatcher::Matcher::Label("shim".to_string()),
        heph::htmatcher::Matcher::PackagePrefix("ep".into()),
    ] {
        let err = err_of(ws.run_matcher(&matcher).await, "selection");
        assert!(
            format!("{err:#}").contains("//ep:tool is an exec.entrypoint target"),
            "{err:#}"
        );
    }
    // A direct build: the execute backstop.
    let err = err_of(ws.run("//ep:tool").await, "direct result_addr");
    assert!(format!("{err:#}").contains("never built"), "{err:#}");
    Ok(())
}

/// Never cached: a second invocation prepares and runs again, and no cache
/// entry — local or remote — is ever written for the entrypoint, though its
/// dep is cached in both (I6).
#[tokio::test]
async fn entrypoint_never_cached() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let remote = tempfile::tempdir()?;
    let counter = root.path().join("ran.txt");
    std::fs::create_dir_all(root.path().join("p"))?;
    std::fs::write(
        root.path().join("p/BUILD"),
        format!(
            r#"
target(name = "dep_unique_name", driver = "bash", run = "printf d > $OUT", out = "d.txt")
target(name = "entrypoint_unique_name", driver = "exec.entrypoint",
       deps = {{"d": ":dep_unique_name"}},
       run = ["/bin/sh", "-c", "echo x >> {}"])
"#,
            counter.display()
        ),
    )?;
    let engine = engine_with_remote(root.path(), remote.path())?;
    for _ in 0..2 {
        let prepared = invoke(
            &engine,
            "//p:entrypoint_unique_name",
            caller(root.path(), &[]),
        )
        .await?;
        assert!(run(&prepared.command)?.status.success());
    }
    assert_eq!(std::fs::read_to_string(&counter)?, "x\nx\n", "ran twice");
    let local_cache = engine.home.join("cache");
    drop(engine);

    // A black-box look at both stores: the entrypoint's address appears in
    // neither, while its dep's does — so the scan can see an address.
    let mut haystack = Vec::new();
    for dir in [local_cache, remote.path().to_path_buf()] {
        for ent in walk(&dir) {
            haystack.extend_from_slice(ent.to_string_lossy().as_bytes());
            if ent.is_file() {
                haystack.extend(std::fs::read(&ent)?);
            }
        }
    }
    let contains = |needle: &str| {
        haystack
            .windows(needle.len())
            .any(|w| w == needle.as_bytes())
    };
    assert!(
        contains("dep_unique_name"),
        "the scan sees cached addresses"
    );
    assert!(
        !contains("entrypoint_unique_name"),
        "no cache entry for the entrypoint"
    );
    Ok(())
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        }
        out.push(p);
    }
    out
}

/// A driver that says "cache me" for its entrypoint — the shape a provider or
/// option default reaching the def would have. The engine forces it off from
/// the capability, in `get_def`, before anything reads it (D43).
struct CachingEntrypoint(pluginexec::entrypoint::Entrypoint);

#[async_trait::async_trait]
impl hplugin::driver::Driver for CachingEntrypoint {
    fn config(
        &self,
        req: hplugin::driver::ConfigRequest,
    ) -> anyhow::Result<hplugin::driver::ConfigResponse> {
        self.0.config(req)
    }
    fn schema(&self) -> hplugin::driver::DriverSchema {
        self.0.schema()
    }
    async fn parse(
        &self,
        req: hplugin::driver::ParseRequest,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ParseResponse> {
        let mut res = self.0.parse(req, ctoken).await?;
        res.target_def.cache = hplugin::driver::targetdef::CacheConfig::on(true);
        Ok(res)
    }
    async fn apply_transitive(
        &self,
        req: hplugin::driver::ApplyTransitiveRequest,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ApplyTransitiveResponse> {
        self.0.apply_transitive(req, ctoken).await
    }
    async fn run<'a, 'io>(
        &self,
        req: hplugin::driver::RunRequest<'a, 'io>,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::RunResponse> {
        self.0.run(req, ctoken).await
    }
    async fn run_shell<'a, 'io>(
        &self,
        req: hplugin::driver::RunRequest<'a, 'io>,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::RunResponse> {
        self.0.run_shell(req, ctoken).await
    }
}

impl hplugin::driver::entrypoint::EntrypointDriver for CachingEntrypoint {
    fn accepts_args(&self, target: &hplugin::driver::targetdef::TargetDef) -> bool {
        self.0.accepts_args(target)
    }
    fn toolbin(
        &self,
        token: &hplugin::driver::entrypoint::Invocation,
        target: &hplugin::driver::targetdef::TargetDef,
        inputs: &[hplugin::driver::entrypoint::StagedInput],
    ) -> anyhow::Result<std::collections::BTreeMap<OsString, PathBuf>> {
        self.0.toolbin(token, target, inputs)
    }
    fn prepare(
        &self,
        token: &hplugin::driver::entrypoint::Invocation,
        req: hplugin::driver::entrypoint::PrepareRequest<'_>,
    ) -> anyhow::Result<PreparedCommand> {
        self.0.prepare(token, req)
    }
}

#[tokio::test]
async fn entrypoint_cache_off_before_execute() -> anyhow::Result<()> {
    let ws = WorkspaceBuilder::new()?
        .with_provider(|init| {
            Box::new(pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
                init.runtime.clone(),
                Arc::clone(&init.functions),
            ))
        })
        .with_plugin("exec", |_| {
            Ok(
                PluginParts::default().with_entrypoint_driver(Arc::new(CachingEntrypoint(
                    pluginexec::entrypoint::Entrypoint::new(),
                ))),
            )
        })
        .build()?;
    ws.write_build_file(
        "app",
        r#"target(name = "ep", driver = "exec.entrypoint", run = ["/bin/true"])"#,
    );
    let def = Arc::clone(&ws.engine)
        .get_def(ws.engine.new_state(), &parse_addr("//app:ep")?)
        .await?;
    assert_eq!(
        def.target_def.cache,
        hplugin::driver::targetdef::CacheConfig::off(),
        "the capability forces cache off whatever the driver said"
    );
    Ok(())
}

/// `${read://…}` in the entrypoint's own options is the producer's content,
/// the producer built first (D46).
#[tokio::test]
async fn entrypoint_resolves_deferred_config() -> anyhow::Result<()> {
    let ws = workspace();
    ws.write_build_file(
        "infra",
        r#"target(name = "version", driver = "bash", run = "printf 1.4.2 > $OUT", out = "v.txt")"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "ep", driver = "exec.entrypoint",
       env = {"VERSION": "${read://infra:version}"},
       run = ["/bin/sh", "-c", "printf %s \"$VERSION\""])"#,
    );
    let prepared = invoke(&ws.engine, "//app:ep", caller(ws.dir.path(), &[])).await?;
    assert_eq!(
        env_of(&prepared.command, "VERSION"),
        Some(&OsString::from("1.4.2"))
    );
    assert_eq!(run(&prepared.command)?.stdout, b"1.4.2");
    Ok(())
}

/// No lock serializes two invocations: a second prepares while the first is
/// still held (I2).
#[tokio::test]
async fn entrypoint_takes_no_execute_lock() -> anyhow::Result<()> {
    let ws = workspace();
    ws.write_build_file("tools", TOOLS_BUILD);
    ws.write_build_file(
        "app",
        r#"target(name = "ep", driver = "exec.entrypoint", deps = {"d": "//tools:data"},
       run = ["/bin/true"])"#,
    );
    let first = invoke(&ws.engine, "//app:ep", caller(ws.dir.path(), &[])).await?;
    let second = tokio::time::timeout(
        Duration::from_secs(60),
        invoke(&ws.engine, "//app:ep", caller(ws.dir.path(), &[])),
    )
    .await
    .context("a second invocation waited on the first")??;
    assert_eq!(first.command.argv, second.command.argv);
    Ok(())
}

/// A dep of an entrypoint is an ordinary cached target: built once, then a
/// hit (I5).
#[tokio::test]
async fn entrypoint_dep_still_cached() -> anyhow::Result<()> {
    let ws = workspace();
    let counter = ws.dir.path().join("built.txt");
    ws.write_build_file(
        "app",
        &format!(
            r#"
target(name = "dep", driver = "bash", run = "echo x >> {} && printf d > $OUT", out = "d")
target(name = "ep", driver = "exec.entrypoint", deps = {{"d": ":dep"}}, run = ["/bin/true"])
"#,
            counter.display()
        ),
    );
    for _ in 0..3 {
        drop(invoke(&ws.engine, "//app:ep", caller(ws.dir.path(), &[])).await?);
    }
    assert_eq!(std::fs::read_to_string(&counter)?, "x\n", "built once");
    Ok(())
}

/// While the tool runs, `gc` keeps every stage entry its closure uses — the
/// dep's, the tool's and the toolbin — and collects them once it is done (I4).
#[tokio::test]
async fn gc_keeps_closure_of_running_tool() -> anyhow::Result<()> {
    let ws = workspace();
    ws.write_build_file("tools", TOOLS_BUILD);
    ws.write_build_file(
        "app",
        r#"target(name = "ep", driver = "exec.entrypoint",
       deps = {"d": "//tools:data"}, tools = {"t": "//tools:tool"}, run = ["/bin/true"])"#,
    );
    let prepared = invoke(&ws.engine, "//app:ep", caller(ws.dir.path(), &[])).await?;
    assert_eq!(prepared.held_entries(), 3, "dep, tool and toolbin entries");
    let src = PathBuf::from(env_of(&prepared.command, "SRC_D").context("SRC_D")?);
    let tool = PathBuf::from(env_of(&prepared.command, "TOOL_T").context("TOOL_T")?);
    let tool_target = std::fs::read_link(&tool)?;

    let stats = Arc::clone(&ws.engine).gc_all(ws.engine.new_state()).await?;
    assert_eq!(stats.stage_entries_removed, 0, "held entries survive gc");
    assert!(src.is_file() && tool.is_file() && tool_target.is_file());

    drop(prepared);
    let stats = Arc::clone(&ws.engine).gc_all(ws.engine.new_state()).await?;
    assert_eq!(
        stats.stage_entries_removed, 3,
        "released, they are collected"
    );
    assert!(!src.exists() && !tool_target.exists());
    Ok(())
}

/// A `--force` rebuild of the same dep revision waits for the running tool,
/// whose result read lock it needs (I4).
#[tokio::test]
async fn force_rebuild_waits_for_running_tool() -> anyhow::Result<()> {
    let ws = workspace();
    ws.write_build_file("tools", TOOLS_BUILD);
    ws.write_build_file(
        "app",
        r#"target(name = "ep", driver = "exec.entrypoint", deps = {"d": "//tools:data"},
       run = ["/bin/true"])"#,
    );
    let prepared = invoke(&ws.engine, "//app:ep", caller(ws.dir.path(), &[])).await?;
    let engine = Arc::clone(&ws.engine);
    let rebuild = tokio::spawn(async move {
        engine
            .clone()
            .result_addr(
                engine.new_state(),
                &parse_addr("//tools:data").expect("addr"),
                OutputMatcher::All,
                &ResultOptions {
                    force: true,
                    ..Default::default()
                },
            )
            .await
            .map(drop)
    });
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!rebuild.is_finished(), "the rebuild must wait for the tool");
    drop(prepared);
    tokio::time::timeout(Duration::from_secs(60), rebuild)
        .await
        .context("the rebuild never finished once the tool was done")???;
    Ok(())
}

/// The process exits as soon as the tool does, so `invoke` drains the cache
/// writes its dep builds queued before handing back the command: with the
/// writer held, it does not return; released, it does, and a second engine
/// finds the dep cached (D43).
///
/// A `hash_deps` dep, on purpose: it is built but never staged, so nothing but
/// the drain waits for its write. (A staged dep's write is waited for anyway,
/// by the staging read.)
#[tokio::test]
async fn fast_exit_flushes_pending_cache_writes() -> anyhow::Result<()> {
    let ws = workspace();
    let counter = ws.dir.path().join("built.txt");
    ws.write_build_file(
        "app",
        &format!(
            r#"
target(name = "dep", driver = "bash", run = "echo x >> {} && printf d > $OUT", out = "d")
target(name = "ep", driver = "exec.entrypoint", hash_deps = [":dep"], run = ["/bin/true"])
"#,
            counter.display()
        ),
    );
    let hold = ws.engine.hold_local_cache_writes();
    let engine = Arc::clone(&ws.engine);
    let root = ws.dir.path().to_path_buf();
    let mut pending = tokio::spawn(async move {
        invoke(&engine, "//app:ep", caller(&root, &[]))
            .await
            .map(drop)
    });
    assert!(
        tokio::time::timeout(Duration::from_secs(3), &mut pending)
            .await
            .is_err(),
        "invoke must not hand back the command while the dep's writes are queued"
    );
    drop(hold);
    tokio::time::timeout(Duration::from_secs(60), pending)
        .await
        .context("invoke never finished once writes were released")???;

    // A second engine on the same home: the dep is a hit.
    let mut e = Engine::new(Config::for_tests(ws.dir.path()))?;
    e.register_provider(|init| {
        Box::new(pluginbuildfile::Provider::new(
            init.root.to_path_buf(),
            init.runtime.clone(),
            Arc::clone(&init.functions),
        ))
    })?;
    e.register_plugin("exec", |_| exec_plugin())?;
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash().with_host_path()))?;
    let e = Arc::new(e);
    drop(invoke(&e, "//app:ep", caller(ws.dir.path(), &[])).await?);
    assert_eq!(
        std::fs::read_to_string(&counter)?,
        "x\n",
        "the dep was cached"
    );
    Ok(())
}
