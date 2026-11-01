//! `RunOptions` honoured by `run_inner`, and `--shell`'s args mode.
//!
//! Nothing here builds an "entrypoint": the options are generic, and these
//! tests drive `run`/`run_shell` with a `RunRequest` built through
//! `RunRequest::new`, the way the engine does.

use super::*;
use hplugin::driver::{CredentialMount, Foreground, RunOptions, RunStdio, Signal};
use std::ffi::OsStr;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

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

    // Success: the options apply, and the log artifact is an empty file.
    let (_tx, stdio) = inherit();
    let (ran, _sbx) = run_opts(
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
    let [log] = res.artifacts.as_slice() else {
        panic!("one log artifact: {:?}", res.artifacts.len());
    };
    let outputartifact::Content::File(file) = &log.content else {
        panic!("a file artifact");
    };
    assert_eq!(std::fs::metadata(file.source_path.as_str())?.len(), 0);

    // A code and a signal come back as data, with an empty log.
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
        let mut log = String::new();
        std::io::Read::read_to_string(&mut pf.log.reader()?, &mut log)?;
        assert!(log.is_empty(), "{script}: {log:?}");
        assert_eq!(pf.log.file_path(), None, "{script}: never a sandbox handle");
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

/// Source `init` with `args` as the positional parameters, as the interactive
/// shell's rcfile sees them, then run `snippet`.
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

/// `args: None` renders `init.sh` byte for byte as before the option existed,
/// `runN` included, for exec and bash (E11): the digests are the render's
/// before args mode existed.
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
        "50a976451839979a"
    );
    assert_eq!(
        digest(&render_shell_init(&[]).expect("render")),
        "212c40f66956e0eb"
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

        // The real shape: an interactive bash reading the file as its rcfile,
        // the args as its positional parameters, the commands on stdin, and a
        // multi-line PROMPT_COMMAND of the caller's restored after the first
        // prompt. Its own status is the session's.
        let argv = args_mode_init(dir.path());
        let (program, rest) = argv.split_first().expect("argv");
        assert_eq!(program, "bash");
        let given = os(&[b"a b", b"", b"-i"]);
        let mut child = std::process::Command::new(bash)
            .args(rest)
            .args(&given)
            .env_clear()
            .env("PROMPT_COMMAND", "__caller_pc=1\n__caller_pc2=2")
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
