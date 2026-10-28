#![expect(
    clippy::panic_in_result_fn,
    clippy::panic,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! `entrypoint = True` on exec/bash end to end, in-process: `Engine::invoke`
//! over a real workspace — the closure built, the driver's ordinary `run`
//! called with `RunOptions` in a per-invocation sandbox, and the outcome.
//!
//! Under `RunStdio::Inherit` the tool writes to the test process's own fds, so
//! every tool here writes what it saw to the file `$HEPH_TEST_OUT` names
//! instead. What only a real process can show — a tty, exit codes, signal
//! deaths, stdin — is in `crates/bin-e2e/tests/entrypoint.rs`. A plugin
//! driver's side (`cdylib_entrypoint_runs`, the seam rows of
//! `entrypoint_survives_apply_transitive`) crosses the stable-ABI seam
//! in-process, as `plugin_fn_declarations.rs` does: no test cdylib is built.

mod common;

use anyhow::Context as _;
use common::Workspace;
use hcore::process::Exit;
use heph::engine::{
    Config, Engine, InvokeOutcome, InvokeRequest, OutputMatcher, PluginParts, RemoteCacheDef,
    ResultOptions,
};
use heph::htaddr::parse_addr;
use heph::{pluginbuildfile, pluginexec};
use htestkit::WorkspaceBuilder;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// A handover hook that counts how often it fired.
fn handover_counter(count: &Arc<AtomicUsize>) -> hplugin::driver::Handover {
    let count = Arc::clone(count);
    hplugin::driver::Handover::new(move || {
        count.fetch_add(1, Ordering::SeqCst);
    })
}

/// The caller's environment every test hands in: a `PATH`, a marker, and the
/// file the tool writes its observations to.
fn caller_env(out: &Path) -> Vec<(OsString, OsString)> {
    vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("HEPH_TEST_CALLER".into(), "from-caller".into()),
        ("HEPH_TEST_OUT".into(), out.as_os_str().to_owned()),
    ]
}

fn request(addr: &str, cwd: &Path, args: &[&str], out: &Path) -> anyhow::Result<InvokeRequest> {
    Ok(InvokeRequest::new(parse_addr(addr)?, cwd, caller_env(out))
        .with_args(args.iter().map(OsString::from).collect()))
}

async fn invoke(engine: &Arc<Engine>, req: InvokeRequest) -> anyhow::Result<InvokeOutcome> {
    Arc::clone(engine).invoke(engine.new_state(), req).await
}

/// How the tool ended; a heph-side failure or a pre-resolution error panics
/// with its message.
fn exited(outcome: anyhow::Result<InvokeOutcome>) -> Exit {
    match outcome {
        Ok(InvokeOutcome::Exited(exit)) => exit,
        Ok(InvokeOutcome::Failed(e)) => panic!("heph failed after resolution: {e:#}"),
        Ok(other) => panic!("unexpected outcome {other:?}"),
        Err(e) => panic!("failed before resolution: {e:#}"),
    }
}

/// The heph-side failure after resolution.
fn failed(outcome: anyhow::Result<InvokeOutcome>) -> anyhow::Error {
    match outcome {
        Ok(InvokeOutcome::Failed(e)) => e,
        Ok(other) => panic!("expected a heph-side failure, got {other:?}"),
        Err(e) => panic!("failed before resolution: {e:#}"),
    }
}

/// Every invocation sandbox of `//<pkg>:<name>` on disk now.
fn invoke_sandboxes(engine: &Engine, pkg: &str, name: &str) -> Vec<PathBuf> {
    let parent = engine
        .home
        .join("sandbox")
        .join(pkg)
        .join("__invoke")
        .join(name);
    match std::fs::read_dir(&parent) {
        Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
        Err(_) => Vec::new(),
    }
}

fn read(path: &Path) -> anyhow::Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
}

/// Wait, under a timeout, for `cond`.
async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(120), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .with_context(|| format!("timed out waiting for {what}"))
}

const TOOLS_BUILD: &str = r#"
target(name = "data", driver = "bash", run = "printf payload > $OUT", out = "data.txt")
target(name = "tool", driver = "bash",
       run = "printf '#!/bin/sh\necho tool-ran \"$@\"\n' > $OUT && chmod +x $OUT",
       out = "mytool")
"#;

/// The closure (a dep at its sandbox path, a tool first on `PATH`), the
/// caller's env and `HEPH_ROOT`, and the args in order and byte for byte, with
/// the program running in the caller's cwd; the sandbox is gone afterwards
/// (N4, N8, R17).
#[tokio::test]
async fn entrypoint_runs_in_caller_cwd_and_env() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("tools", TOOLS_BUILD);
    ws.write_build_file(
        "app",
        r#"
target(name = "ep", driver = "bash", entrypoint = True, args = True,
       deps = {"d": "//tools:data"}, tools = {"t": "//tools:tool"},
       env = {"FROM_TARGET": "target-env"},
       run = "exec > \"$HEPH_TEST_OUT\"; cat $SRC_D; echo; mytool x; echo \"cwd=$(pwd)\"; echo \"caller=$HEPH_TEST_CALLER\"; echo \"root=$HEPH_ROOT\"; echo \"src=$SRC_D\"; echo \"target=$FROM_TARGET\"; printf '<%s>' \"$@\"")
"#,
    );
    let nested = ws.dir.path().join("app/sub");
    std::fs::create_dir_all(&nested)?;
    let out = ws.dir.path().join("out.txt");
    let args = ["", "-x", "--quiet", "a b", "é"];
    let exit = exited(invoke(&ws.engine, request("//app:ep", &nested, &args, &out)?).await);
    assert_eq!(exit, Exit::Code(0));

    let got = read(&out)?;
    let lines: Vec<&str> = got.lines().collect();
    assert_eq!(lines.first(), Some(&"payload"), "{got}");
    assert_eq!(lines.get(1), Some(&"tool-ran x"), "{got}");
    assert_eq!(
        lines.get(2).copied(),
        Some(format!("cwd={}", nested.canonicalize()?.display()).as_str()),
        "{got}"
    );
    assert_eq!(lines.get(3), Some(&"caller=from-caller"), "{got}");
    let root = lines
        .get(4)
        .and_then(|l| l.strip_prefix("root="))
        .context("root line")?;
    assert_eq!(
        Path::new(root).canonicalize()?,
        ws.dir.path().canonicalize()?,
        "HEPH_ROOT is the real workspace root"
    );
    let src = lines
        .get(5)
        .and_then(|l| l.strip_prefix("src="))
        .context("src line")?;
    assert!(
        Path::new(src).starts_with(ws.engine.home.join("sandbox/app/__invoke/ep"))
            && src.ends_with("tools/data.txt"),
        "the dep is in the invocation's own sandbox: {src}"
    );
    assert_eq!(lines.get(6), Some(&"target=target-env"), "{got}");
    assert_eq!(lines.get(7), Some(&"<><-x><--quiet><a b><é>"), "{got}");
    assert!(
        invoke_sandboxes(&ws.engine, "app", "ep").is_empty(),
        "the sandbox goes with the invocation"
    );
    Ok(())
}

/// Undeclared args are refused after the def resolves and before any dep
/// builds, and no sandbox is created; declared, they are appended in order,
/// and none given is `Some(vec![])` (N5, R4).
#[tokio::test]
async fn entrypoint_args_must_be_declared() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let built = ws.dir.path().join("boom-built.txt");
    ws.write_build_file(
        "app",
        &format!(
            r#"
target(name = "boom", driver = "bash", run = "echo x >> {} && exit 1", out = "x")
target(name = "noargs", driver = "exec", entrypoint = True, deps = {{"b": ":boom"}},
       run = ["/bin/echo", "hi"])
target(name = "withargs", driver = "exec", entrypoint = True, args = True,
       run = ["/bin/sh", "-c", "printf '<%s>' \"$@\" > \"$HEPH_TEST_OUT\"", "sh"])
"#,
            built.display()
        ),
    );
    let cwd = ws.dir.path().to_path_buf();
    let out = ws.dir.path().join("out.txt");
    let err = failed(invoke(&ws.engine, request("//app:noargs", &cwd, &["x"], &out)?).await);
    let msg = format!("{err:#}");
    assert!(
        msg.contains("//app:noargs") && msg.contains("args = True"),
        "{msg}"
    );
    assert!(
        err.downcast_ref::<heph::engine::invoke::UndeclaredArgs>()
            .is_some(),
        "{msg}"
    );
    assert!(!built.exists(), "refused before the dep built");
    assert!(
        !ws.engine.home.join("sandbox/app/__invoke").exists(),
        "no sandbox for a refused invocation"
    );

    // No args at all is fine — the dep then builds, and fails, as it should,
    // before any sandbox exists.
    let err = failed(invoke(&ws.engine, request("//app:noargs", &cwd, &[], &out)?).await);
    assert!(
        err.downcast_ref::<heph::engine::invoke::UndeclaredArgs>()
            .is_none()
    );
    assert!(built.exists(), "the dep built");
    assert!(
        hplugin::error::process_exit(&err).is_none(),
        "a dep's exit never passes for the tool's: {err:#}"
    );
    assert!(invoke_sandboxes(&ws.engine, "app", "noargs").is_empty());

    for (args, want) in [
        (vec!["", "-n", "--quiet", "x", "é"], "<><-n><--quiet><x><é>"),
        (vec![], "<>"),
    ] {
        let exit = exited(invoke(&ws.engine, request("//app:withargs", &cwd, &args, &out)?).await);
        assert_eq!(exit, Exit::Code(0));
        assert_eq!(read(&out)?, want, "{args:?}");
    }
    Ok(())
}

/// `bash` with `entrypoint = True`: its script through bash in the caller's
/// cwd, the closure on its environment and the caller's args as `"$@"`.
#[tokio::test]
async fn bash_entrypoint_receives_args() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("tools", TOOLS_BUILD);
    ws.write_build_file(
        "app",
        r#"
target(name = "sh", driver = "bash", entrypoint = True, args = True,
       deps = {"d": "//tools:data"}, tools = {"t": "//tools:tool"},
       run = ["exec > \"$HEPH_TEST_OUT\"", "cat \"$SRC_D\"; echo", "mytool x", "echo \"cwd=$(pwd)\"", "printf '<%s>' \"$@\""])
"#,
    );
    let nested = ws.dir.path().join("app/sub");
    std::fs::create_dir_all(&nested)?;
    let out = ws.dir.path().join("out.txt");
    let args = ["", "-x", "a b", "$HOME", "é"];
    let exit = exited(invoke(&ws.engine, request("//app:sh", &nested, &args, &out)?).await);
    assert_eq!(exit, Exit::Code(0));
    assert_eq!(
        read(&out)?,
        format!(
            "payload\ntool-ran x\ncwd={}\n<><-x><a b><$HOME><é>",
            nested.canonicalize()?.display()
        )
    );

    let exit = exited(invoke(&ws.engine, request("//app:sh", &nested, &[], &out)?).await);
    assert_eq!(exit, Exit::Code(0));
    let got = read(&out)?;
    assert!(got.ends_with("\n<>"), "no args: `\"$@\"` is empty: {got}");
    Ok(())
}

/// `workdir = "caller"` is the caller's cwd, nested or outside the workspace;
/// `"root"` the workspace root; `"sandbox"` the invocation's own sandbox
/// package dir. `scratch` is accepted only in sandbox mode (E21).
#[tokio::test]
async fn workdir_modes() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "build",
        r#"target(name = "c", driver = "scratch", env = "MYCACHE")"#,
    );
    ws.write_build_file(
        "app",
        r#"
target(name = "caller", driver = "exec", entrypoint = True,
       run = ["/bin/sh", "-c", "{ pwd; echo $HEPH_TEST_CALLER; } > \"$HEPH_TEST_OUT\""])
target(name = "root", driver = "exec", entrypoint = True, workdir = "root",
       run = ["/bin/sh", "-c", "{ pwd; echo $HEPH_TEST_CALLER; } > \"$HEPH_TEST_OUT\""])
target(name = "sbx", driver = "exec", entrypoint = True, workdir = "sandbox",
       run = ["/bin/sh", "-c", "{ pwd -P; echo $HEPH_TEST_CALLER; } > \"$HEPH_TEST_OUT\""])
target(name = "scr_caller", driver = "exec", entrypoint = True, scratch = ["//build:c"],
       run = ["true"])
target(name = "scr_sandbox", driver = "exec", entrypoint = True, workdir = "sandbox",
       scratch = ["//build:c"],
       run = ["/bin/sh", "-c", "printf '%s' \"$MYCACHE\" > \"$HEPH_TEST_OUT\""])
"#,
    );
    let nested = ws.dir.path().join("app/deep/er");
    std::fs::create_dir_all(&nested)?;
    let outside = tempfile::tempdir()?;
    let out = outside.path().join("out.txt");
    let root = ws.dir.path().canonicalize()?;
    for (addr, cwd, want) in [
        ("//app:caller", nested.clone(), Some(nested.canonicalize()?)),
        (
            "//app:caller",
            outside.path().to_path_buf(),
            Some(outside.path().canonicalize()?),
        ),
        ("//app:root", nested.clone(), Some(root.clone())),
        (
            "//app:root",
            outside.path().to_path_buf(),
            Some(root.clone()),
        ),
        ("//app:sbx", nested.clone(), None),
    ] {
        let exit = exited(invoke(&ws.engine, request(addr, &cwd, &[], &out)?).await);
        assert_eq!(exit, Exit::Code(0), "{addr} from {cwd:?}");
        let got = read(&out)?;
        let (pwd, caller) = got.split_once('\n').context("two lines")?;
        assert_eq!(caller, "from-caller\n", "{addr} from {cwd:?}");
        match want {
            Some(want) => assert_eq!(Path::new(pwd), want, "{addr} from {cwd:?}"),
            None => {
                let sandboxes = ws
                    .engine
                    .home
                    .join("sandbox/app/__invoke/sbx")
                    .canonicalize()?;
                assert!(
                    Path::new(pwd).starts_with(&sandboxes) && pwd.ends_with("ws/app"),
                    "sandbox mode runs in the invocation's package dir: {pwd}"
                );
            }
        }
    }

    let err = match invoke(&ws.engine, request("//app:scr_caller", &root, &[], &out)?).await {
        Err(e) => e,
        Ok(o) => panic!("scratch outside sandbox mode must not parse: {o:?}"),
    };
    assert!(
        format!("{err:#}").contains("workdir = \"sandbox\""),
        "{err:#}"
    );
    let exit = exited(invoke(&ws.engine, request("//app:scr_sandbox", &root, &[], &out)?).await);
    assert_eq!(exit, Exit::Code(0));
    assert!(read(&out)?.starts_with('/'), "the scratch is presented");
    Ok(())
}

/// Every path by which an entrypoint could be pulled in as an input, or built,
/// refuses it, naming it (N2, R14). `inspect outputs` and `inspect hashout`
/// are `result_addr` on the target — the direct-build row.
#[tokio::test]
async fn entrypoint_refused_as_input() -> anyhow::Result<()> {
    let _env = EnvVar::set("HEPH_E2E_EP_REFUSE_TOKEN", "t");
    let ws = Workspace::new();
    ws.write_build_file(
        "ep",
        r#"
target(name = "tool", driver = "exec", entrypoint = True, run = ["true"], labels = ["shim"])
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
target(name = "group", driver = "group", deps = ["//ep:tool"])
target(name = "via_group", driver = "bash", run = "true", out = "o", deps = [":group"])
target(name = "via_query", driver = "bash", run = "true", out = "o",
       deps = {"q": ["//@heph/query:q@expr=label(shim),exclude_provider=__none__"]})
target(name = "cred", driver = "auth.credential", sources = ["//ep:tool"],
       present = {"env": {"TOKEN": "x"}})
target(name = "via_cred", driver = "bash", run = "true", out = "o", cache = False,
       credentials = [":cred"])
target(name = "ep_dep", driver = "exec", entrypoint = True, run = ["true"], deps = ["//ep:tool"])
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
        "//use:group",
        "//use:via_group",
        "//use:via_query",
        "//use:via_cred",
    ] {
        let err = match ws.run(consumer).await {
            Err(e) => e,
            Ok(_) => panic!("{consumer}: an entrypoint was accepted as an input"),
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("//ep:tool is an entrypoint"),
            "{consumer}: {msg}"
        );
    }

    // An entrypoint's own deps go through the same check.
    let out = ws.dir.path().join("out.txt");
    let err = failed(
        invoke(
            &ws.engine,
            request("//use:ep_dep", ws.dir.path(), &[], &out)?,
        )
        .await,
    );
    assert!(format!("{err:#}").contains("cannot be an input"), "{err:#}");

    // Selections: a label, a package pattern. Under keep-going (the CLI's
    // default) the walk finishes and the refusal names the selection's size
    // (R18): even a selection of one is not an invocation. Fail-fast stops the
    // walk at the refusal, so it names no count.
    for (matcher, size) in [
        (
            heph::htmatcher::Matcher::Label("shim".to_string()),
            "the only target the selection resolved to",
        ),
        (
            heph::htmatcher::Matcher::PackagePrefix("ep".into()),
            "one of the 3 targets the selection resolved to",
        ),
    ] {
        let err = match ws.run_matcher(&matcher).await {
            Err(e) => e,
            Ok(_) => panic!("a selection built an entrypoint"),
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("//ep:tool is an entrypoint"), "{msg}");
        assert!(!msg.contains("selection resolved to"), "fail-fast: {msg}");

        let batch = Arc::clone(&ws.engine)
            .result(
                ws.engine.new_state_with_fail_fast(false),
                &matcher,
                OutputMatcher::All,
                &ResultOptions::default(),
            )
            .await?;
        let refused: Vec<String> = batch
            .errors
            .iter()
            .map(|(_, e)| format!("{e:#}"))
            .filter(|m| m.contains("//ep:tool is an entrypoint"))
            .collect();
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!(refused[0].contains(size), "{}", refused[0]);
    }
    // A direct build — `heph run` of it as a build, `inspect outputs`,
    // `inspect hashout`, all `result_addr` on it — hits the execute backstop.
    let err = match Arc::clone(&ws.engine)
        .result_addr(
            ws.engine.new_state(),
            &parse_addr("//ep:tool")?,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await
    {
        Err(e) => e,
        Ok(_) => panic!("an entrypoint was built"),
    };
    assert!(format!("{err:#}").contains("never built"), "{err:#}");
    Ok(())
}

/// Never cached: a second invocation runs again, and no cache entry — local
/// or remote — is ever written for the entrypoint, though its dep is cached in
/// both (N1).
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
target(name = "entrypoint_unique_name", driver = "exec", entrypoint = True,
       deps = {{"d": ":dep_unique_name"}},
       run = ["/bin/sh", "-c", "echo x >> {}"])
"#,
            counter.display()
        ),
    )?;
    let engine = engine_with_remote(root.path(), remote.path())?;
    let out = root.path().join("out.txt");
    for _ in 0..2 {
        let req = request("//p:entrypoint_unique_name", root.path(), &[], &out)?;
        assert_eq!(exited(invoke(&engine, req).await), Exit::Code(0));
    }
    assert_eq!(read(&counter)?, "x\nx\n", "ran twice");
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

/// An engine over `root` with a `file://` remote cache.
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
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_exec().with_host_path()))?;
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash().with_host_path()))?;
    let e = Arc::new(e);
    e.install_exec_runner_host();
    Ok(e)
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

/// What [`RecordingEntrypoint`]'s run saw.
#[derive(Debug, Default, Clone)]
struct SeenOptions {
    cwd: Option<PathBuf>,
    heph_root: Option<OsString>,
    caller_marker: bool,
    args: Option<Vec<OsString>>,
    inherit: bool,
    sandbox_dir: PathBuf,
    /// The handover was still unfired when the run reached the driver.
    handover_pending: bool,
}

/// A plain driver whose def is an entrypoint that says "cache me" — the shape
/// a provider or option default reaching the def would have — and whose run
/// records the options the engine sent.
struct RecordingEntrypoint {
    seen: Arc<std::sync::Mutex<Option<SeenOptions>>>,
}

#[async_trait::async_trait]
impl hplugin::driver::Driver for RecordingEntrypoint {
    fn config(
        &self,
        _req: hplugin::driver::ConfigRequest,
    ) -> anyhow::Result<hplugin::driver::ConfigResponse> {
        Ok(hplugin::driver::ConfigResponse {
            name: "recording".to_string(),
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
        Ok(hplugin::driver::ParseResponse {
            target_def: hplugin::driver::targetdef::TargetDef {
                addr: req.target_spec.addr.clone(),
                cache: hplugin::driver::targetdef::CacheConfig::on(true),
                entrypoint: Some(hplugin::driver::targetdef::Entrypoint::new(
                    false,
                    hplugin::driver::targetdef::Workdir::Root,
                )),
                hash: b"recording".to_vec(),
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
        req: hplugin::driver::RunRequest<'a, 'io>,
        _ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::RunResponse> {
        let o = &req.options;
        let env = o.base_env.as_deref().unwrap_or_default();
        *self.seen.lock().expect("seen") = Some(SeenOptions {
            cwd: o.cwd.clone(),
            heph_root: env
                .iter()
                .rev()
                .find(|(k, _)| k == "HEPH_ROOT")
                .map(|(_, v)| v.clone()),
            caller_marker: env.iter().any(|(k, _)| k == "HEPH_TEST_CALLER"),
            args: o.args.clone(),
            inherit: matches!(o.stdio, hplugin::driver::RunStdio::Inherit(_)),
            sandbox_dir: req.sandbox_dir.clone(),
            handover_pending: matches!(
                &o.stdio,
                hplugin::driver::RunStdio::Inherit(fg) if fg.handover.is_some()
            ),
        });
        anyhow::ensure!(req.sandbox_dir.is_dir(), "the sandbox exists for the run");
        Ok(hplugin::driver::RunResponse {
            artifacts: vec![],
            sandbox_cleanup: None,
            sandbox_guards: vec![],
        })
    }
    async fn run_shell<'a, 'io>(
        &self,
        _req: hplugin::driver::RunRequest<'a, 'io>,
        _ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::RunResponse> {
        anyhow::bail!("not a shell test")
    }
}

/// Any driver: the engine forces cache off from the def's attribute in
/// `get_def`, whatever the driver said (N1, E15), and `invoke` reaches the
/// driver's ordinary `run` with the options resolved from the def — the
/// workdir, the caller's env plus `HEPH_ROOT`, no args for a target that takes
/// none, the caller's terminal (N8, N9, R17).
#[tokio::test]
async fn entrypoint_runs_through_driver_run_with_options() -> anyhow::Result<()> {
    let handovers = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(std::sync::Mutex::new(None));
    let driver_seen = Arc::clone(&seen);
    let ws = WorkspaceBuilder::new()?
        .with_provider(|init| {
            Box::new(pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
                init.runtime.clone(),
                Arc::clone(&init.functions),
            ))
        })
        .with_plugin("recording", move |_| {
            Ok(
                PluginParts::default().with_driver(Box::new(RecordingEntrypoint {
                    seen: Arc::clone(&driver_seen),
                })),
            )
        })
        .build()?;
    ws.write_build_file("app", r#"target(name = "ep", driver = "recording")"#);
    let def = Arc::clone(&ws.engine)
        .get_def(ws.engine.new_state(), &parse_addr("//app:ep")?)
        .await?;
    assert_eq!(
        def.target_def.cache,
        hplugin::driver::targetdef::CacheConfig::off(),
        "the def's attribute forces cache off whatever the driver said"
    );

    let elsewhere = tempfile::tempdir()?;
    let out = elsewhere.path().join("out");
    let exit = exited(
        invoke(
            &ws.engine,
            request("//app:ep", elsewhere.path(), &[], &out)?
                .with_handover(handover_counter(&handovers)),
        )
        .await,
    );
    assert_eq!(exit, Exit::Code(0));
    assert_eq!(handovers.load(Ordering::SeqCst), 1, "handed over once");
    let seen = seen
        .lock()
        .expect("seen")
        .clone()
        .context("run was called")?;
    let root = ws.dir.path().canonicalize()?;
    assert_eq!(
        seen.cwd.as_deref().map(Path::canonicalize).transpose()?,
        Some(root.clone()),
        "workdir = root"
    );
    assert_eq!(
        seen.heph_root
            .as_deref()
            .map(|r| Path::new(r).canonicalize())
            .transpose()?,
        Some(root),
        "HEPH_ROOT, last in the base env"
    );
    assert!(seen.caller_marker, "the caller's env is the base");
    assert_eq!(seen.args, None, "a target without `args` gets none");
    assert!(seen.inherit, "the caller's terminal");
    assert!(
        seen.sandbox_dir
            .starts_with(ws.engine.home.join("sandbox/app/__invoke/ep")),
        "{:?}",
        seen.sandbox_dir
    );
    assert!(!seen.sandbox_dir.exists(), "and it is gone");
    assert!(
        !seen.handover_pending,
        "a plain driver spawns from `run`, so the engine fired the handover before it"
    );
    Ok(())
}

/// The process exit is the driver's `ProcessFailed.exit`: a code, or the
/// signal that killed the tool (N8, R4).
#[tokio::test]
async fn entrypoint_runs_through_driver_run() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"
target(name = "three", driver = "exec", entrypoint = True, run = ["/bin/sh", "-c", "exit 3"])
target(name = "term", driver = "exec", entrypoint = True, run = ["/bin/sh", "-c", "kill -TERM $$"])
"#,
    );
    let out = ws.dir.path().join("out");
    for (addr, want) in [
        ("//app:three", Exit::Code(3)),
        ("//app:term", Exit::Signal(15)),
    ] {
        let exit = exited(invoke(&ws.engine, request(addr, ws.dir.path(), &[], &out)?).await);
        assert_eq!(exit, want, "{addr}");
    }
    Ok(())
}

/// A managed driver standing in for a third-party one: exec's parse and
/// `apply_transitive`, then a change to the entrypoint attribute.
struct TransitiveTamper {
    exec: pluginexec::Driver,
    tamper: fn(&mut Option<hplugin::driver::targetdef::Entrypoint>),
    /// Behind the seam the def arrives with its `raw_def` as serialized bytes,
    /// which exec's `apply_transitive` (an in-process driver, reading it with
    /// `def::<T>()`) cannot take: pass the def through instead, and tamper.
    passthrough: bool,
}

#[async_trait::async_trait]
impl heph::engine::driver_managed::ManagedDriver for TransitiveTamper {
    fn config(
        &self,
        _req: hplugin::driver::ConfigRequest,
    ) -> anyhow::Result<hplugin::driver::ConfigResponse> {
        Ok(hplugin::driver::ConfigResponse {
            name: "tamper".to_string(),
        })
    }
    fn schema(&self) -> hplugin::driver::DriverSchema {
        heph::engine::driver_managed::ManagedDriver::schema(&self.exec)
    }
    async fn parse(
        &self,
        req: hplugin::driver::ParseRequest,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ParseResponse> {
        heph::engine::driver_managed::ManagedDriver::parse(&self.exec, req, ctoken).await
    }
    async fn apply_transitive(
        &self,
        req: hplugin::driver::ApplyTransitiveRequest,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ApplyTransitiveResponse> {
        let mut res = if self.passthrough {
            hplugin::driver::ApplyTransitiveResponse {
                target_def: req.target_def,
            }
        } else {
            heph::engine::driver_managed::ManagedDriver::apply_transitive(&self.exec, req, ctoken)
                .await?
        };
        (self.tamper)(&mut res.target_def.entrypoint);
        Ok(res)
    }
    async fn run<'a, 'io>(
        &self,
        req: heph::engine::driver_managed::ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<heph::engine::driver_managed::ManagedRunResponse> {
        heph::engine::driver_managed::ManagedDriver::run(&self.exec, req, ctoken).await
    }
}

/// Tell this process's SDK the host's capabilities the way a cdylib learns
/// them: the `CreateConfig` bytes `heph_plugin_create` is handed, decoded by
/// the SDK. In-process the SDK's static is the test binary's, shared by every
/// test here, and every caller records the same thing — this host's bits.
fn create_seam_plugin(root: &Path, name: &str) -> anyhow::Result<()> {
    use buffa::Message as _;
    let cfg =
        hplugin_abi::convert::CreateConfig::new(root, root.join(".heph"), name, Default::default());
    let bytes = hplugin_abi::convert::create_config_to_pb(&cfg).encode_to_vec();
    let decoded = hplugin_sdk::stabby::create_config_from_bytes(&bytes)?;
    anyhow::ensure!(
        hplugin_sdk::stabby::host_capabilities().entrypoints,
        "the SDK recorded the entrypoint bit ({decoded:?})"
    );
    Ok(())
}

/// `driver` exported by the SDK and decoded by the host loader — the same
/// `named_managed_driver` → `decode_drivers` pair a cdylib's components go
/// through — as the parts of plugin `name`.
fn behind_the_seam(
    root: &Path,
    name: &str,
    driver: impl heph::engine::driver_managed::ManagedDriver + 'static,
) -> anyhow::Result<PluginParts> {
    create_seam_plugin(root, name)?;
    let exported = hplugin_sdk::stabby::named_managed_driver(driver)?;
    let mut parts = PluginParts::default();
    for (_local, host) in hplugin_stabby::load_stable::decode_drivers([exported])? {
        parts = parts.with_managed_driver(Box::new(host));
    }
    Ok(parts)
}

/// The entrypoint attribute is the parse's: a driver whose `apply_transitive`
/// drops it does not make the target buildable or cacheable, and one that
/// changes it — or makes a plain target an entrypoint — is refused (R13).
///
/// Every row runs twice: the driver in-process, and the same driver behind the
/// stable-ABI seam (SDK guest ↔ host proxy, the def encoded both ways), which
/// is where a third-party cdylib's `apply_transitive` reaches the engine from.
/// Not through `dlopen`: the def rides the `ApplyTransitive` payload, so a
/// loaded library would have nothing new to reject.
#[tokio::test]
async fn entrypoint_survives_apply_transitive() -> anyhow::Result<()> {
    type Tamper = fn(&mut Option<hplugin::driver::targetdef::Entrypoint>);
    let rows: [(&str, &str, Tamper, Result<bool, &str>); 4] = [
        ("dropped", "ep", |e| *e = None, Ok(true)),
        ("kept", "ep", |_| {}, Ok(true)),
        (
            "changed",
            "ep",
            |e| {
                *e = Some(hplugin::driver::targetdef::Entrypoint::new(
                    true,
                    hplugin::driver::targetdef::Workdir::Root,
                ))
            },
            Err("changed the target's entrypoint attribute"),
        ),
        (
            "added to a plain target",
            "plain",
            |e| {
                *e = Some(hplugin::driver::targetdef::Entrypoint::new(
                    false,
                    hplugin::driver::targetdef::Workdir::Caller,
                ))
            },
            Err("changed the target's entrypoint attribute"),
        ),
    ];
    for (via, (row, name, tamper, want)) in ["in-process", "seam"]
        .into_iter()
        .flat_map(|via| rows.map(|r| (via, r)))
    {
        let row = format!("{row} ({via})");
        let ws = WorkspaceBuilder::new()?
            .with_provider(|init| {
                Box::new(pluginbuildfile::Provider::new(
                    init.root.to_path_buf(),
                    init.runtime.clone(),
                    Arc::clone(&init.functions),
                ))
            })
            .with_managed_driver(Box::new(pluginexec::Driver::new_bash().with_host_path()))
            .with_plugin("tamper", move |init| {
                let driver = TransitiveTamper {
                    exec: pluginexec::Driver::new_exec().with_host_path(),
                    tamper,
                    passthrough: via == "seam",
                };
                if via == "seam" {
                    behind_the_seam(&init.root, "tamper", driver)
                } else {
                    Ok(PluginParts::default().with_managed_driver(Box::new(driver)))
                }
            })
            .build()?;
        ws.write_build_file(
            "app",
            r#"
target(name = "carrier", driver = "bash", run = "printf c > $OUT", out = "c",
       transitive = {"env": {"FROM_CARRIER": "1"}})
target(name = "ep", driver = "tamper", entrypoint = True, deps = [":carrier"], run = ["true"])
target(name = "plain", driver = "tamper", deps = [":carrier"], run = ["true"], out = [])
"#,
        );
        let def = Arc::clone(&ws.engine)
            .get_def(
                ws.engine.new_state(),
                &parse_addr(&format!("//app:{name}"))?,
            )
            .await;
        match (def, want) {
            (Ok(def), Ok(is_entrypoint)) => {
                assert_eq!(def.target_def.entrypoint.is_some(), is_entrypoint, "{row}");
                assert!(
                    def.applied_transitive.is_some(),
                    "{row}: transitive applied"
                );
                assert!(!def.target_def.cache.enabled, "{row}: still uncached");
            }
            (Err(e), Err(needle)) => {
                assert!(format!("{e:#}").contains(needle), "{row}: {e:#}");
            }
            (got, want) => panic!("{row}: got {:?}, want {want:?}", got.map(|_| ())),
        }
    }
    Ok(())
}

/// N invocations of one target run at once — two engines on one home, several
/// calls in each — each in its own sandbox, sharing one build of the dep. Each
/// tool waits for every other's marker under a timeout, so a serialized run
/// fails rather than passing slowly; `gc` and the sibling sweep run while all
/// of them are live and take nothing (N10, R23).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_invocations_run_in_parallel() -> anyhow::Result<()> {
    const N: usize = 16;
    let ws = Workspace::new();
    let markers = tempfile::tempdir()?;
    let counter = ws.dir.path().join("dep-built.txt");
    ws.write_build_file(
        "app",
        &format!(
            r#"
target(name = "dep", driver = "bash", run = "echo x >> {counter} && printf d > $OUT", out = "d")
target(name = "ep", driver = "bash", entrypoint = True, deps = {{"d": ":dep"}},
       run = "d={markers}; printf '%s' \"$WORKSPACE_ROOT\" > \"$d/m-$$\"; i=0; while [ \"$(ls \"$d\" | grep -c '^m-')\" -lt {N} ] || [ ! -e \"$d/go\" ]; do i=$((i+1)); [ $i -gt 2400 ] && exit 7; sleep 0.05; done; [ \"$(cat \"$SRC_D\")\" = d ] || exit 8")
"#,
            counter = counter.display(),
            markers = markers.path().display(),
        ),
    );
    let engines = [Arc::clone(&ws.engine), ws.reopen()?];
    let out = ws.dir.path().join("unused");
    let marker_files = || -> Vec<PathBuf> {
        std::fs::read_dir(markers.path())
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .is_some_and(|n| n.to_string_lossy().starts_with("m-"))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    // All cold, all at once, alternating engines: every request but one
    // misses on the dep, and each must share the one build rather than wait
    // for the running tools — which hold the dep's read for their whole run —
    // to exit.
    let mut runs = Vec::new();
    for i in 0..N {
        let engine = Arc::clone(&engines[i % 2]);
        let req = request("//app:ep", ws.dir.path(), &[], &out)?;
        runs.push(tokio::spawn(async move { invoke(&engine, req).await }));
    }
    wait_for("every invocation to be running at once", || {
        marker_files().len() >= N
    })
    .await?;

    let sandboxes: std::collections::BTreeSet<PathBuf> = marker_files()
        .iter()
        .map(|m| read(m).map(PathBuf::from))
        .collect::<anyhow::Result<_>>()?;
    assert_eq!(sandboxes.len(), N, "each invocation has its own sandbox");

    // `gc` (with its sandbox sweep) while all N are live, from each engine.
    for engine in &engines {
        let stats = Arc::clone(engine).gc_all(engine.new_state()).await?;
        assert_eq!(stats.invoke_sandboxes_removed, 0, "live sandboxes are held");
    }
    for s in &sandboxes {
        assert!(s.is_dir(), "{} survives gc and the sweep", s.display());
    }

    std::fs::write(markers.path().join("go"), b"")?;
    for run in runs {
        let exit = exited(
            tokio::time::timeout(Duration::from_secs(180), run)
                .await
                .context("an invocation never finished")??,
        );
        assert_eq!(exit, Exit::Code(0));
    }
    assert_eq!(read(&counter)?, "x\n", "the shared dep built once");
    assert!(invoke_sandboxes(&ws.engine, "app", "ep").is_empty());
    Ok(())
}

/// The invocation's sandbox is gone after every exit — success, failure, a
/// signal death, a cancellation, and each heph-side failure inside the
/// sandbox — and none is created for a failure before it (N10, E20, R23).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invoke_sandbox_always_removed() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let started = ws.dir.path().join("started");
    ws.write_build_file(
        "app",
        &format!(
            r#"
target(name = "boom", driver = "bash", run = "exit 1", out = "x")
target(name = "x1", driver = "bash", run = "printf 1 > $OUT", out = "same.txt")
target(name = "x2", driver = "bash", run = "printf 2 > $OUT", out = "same.txt")
target(name = "cred", driver = "auth.credential",
       sources = [heph.auth.env(["HEPH_E2E_EP_UNSET_TOKEN_NEVER_SET"])],
       present = {{"env": {{"TOKEN": "${{heph_e2e_ep_unset_token_never_set}}"}}}})

target(name = "ok", driver = "exec", entrypoint = True, run = ["true"])
target(name = "fail", driver = "exec", entrypoint = True, run = ["/bin/sh", "-c", "exit 1"])
target(name = "signal", driver = "exec", entrypoint = True, run = ["/bin/sh", "-c", "kill -TERM $$"])
target(name = "spawn", driver = "exec", entrypoint = True, run = ["/nonexistent/heph-e2e-tool"])
target(name = "materialize", driver = "exec", entrypoint = True,
       deps = {{"a": ":x1", "b": ":x2"}}, run = ["true"])
target(name = "credential", driver = "exec", entrypoint = True, credentials = [":cred"],
       run = ["true"])
target(name = "dep", driver = "exec", entrypoint = True, deps = [":boom"], run = ["true"])
target(name = "args", driver = "exec", entrypoint = True, run = ["true"])
target(name = "cancel", driver = "exec", entrypoint = True,
       run = ["/bin/sh", "-c", "touch {started}; sleep 60"])
"#,
            started = started.display()
        ),
    );
    let cwd = ws.dir.path();
    let out = cwd.join("out");
    for (name, args, want) in [
        ("ok", &[][..], Some(Exit::Code(0))),
        ("fail", &[], Some(Exit::Code(1))),
        ("signal", &[], Some(Exit::Signal(15))),
        ("spawn", &[], None),
        ("materialize", &[], None),
        ("credential", &[], None),
        ("dep", &[], None),
        ("args", &["x"], None),
    ] {
        let outcome = invoke(
            &ws.engine,
            request(&format!("//app:{name}"), cwd, args, &out)?,
        )
        .await;
        match want {
            Some(want) => assert_eq!(exited(outcome), want, "{name}"),
            None => {
                let err = failed(outcome);
                assert!(
                    hplugin::error::process_exit(&err).is_none(),
                    "{name}: {err:#}"
                );
            }
        }
        assert!(
            invoke_sandboxes(&ws.engine, "app", name).is_empty(),
            "{name}: no sandbox left"
        );
    }

    // Cancelled while the tool runs: the driver kills it, and the sandbox
    // still goes before `invoke` returns.
    let rs = ws.engine.new_state();
    let req = request("//app:cancel", cwd, &[], &out)?;
    let run = tokio::spawn({
        let (engine, rs) = (Arc::clone(&ws.engine), Arc::clone(&rs));
        async move { engine.invoke(rs, req).await }
    });
    wait_for("the tool to start", || started.exists()).await?;
    rs.ctoken().cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(60), run)
        .await
        .context("a cancelled invocation never returned")??;
    let err = failed(outcome);
    // Exec's run reports a cancellation as the bare `cancelled` it always has.
    assert!(
        hplugin::error::is_cancelled(&err) || format!("{err:#}").ends_with("cancelled"),
        "{err:#}"
    );
    assert!(invoke_sandboxes(&ws.engine, "app", "cancel").is_empty());
    Ok(())
}

/// An invocation copies its whole closure — deps and tools — and links
/// nothing from the stage: the files are writable, and a change to one
/// cannot reach what cached builds share (R1, replacing E13).
#[tokio::test]
async fn entrypoint_deps_are_copied_not_linked() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file("tools", TOOLS_BUILD);
    ws.write_build_file(
        "app",
        r#"
target(name = "ep", driver = "bash", entrypoint = True,
       deps = {"d": "//tools:data"}, tools = {"t": "//tools:tool"},
       run = "exec > \"$HEPH_TEST_OUT\"; t=$(command -v mytool); for f in \"$SRC_D\" \"$t\"; do [ -L \"$f\" ] && echo \"link $(readlink \"$f\")\"; [ -w \"$f\" ] && echo writable; done; echo more >> \"$SRC_D\" && cat \"$SRC_D\"")
"#,
    );
    let out = ws.dir.path().join("out.txt");
    let exit = exited(invoke(&ws.engine, request("//app:ep", ws.dir.path(), &[], &out)?).await);
    assert_eq!(exit, Exit::Code(0));
    let got = read(&out)?;
    assert!(
        !got.contains("/stage/"),
        "nothing links into the stage: {got}"
    );
    assert_eq!(
        got.matches("writable").count(),
        2,
        "the dep and the tool are writable copies: {got}"
    );
    assert!(got.ends_with("payloadmore\n"), "{got}");
    let stage = ws.engine.home.join("stage");
    assert!(
        walk(&stage)
            .iter()
            .all(|p| !p.to_string_lossy().contains("tools")),
        "the closure was never staged"
    );
    // The cached dep is untouched by the tool's write.
    let data = ws.run("//tools:data").await?;
    assert_eq!(common::artifact_string(&data), "payload");
    Ok(())
}

/// Sets an env var for the test's duration.
struct EnvVar(&'static str);

impl EnvVar {
    fn set(name: &'static str, value: &str) -> Self {
        // SAFETY: each test uses a variable name of its own.
        unsafe { std::env::set_var(name, value) };
        Self(name)
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        // SAFETY: as in `set`.
        unsafe { std::env::remove_var(self.0) };
    }
}

/// A credential's env var and presented file reach the tool unredacted, the
/// file inside the invocation's sandbox and gone with it; one whose acquire
/// fails is a heph-side failure that leaves no sandbox (E18, R23).
#[tokio::test]
async fn entrypoint_receives_credentials() -> anyhow::Result<()> {
    let _env = EnvVar::set("HEPH_E2E_EP_CRED_TOKEN", "s3cr3t-material");
    let ws = Workspace::new();
    ws.write_build_file(
        "auth",
        r#"
target(name = "t", driver = "auth.credential",
       sources = [heph.auth.env(["HEPH_E2E_EP_CRED_TOKEN"])],
       present = {"files": {"tok": "${heph_e2e_ep_cred_token}"},
                  "env": {"TOKEN": "${heph_e2e_ep_cred_token}", "TOKFILE": "${file:tok}"}})
target(name = "missing", driver = "auth.credential",
       sources = [heph.auth.env(["HEPH_E2E_EP_CRED_NEVER_SET"])],
       present = {"env": {"TOKEN": "${heph_e2e_ep_cred_never_set}"}})
"#,
    );
    ws.write_build_file(
        "app",
        r#"
target(name = "ep", driver = "bash", entrypoint = True, credentials = ["//auth:t"],
       run = "exec > \"$HEPH_TEST_OUT\"; echo \"$TOKEN\"; cat \"$TOKFILE\"; echo; echo \"$TOKFILE\"")
target(name = "nocred", driver = "bash", entrypoint = True, credentials = ["//auth:missing"],
       run = "true")
"#,
    );
    let out = ws.dir.path().join("out.txt");
    let exit = exited(invoke(&ws.engine, request("//app:ep", ws.dir.path(), &[], &out)?).await);
    assert_eq!(exit, Exit::Code(0));
    let got = read(&out)?;
    let lines: Vec<&str> = got.lines().collect();
    assert_eq!(
        lines.first(),
        Some(&"s3cr3t-material"),
        "the material reaches the tool unredacted (X4 frozen): {got}"
    );
    assert_eq!(lines.get(1), Some(&"s3cr3t-material"), "{got}");
    let file = Path::new(lines.get(2).context("the file's path")?);
    assert!(
        file.starts_with(ws.engine.home.join("sandbox/app/__invoke/ep")),
        "presented inside the invocation's sandbox: {}",
        file.display()
    );
    assert!(!file.exists(), "and gone with it");

    let err = failed(
        invoke(
            &ws.engine,
            request("//app:nocred", ws.dir.path(), &[], &out)?,
        )
        .await,
    );
    assert!(format!("{err:#}").contains("//auth:missing"), "{err:#}");
    assert!(invoke_sandboxes(&ws.engine, "app", "nocred").is_empty());
    Ok(())
}

/// While the tool runs, `gc` takes neither its sandbox nor anything in it:
/// the closure read after the sweep is whole (N3, N10).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gc_keeps_closure_of_running_tool() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let gate = tempfile::tempdir()?;
    ws.write_build_file("tools", TOOLS_BUILD);
    ws.write_build_file(
        "app",
        &format!(
            r#"
target(name = "ep", driver = "bash", entrypoint = True,
       deps = {{"d": "//tools:data"}}, tools = {{"t": "//tools:tool"}},
       run = "g={gate}; touch \"$g/started\"; i=0; while [ ! -e \"$g/go\" ]; do i=$((i+1)); [ $i -gt 2400 ] && exit 7; sleep 0.05; done; exec > \"$HEPH_TEST_OUT\"; cat \"$SRC_D\"; mytool after-gc")
"#,
            gate = gate.path().display()
        ),
    );
    let out = ws.dir.path().join("out.txt");
    let req = request("//app:ep", ws.dir.path(), &[], &out)?;
    let engine = Arc::clone(&ws.engine);
    let run = tokio::spawn(async move { invoke(&engine, req).await });
    wait_for("the tool to start", || gate.path().join("started").exists()).await?;

    let stats = Arc::clone(&ws.engine).gc_all(ws.engine.new_state()).await?;
    assert_eq!(
        stats.invoke_sandboxes_removed, 0,
        "the live sandbox is held"
    );
    assert_eq!(invoke_sandboxes(&ws.engine, "app", "ep").len(), 1);

    std::fs::write(gate.path().join("go"), b"")?;
    let exit = exited(
        tokio::time::timeout(Duration::from_secs(120), run)
            .await
            .context("the tool never finished")??,
    );
    assert_eq!(exit, Exit::Code(0));
    assert_eq!(read(&out)?, "payloadtool-ran after-gc\n");
    Ok(())
}

/// A `--force` rebuild of a dep revision the running tool came from waits for
/// the tool, whose closure carries the revision's read lock (N3).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn force_rebuild_waits_for_running_tool() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let gate = tempfile::tempdir()?;
    ws.write_build_file("tools", TOOLS_BUILD);
    ws.write_build_file(
        "app",
        &format!(
            r#"
target(name = "ep", driver = "bash", entrypoint = True, deps = {{"d": "//tools:data"}},
       run = "g={gate}; touch \"$g/started\"; i=0; while [ ! -e \"$g/go\" ]; do i=$((i+1)); [ $i -gt 2400 ] && exit 7; sleep 0.05; done")
"#,
            gate = gate.path().display()
        ),
    );
    let out = ws.dir.path().join("out.txt");
    let req = request("//app:ep", ws.dir.path(), &[], &out)?;
    let engine = Arc::clone(&ws.engine);
    let run = tokio::spawn(async move { invoke(&engine, req).await });
    wait_for("the tool to start", || gate.path().join("started").exists()).await?;

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
    std::fs::write(gate.path().join("go"), b"")?;
    let exit = exited(
        tokio::time::timeout(Duration::from_secs(120), run)
            .await
            .context("the tool never finished")??,
    );
    assert_eq!(exit, Exit::Code(0));
    tokio::time::timeout(Duration::from_secs(60), rebuild)
        .await
        .context("the rebuild never finished once the tool was done")???;
    Ok(())
}

/// The process exits as soon as the tool does, so `invoke` drains the cache
/// writes its dep builds queued before the tool starts: with the writer held,
/// it does not finish; released, it does, and a second engine finds the dep
/// cached (E15).
///
/// A `hash_deps` dep, on purpose: it is built but never materialized, so
/// nothing but the drain waits for its write.
#[tokio::test]
async fn fast_exit_flushes_pending_cache_writes() -> anyhow::Result<()> {
    let ws = Workspace::new();
    let counter = ws.dir.path().join("built.txt");
    ws.write_build_file(
        "app",
        &format!(
            r#"
target(name = "dep", driver = "bash", run = "echo x >> {} && printf d > $OUT", out = "d")
target(name = "ep", driver = "exec", entrypoint = True, hash_deps = [":dep"], run = ["true"])
"#,
            counter.display()
        ),
    );
    let out = ws.dir.path().join("out.txt");
    let hold = ws.engine.hold_local_cache_writes();
    let engine = Arc::clone(&ws.engine);
    let req = request("//app:ep", ws.dir.path(), &[], &out)?;
    let mut pending = tokio::spawn(async move { invoke(&engine, req).await });
    assert!(
        tokio::time::timeout(Duration::from_secs(3), &mut pending)
            .await
            .is_err(),
        "invoke must not run the tool while the dep's writes are queued"
    );
    drop(hold);
    let exit = exited(
        tokio::time::timeout(Duration::from_secs(60), pending)
            .await
            .context("invoke never finished once writes were released")??,
    );
    assert_eq!(exit, Exit::Code(0));

    // A second engine on the same home: the dep is a hit.
    let e = ws.reopen()?;
    let exit = exited(invoke(&e, request("//app:ep", ws.dir.path(), &[], &out)?).await);
    assert_eq!(exit, Exit::Code(0));
    assert_eq!(read(&counter)?, "x\n", "the dep was cached");
    Ok(())
}

/// `${read://…}` in the entrypoint's own options is the producer's content,
/// the producer built first.
#[tokio::test]
async fn entrypoint_resolves_deferred_config() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "infra",
        r#"target(name = "version", driver = "bash", run = "printf 1.4.2 > $OUT", out = "v.txt")"#,
    );
    ws.write_build_file(
        "app",
        r#"target(name = "ep", driver = "exec", entrypoint = True,
       env = {"VERSION": "${read://infra:version}"},
       run = ["/bin/sh", "-c", "printf %s \"$VERSION\" > \"$HEPH_TEST_OUT\""])"#,
    );
    let out = ws.dir.path().join("out.txt");
    let exit = exited(invoke(&ws.engine, request("//app:ep", ws.dir.path(), &[], &out)?).await);
    assert_eq!(exit, Exit::Code(0));
    assert_eq!(read(&out)?, "1.4.2");
    Ok(())
}

/// `--shell` keeps the args rule: undeclared trailing args are refused before
/// anything builds (N5). The shell itself needs a tty: `entrypoint_shell_pty`
/// in `crates/bin-e2e`.
#[tokio::test]
async fn entrypoint_shell_refuses_undeclared_args() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"
target(name = "boom", driver = "bash", run = "exit 1", out = "x")
target(name = "noargs", driver = "exec", entrypoint = True, deps = {"b": ":boom"}, run = ["/bin/echo"])
"#,
    );
    let out = ws.dir.path().join("out.txt");
    let req = request("//app:noargs", ws.dir.path(), &["x"], &out)?.with_shell(true);
    let err = failed(invoke(&ws.engine, req).await);
    assert!(
        err.downcast_ref::<heph::engine::invoke::UndeclaredArgs>()
            .is_some(),
        "refused before the failing dep built: {err:#}"
    );
    Ok(())
}

/// The handover — where the CLI stops reading Ctrl-C as "cancel" (R9) —
/// fires exactly once, after everything heph does first and before the tool
/// runs: the managed layer fires it for exec once the closure is
/// materialized. A run that fails before the tool (a refused arg, a failing
/// dep) never hands over, so Ctrl-C keeps cancelling it.
#[tokio::test]
async fn entrypoint_handover_fires_once_before_the_tool() -> anyhow::Result<()> {
    let ws = Workspace::new();
    ws.write_build_file(
        "app",
        r#"
target(name = "dep", driver = "bash", run = "printf d > $OUT", out = "d")
target(name = "boom", driver = "bash", run = "exit 1", out = "x")
target(name = "check", driver = "exec", entrypoint = True, deps = {"d": ":dep"},
       run = ["/bin/sh", "-c", "test -f \"$HEPH_TEST_OUT\""])
target(name = "broken", driver = "exec", entrypoint = True, deps = {"b": ":boom"},
       run = ["/bin/true"])
"#,
    );
    let cwd = ws.dir.path().to_path_buf();
    let marker = ws.dir.path().join("handed-over");
    let handovers = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&handovers);
    let marked = marker.clone();
    // The tool exits 0 only if the handover already ran when it started.
    let req = request("//app:check", &cwd, &[], &marker)?.with_handover(
        hplugin::driver::Handover::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
            drop(std::fs::write(&marked, b""));
        }),
    );
    assert_eq!(exited(invoke(&ws.engine, req).await), Exit::Code(0));
    assert_eq!(handovers.load(Ordering::SeqCst), 1);

    for (addr, args) in [("//app:check", &["x"][..]), ("//app:broken", &[][..])] {
        let never = Arc::new(AtomicUsize::new(0));
        let req = request(addr, &cwd, args, &marker)?.with_handover(handover_counter(&never));
        failed(invoke(&ws.engine, req).await);
        assert_eq!(never.load(Ordering::SeqCst), 0, "{addr} {args:?}");
    }
    Ok(())
}

/// A third-party driver as a cdylib would ship it: its own parse declares the
/// entrypoint (no exec underneath), and its run spawns the tool with the SDK's
/// `foreground` spawn, honouring every `RunOptions` field it is sent. The tool
/// is `/bin/sh -c SCRIPT sh <args>` — it writes what it saw to
/// `$HEPH_TEST_OUT`, then exits 42, or with `HEPH_TEST_MODE=wait` traps TERM
/// (marker, exit 7) and waits for one.
struct ForegroundTool;

const FOREGROUND_TOOL_SCRIPT: &str = r#"
{
  echo "cwd=$(pwd -P)"
  echo "caller=$HEPH_TEST_CALLER"
  echo "root=$HEPH_ROOT"
  for a in "$@"; do echo "arg=[$a]"; done
} > "$HEPH_TEST_OUT"
if [ "$HEPH_TEST_MODE" = wait ]; then
  trap 'echo got-term > "$HEPH_TEST_OUT.term"; exit 7' TERM
  : > "$HEPH_TEST_OUT.ready"
  while :; do sleep 0.05; done
fi
exit 42
"#;

#[async_trait::async_trait]
impl heph::engine::driver_managed::ManagedDriver for ForegroundTool {
    fn config(
        &self,
        _req: hplugin::driver::ConfigRequest,
    ) -> anyhow::Result<hplugin::driver::ConfigResponse> {
        Ok(hplugin::driver::ConfigResponse {
            name: "fgtool".to_string(),
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
        Ok(hplugin::driver::ParseResponse {
            target_def: hplugin::driver::targetdef::TargetDef {
                addr: req.target_spec.addr.clone(),
                entrypoint: Some(hplugin::driver::targetdef::Entrypoint::new(
                    true,
                    hplugin::driver::targetdef::Workdir::Caller,
                )),
                hash: b"fgtool".to_vec(),
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
        req: heph::engine::driver_managed::ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<heph::engine::driver_managed::ManagedRunResponse> {
        use hplugin_sdk::foreground::{PathPolicy, RunnerRef, spawn_foreground};
        let options = req.request.options;
        let hplugin::driver::RunStdio::Inherit(fg) = options.stdio else {
            anyhow::bail!("an entrypoint run is in the caller's terminal");
        };
        let mut args: Vec<OsString> = vec!["-c".into(), FOREGROUND_TOOL_SCRIPT.into(), "sh".into()];
        args.extend(options.args.unwrap_or_default());
        let cmd = hplugin_sdk::foreground::ForegroundCommand::new(
            "/bin/sh",
            args,
            options.base_env.unwrap_or_default(),
            options.cwd.unwrap_or(req.sandbox_pkg_dir),
        )
        .with_dispositions(fg.dispositions);
        let (child, _) =
            spawn_foreground(RunnerRef::local(), cmd, &PathPolicy::default(), ctoken).await?;
        let exit = child
            .context("spawn the tool")?
            .wait(fg.signals, ctoken)
            .await
            .context("wait for the tool")?;
        if exit == Exit::Code(0) {
            return Ok(heph::engine::driver_managed::ManagedRunResponse { artifacts: vec![] });
        }
        Err(hplugin::error::ProcessFailed {
            status: format!("{exit:?}"),
            log: Arc::new(hcore::hartifactcontent::FileContent::new("/dev/null")),
            exit: Some(exit),
        }
        .into())
    }
}

/// `cdylib_entrypoint_runs`, across the stable-ABI seam in-process: a guest
/// driver behind the SDK (host capability bit set by `CreateConfig`, as the
/// loader does) declares an entrypoint and spawns its tool through
/// `hplugin_sdk::foreground`, and the real `Engine::invoke` drives it. The tool
/// runs in the caller's cwd with the caller's env plus `HEPH_ROOT` and the args
/// byte for byte; its exit code is the outcome; and a SIGTERM pushed into the
/// request's signal channel crosses the `RunInFrame.signal` relay, reaches the
/// tool's trap, and its chosen exit code comes back (E19, R5, N8).
///
/// What this does not cover, against a real `dlopen` of a cdylib: the symbol
/// and stabby type-report checks (`heph_plugin_abi_semver`, `get_stabbied`),
/// the plugin's own copies of the SDK's statics and of `tracing`/the
/// supervisor sink (here they are the test binary's, shared with the host), and
/// the cdylib runtime being a separate library instance. None of those carries
/// anything entrypoint-specific: the options, the signal stream and the exit
/// all ride the run's protobuf frames, which this exercises. A tty cannot be
/// asserted in-process — the tool inherits the test runner's fds; the terminal
/// is `crates/bin-e2e`'s (`entrypoint_pty`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cdylib_entrypoint_runs() -> anyhow::Result<()> {
    let ws = WorkspaceBuilder::new()?
        .with_provider(|init| {
            Box::new(pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
                init.runtime.clone(),
                Arc::clone(&init.functions),
            ))
        })
        .with_plugin("fgtool", |init| {
            behind_the_seam(&init.root, "fgtool", ForegroundTool)
        })
        .build()?;
    ws.write_build_file("app", r#"target(name = "tool", driver = "fgtool")"#);
    // `HEPH_ROOT` is the engine's root as configured, not canonicalized.
    let root = ws.dir.path();
    let caller = tempfile::tempdir()?;
    let cwd = caller.path().canonicalize()?;

    // Ran to completion: what it saw, and its exit code.
    let out = cwd.join("out");
    let exit = exited(
        invoke(
            &ws.engine,
            request("//app:tool", &cwd, &["a b", "c"], &out)?,
        )
        .await,
    );
    assert_eq!(exit, Exit::Code(42), "the tool's exit code is the outcome");
    assert_eq!(
        read(&out)?,
        format!(
            "cwd={}\ncaller=from-caller\nroot={}\narg=[a b]\narg=[c]\n",
            cwd.display(),
            root.display()
        ),
    );
    assert!(
        invoke_sandboxes(&ws.engine, "app", "tool").is_empty(),
        "the sandbox is gone"
    );

    // A signal to heph, relayed: the tool's TERM trap answers with exit 7.
    let out = cwd.join("waits");
    let mut env = caller_env(&out);
    env.push(("HEPH_TEST_MODE".into(), "wait".into()));
    let (tx, rx) = hplugin::driver::signal_channel();
    let req = InvokeRequest::new(parse_addr("//app:tool")?, &cwd, env).with_signals(rx);
    let engine = Arc::clone(&ws.engine);
    let run = tokio::spawn(async move { invoke(&engine, req).await });
    let ready = cwd.join("waits.ready");
    wait_for("the tool to set its trap", || ready.exists()).await?;
    // SIGTERM is 15 on every supported target.
    tx.send(hplugin::driver::Signal::from_raw(15))
        .context("the relay is open while the tool runs")?;
    let outcome = tokio::time::timeout(Duration::from_secs(120), run)
        .await
        .context("the tool exits once TERM reaches it")??;
    assert_eq!(exited(outcome), Exit::Code(7), "the trap's exit code");
    assert_eq!(read(&cwd.join("waits.term"))?, "got-term\n");
    Ok(())
}
