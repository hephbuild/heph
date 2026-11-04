//! `RunOptions` honoured by `run_inner`, and `--shell`'s args mode.
//!
//! Nothing here builds an "entrypoint": the options are generic, and these
//! tests drive `run`/`run_shell` with a `RunRequest` built through
//! `RunRequest::new`, the way the engine does.

use super::*;
use hplugin::driver::{CredentialMount, Foreground, RunOptions, RunStdio, Signal};
use std::ffi::OsStr;

/// A sandbox laid out as the managed layer lays it out:
/// `<root>/sbx`, `<root>/sbx/ws`, `<root>/sbx/ws/pkg`.
struct Sandbox {
    _root: tempfile::TempDir,
    dir: PathBuf,
    ws: PathBuf,
    pkg: PathBuf,
}

impl Sandbox {
    fn new() -> anyhow::Result<Self> {
        let root = tempfile::tempdir()?;
        let dir = root.path().join("sbx");
        let ws = dir.join("ws");
        let pkg = ws.join("pkg");
        std::fs::create_dir_all(&pkg)?;
        std::fs::create_dir_all(dir.join("list"))?;
        Ok(Self {
            _root: root,
            dir,
            ws,
            pkg,
        })
    }
}

fn exec_def(run: &[&str]) -> EngineTargetDef {
    def_with(run, BTreeMap::new())
}

fn def_with(run: &[&str], env: BTreeMap<String, D<String>>) -> EngineTargetDef {
    EngineTargetDef {
        raw_def: Arc::new(TargetDef {
            run: run.iter().map(|s| D::new((*s).to_string())).collect(),
            env,
            ..Default::default()
        }),
        pty: true,
        ..Default::default()
    }
}

/// `sh -c <script>`: what each row runs, by absolute path so no row depends
/// on the `PATH` it is testing.
fn sh(script: &str) -> EngineTargetDef {
    exec_def(&["/bin/sh", "-c", script, "sh"])
}

fn kv(k: &str, v: &str) -> (OsString, OsString) {
    (OsString::from(k), OsString::from(v))
}

struct Ran {
    result: anyhow::Result<ManagedRunResponse>,
    stdout: Vec<u8>,
}

impl Ran {
    fn out(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    fn ok(&self) -> String {
        if let Err(e) = &self.result {
            panic!("run failed: {e:#}");
        }
        self.out()
    }

    fn err(&self) -> String {
        match &self.result {
            Ok(_) => panic!("run must fail; stdout: {}", self.out()),
            Err(e) => format!("{e:#}"),
        }
    }
}

async fn run_with(
    driver: &Driver,
    def: &EngineTargetDef,
    sbx: &Sandbox,
    options: RunOptions,
    inputs: Vec<hdriver_support::driver_managed::ManagedRunInput>,
    credentials: Vec<CredentialMount>,
) -> Ran {
    let request_id = "run-options".to_string();
    let mut stdout = Vec::new();
    let mut req = RunRequest::new(&request_id, def, &sbx.dir);
    req.options = options;
    req.credentials = credentials;
    if matches!(req.options.stdio, RunStdio::Captured) {
        req.stdout = Some(&mut stdout);
    }
    let result = driver
        .run(
            ManagedRunRequest {
                request: req,
                sandbox_dir: sbx.dir.clone(),
                sandbox_ws_dir: sbx.ws.clone(),
                sandbox_pkg_dir: sbx.pkg.clone(),
                inputs,
            },
            &StdCancellationToken::new(),
        )
        .await;
    Ran { result, stdout }
}

async fn run_opts(driver: &Driver, def: &EngineTargetDef, options: RunOptions) -> (Ran, Sandbox) {
    let sbx = Sandbox::new().expect("sandbox");
    let ran = run_with(driver, def, &sbx, options, vec![], vec![]).await;
    (ran, sbx)
}

fn canonical(p: &std::path::Path) -> String {
    std::fs::canonicalize(p)
        .expect("canonicalize")
        .to_string_lossy()
        .into_owned()
}

/// The size of one environment entry no `execve` accepts: past `ARG_MAX` on
/// macOS (a fixed 1 MiB), and past `MAX_ARG_STRLEN` (128 KiB per string) on
/// Linux whatever its stack-derived `ARG_MAX`.
fn too_big() -> usize {
    // SAFETY: no preconditions; reads a system constant.
    let arg_max = unsafe { libc::sysconf(libc::_SC_ARG_MAX) };
    usize::try_from(arg_max)
        .ok()
        .filter(|&n| n > 0 && n <= 4 << 20)
        .map_or(4 << 20, |n| n + 1)
}

fn inherit() -> (hplugin::driver::SignalSender, RunStdio) {
    let (tx, rx) = hplugin::driver::signal_channel();
    (tx, RunStdio::Inherit(Foreground::new(rx, vec![])))
}

/// Every row of the spec's `exec_run_options_table` and R23's additions that
/// has an in-process form. Closed fds 0–2 is `foreground_spawn_keeps_closed_fds_closed`
/// in execrunner, which re-executes its test binary to close them safely.
#[tokio::test]
async fn exec_run_options_table() -> anyhow::Result<()> {
    let exec = Driver::new_exec().with_host_path();
    let elsewhere = tempfile::tempdir()?;

    // cwd: an override, outside the workspace and the sandbox.
    let (ran, sbx) = run_opts(
        &exec,
        &sh("pwd -P"),
        RunOptions::default().with_cwd(elsewhere.path()),
    )
    .await;
    assert_eq!(ran.ok().trim(), canonical(elsewhere.path()), "cwd override");
    assert!(
        !ran.out().starts_with(&canonical(&sbx.dir)),
        "outside the sandbox"
    );

    // cwd: none given is the sandbox package dir, as for any build.
    let (ran, sbx) = run_opts(&exec, &sh("pwd -P"), RunOptions::default()).await;
    assert_eq!(ran.ok().trim(), canonical(&sbx.pkg), "default cwd");

    // cwd: through a symlink.
    let link = elsewhere.path().join("link");
    let target = elsewhere.path().join("target");
    std::fs::create_dir(&target)?;
    std::os::unix::fs::symlink(&target, &link)?;
    let (ran, _sbx) = run_opts(&exec, &sh("pwd -P"), RunOptions::default().with_cwd(&link)).await;
    assert_eq!(ran.ok().trim(), canonical(&target), "symlinked cwd");

    // cwd: a non-UTF-8 name. APFS refuses one (EILSEQ), so the row only runs
    // where the filesystem can hold it.
    let raw = elsewhere.path().join(OsStr::from_bytes(b"d\xff"));
    match std::fs::create_dir(&raw) {
        Ok(()) => {
            let (ran, _sbx) =
                run_opts(&exec, &sh("pwd -P"), RunOptions::default().with_cwd(&raw)).await;
            assert!(ran.result.is_ok(), "non-UTF-8 cwd: {:?}", ran.result.err());
            assert!(
                ran.stdout.ends_with(b"d\xff\n"),
                "non-UTF-8 cwd: {:?}",
                ran.stdout
            );
        }
        Err(e) => eprintln!("skipping the non-UTF-8 cwd row: {e}"),
    }

    // cwd: deleted before the run is named as such, not as a missing program.
    let gone = elsewhere.path().join("gone");
    std::fs::create_dir(&gone)?;
    std::fs::remove_dir(&gone)?;
    let (ran, _sbx) = run_opts(&exec, &sh("true"), RunOptions::default().with_cwd(&gone)).await;
    let err = ran.err();
    assert!(err.contains("does not exist"), "deleted cwd: {err}");
    assert!(err.contains("gone"), "deleted cwd names it: {err}");

    // base env: None keeps the driver's own PATH; empty is an explicitly empty
    // base, so nothing supplies one. Read by `env`, not a shell, which would
    // invent a default `PATH` of its own.
    let env = exec_def(&["/usr/bin/env"]);
    let path_of = |out: String| -> Option<String> {
        out.lines()
            .find_map(|l| l.strip_prefix("PATH="))
            .map(str::to_string)
    };
    let (ran, _sbx) = run_opts(&exec, &env, RunOptions::default()).await;
    assert_eq!(
        path_of(ran.ok()).as_deref(),
        Some("/usr/local/bin:/usr/bin:/bin"),
        "base_env None"
    );
    let (ran, _sbx) = run_opts(&exec, &env, RunOptions::default().with_base_env(vec![])).await;
    assert_eq!(path_of(ran.ok()), None, "base_env empty");
    let path = sh("printf %s \"${PATH-unset}\"");
    let (ran, _sbx) = run_opts(
        &exec,
        &path,
        RunOptions::default().with_base_env(vec![kv("PATH", "/caller/bin:/caller/sbin")]),
    )
    .await;
    assert_eq!(ran.ok(), "/caller/bin:/caller/sbin", "PATH from base_env");

    // base env: a duplicate key resolves to the later value.
    let (ran, _sbx) = run_opts(
        &exec,
        &sh("printf %s \"$A\""),
        RunOptions::default().with_base_env(vec![kv("A", "1"), kv("A", "2")]),
    )
    .await;
    assert_eq!(ran.ok(), "2", "duplicate keys");

    // base env: values reach the process byte for byte, non-UTF-8 included.
    let (ran, _sbx) = run_opts(
        &exec,
        &sh("printf %s \"$RAW\""),
        RunOptions::default().with_base_env(vec![(
            OsString::from("RAW"),
            OsString::from_vec(b"a\xffb".to_vec()),
        )]),
    )
    .await;
    assert!(ran.result.is_ok(), "{:?}", ran.result.err());
    assert_eq!(ran.stdout, b"a\xffb", "non-UTF-8 value");

    // base env: the run's own layers sit on top of the caller's.
    let (ran, sbx) = run_opts(
        &exec,
        &sh("printf %s \"$WORKSPACE_ROOT\""),
        RunOptions::default().with_base_env(vec![kv("WORKSPACE_ROOT", "/caller")]),
    )
    .await;
    assert_eq!(ran.ok(), sbx.ws.to_string_lossy(), "caller WORKSPACE_ROOT");
    let (ran, _sbx) = run_opts(
        &exec,
        &def_with(
            &["/bin/sh", "-c", "printf %s \"$FOO\""],
            BTreeMap::from([("FOO".to_string(), D::new("target".to_string()))]),
        ),
        RunOptions::default().with_base_env(vec![kv("FOO", "caller")]),
    )
    .await;
    assert_eq!(ran.ok(), "target", "the target's env over the caller's");

    // R17: the engine puts the real root in the base env as `HEPH_ROOT`; exec
    // passes it through like any caller variable.
    let (ran, _sbx) = run_opts(
        &exec,
        &sh("printf %s \"$HEPH_ROOT\""),
        RunOptions::default().with_base_env(vec![kv("HEPH_ROOT", "/real/root")]),
    )
    .await;
    assert_eq!(ran.ok(), "/real/root", "HEPH_ROOT");

    // A credential's presentation over the caller's own variable.
    let sbx = Sandbox::new()?;
    let ran = run_with(
        &exec,
        &sh("printf %s \"$GH_TOKEN\""),
        &sbx,
        RunOptions::default().with_base_env(vec![kv("GH_TOKEN", "caller")]),
        vec![],
        vec![CredentialMount {
            addr: Addr::default(),
            env: BTreeMap::from([("GH_TOKEN".to_string(), "credential".to_string())]),
            path_prefix: vec![],
            redact: vec![],
        }],
    )
    .await;
    assert_eq!(ran.ok(), "credential", "credential over caller");

    // PATH: the target's tools first, then the caller's, then the builtins.
    let coreutils = tempfile::tempdir()?;
    let coreutils_dir = coreutils.path().to_path_buf();
    let with_toolbox = Driver::new_exec()
        .with_coreutils_enabled_for_test()
        .with_coreutils(1, Arc::new(move || Ok(coreutils_dir.clone())));
    let sbx = Sandbox::new()?;
    let tool = make_tool_binary(elsewhere.path(), "mytool", "printf %s \"$PATH\"")?;
    let input = make_tool_managed_input("tool||0", &tool, &sbx.dir.join("list"))?;
    let ran = run_with(
        &with_toolbox,
        &make_tool_target_def(vec!["mytool".to_string()], "tool||0", ""),
        &sbx,
        RunOptions::default().with_base_env(vec![kv("PATH", "/caller/bin")]),
        vec![input],
        vec![],
    )
    .await;
    assert_eq!(
        ran.ok(),
        format!(
            "{}:/caller/bin:{}",
            sbx.dir.join("bin").display(),
            coreutils.path().display()
        ),
        "bin/ first, the caller's PATH, builtins last"
    );

    // args: appended to exec's argv, byte for byte.
    let each = sh("for a in \"$@\"; do printf '<%s>' \"$a\"; done");
    let args = |v: &[&[u8]]| -> Vec<OsString> {
        v.iter().map(|a| OsString::from_vec(a.to_vec())).collect()
    };
    for (row, given, want) in [
        ("none given", args(&[]), b"".to_vec()),
        ("empty", args(&[b""]), b"<>".to_vec()),
        ("spaces", args(&[b"a b", b" "]), b"<a b>< >".to_vec()),
        ("leading dash", args(&[b"-x", b"--"]), b"<-x><-->".to_vec()),
        (
            "non-ASCII",
            args(&["é".as_bytes()]),
            "<é>".as_bytes().to_vec(),
        ),
        ("non-UTF-8", args(&[b"\xff"]), b"<\xff>".to_vec()),
        (
            "shell syntax",
            args(&[b"$HOME", b"`id`"]),
            b"<$HOME><`id`>".to_vec(),
        ),
    ] {
        let (ran, _sbx) = run_opts(&exec, &each, RunOptions::default().with_args(given)).await;
        assert!(ran.result.is_ok(), "{row}: {:?}", ran.result.err());
        assert_eq!(ran.stdout, want, "args: {row}");
    }

    // bash: the args are "$@", with `bash` as $0, for an inline script and
    // for one long enough to be spilled to a file (its own $0).
    let bash = Driver::new_bash().with_host_path();
    let given = args(&[b"a b", b"", b"$HOME"]);
    let short = exec_def(&["printf '<%s>' \"$@\"", "printf '[%s]' \"$#\""]);
    let (ran, _sbx) = run_opts(
        &bash,
        &short,
        RunOptions::default().with_args(given.clone()),
    )
    .await;
    assert_eq!(ran.ok(), "<a b><><$HOME>[3]", "bash \"$@\"");
    let padding = format!(": '{}'", "x".repeat(BASH_C_INLINE_MAX));
    let long = exec_def(&[
        padding.as_str(),
        "printf '<%s>' \"$@\"",
        "printf '[%s]' \"$#\"",
    ]);
    let (ran, _sbx) = run_opts(&bash, &long, RunOptions::default().with_args(given)).await;
    assert_eq!(ran.ok(), "<a b><><$HOME>[3]", "bash \"$@\", spilled script");
    let (ran, _sbx) = run_opts(&bash, &short, RunOptions::default().with_args(vec![])).await;
    assert_eq!(ran.ok(), "<>[0]", "bash with no args given");

    // E2BIG from the environment and from argv is a named error, captured or
    // inherited.
    let big = "x".repeat(too_big());
    for (row, options) in [
        (
            "env, captured",
            RunOptions::default().with_base_env(vec![kv("BIG", &big)]),
        ),
        (
            "argv, captured",
            RunOptions::default().with_args(vec![OsString::from(&big)]),
        ),
        (
            "env, inherited",
            RunOptions::default()
                .with_base_env(vec![kv("BIG", &big)])
                .with_stdio(inherit().1),
        ),
        (
            "argv, inherited",
            RunOptions::default()
                .with_args(vec![OsString::from(&big)])
                .with_stdio(inherit().1),
        ),
    ] {
        let (ran, _sbx) = run_opts(&exec, &sh("true"), options).await;
        let err = ran.err();
        assert!(err.contains("E2BIG"), "{row}: {err}");
        assert!(err.contains("bytes of environment"), "{row}: {err}");
    }
    Ok(())
}

/// The caller's terminal: inherited stdio, the structured exit, an empty log
/// that is never a handle into the sandbox (R22), and the relay.
#[tokio::test]
async fn exec_run_inherited_stdio() -> anyhow::Result<()> {
    let exec = Driver::new_exec().with_host_path();
    let out = tempfile::tempdir()?;
    let report = out.path().join("report");
    let base = vec![
        kv("REPORT", &report.to_string_lossy()),
        kv("A", "from-caller"),
    ];

    // Success: the options apply, and nothing was teed, so there is no log: no
    // file in the sandbox and no artifact.
    let (_tx, stdio) = inherit();
    let (ran, sbx) = run_opts(
        &exec,
        &sh("{ pwd -P; printf %s \"$A\"; } > \"$REPORT\""),
        RunOptions::default()
            .with_cwd(out.path())
            .with_base_env(base.clone())
            .with_stdio(stdio),
    )
    .await;
    let res = ran.result.as_ref().map_err(|e| anyhow::anyhow!("{e:#}"))?;
    assert_eq!(
        std::fs::read_to_string(&report)?,
        format!("{}\nfrom-caller", canonical(out.path()))
    );
    assert_eq!(res.artifacts.len(), 0, "no log artifact");
    assert!(
        !sbx.dir.join("log.txt").exists(),
        "an inherited run has no log file"
    );

    // A code and a signal come back as data, with no log.
    for (script, want) in [
        ("exit 3", hplugin::error::Exit::Code(3)),
        ("kill -TERM $$", hplugin::error::Exit::Signal(libc::SIGTERM)),
    ] {
        let (_tx, stdio) = inherit();
        let (ran, _sbx) =
            run_opts(&exec, &sh(script), RunOptions::default().with_stdio(stdio)).await;
        let err = ran.result.err().expect("must fail");
        let pf = err
            .downcast_ref::<hplugin::error::ProcessFailed>()
            .expect("a ProcessFailed");
        assert_eq!(pf.exit, Some(want), "{script}");
        assert!(
            hplugin::error::failure_artifacts(&err).is_none(),
            "{script}: never a sandbox handle"
        );
    }

    // A signal relayed before the spawn is delivered once the tool exists.
    let (tx, stdio) = inherit();
    tx.send(Signal::from_raw(libc::SIGTERM))?;
    let (ran, _sbx) = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        run_opts(
            &exec,
            &sh("exec sleep 30"),
            RunOptions::default().with_stdio(stdio),
        ),
    )
    .await?;
    let err = ran.result.err().expect("must fail");
    assert_eq!(
        hplugin::error::process_exit(&err),
        Some(hplugin::error::Exit::Signal(libc::SIGTERM))
    );

    // A missing program on the caller's PATH says where it looked.
    let (_tx, stdio) = inherit();
    let (ran, _sbx) = run_opts(
        &exec,
        &exec_def(&["heph-no-such-tool"]),
        RunOptions::default()
            .with_base_env(vec![kv("PATH", "/nonexistent")])
            .with_stdio(stdio),
    )
    .await;
    let err = ran.err();
    assert!(err.contains("the caller's PATH"), "{err}");
    Ok(())
}

// ---- `--shell` ----

/// Every bash we can find: the one on `PATH`, and macOS's `/bin/bash` 3.2,
/// where `set -u` trips on an empty array. A Linux CI host has one version;
/// the 3.2 rows run where 3.2 is installed.
fn bashes() -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for b in ["bash", "/bin/bash"] {
        if let Ok(p) = which::which(b) {
            let p = std::fs::canonicalize(&p)
                .unwrap_or(p)
                .to_string_lossy()
                .into_owned();
            if !found.contains(&p) {
                found.push(p);
            }
        }
    }
    assert!(!found.is_empty(), "no bash to test with");
    found
}

/// Source `init` with `args` as the positional parameters, then run `snippet`.
fn in_shell(
    bash: &str,
    init: &std::path::Path,
    args: &[&OsStr],
    snippet: &str,
) -> (Vec<u8>, String) {
    let out = std::process::Command::new(bash)
        .args(["--noprofile", "--norc", "-c"])
        .arg(format!(". \"$0\" \"$@\" >/dev/null 2>&1; {snippet}"))
        .arg(init)
        .args(args)
        .env_clear()
        .output()
        .expect("run bash");
    (
        out.stdout,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn os(v: &[&[u8]]) -> Vec<OsString> {
    v.iter().map(|a| OsString::from_vec(a.to_vec())).collect()
}

fn refs(v: &[OsString]) -> Vec<&OsStr> {
    v.iter().map(OsString::as_os_str).collect()
}

/// Args mode's `init.sh` for an exec target running `printf <%s>`, written
/// where `--shell` writes it; returns the argv the shell is started with.
fn args_mode_init(dir: &std::path::Path) -> Vec<String> {
    Driver::new_exec()
        .args_mode_shell(dir, &["printf".to_string(), "<%s>".to_string()], true)
        .expect("args mode")
}

/// `args: None` renders the same `init.sh` through each driver's wrapper,
/// `runN` included, for exec and bash (E11), pinned by digest.
#[test]
fn shell_init_without_args_is_unchanged() {
    let digest = |text: &str| {
        let mut h = Xxh3Default::new();
        std::hash::Hasher::write(&mut h, text.as_bytes());
        format!("{:016x}", std::hash::Hasher::finish(&h))
    };
    let run = [
        "echo 'it''s' \"$HOME\"".to_string(),
        "ls -la\nmore".to_string(),
    ];
    assert_eq!(
        digest(&render_shell_init(&run).expect("render")),
        "14a552b786b826c4"
    );
    assert_eq!(
        digest(&render_shell_init(&[]).expect("render")),
        "d43dffadc812866d"
    );

    // What `run_shell` writes without args: the same render, through each
    // driver's own shell wrapper.
    for driver in [Driver::new_exec(), Driver::new_bash()] {
        let dir = tempfile::tempdir().expect("tempdir");
        let argv = (driver.wrap_run_shell)(dir.path(), &run).expect("wrap");
        assert!(
            !argv.iter().any(|a| a == "-s"),
            "no positional params: {argv:?}"
        );
        let written = std::fs::read_to_string(dir.path().join("init.sh")).expect("init.sh");
        let joined = [run.join(" ")];
        let want = if driver.wraps_in_shell {
            render_shell_init(&run)
        } else {
            render_shell_init(&joined)
        }
        .expect("render");
        assert_eq!(written, want, "{}", driver.name);
        assert!(written.contains("run1()"), "{}: runN kept", driver.name);
    }
}

/// `Some(vec![])` is args mode (E11): no `runN`/`xrunN`, a bare `run` runs the
/// command as is, `run x` overrides for that call.
#[test]
fn shell_init_accepts_args_with_none_given() {
    let dir = tempfile::tempdir().expect("tempdir");
    let argv = args_mode_init(dir.path());
    assert_eq!(argv.last().map(String::as_str), Some("--"), "{argv:?}");
    assert!(argv.iter().any(|a| a == "-s"), "{argv:?}");
    let init = dir.path().join("init.sh");
    for bash in &bashes() {
        for (snippet, want) in [
            ("run", "<>"),
            ("run x", "<x>"),
            ("run x; run", "<x><>"),
            (
                "for f in run1 xrun1; do declare -F \"$f\" >/dev/null && echo \"$f\"; done; echo none",
                "none\n",
            ),
        ] {
            let (out, err) = in_shell(bash, &init, &[], snippet);
            assert_eq!(
                String::from_utf8_lossy(&out),
                want,
                "{bash}: `{snippet}`; {err}"
            );
        }
    }
}

/// R23's rows as a round-trip property: whatever the args, `run` hands the
/// command exactly them, and `eval` of what `show` prints reproduces them.
#[test]
fn shell_init_args_exact() {
    let dir = tempfile::tempdir().expect("tempdir");
    args_mode_init(dir.path());
    let init = dir.path().join("init.sh");
    let expect = |args: &[OsString]| -> Vec<u8> {
        let mut s = Vec::new();
        if args.is_empty() {
            s.extend_from_slice(b"<>");
        }
        for a in args {
            s.push(b'<');
            s.extend_from_slice(a.as_bytes());
            s.push(b'>');
        }
        s
    };
    let ten: Vec<&[u8]> = vec![
        b"1", b"2", b"3", b"4", b"5", b"6", b"7", b"8", b"9", b"10", b"11",
    ];
    let rows: Vec<Vec<OsString>> = vec![
        os(&[]),
        os(&[b""]),
        os(&[b" "]),
        os(&[b"a\nb"]),
        os(&[b"'", b"\"", b"it's \"q\""]),
        os(&[b"$HOME", b"`id`", b"$(id)", b"$10"]),
        os(&[b"-s", b"-i", b"--rcfile", b"--", b"-n"]),
        os(&["é".as_bytes(), "日本".as_bytes()]),
        os(&[b"\x01\x7f"]),
        os(&[b"\xff\xfe"]),
        os(&[b"", b"x", b""]),
        os(&ten),
    ];
    for bash in &bashes() {
        for args in &rows {
            for helper in ["run", "xrun"] {
                let (out, err) = in_shell(bash, &init, &refs(args), helper);
                assert_eq!(out, expect(args), "{bash} {helper} {args:?}; {err}");
            }
            // `show` quotes the args so that the shell reads them back.
            let (out, err) = in_shell(bash, &init, &refs(args), "eval \"$(show)\"");
            assert_eq!(out, expect(args), "{bash} eval show {args:?}; {err}");
            let (out, err) = in_shell(
                bash,
                &init,
                &refs(args),
                "__l=$(showl); eval \"${__l#1 │ }\"",
            );
            assert_eq!(out, expect(args), "{bash} eval showl {args:?}; {err}");
        }

        // A helper's own args replace the originals for that call only.
        let originals = os(&[b"orig1", b"orig 2"]);
        for (snippet, want) in [
            ("run a 'b c'", expect(&os(&[b"a", b"b c"]))),
            ("run ''", expect(&os(&[b""]))),
            ("xrun z", expect(&os(&[b"z"]))),
            (
                "run x; run",
                [expect(&os(&[b"x"])), expect(&originals)].concat(),
            ),
        ] {
            let (out, err) = in_shell(bash, &init, &refs(&originals), snippet);
            assert_eq!(out, want, "{bash}: `{snippet}`; {err}");
        }

        // `set -u` with no args at all: the empty array must not trip it.
        let (out, err) = in_shell(bash, &init, &[], "set -u; run; show");
        assert_eq!(
            String::from_utf8_lossy(&out),
            "<>'printf' '<%s>'\n",
            "{bash}: set -u, no args; {err}"
        );

        // The real shape: an interactive bash reading the file from
        // PROMPT_COMMAND, the args as its positional parameters, the commands
        // on stdin, and a multi-line PROMPT_COMMAND of the caller's restored.
        // Its own status is the session's.
        let argv = args_mode_init(dir.path());
        let (program, rest) = argv.split_first().expect("argv");
        assert_eq!(program, "bash");
        let given = os(&[b"a b", b"", b"-i"]);
        let mut child = std::process::Command::new(bash)
            .args(rest)
            .args(&given)
            .env_clear()
            .envs(shell_env(
                &[("PROMPT_COMMAND", "__caller_pc=1\n__caller_pc2=2")],
                &init,
            ))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn interactive bash");
        {
            use std::io::Write as _;
            let mut stdin = child.stdin.take().expect("stdin");
            stdin
                .write_all(
                    b"echo; echo BEGIN; run; echo; printf '%s\\n' \"$PROMPT_COMMAND\"; exit 3\n",
                )
                .expect("type");
        }
        let done = child.wait_with_output().expect("wait");
        let stdout = String::from_utf8_lossy(&done.stdout);
        let after = stdout.split("BEGIN\n").nth(1).unwrap_or_default();
        assert_eq!(
            after,
            format!(
                "{}\n__caller_pc=1\n__caller_pc2=2\n",
                String::from_utf8_lossy(&expect(&given))
            ),
            "{bash}: {stdout}"
        );
        assert_eq!(
            done.status.code(),
            Some(3),
            "{bash}: `exit 3` is the session's status"
        );
    }
}

/// `caller` as the finished environment, with what only a `--shell` adds.
fn shell_env(caller: &[(&str, &str)], init: &std::path::Path) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = caller.iter().map(|(k, v)| kv(k, v)).collect();
    add_shell_env(&mut env, init);
    env
}

/// Everything an interactive `bash` with `argv` printed, given one typed line.
/// Returns stdout, stderr and the exit code.
fn interactive(
    bash: &str,
    argv: &[String],
    args: &[&str],
    env: &[(OsString, OsString)],
    typed: &str,
) -> (String, String, Option<i32>) {
    use std::io::Write as _;
    let (_, rest) = argv.split_first().expect("argv");
    let mut child = std::process::Command::new(bash)
        .args(rest)
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        // `showl` needs a `cat`, and a nix bash's default `PATH` has none.
        .env("PATH", "/usr/bin:/bin")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn interactive bash");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(format!("{typed}\n").as_bytes())
        .expect("type");
    let done = child.wait_with_output().expect("wait");
    (
        String::from_utf8_lossy(&done.stdout).into_owned(),
        String::from_utf8_lossy(&done.stderr).into_owned(),
        done.status.code(),
    )
}

/// What a line typed at the prompt reports about the variables `init.sh` owns:
/// their values in the shell, then how many of them a child inherits.
const REPORT: &str = "echo BEGIN; printf '[%s][%s]\\n' \"${PROMPT_COMMAND-unset}\" \
    \"${__HEPH_PROMPT_COMMAND-unset}\"; /usr/bin/env | /usr/bin/grep -c PROMPT_COMMAND; exit 3";

/// `--shell` reads no bash config at all, in every mode: `--norc` is what keeps
/// a Debian bash's compiled-in `/etc/bash.bashrc` out, which `--rcfile` does not.
#[test]
fn shell_argv_reads_no_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let run = ["true".to_string()];
    let argvs = [
        (Driver::new_exec().wrap_run_shell)(dir.path(), &run).expect("exec"),
        (Driver::new_bash().wrap_run_shell)(dir.path(), &run).expect("bash"),
        args_mode_init(dir.path()),
    ];
    for argv in &argvs {
        let at = |flag: &str| argv.iter().position(|a| a == flag);
        let interactive = at("-i").unwrap_or_else(|| panic!("no -i: {argv:?}"));
        for flag in ["--norc", "--noprofile"] {
            // A long option after a short one is not read as an option at all.
            assert!(
                at(flag).is_some_and(|i| i < interactive),
                "{flag} ahead of -i: {argv:?}"
            );
        }
        assert_eq!(at("--rcfile"), None, "{argv:?}");
        assert!(
            !argv.iter().any(|a| a.contains("init.sh")),
            "init.sh travels in the environment: {argv:?}"
        );
    }
    assert!(dir.path().join("init.sh").is_file());
}

/// The init path reaches `PROMPT_COMMAND` exactly, quoted for bash byte by byte,
/// and the caller's value moves aside rather than being lost.
#[test]
fn shell_env_carries_the_init_path_and_the_callers_prompt_command() {
    let get = |env: &[(OsString, OsString)], key: &str| {
        env.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_bytes().to_vec())
    };

    let init = std::path::Path::new(OsStr::from_bytes(b"/sbx/a b'c\xff/init.sh"));
    let env = shell_env(&[("KEEP", "1"), ("PROMPT_COMMAND", "mine")], init);
    let pc = get(&env, "PROMPT_COMMAND").expect("PROMPT_COMMAND");
    let want: &[u8] = b". '/sbx/a b'\\''c\xff/init.sh' || ";
    assert!(pc.starts_with(want), "{}", String::from_utf8_lossy(&pc));
    assert_eq!(
        get(&env, "__HEPH_PROMPT_COMMAND").as_deref(),
        Some(&b"mine"[..])
    );
    assert_eq!(get(&env, "KEEP").as_deref(), Some(&b"1"[..]));
    assert_eq!(
        env.iter().filter(|(k, _)| k == "PROMPT_COMMAND").count(),
        1,
        "one PROMPT_COMMAND: {env:?}"
    );

    let env = shell_env(&[], init);
    assert_eq!(
        get(&env, "__HEPH_PROMPT_COMMAND").as_deref(),
        Some(&b""[..])
    );
}

/// The whole mechanism in a real interactive bash, on each one we can find:
/// `init.sh` runs before the first prompt, once, in order, from a path needing
/// quoting. `"$@"` is the caller's, `PROMPT_COMMAND` is gone or the caller's
/// again, and the private variable reaches nothing.
#[test]
fn shell_init_runs_from_prompt_command() {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = root.path().join("a b'c");
    std::fs::create_dir(&dir).expect("mkdir");
    let init = dir.join("init.sh");
    for bash in &bashes() {
        for args_mode in [false, true] {
            let argv = if args_mode {
                args_mode_init(&dir)
            } else {
                (Driver::new_bash().wrap_run_shell)(&dir, &["echo the-command".to_string()])
                    .expect("wrap")
            };
            let args: &[&str] = if args_mode { &["a b", ""] } else { &[] };
            let what = format!("{bash} args_mode={args_mode}");

            let env = shell_env(&[], &init);
            let (out, err, code) = interactive(bash, &argv, args, &env, REPORT);
            let after = out.split("BEGIN\n").nth(1).unwrap_or_default();
            assert_eq!(after, "[unset][unset]\n0\n", "{what}: {out}\n{err}");
            assert_eq!(code, Some(3), "{what}: {err}");
            assert!(!err.contains("heph:"), "{what}: {err}");
            let order: Vec<_> = ["Shell mode, to exit", "=== Run ===", "1 │", "BEGIN"]
                .iter()
                .map(|needle| {
                    assert_eq!(out.matches(needle).count(), 1, "{what}: {needle}: {out}");
                    out.find(needle)
                })
                .collect();
            assert!(order.is_sorted(), "{what}: {order:?}\n{out}");

            // A caller's `PROMPT_COMMAND` is back, still exported as it was, and
            // has had the first prompt's turn: bash would only run it from the
            // second, and the typed line is read at the first.
            let env = shell_env(&[("PROMPT_COMMAND", "__c=x${__c-}")], &init);
            let (out, err, _) = interactive(
                bash,
                &argv,
                args,
                &env,
                &REPORT.replace("echo BEGIN", "echo BEGIN \"<${__c-}>\""),
            );
            let after = out.split("BEGIN ").nth(1).unwrap_or_default();
            assert_eq!(
                after, "<x>\n[__c=x${__c-}][unset]\n1\n",
                "{what}: {out}\n{err}"
            );

            if args_mode {
                let env = shell_env(&[], &init);
                let (out, err, _) = interactive(
                    bash,
                    &argv,
                    args,
                    &env,
                    "echo BEGIN; printf '<%s>' \"$@\"; echo; run; exit",
                );
                let after = out.split("BEGIN\n").nth(1).unwrap_or_default();
                assert_eq!(after, "<a b><>\n<a b><>", "{what}: {out}\n{err}");
            }
        }
    }
}

/// A source that fails says so once, and the session is still usable, with
/// `PROMPT_COMMAND` no longer heph's: both when the file was never read and when
/// it stopped partway (the command lines are the target's, and can fail to parse).
#[test]
fn shell_init_failure_says_so_and_clears_prompt_command() {
    const SAID: &str = "heph: the shell's setup failed to load";
    let dir = tempfile::tempdir().expect("tempdir");
    let init = dir.path().join("init.sh");
    let argv =
        (Driver::new_bash().wrap_run_shell)(dir.path(), &["if".to_string()]).expect("render");
    let missing = dir.path().join("missing.sh");
    for bash in &bashes() {
        for path in [&missing, &init] {
            for (caller, want) in [
                (&[][..], "[unset][unset]\n0\n"),
                (&[("PROMPT_COMMAND", ":")][..], "[:][unset]\n1\n"),
            ] {
                let what = format!("{bash} {path:?} {caller:?}");
                let env = shell_env(caller, path);
                let (out, err, code) = interactive(bash, &argv, &[], &env, REPORT);
                assert_eq!(err.matches(SAID).count(), 1, "{what}: {err}");
                let after = out.split("BEGIN\n").nth(1).unwrap_or_default();
                assert_eq!(after, want, "{what}: {out}\n{err}");
                assert_eq!(code, Some(3), "{what}: {err}");
            }
        }
    }
}

/// Only a `--shell` gets any of this: a plain run sees the caller's
/// `PROMPT_COMMAND` where it was, and none of the shell's variables.
#[tokio::test]
async fn shell_env_stays_out_of_a_plain_run() {
    let exec = Driver::new_exec();
    let def = sh(
        "printf '[%s][%s][%s]' \"${PROMPT_COMMAND-unset}\" \"${__HEPH_PROMPT_COMMAND-unset}\" \
         \"${BASH_SILENCE_DEPRECATION_WARNING-unset}\"",
    );
    let (ran, _sbx) = run_opts(&exec, &def, RunOptions::default()).await;
    assert_eq!(ran.ok(), "[unset][unset][unset]");
    let (ran, _sbx) = run_opts(
        &exec,
        &def,
        RunOptions::default().with_base_env(vec![kv("PROMPT_COMMAND", "mine")]),
    )
    .await;
    assert_eq!(ran.ok(), "[mine][unset][unset]");
}
