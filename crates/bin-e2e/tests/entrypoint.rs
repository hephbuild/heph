#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! `entrypoint = True` through the shipped binary: what only a real process can
//! show. The program's exit status becomes heph's, a signal death is re-raised
//! rather than mapped to `128 + n`, and stdin reaches the program untouched.
//!
//! Everything about the closure, caching and refusals is in-process, in
//! `crates/e2e/tests/entrypoint.rs`.

mod common;

use common::{Dist, Workspace, describe};
use std::io::Write as _;
use std::os::unix::process::ExitStatusExt as _;
use std::process::Stdio;
use std::time::Duration;

const BUILD: &str = r#"
target(name = "code", driver = "exec", entrypoint = True,
       run = ["/bin/sh", "-c", "exit \"$1\"", "sh"], args = True)
target(name = "cat", driver = "exec", entrypoint = True, run = ["/bin/cat"])
target(name = "die", driver = "exec", entrypoint = True,
       run = ["/bin/sh", "-c", "ulimit -c 0; kill -s \"$1\" $$", "sh"], args = True)
"#;

fn workspace() -> anyhow::Result<Workspace> {
    let ws = Workspace::new()?;
    ws.write("app/BUILD", BUILD)?;
    Ok(ws)
}

/// Run `cmd` to completion with both streams captured, bounded by
/// [`common::RUN_DEADLINE`].
fn output(mut cmd: std::process::Command) -> anyhow::Result<std::process::Output> {
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    common::wait_output(child, common::RUN_DEADLINE)
}

/// `heph <args>` started by `/bin/sh -c '<setup>; exec heph …'`: a caller
/// that changed its own state (a trap, a limit) before running heph, with the
/// workspace's environment.
fn via_shell(ws: &Workspace, dist: &Dist, setup: &str, args: &[&str]) -> std::process::Command {
    let envs: Vec<_> = ws
        .cmd(dist, &[])
        .get_envs()
        .filter_map(|(k, v)| v.map(|v| (k.to_os_string(), v.to_os_string())))
        .collect();
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.args(["-c", &format!("{setup}; exec \"$0\" \"$@\"")])
        .arg(dist.heph())
        .args(args)
        .current_dir(ws.root())
        .envs(envs);
    cmd
}

/// The program's exit code is heph's, unchanged.
#[test]
fn entrypoint_exit_code_passes_through() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    for code in [0, 1, 2, 42, 127, 255] {
        let out = ws.run(&dist, &["run", "//app:code", "--", &code.to_string()])?;
        assert_eq!(out.status.code(), Some(code), "{}", describe(&out));
    }
    Ok(())
}

/// Piped stdin — binary, every byte value — reaches the program exactly, with
/// its EOF; heph reads none of it.
#[test]
fn entrypoint_stdin_passes_through() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let input: Vec<u8> = (0..=255u8).cycle().take(256 * 64).collect();
    let mut child = ws
        .cmd(&dist, &["run", "//app:cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let writer = {
        let input = input.clone();
        std::thread::spawn(move || {
            stdin.write_all(&input)?;
            drop(stdin);
            std::io::Result::Ok(())
        })
    };
    let out = common::wait_output(child, common::RUN_DEADLINE)?;
    writer
        .join()
        .map_err(|_panic| anyhow::anyhow!("the stdin writer panicked"))??;
    assert!(out.status.success(), "{}", describe(&out));
    assert!(
        out.stdout == input,
        "stdout differs from stdin ({} vs {} bytes)",
        out.stdout.len(),
        input.len()
    );
    Ok(())
}

/// A program killed by a signal kills heph with the same signal — never
/// `exit 128 + n`, which a shell reads differently — and heph dumps no core of
/// its own even when the caller allows one.
#[test]
fn entrypoint_signal_death_is_reraised() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    for (name, number) in [
        ("INT", 2),
        ("QUIT", 3),
        ("ABRT", 6),
        ("KILL", 9),
        ("SEGV", 11),
        ("PIPE", 13),
        ("TERM", 15),
    ] {
        // heph under an unlimited core limit; the program lowers its own, so
        // only a heph core could appear.
        let out = output(via_shell(
            &ws,
            &dist,
            "ulimit -c \"$(ulimit -H -c)\"",
            &["run", "//app:die", "--", name],
        ))?;
        assert_eq!(
            out.status.signal(),
            Some(number),
            "SIG{name}: heph must die of it\n{}",
            describe(&out)
        );
        assert!(!out.status.core_dumped(), "SIG{name}: heph dumped core");
        assert_eq!(out.status.code(), None, "SIG{name}: not an exit code");
    }
    Ok(())
}

const SIGTERM: i32 = libc::SIGTERM;

/// The tool reports its SIGINT, SIGTTOU, SIGTTIN, SIGQUIT and SIGPIPE
/// dispositions. heph itself ignores TTOU/TTIN and PIPE (std, before `main`)
/// and handles INT and QUIT, so a child that merely inherited heph's state
/// would read `IGNORE IGNORE IGNORE DEFAULT IGNORE` (handlers reset to default
/// across exec, ignores do not) whatever the caller gave.
const DISPOSITIONS_BUILD: &str = r#"
target(name = "disp", driver = "exec", entrypoint = True,
       run = ["/usr/bin/perl", "-e",
              "print join(q( ), map { $SIG{$_} // q(DEFAULT) } qw(INT TTOU TTIN QUIT PIPE)), qq(\n)"])
"#;

/// The tool starts with the dispositions heph inherited, not heph's own (N4,
/// E9, R8): under a `nohup`-like caller SIGINT (and here TTOU/TTIN) stay
/// ignored, as does a SIGPIPE ignored with `trap '' PIPE` — recorded before
/// std's runtime ignores it, so X6 is no longer a limit; under a plain shell
/// every one is default, although heph ignores TTOU/TTIN and PIPE and handles
/// INT/QUIT itself.
#[test]
fn entrypoint_keeps_inherited_dispositions() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = Workspace::new()?;
    ws.write("app/BUILD", DISPOSITIONS_BUILD)?;

    let out = ws.run(&dist, &["run", "//app:disp"])?;
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "DEFAULT DEFAULT DEFAULT DEFAULT DEFAULT\n",
        "plain caller\n{}",
        describe(&out)
    );

    let out = output(via_shell(
        &ws,
        &dist,
        "trap '' INT TTOU TTIN",
        &["run", "//app:disp"],
    ))?;
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "IGNORE IGNORE IGNORE DEFAULT DEFAULT\n",
        "caller ignored INT, TTOU and TTIN\n{}",
        describe(&out)
    );

    let out = output(via_shell(
        &ws,
        &dist,
        "trap '' PIPE",
        &["run", "//app:disp"],
    ))?;
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "DEFAULT DEFAULT DEFAULT DEFAULT IGNORE\n",
        "caller ignored PIPE\n{}",
        describe(&out)
    );
    Ok(())
}

/// The tool reports its open-file soft limit and whether SIGUSR1 is blocked.
const LIMITS_BUILD: &str = r#"
target(name = "limits", driver = "exec", entrypoint = True,
       run = ["/bin/sh", "-c",
              "printf '%s ' \"$(ulimit -n)\"; exec /usr/bin/perl -MPOSIX -e '$s = POSIX::SigSet->new; sigprocmask(SIG_BLOCK, undef, $s) or die; print $s->ismember(SIGUSR1) ? qq(BLOCKED\\n) : qq(OPEN\\n)'"])
"#;

/// The tool gets the caller's open-file soft limit, not the one heph raised
/// for itself, and the caller's signal mask, which std clears in every child
/// (R8, D5).
#[test]
fn entrypoint_keeps_callers_open_file_limit_and_mask() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = Workspace::new()?;
    ws.write("app/BUILD", LIMITS_BUILD)?;

    // Soft only: heph raises its own soft limit towards the hard one, so the
    // tool must not just be seeing a cap.
    let out = output(via_shell(
        &ws,
        &dist,
        "ulimit -S -n 200",
        &["run", "//app:limits"],
    ))?;
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "200 OPEN\n",
        "a lowered limit\n{}",
        describe(&out)
    );

    // perl blocks SIGUSR1, then execs the shell that lowers the limit and
    // execs heph: both survive `exec`.
    let mut cmd = via_shell(&ws, &dist, "ulimit -S -n 300", &["run", "//app:limits"]);
    let shell_args: Vec<_> = cmd.get_args().map(|a| a.to_os_string()).collect();
    let mut blocked = std::process::Command::new("/usr/bin/perl");
    blocked
        .args([
            "-MPOSIX",
            "-e",
            "sigprocmask(SIG_BLOCK, POSIX::SigSet->new(SIGUSR1)) or die; exec @ARGV or die",
            "/bin/sh",
        ])
        .args(shell_args)
        .current_dir(ws.root())
        .envs(
            cmd.get_envs()
                .filter_map(|(k, v)| v.map(|v| (k.to_os_string(), v.to_os_string()))),
        );
    cmd = blocked;
    let out = output(cmd)?;
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "300 BLOCKED\n",
        "a blocked signal\n{}",
        describe(&out)
    );
    Ok(())
}

/// A tool that logs every TERM and USR1 it receives and exits 4 once told to.
const TRAP_BUILD: &str = r#"
target(name = "trap", driver = "bash", entrypoint = True, run = """
trap 'echo term >> "$HEPH_E2E_LOG"' TERM
trap 'echo usr1 >> "$HEPH_E2E_LOG"' USR1
echo ready >> "$HEPH_E2E_LOG"
while [ ! -e "$HEPH_E2E_STOP" ]; do sleep 0.05; done
exit 4
""")
target(name = "hold", driver = "bash", entrypoint = True, run = """
echo $$ > "$HEPH_E2E_PID"
exec sleep 30
""")
"#;

/// The tool's pid, once it has written it to `path`.
fn read_pid(path: &std::path::Path) -> i32 {
    common::wait_until("the tool to write its pid", Duration::from_secs(60), || {
        std::fs::read_to_string(path).is_ok_and(|s| s.ends_with('\n'))
    });
    std::fs::read_to_string(path)
        .expect("pid file")
        .trim()
        .parse()
        .expect("a pid")
}

/// SIGTERM and SIGUSR1 sent to heph's pid reach the tool exactly once each
/// — they are not the terminal's, so only the relay delivers them (E17) — and
/// heph neither dies of them nor acts on them; it exits with the tool's
/// status. A TERM that kills the tool (what `timeout` sends) leaves no tool
/// running: heph forwards it, then dies of it as the tool did.
#[test]
fn entrypoint_signal_to_heph_is_forwarded() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = Workspace::new()?;
    ws.write("app/BUILD", TRAP_BUILD)?;
    let log = ws.root().join("signals.log");
    let stop = ws.root().join("stop");
    let lines = || std::fs::read_to_string(&log).unwrap_or_default();

    let child = ws
        .cmd(&dist, &["run", "//app:trap"])
        .env("HEPH_E2E_LOG", &log)
        .env("HEPH_E2E_STOP", &stop)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let heph = i32::try_from(child.id())?;
    common::wait_until("the tool to set its traps", Duration::from_secs(60), || {
        lines() == "ready\n"
    });
    common::signal(heph, SIGTERM);
    common::wait_until("TERM to reach the tool", Duration::from_secs(10), || {
        lines() == "ready\nterm\n"
    });
    common::signal(heph, libc::SIGUSR1);
    common::wait_until("USR1 to reach the tool", Duration::from_secs(10), || {
        lines() == "ready\nterm\nusr1\n"
    });
    // Long enough for a second, duplicate delivery to have landed.
    std::thread::sleep(Duration::from_millis(300));
    std::fs::write(&stop, b"")?;
    let out = common::wait_output(child, common::RUN_DEADLINE)?;
    assert_eq!(out.status.code(), Some(4), "{}", describe(&out));
    assert_eq!(lines(), "ready\nterm\nusr1\n", "each delivered once");
    assert!(
        out.stdout.is_empty(),
        "heph wrote to stdout: {}",
        describe(&out)
    );

    // `timeout`'s shape: one TERM at heph, the tool does not trap it.
    let pid_file = ws.root().join("hold.pid");
    let mut child = ws
        .cmd(&dist, &["run", "//app:hold"])
        .env("HEPH_E2E_PID", &pid_file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let tool = read_pid(&pid_file);
    common::signal(i32::try_from(child.id())?, SIGTERM);
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "heph outlived its TERM"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(
        status.signal(),
        Some(SIGTERM),
        "heph dies of the tool's TERM"
    );
    assert!(!common::alive(tool), "the tool is gone with heph");
    Ok(())
}

/// heph's own failure after the target resolved exits 125 with its box, so it
/// is never read as the tool's status (R4): a program that does not exist (an
/// error from the driver's run that carries no exit status), a dependency
/// that fails to build, an argument the target does not take. Before
/// resolution it is 1, as for any command.
#[test]
fn error_frame_without_exit_exits_125_with_box() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = Workspace::new()?;
    ws.write(
        "app/BUILD",
        r#"
target(name = "missing", driver = "exec", entrypoint = True, run = ["heph-e2e-no-such-program"])
target(name = "boom", driver = "bash", run = "echo e2e-dep-broke; exit 1", out = "x")
target(name = "broken", driver = "exec", entrypoint = True, deps = {"b": ":boom"}, run = ["/bin/true"])
target(name = "noargs", driver = "exec", entrypoint = True, run = ["/bin/true"])
"#,
    )?;
    for (args, code, says) in [
        (
            &["run", "//app:missing"][..],
            125,
            "heph-e2e-no-such-program",
        ),
        (&["run", "//app:broken"][..], 125, "//app:boom"),
        (
            &["run", "//app:noargs", "--", "x"][..],
            125,
            "takes no arguments",
        ),
        (&["run", "//app:nosuch"][..], 1, "nosuch"),
    ] {
        let out = ws.run(&dist, args)?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(code),
            "{args:?}\n{}",
            describe(&out)
        );
        assert!(stderr.contains('×'), "{args:?}: no box\n{}", describe(&out));
        assert!(
            stderr.contains(says),
            "{args:?}: {says:?}\n{}",
            describe(&out)
        );
        assert!(
            out.stdout.is_empty(),
            "{args:?}: stdout\n{}",
            describe(&out)
        );
    }
    Ok(())
}

/// heph SIGKILLed while its tool runs: the supervisor reaps the tool, and the
/// sandbox the dead invocation left is swept by the next invocation of the
/// target (E20, R23).
#[test]
fn sigkilled_heph_tool_reaped_and_sandbox_swept() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = Workspace::new()?;
    ws.write(
        "app/BUILD",
        r#"
target(name = "hold", driver = "bash", entrypoint = True, args = True, run = """
[ "${1:-}" = quick ] && exit 0
echo $$ > "$HEPH_E2E_PID"
exec sleep 30
""")
"#,
    )?;
    let sandboxes = || -> Vec<std::path::PathBuf> {
        std::fs::read_dir(ws.root().join(".heph/sandbox/app/__invoke/hold"))
            .map(|d| d.filter_map(Result::ok).map(|e| e.path()).collect())
            .unwrap_or_default()
    };
    let pid_file = ws.root().join("hold.pid");
    let mut child = ws
        .cmd(&dist, &["run", "//app:hold"])
        .env("HEPH_E2E_PID", &pid_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let tool = read_pid(&pid_file);
    let left = sandboxes();
    assert_eq!(left.len(), 1, "one live invocation: {left:?}");
    common::signal(i32::try_from(child.id())?, libc::SIGKILL);
    child.wait()?;
    common::wait_until(
        "the supervisor to reap the tool",
        Duration::from_secs(20),
        || !common::alive(tool),
    );
    assert!(
        left[0].exists(),
        "a SIGKILLed heph cannot clean up after itself"
    );

    let out = ws.run(&dist, &["run", "//app:hold", "--", "quick"])?;
    assert!(out.status.success(), "{}", describe(&out));
    assert!(
        !left[0].exists(),
        "swept by the next invocation: {:?}",
        sandboxes()
    );
    Ok(())
}

/// An entrypoint whose dependency builds until `go` exists (or two minutes
/// pass), and a tool that marks that it ran and lives a second — long enough
/// for a signal queued before it existed to reach it.
struct SlowDep {
    ws: Workspace,
    started: std::path::PathBuf,
    go: std::path::PathBuf,
    ran: std::path::PathBuf,
}

impl SlowDep {
    fn new() -> anyhow::Result<Self> {
        let ws = Workspace::new()?;
        let started = ws.root().join("dep-started");
        let go = ws.root().join("go");
        let ran = ws.root().join("tool-ran");
        ws.write(
            "app/BUILD",
            &format!(
                r#"
target(name = "slow", driver = "bash", out = "x", cache = False, run = """
printf x > {started}
i=0
while [ ! -e {go} ] && [ $i -lt 2400 ]; do sleep 0.05; i=$((i+1)); done
printf x > x
""")
target(name = "tool", driver = "exec", entrypoint = True, deps = {{"s": ":slow"}},
       run = ["/bin/sh", "-c", "touch {ran}; sleep 1"])
"#,
                started = started.display(),
                go = go.display(),
                ran = ran.display(),
            ),
        )?;
        Ok(Self {
            ws,
            started,
            go,
            ran,
        })
    }

    /// `heph run //app:tool`, once its dependency is building.
    fn start(&self, dist: &Dist) -> anyhow::Result<std::process::Child> {
        let child = self
            .ws
            .cmd(dist, &["run", "//app:tool"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        common::wait_until("the dependency to start", Duration::from_secs(60), || {
            self.started.exists()
        });
        Ok(child)
    }

    /// The invocation sandboxes on disk.
    fn sandboxes(&self) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(self.ws.root().join(".heph/sandbox/app/__invoke/tool"))
            .map(|d| d.filter_map(Result::ok).map(|e| e.path()).collect())
            .unwrap_or_default()
    }
}

/// Ctrl-C (SIGINT at heph) while the entrypoint's dependency is still
/// building is an ordinary cancel: the tool never starts, and the run exits
/// 130 (R9, R23).
#[test]
fn ctrl_c_during_dep_build_cancels() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let slow = SlowDep::new()?;
    let child = slow.start(&dist)?;
    common::signal(i32::try_from(child.id())?, libc::SIGINT);
    let out = common::wait_output(child, Duration::from_secs(60))?;
    assert_eq!(out.status.code(), Some(130), "{}", describe(&out));
    assert!(!slow.ran.exists(), "the tool ran after a cancel");
    assert!(slow.sandboxes().is_empty(), "{:?}", slow.sandboxes());
    Ok(())
}

/// Before the tool exists heph is an ordinary run (N4a: the relay starts at
/// the spawn): a TERM or HUP while a dependency builds ends heph at once, as
/// on any command, rather than waiting in the relay for a tool that is never
/// spawned. Nothing ran and no invocation sandbox is left.
#[test]
fn term_or_hup_during_dep_build_ends_heph() -> anyhow::Result<()> {
    let dist = Dist::locate();
    for (name, sig) in [("TERM", libc::SIGTERM), ("HUP", libc::SIGHUP)] {
        let slow = SlowDep::new()?;
        let child = slow.start(&dist)?;
        common::signal(i32::try_from(child.id())?, sig);
        let out = common::wait_output(child, Duration::from_secs(20))
            .map_err(|e| anyhow::anyhow!("SIG{name} did not end heph promptly: {e:#}"))?;
        assert_eq!(
            out.status.signal(),
            Some(sig),
            "SIG{name}: heph dies of it\n{}",
            describe(&out)
        );
        assert!(!slow.ran.exists(), "SIG{name}: the tool ran");
        assert!(
            slow.sandboxes().is_empty(),
            "SIG{name}: {:?}",
            slow.sandboxes()
        );
    }
    Ok(())
}

/// A SIGQUIT sent to look at a slow dependency build is heph's diagnostic
/// dump, as on any run — it is not queued for the tool, which would then die
/// of it the moment it starts.
#[test]
fn quit_during_dep_build_does_not_reach_the_tool() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let slow = SlowDep::new()?;
    let child = slow.start(&dist)?;
    common::signal(i32::try_from(child.id())?, libc::SIGQUIT);
    std::thread::sleep(Duration::from_millis(300));
    std::fs::write(&slow.go, b"")?;
    let out = common::wait_output(child, common::RUN_DEADLINE)?;
    assert_eq!(out.status.code(), Some(0), "{}", describe(&out));
    assert!(slow.ran.exists(), "the tool never ran\n{}", describe(&out));
    Ok(())
}
