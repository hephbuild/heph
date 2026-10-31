#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! `entrypoint = True` on a real terminal: what only a PTY can show. The TUI
//! steps aside for the tool, which sees a tty; the terminal's Ctrl-C belongs
//! to the tool and reaches it once, with heph silent; the supervisor survives
//! it; and the terminal is handed back on exit. `--shell` on an entrypoint is
//! `shell_pty.rs`'s `entrypoint_shell_pty`.
//!
//! Every assertion reads the screen through `vt100` (`common::pty`).

mod common;

use common::pty::{Ended, Pty};
use common::{Dist, Workspace};
use std::time::Duration;

/// Generous: a release binary building a dependency, then a tool, on a shared
/// CI runner.
const DEADLINE: Duration = Duration::from_secs(120);
/// What the terminal or the relay answers within.
const SHORT: Duration = Duration::from_secs(30);

/// The dependency keeps the TUI up for a second, so its box (labelled with
/// what the run does) is painted before the tool takes the terminal.
///
/// `trapper`'s `sleep 0.05 || true`: the terminal's Ctrl-C reaches the whole
/// foreground group, its `sleep` included, and the bash driver runs with
/// `-e`, so a `sleep` killed by SIGINT would end the tool with 130. It ignores
/// SIGHUP so that, once heph (the PTY's session leader) is gone, only the
/// supervisor can end it.
const BUILD: &str = r#"
target(name = "slow", driver = "bash", run = "sleep 1", cache = False)
target(name = "tty", driver = "bash", entrypoint = True, deps = {"s": ":slow"}, run = """
if [ -t 0 ] && [ -t 1 ] && [ -t 2 ]; then echo "tool-sees-a-""tty"; fi
exit 0
""")
target(name = "trapper", driver = "bash", entrypoint = True, run = """
n=0
trap 'n=$((n+1)); echo "got-int-$n"; [ "$n" -ge 2 ] && exit 7' INT
m=0
trap 'm=$((m+1)); echo "got-usr1-$m"' USR1
t=0
trap 't=$((t+1)); echo "got-term-$t"' TERM
trap '' HUP
echo "tool-pid-$$."
echo "trap-""ready"
while true; do sleep 0.05 || true; done
""")
target(name = "status", driver = "bash", entrypoint = True, args = True, run = """
if [ -t 0 ] && [ -t 1 ] && [ -t 2 ]; then echo "all-fds-a-""tty"; fi
if [ "${1:-}" = die ]; then kill -TERM $$; sleep 30; fi
exit 7
""")
target(name = "size", driver = "bash", entrypoint = True, run = """
trap 'echo "resized-to-$(/bin/stty size)-end"' WINCH
trap 'exit 3' ALRM
echo "tool-pid-$$."
echo "size-""ready"
while true; do sleep 0.05 || true; done
""")
target(name = "reader", driver = "bash", entrypoint = True, run = """
echo "tool-pid-$$."
echo "heph-pid-$PPID."
echo "read-""ready"
while read -r line; do
  [ "$line" = quit ] && exit 5
  echo "line-[$line]"
done
exit 6
""")
target(name = "tstp", driver = "bash", entrypoint = True, run = """
t=0
trap 't=$((t+1)); echo "got-tstp-$t"' TSTP
trap 'exit 3' ALRM
echo "tool-pid-$$."
echo "heph-pid-$PPID."
echo "tstp-""ready"
while true; do sleep 0.05 || true; done
""")
target(name = "stopper", driver = "bash", entrypoint = True, args = True, run = """
if [ "${1:-}" = alt ]; then printf '\033[?1049h'; fi
echo "tool-pid-$$."
echo "heph-pid-$PPID."
echo "about-to-""stop"
kill -STOP $$
if [ "${1:-}" = alt ]; then printf '\033[?1049l'; fi
echo "resumed-""after-fg"
exit 4
""")
target(name = "twostops", driver = "bash", entrypoint = True, run = """
kill -STOP $$
echo "first-""resume"
kill -STOP $$
echo "second-""resume"
exit 3
""")
target(name = "stopped_counter", driver = "bash", entrypoint = True, run = """
w=0
trap 'w=$((w+1)); echo "winch-$w-$(/bin/stty size)-end"' WINCH
t=0
trap 't=$((t+1)); echo "got-term-$t"' TERM
trap 'echo "barrier-""seen"' USR1
trap 'exit 8' ALRM
echo "tool-pid-$$."
echo "heph-pid-$PPID."
kill -STOP $$
echo "resumed-""tool"
while true; do sleep 0.05 || true; done
""")
target(name = "hold", driver = "bash", entrypoint = True, args = True, run = """
[ "${1:-}" = quick ] && exit 0
echo "tool-pid-$$."
echo "heph-pid-$PPID."
echo "hold-""ready"
while true; do sleep 0.05; done
""")
target(name = "missing", driver = "exec", entrypoint = True, run = ["/no/such/program"])
target(name = "counting", driver = "bash", entrypoint = True, run = """
t=0; trap 't=$((t+1)); echo "got-TERM-$t"' TERM
h=0; trap 'h=$((h+1)); echo "got-HUP-$h"' HUP
i=0; trap 'i=$((i+1)); echo "got-INT-$i"' INT
q=0; trap 'q=$((q+1)); echo "got-QUIT-$q"' QUIT
u=0; trap 'u=$((u+1)); echo "got-USR1-$u"' USR1
b=0; trap 'b=$((b+1)); echo "barrier-$b-"' USR2
w=0; trap 'w=$((w+1)); echo "winch-$w-"' WINCH
trap 'exit 8' ALRM
echo "tool-pid-$$."
echo "counting-""ready"
while true; do sleep 0.05 2>/dev/null || true; done
""")
target(name = "fgwork", driver = "bash", entrypoint = True, run = """
trap 'echo "got-""int"' INT
echo "fgwork-""ready"
sleep 100 || true
echo "sleep-was-""cut"
exit 9
""")
target(name = "many", driver = "bash", entrypoint = True, args = True, run = """
printf 'line-%06d\\n' {1..40000}
echo "sentinel-""end"
exit "${1:-0}"
""")
target(name = "closes", driver = "bash", entrypoint = True, run = """
echo "closing-""stdio"
exec 0<&- 1>&- 2>&-
: > "$HEPH_E2E_CLOSED"
while [ ! -e "$HEPH_E2E_GO" ]; do sleep 0.02; done
exit 6
""")
target(name = "quick", driver = "bash", entrypoint = True, run = """
echo "tool-pid-$$."
echo "quick-""end"
exit 0
""")
target(name = "daemon", driver = "bash", entrypoint = True, run = """
(
  trap '' HUP
  echo "$BASHPID" > "$HEPH_E2E_PIDFILE"
  n=0
  while [ ! -e "$HEPH_E2E_GO" ] && [ "$n" -lt 3000 ]; do
    n=$((n+1))
    echo "tick-$n" || true
    sleep 0.02
  done
  echo "late-""barrier" || true
) &
while [ ! -s "$HEPH_E2E_PIDFILE" ]; do sleep 0.01; done
echo "tool-pid-$$."
echo "heph-pid-$PPID."
echo "daemon-""ready"
exit 5
""")
target(name = "hupstop", driver = "bash", entrypoint = True, run = """
trap '' HUP
echo "tool-pid-$$."
echo "hupstop-""ready"
while [ ! -e "$HEPH_E2E_GO" ]; do sleep 0.02; done
kill -STOP $$
exit 9
""")
target(name = "credit", driver = "bash", entrypoint = True, credentials = ["//auth:cred"], run = """
echo "using ${TOKEN}"
exit 3
""")
target(name = "bytes", driver = "bash", entrypoint = True, run = """
printf 'one\\ntwo\\r\\nthree\\n\\001\\377end'
""")
target(name = "copy", driver = "bash", entrypoint = True, run = "cat")
target(name = "fds", driver = "bash", entrypoint = True, deps = {"s": ":slow"}, run = """
if [ -t 0 ]; then echo "stdin-tty"; else echo "stdin-not"; fi
if [ -t 1 ]; then echo "stdout-tty"; else echo "stdout-not"; fi
""")
target(name = "bgread", driver = "bash", entrypoint = True, deps = {"s": ":slow"}, run = """
echo "bg-tool-""ran"
read -r line
echo "typed-[$line]"
""")
target(name = "only", driver = "bash", entrypoint = True, deps = {"s": ":slow"}, run = """
echo "only-tool-bytes"
""")
target(name = "stdin_action", driver = "bash", out = "seen.txt", run = """
x=$(cat)
echo "saw=[$x]"
echo "saw=[$x]" > $OUT
""")
"#;

/// A credential read from `HEPH_E2E_SECRET`, presented as `$TOKEN`.
const AUTH_BUILD: &str = r#"
target(name = "cred", driver = "auth.credential",
       sources = [heph.auth.env(["HEPH_E2E_SECRET"])],
       present = {"env": {"TOKEN": "${heph_e2e_secret}"}})
"#;

/// The invocation sandboxes of `//app:<name>`.
fn sandboxes(ws: &Workspace, name: &str) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(ws.root().join(".heph/sandbox/app/__invoke").join(name))
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// Every file named `log.txt` below `dir`.
fn logs_below(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut logs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            logs.extend(logs_below(&path));
        } else if path.file_name().is_some_and(|name| name == "log.txt") {
            logs.push(path);
        }
    }
    logs
}

/// Run `command` under a shell that holds the terminal open around it, which a
/// heph leading the session could not, and hand the terminal's settings back
/// from before and after. `act` gets the terminal while the command runs. Returns
/// the command's status.
fn run_held(
    dist: &Dist,
    ws: &Workspace,
    command: &str,
    env: &[(&str, &str)],
    act: impl FnOnce(&mut Pty),
) -> (Pty, i32) {
    let script = format!(
        r#"
echo "before-""run"; read -r _
{command}
echo "after-run=$?="; read -r _
"#
    );
    let mut pty = Pty::script(dist, ws.root(), &script, env);
    pty.wait_for("before-run", DEADLINE);
    let before = pty.termios();
    assert!(before.is_cooked(), "{before:?}");
    pty.send(b"\n");
    act(&mut pty);
    pty.wait_for("after-run=", DEADLINE);
    let status = pty.number_after("after-run=");
    assert_eq!(
        pty.termios(),
        before,
        "`{command}` left the terminal changed\n{}",
        pty.screen()
    );
    (pty, status)
}

/// How far past the drain limit heph may exit: what it still does on the way out,
/// on a shared CI runner. The child would hold the PTY for a minute.
const DRAIN_MARGIN: Duration = Duration::from_secs(10);

/// `//app:daemon`'s child, which the tool waited for before exiting.
fn daemon_pid(pidfile: &std::path::Path) -> anyhow::Result<i32> {
    Ok(std::fs::read_to_string(pidfile)?.trim().parse()?)
}

/// Whether `pid` is running: a zombie has already died.
fn running(pid: i32) -> bool {
    let state = common::proc_state(pid);
    !state.is_empty() && !state.starts_with('Z')
}

/// A shell at its prompt with `//app:<target>` stopped under it, having stopped
/// itself: the shell's "Stopped" is on screen. Returns heph's and the tool's pids.
fn stopped_under_a_shell(pty: &mut Pty, command: &str) -> (i32, i32) {
    pty.type_line(command);
    pty.wait_prompt();
    let (heph, tool) = (pty.number_after("heph-pid-"), pty.number_after("tool-pid-"));
    common::wait_until("heph to be stopped", SHORT, || common::is_stopped(heph));
    assert!(
        common::is_stopped(tool),
        "the tool is {:?}\n{}",
        common::proc_state(tool),
        pty.screen()
    );
    (heph, tool)
}

fn workspace() -> anyhow::Result<Workspace> {
    let ws = Workspace::new()?;
    ws.write("app/BUILD", BUILD)?;
    ws.write("auth/BUILD", AUTH_BUILD)?;
    Ok(ws)
}

/// The TUI draws while the dependency builds, labelled "Invoking", then steps
/// aside: the tool gets the terminal on all three fds, and the cursor is
/// visible again once heph exits.
#[test]
fn entrypoint_tui_pauses_for_tool() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::spawn(&dist, ws.root(), &["run", "//app:tty"], &[]);
    pty.wait_for("Invoking //app:tty", DEADLINE);
    pty.wait_for("tool-sees-a-tty", DEADLINE);
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(0), "{}", pty.seen());
    assert!(pty.cursor_visible(), "cursor left hidden\n{}", pty.screen());
    Ok(())
}

/// The terminal's Ctrl-C is the tool's (N4a): a tool that traps SIGINT and
/// keeps running sees each press exactly once — the kernel delivers it, and
/// the relay skips it — heph prints nothing about it and does not exit 130,
/// and the run ends with the tool's later status. A USR1 sent to heph's pid
/// under the terminal is relayed, once.
#[test]
fn entrypoint_ctrl_c_goes_to_tool() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::spawn(&dist, ws.root(), &["run", "//app:trapper"], &[]);
    pty.wait_for("trap-ready", DEADLINE);

    pty.send(&[0x03]);
    pty.wait_for("got-int-1", Duration::from_secs(20));
    // Time enough for a duplicate (the relay re-sending the terminal's
    // signal) to land before the second press.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !pty.seen().contains("got-int-2"),
        "one press, two deliveries\n{}",
        pty.screen()
    );

    common::signal(pty.pid(), libc::SIGUSR1);
    pty.wait_for("got-usr1-1", Duration::from_secs(20));
    std::thread::sleep(Duration::from_millis(300));

    pty.send(&[0x03]);
    pty.wait_for("got-int-2", Duration::from_secs(20));
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(7), "{}", pty.seen());

    let seen = pty.seen();
    assert!(!seen.contains("got-usr1-2"), "USR1 delivered twice\n{seen}");
    assert!(
        !seen.contains("got-int-3"),
        "a press was delivered twice\n{seen}"
    );
    for heph_said in ["ctrl-c", "cancel", "abort"] {
        assert!(
            !seen.to_lowercase().contains(heph_said),
            "heph reacted to the tool's Ctrl-C ({heph_said:?})\n{seen}"
        );
    }
    assert!(pty.cursor_visible(), "cursor left hidden\n{}", pty.screen());
    Ok(())
}

/// Signals sent with `kill` under `--raw` while heph is the terminal's foreground
/// (R2), where the tool shares heph's group. A default run has its own session,
/// so none of this applies to it:
/// - a TERM to heph's pid is relayed, exactly once;
/// - an INT to heph's pid is dropped (X5): in the shared group it cannot be
///   told apart from the terminal's own, which already reached the tool;
/// - a TERM to the whole group (`kill -TERM -<pgid>`, `kill 0`) reaches the
///   tool at least once — from the kernel, and again from the relay, which
///   cannot tell it from one sent to heph's pid alone (D11).
///
/// heph acts on none of them.
#[test]
fn entrypoint_kill_under_a_terminal() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::spawn(&dist, ws.root(), &["run", "//app:trapper", "--raw"], &[]);
    pty.wait_for("trap-ready", DEADLINE);

    common::signal(pty.pid(), libc::SIGTERM);
    pty.wait_for("got-term-1", Duration::from_secs(20));
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !pty.seen().contains("got-term-2"),
        "one TERM, two deliveries\n{}",
        pty.screen()
    );

    common::signal(pty.pid(), libc::SIGINT);
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !pty.seen().contains("got-int-1"),
        "an INT to heph's pid under the terminal is dropped (X5)\n{}",
        pty.screen()
    );

    // heph leads the PTY's session, so its group is its pid.
    common::signal(-pty.pid(), libc::SIGTERM);
    pty.wait_for("got-term-2", Duration::from_secs(20));

    pty.send(&[0x03]);
    pty.wait_for("got-int-1", Duration::from_secs(20));
    pty.send(&[0x03]);
    pty.wait_for("got-int-2", Duration::from_secs(20));
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(7), "{}", pty.seen());
    assert!(pty.cursor_visible(), "cursor left hidden\n{}", pty.screen());
    Ok(())
}

/// The supervisor sidecar shares heph's process group, so the terminal's
/// Ctrl-C reaches it too. It must survive it: after a Ctrl-C the tool keeps
/// running (it is the tool's), and a `kill -9` of heph then still has a
/// supervisor to reap the tool (R20's documented way out of a tool that traps
/// INT and QUIT).
#[test]
fn supervisor_survives_ctrl_c_and_reaps_after_kill_9() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::spawn(&dist, ws.root(), &["run", "//app:trapper"], &[]);
    pty.wait_for("trap-ready", DEADLINE);
    let seen = pty.seen();
    let tool: Option<i32> = seen
        .split("tool-pid-")
        .nth(1)
        .and_then(|rest| rest.split('.').next())
        .and_then(|pid| pid.parse().ok());
    let tool = anyhow::Context::with_context(tool, || format!("the tool printed no pid\n{seen}"))?;

    pty.send(&[0x03]);
    pty.wait_for("got-int-1", Duration::from_secs(20));
    common::signal(pty.pid(), libc::SIGKILL);
    common::wait_until(
        "the supervisor to reap the tool",
        Duration::from_secs(20),
        || !common::alive(tool),
    );
    Ok(())
}

/// The default run: the tool has a terminal on all three fds, and its status is
/// heph's, whether it exited or died of a signal.
#[test]
fn default_run_is_a_tty_with_exit_status() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::spawn(&dist, ws.root(), &["run", "//app:status"], &[]);
    pty.wait_for("all-fds-a-tty", DEADLINE);
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(7), "{}", pty.seen());

    let mut pty = Pty::spawn(&dist, ws.root(), &["run", "//app:status", "--", "die"], &[]);
    pty.wait_for("all-fds-a-tty", DEADLINE);
    let ended = pty.wait_exit(DEADLINE);
    assert!(
        matches!(&ended, Ended::Signal(name) if name.to_lowercase().contains("term")),
        "a tool's signal death is heph's: {ended:?}\n{}",
        pty.seen()
    );
    Ok(())
}

/// A resize of the caller's window reaches the tool, once the PTY has the new
/// size, so the size the tool reads from its trap is the new one.
#[test]
fn default_run_forwards_resize() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::spawn(&dist, ws.root(), &["run", "//app:size"], &[]);
    pty.wait_for("size-ready", DEADLINE);
    let tool = pty.number_after("tool-pid-");
    pty.resize(30, 100);
    pty.wait_for("resized-to-30 100-end", SHORT);
    common::signal(tool, libc::SIGALRM);
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(3), "{}", pty.seen());
    Ok(())
}

/// A Ctrl-C through the PTY: a tool that traps INT sees it, and heph runs on
/// until the tool says otherwise.
#[test]
fn default_run_ctrl_c_reaches_the_tool() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::spawn(&dist, ws.root(), &["run", "//app:trapper"], &[]);
    pty.wait_for("trap-ready", DEADLINE);
    pty.send(&[0x03]);
    pty.wait_for("got-int-1", SHORT);
    assert!(common::alive(pty.pid()), "heph went with the Ctrl-C");
    pty.send(&[0x03]);
    pty.wait_for("got-int-2", SHORT);
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(7), "{}", pty.seen());
    Ok(())
}

/// The tool is alone in its session, so its group is orphaned and the terminal's
/// SIGTSTP does nothing to a tool that does not handle it. The line typed after
/// the Ctrl-Z goes through the same PTY behind it, so its echo is the barrier.
#[test]
fn plain_tool_under_ctrl_z_keeps_running() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::shell(&dist, ws.root(), &[]);
    pty.type_line(r#""$HEPH" run //app:reader"#);
    pty.wait_for("read-ready", DEADLINE);
    let (heph, tool) = (pty.number_after("heph-pid-"), pty.number_after("tool-pid-"));

    pty.send(&[0x1a]);
    pty.send(b"after-z\n");
    pty.wait_for("line-[after-z]", SHORT);
    for (who, pid) in [("heph", heph), ("the tool", tool)] {
        let state = common::proc_state(pid);
        assert!(
            !state.is_empty() && !state.starts_with('T'),
            "{who} is in state {state:?}\n{}",
            pty.screen()
        );
    }

    pty.send(b"quit\n");
    pty.wait_prompt();
    assert_eq!(pty.last_status(), 5, "{}", pty.seen());
    Ok(())
}

/// A tool that handles SIGTSTP gets it from the Ctrl-Z, and nothing stops.
#[test]
fn tool_handling_sigtstp_sees_it() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::shell(&dist, ws.root(), &[]);
    pty.type_line(r#""$HEPH" run //app:tstp"#);
    pty.wait_for("tstp-ready", DEADLINE);
    let (heph, tool) = (pty.number_after("heph-pid-"), pty.number_after("tool-pid-"));

    pty.send(&[0x1a]);
    pty.wait_for("got-tstp-1", SHORT);
    assert!(!common::is_stopped(heph), "heph stopped\n{}", pty.screen());

    common::signal(tool, libc::SIGALRM);
    pty.wait_prompt();
    assert_eq!(pty.last_status(), 3, "{}", pty.seen());
    Ok(())
}

/// A tool that stops itself takes heph with it, and the shell gets the terminal
/// back: cooked, and showing all the tool wrote before stopping, the switch to
/// the alternate screen included. `fg` resumes both.
#[test]
fn self_sigstop_suspends_and_resumes() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::shell(&dist, ws.root(), &[]);
    let (heph, tool) = stopped_under_a_shell(&mut pty, r#""$HEPH" run //app:stopper -- alt"#);

    let screen = pty.screen();
    let stopped_at = screen.find("Stopped");
    let wrote_at = screen.find("about-to-stop");
    assert!(
        wrote_at.is_some() && wrote_at < stopped_at,
        "the tool's bytes were not flushed before the stop\n{screen}"
    );
    assert!(
        pty.alternate_screen(),
        "the switch to the alternate screen was not flushed before the stop\n{screen}"
    );
    assert!(pty.termios().is_cooked(), "{:?}", pty.termios());

    pty.type_line("fg");
    pty.wait_for("resumed-after-fg", SHORT);
    pty.wait_prompt();
    assert_eq!(pty.last_status(), 4, "{}", pty.seen());
    assert!(!pty.alternate_screen(), "{}", pty.screen());
    assert!(pty.termios().is_cooked(), "{:?}", pty.termios());
    assert!(pty.cursor_visible(), "cursor left hidden\n{}", pty.screen());
    for pid in [heph, tool] {
        assert!(!common::alive(pid), "{pid} outlived the run");
    }
    Ok(())
}

/// The tool dies while heph is stopped and cannot reap it. On `fg` the continue
/// finds nobody and no error comes of it, and the status is the tool's.
#[test]
fn tool_dies_while_heph_stopped() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::shell(&dist, ws.root(), &[]);
    let (_heph, tool) = stopped_under_a_shell(&mut pty, r#""$HEPH" run //app:stopper"#);

    common::signal(tool, libc::SIGKILL);
    common::wait_until("the tool to be a zombie", SHORT, || {
        common::proc_state(tool).starts_with('Z')
    });

    pty.type_line("fg");
    pty.wait_prompt();
    assert_eq!(pty.last_status(), 128 + libc::SIGKILL, "{}", pty.seen());
    assert!(pty.termios().is_cooked(), "{:?}", pty.termios());
    Ok(())
}

/// Each stop hands back the termios the caller had, the second one included: a
/// stop that captured the terminal again would capture the raw one. This shell
/// does not reset the terminal when a job stops, as an interactive one does.
#[test]
fn second_stop_restores_the_callers_termios() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::script(
        &dist,
        ws.root(),
        r#"
set -m
echo "before-""run"; read -r _
"$HEPH" run //app:twostops
echo "first-""stopped"; read -r _
fg
echo "second-""stopped"; read -r _
fg
echo "finished-$?-"; read -r _
"#,
        &[],
    );
    pty.wait_for("before-run", DEADLINE);
    let callers = pty.termios();
    assert!(callers.is_cooked(), "{callers:?}");
    pty.send(b"\n");

    pty.wait_for("first-stopped", DEADLINE);
    assert_eq!(pty.termios(), callers, "after the first stop");
    pty.send(b"\n");

    pty.wait_for("first-resume", SHORT);
    pty.wait_for("second-stopped", SHORT);
    assert_eq!(pty.termios(), callers, "after the second stop");
    pty.send(b"\n");

    pty.wait_for("second-resume", SHORT);
    pty.wait_for("finished-3-", SHORT);
    assert_eq!(pty.termios(), callers, "after the run");
    pty.send(b"\n");
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(0), "{}", pty.seen());
    Ok(())
}

/// The window is resized while heph is stopped: nothing can tell the tool yet.
/// `fg` tells it, once. The USR1 relayed behind it is the barrier for a second.
#[test]
fn resize_while_stopped_seen_once() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::shell(&dist, ws.root(), &[]);
    let (heph, tool) = stopped_under_a_shell(&mut pty, r#""$HEPH" run //app:stopped_counter"#);

    pty.resize(30, 100);
    pty.type_line("fg");
    pty.wait_for("winch-1-30 100-end", SHORT);

    common::signal(heph, libc::SIGUSR1);
    pty.wait_for("barrier-seen", SHORT);
    let seen = pty.seen();
    assert!(!seen.contains("winch-2"), "two resizes delivered\n{seen}");

    common::signal(tool, libc::SIGALRM);
    pty.wait_prompt();
    assert_eq!(pty.last_status(), 8, "{}", pty.seen());
    Ok(())
}

/// `bg` cannot run the tool, whose terminal only a foreground heph can serve:
/// heph wakes, sees it is not the foreground and stops again, and the tool never
/// wakes. `fg` then resumes both and the output arrives.
#[test]
fn bg_leaves_tool_stopped_until_fg() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::shell(&dist, ws.root(), &[]);
    let (heph, tool) = stopped_under_a_shell(&mut pty, r#""$HEPH" run //app:stopper"#);

    pty.type_line("bg");
    pty.wait_prompt();
    // The shell saw heph go through running and stop again, which `ps` alone
    // could not tell from heph never having woken.
    let started = std::time::Instant::now();
    let mut asked = 0;
    loop {
        asked += 1;
        let typed = format!("jobs; echo asked''-{asked}-");
        pty.type_line(&typed);
        pty.wait_for(&format!("asked-{asked}-"), SHORT);
        pty.wait_prompt();
        let seen = pty.seen();
        let answer = seen
            .rsplit_once(typed.as_str())
            .map_or("", |(_, answer)| answer);
        if answer.contains("Stopped") {
            break;
        }
        assert!(
            started.elapsed() < SHORT,
            "heph did not stop again\n{}",
            pty.screen()
        );
    }
    assert!(common::is_stopped(heph), "{}", common::proc_state(heph));
    assert!(common::is_stopped(tool), "{}", common::proc_state(tool));
    let seen = pty.seen();
    assert!(!seen.contains("resumed-after-fg"), "the tool ran\n{seen}");

    pty.type_line("fg");
    pty.wait_for("resumed-after-fg", SHORT);
    pty.wait_prompt();
    assert_eq!(pty.last_status(), 4, "{}", pty.seen());
    Ok(())
}

/// A TERM to a stopped heph waits like any signal to a stopped process: the
/// shell's echo proves the window passed with heph still stopped and silent.
/// After `fg` it is forwarded, once; the USR1 behind it is the barrier.
#[test]
fn term_while_stopped_pending_until_fg() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::shell(&dist, ws.root(), &[]);
    let (heph, tool) = stopped_under_a_shell(&mut pty, r#""$HEPH" run //app:stopped_counter"#);

    common::signal(heph, libc::SIGTERM);
    pty.type_line("echo window''-closed");
    pty.wait_for("window-closed", SHORT);
    pty.wait_prompt();
    assert!(common::is_stopped(heph), "{}", common::proc_state(heph));
    assert!(common::is_stopped(tool), "{}", common::proc_state(tool));
    let seen = pty.seen();
    assert!(!seen.contains("got-term"), "TERM acted on\n{seen}");

    pty.type_line("fg");
    pty.wait_for("got-term-1", SHORT);
    common::signal(heph, libc::SIGUSR1);
    pty.wait_for("barrier-seen", SHORT);
    let seen = pty.seen();
    assert!(!seen.contains("got-term-2"), "two deliveries\n{seen}");

    common::signal(tool, libc::SIGALRM);
    pty.wait_prompt();
    assert_eq!(pty.last_status(), 8, "{}", pty.seen());
    Ok(())
}

/// The tool has gone and a child of its still holds the PTY. A SIGTSTP then stops
/// nothing, and the shell sees no stop at all.
///
/// Only Linux has a drain window to send it in, whose barrier is the child's next
/// line, printed by the pump of a heph still running. On macOS the tool's exit,
/// as the session leader, revokes the PTY: the master reads EOF within
/// milliseconds and the child's writes fail with EIO (measured), so heph is
/// leaving, or gone, when the signal arrives.
#[test]
fn stop_during_drain_is_ignored() -> anyhow::Result<()> {
    const LIMIT: Duration = Duration::from_secs(20);
    let dist = Dist::locate();
    let ws = workspace()?;
    let dir = tempfile::tempdir()?;
    let (pidfile, go) = (dir.path().join("pid"), dir.path().join("go"));
    let limit = LIMIT.as_millis().to_string();
    let mut pty = Pty::shell(
        &dist,
        ws.root(),
        &[
            ("HEPH_E2E_PIDFILE", &pidfile.to_string_lossy()),
            ("HEPH_E2E_GO", &go.to_string_lossy()),
            ("HEPH_PTY_DRAIN_MS", &limit),
        ],
    );
    pty.type_line(r#""$HEPH" run //app:daemon"#);
    pty.wait_for("daemon-ready", DEADLINE);
    let (heph, tool) = (pty.number_after("heph-pid-"), pty.number_after("tool-pid-"));
    let daemon = daemon_pid(&pidfile)?;
    common::wait_until("the tool to be gone", SHORT, || !common::alive(tool));
    let gone = std::time::Instant::now();

    // Not `common::signal`: on macOS heph may have left already.
    // SAFETY: a plain syscall on integers.
    unsafe { libc::kill(heph, libc::SIGTSTP) };
    if cfg!(target_os = "linux") {
        pty.wait_for("tick-", SHORT);
        assert!(running(daemon), "the child died\n{}", pty.seen());
        std::fs::write(&go, b"")?;
        pty.wait_for("late-barrier", SHORT);
        assert!(!common::is_stopped(heph), "{}", common::proc_state(heph));
    }

    pty.wait_prompt();
    let elapsed = gone.elapsed();
    if !cfg!(target_os = "linux") {
        let alive_at_exit = running(daemon);
        std::fs::write(&go, b"")?;
        assert!(alive_at_exit, "the child died first\n{}", pty.seen());
    }
    assert!(
        elapsed < LIMIT + DRAIN_MARGIN,
        "heph left after {elapsed:?}\n{}",
        pty.seen()
    );
    assert_eq!(pty.last_status(), 5, "{}", pty.seen());
    let seen = pty.seen();
    assert_eq!(
        seen.matches("Stopped").count(),
        0,
        "a stop during the drain was taken\n{seen}"
    );
    Ok(())
}

/// The terminal is the caller's again after every way out: a normal exit, a
/// tool's signal death, a TERM or a HUP to heph, and a spawn that failed after
/// the terminal went raw, whose error prints cooked. A PTY that cannot be opened
/// fails the run loudly, and the tool never runs without it.
#[test]
fn default_run_restores_the_terminal() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let run = |target: &str| format!(r#""$HEPH" run //app:{target}"#);

    let (_, status) = run_held(&dist, &ws, &run("status"), &[], |_| {});
    assert_eq!(status, 7, "normal exit");

    let (_, status) = run_held(&dist, &ws, &run("status -- die"), &[], |_| {});
    assert_eq!(status, 128 + libc::SIGTERM, "signal death");

    for (name, sig) in [("TERM", libc::SIGTERM), ("HUP", libc::SIGHUP)] {
        let (_, status) = run_held(&dist, &ws, &run("hold"), &[], |pty| {
            pty.wait_for("hold-ready", DEADLINE);
            common::signal(pty.number_after("heph-pid-"), sig);
        });
        assert_eq!(status, 128 + sig, "{name} to heph");
    }

    let (pty, status) = run_held(&dist, &ws, &run("missing"), &[], |_| {});
    assert_ne!(status, 0, "a missing program\n{}", pty.screen());
    let screen = pty.screen();
    assert!(screen.contains("/no/such/program"), "{screen}");
    // On a raw terminal a line would start where the last one ended.
    let error_line = screen.lines().find(|l| l.contains("/no/such/program"));
    assert!(
        error_line.is_some_and(|l| !l.starts_with(' ')),
        "the error printed on a raw terminal\n{screen}"
    );

    let (pty, status) = run_held(
        &dist,
        &ws,
        &run("status"),
        &[("HEPH_PTY_OPEN_FAILS", "1")],
        |_| {},
    );
    assert_ne!(status, 0, "no PTY\n{}", pty.screen());
    assert_ne!(status, 7, "the tool ran without its PTY\n{}", pty.screen());
    let seen = pty.seen();
    assert!(seen.contains("PTY"), "failed silently\n{seen}");
    assert!(!seen.contains("all-fds-a-tty"), "fell back\n{seen}");

    // The terminal closes under heph: there are no settings left to read, so
    // what is left to prove is that the run ends and the tool with it.
    let mut pty = Pty::spawn(&dist, ws.root(), &["run", "//app:hold"], &[]);
    pty.wait_for("hold-ready", DEADLINE);
    let tool = pty.number_after("tool-pid-");
    pty.close_terminal();
    pty.wait_exit(DEADLINE);
    common::wait_until("the tool to go", SHORT, || !common::alive(tool));
    Ok(())
}

/// heph cannot clean up after `kill -9`. The supervisor reaps the tool, and the
/// next invocation sweeps the sandbox. Nothing here says the terminal stays raw,
/// which is only a known cost.
#[test]
fn default_run_sigkill_cleans_up() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::spawn(&dist, ws.root(), &["run", "//app:hold"], &[]);
    pty.wait_for("hold-ready", DEADLINE);
    let tool = pty.number_after("tool-pid-");
    let left = sandboxes(&ws, "hold");
    assert_eq!(left.len(), 1, "one live invocation: {left:?}");

    common::signal(pty.pid(), libc::SIGKILL);
    common::wait_until("the supervisor to reap the tool", SHORT, || {
        !common::alive(tool)
    });
    assert_eq!(
        pty.wait_exit(DEADLINE),
        Ended::Signal(pty_signal_name(&dist, &ws)?),
        "{}",
        pty.seen()
    );

    let out = ws.run(&dist, &["run", "//app:hold", "--", "quick"])?;
    assert!(out.status.success(), "{}", common::describe(&out));
    assert!(
        !left[0].exists(),
        "swept by the next invocation: {:?}",
        sandboxes(&ws, "hold")
    );
    Ok(())
}

/// How `portable_pty` spells SIGKILL on this host, from a process that really
/// died of it.
fn pty_signal_name(dist: &Dist, ws: &Workspace) -> anyhow::Result<String> {
    let mut victim = Pty::spawn(dist, ws.root(), &["run", "//app:hold"], &[]);
    victim.wait_for("hold-ready", DEADLINE);
    common::signal(victim.pid(), libc::SIGKILL);
    match victim.wait_exit(DEADLINE) {
        Ended::Signal(name) => Ok(name),
        other => anyhow::bail!("SIGKILL ended it as {other:?}"),
    }
}

/// On the PTY a credential the tool echoes prints as is, and nothing keeps a
/// copy: the host never sees the bytes as anything but bytes for the terminal.
#[test]
fn default_run_no_log_no_redaction() -> anyhow::Result<()> {
    const SECRET: &str = "not-redacted-e2e-secret";
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::spawn(
        &dist,
        ws.root(),
        &["run", "//app:credit"],
        &[("HEPH_E2E_SECRET", SECRET)],
    );
    pty.wait_for(&format!("using {SECRET}"), DEADLINE);
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(3), "{}", pty.seen());
    assert_eq!(
        logs_below(&ws.root().join(".heph/sandbox")),
        [] as [std::path::PathBuf; 0]
    );
    Ok(())
}

/// A redirected fd is that file or pipe, and a tool's bytes reach it untouched:
/// no `\r\n` from a PTY, whichever of `>`, `|` and `<` it is.
#[test]
fn redirected_run_is_byte_exact() -> anyhow::Result<()> {
    const BYTES: &[u8] = b"one\ntwo\r\nthree\n\x01\xffend";
    let dist = Dist::locate();
    let ws = workspace()?;
    let dir = tempfile::tempdir()?;
    let input: Vec<u8> = (0..=255).collect();
    std::fs::write(dir.path().join("in"), &input)?;
    let mut pty = Pty::script(
        &dist,
        ws.root(),
        r#"
"$HEPH" run //app:bytes > "$D/file"
"$HEPH" run //app:bytes | cat > "$D/pipe"
"$HEPH" run //app:copy < "$D/in" > "$D/stdin"
cat "$D/in" | "$HEPH" run //app:copy > "$D/piped"
echo "all-""redirected"
"#,
        &[("D", &dir.path().to_string_lossy())],
    );
    pty.wait_for("all-redirected", DEADLINE);
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(0), "{}", pty.seen());
    assert_eq!(std::fs::read(dir.path().join("file"))?, BYTES, ">");
    assert_eq!(std::fs::read(dir.path().join("pipe"))?, BYTES, "|");
    assert_eq!(
        std::fs::read(dir.path().join("stdin"))?,
        input,
        "<\n{}",
        pty.seen()
    );
    assert_eq!(
        std::fs::read(dir.path().join("piped"))?,
        input,
        "piped stdin\n{}",
        pty.seen()
    );
    Ok(())
}

/// Only stdout redirected: the run inherits, so the tool keeps the terminal on
/// fd 0 and the file on fd 1, and nothing of a TUI lands in the file.
#[test]
fn mixed_redirect_inherits() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let dir = tempfile::tempdir()?;
    let out = dir.path().join("out");
    let (pty, status) = run_held(
        &dist,
        &ws,
        r#""$HEPH" run //app:fds > "$OUT""#,
        &[("OUT", &out.to_string_lossy())],
        |_| {},
    );
    assert_eq!(status, 0, "{}", pty.seen());
    assert_eq!(std::fs::read_to_string(&out)?, "stdin-tty\nstdout-not\n");
    Ok(())
}

/// A run that is not the terminal's foreground job, by `&` or under `timeout`,
/// starts no TUI and leaves the shell's terminal alone. After `fg` the keyboard
/// is the tool's.
#[test]
fn background_run_inherits() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::shell(&dist, ws.root(), &[]);
    let before = pty.termios();

    pty.type_line(r#""$HEPH" run //app:bgread &"#);
    pty.wait_prompt();
    pty.wait_for("bg-tool-ran", DEADLINE);
    assert_eq!(pty.termios(), before, "{}", pty.screen());
    pty.type_line("fg");
    pty.wait_for("run //app:bgread", SHORT);
    pty.send(b"keys\n");
    pty.wait_for("typed-[keys]", SHORT);
    pty.wait_prompt();
    assert_eq!(pty.last_status(), 0, "{}", pty.seen());
    assert_eq!(pty.termios(), before, "{}", pty.screen());

    // Under a shell without job control, as `timeout` is then what makes the group.
    // An interactive shell's job already leads its group, so its `setpgid` changes
    // nothing and heph stays the foreground there, TUI included.
    let (timed, status) = run_held(
        &dist,
        &ws,
        r#"timeout 120 "$HEPH" run //app:only"#,
        &[],
        |_| {},
    );
    assert_eq!(status, 0, "{}", timed.seen());
    assert_eq!(timed.dsr_queries(), 0, "{}", timed.seen());
    assert!(timed.seen().contains("only-tool-bytes"), "{}", timed.seen());
    assert!(!timed.seen().contains("╭─"), "{}", timed.seen());

    let seen = pty.seen();
    assert_eq!(pty.dsr_queries(), 0, "a TUI asked for the cursor\n{seen}");
    // The TUI's box. A background run's plain log lines may name the target.
    for tui_said in ["╭─", "╰─"] {
        assert!(!seen.contains(tui_said), "a TUI drew {tui_said:?}\n{seen}");
    }
    Ok(())
}

/// `--raw` leaves the terminal to the tool, a slow dependency or not: what is
/// on screen is what the tool printed, heph asks the terminal nothing, and a
/// redirected stdout holds the tool's bytes alone.
#[test]
fn raw_prints_only_the_tool() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::spawn(&dist, ws.root(), &["run", "//app:only", "--raw"], &[]);
    pty.wait_for("only-tool-bytes", DEADLINE);
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(0), "{}", pty.seen());
    assert_eq!(pty.screen().trim(), "only-tool-bytes", "{}", pty.seen());
    assert_eq!(pty.dsr_queries(), 0, "{}", pty.seen());
    assert!(pty.cursor_visible(), "{}", pty.seen());
    assert!(!pty.alternate_screen(), "{}", pty.seen());

    let dir = tempfile::tempdir()?;
    let out = dir.path().join("out");
    let (pty, status) = run_held(
        &dist,
        &ws,
        r#""$HEPH" run //app:only --raw > "$OUT""#,
        &[("OUT", &out.to_string_lossy())],
        |_| {},
    );
    assert_eq!(status, 0, "{}", pty.seen());
    assert_eq!(std::fs::read_to_string(&out)?, "only-tool-bytes\n");
    Ok(())
}

/// Every signal sent to heph, or to heph's group, reaches the tool exactly once,
/// and a resize once, through the PTY. A USR2 relayed behind each is the barrier
/// a duplicate would have to beat, and is counted as well.
#[test]
fn default_run_signals_forwarded_once() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::spawn(&dist, ws.root(), &["run", "//app:counting"], &[]);
    pty.wait_for("counting-ready", DEADLINE);
    let heph = pty.pid();
    let tool = pty.number_after("tool-pid-");

    let mut barriers = 0;
    let mut barrier = |pty: &mut Pty| {
        barriers += 1;
        common::signal(heph, libc::SIGUSR2);
        pty.wait_for(&format!("barrier-{barriers}-"), SHORT);
    };
    for (name, sig) in [
        ("TERM", libc::SIGTERM),
        ("HUP", libc::SIGHUP),
        ("INT", libc::SIGINT),
        ("QUIT", libc::SIGQUIT),
        ("USR1", libc::SIGUSR1),
    ] {
        common::signal(heph, sig);
        pty.wait_for(&format!("got-{name}-1"), SHORT);
        barrier(&mut pty);
        let seen = pty.seen();
        assert!(!seen.contains(&format!("got-{name}-2")), "{name}\n{seen}");
    }

    // heph leads the PTY's session, so its group is its pid.
    common::signal(-heph, libc::SIGTERM);
    pty.wait_for("got-TERM-2", SHORT);
    barrier(&mut pty);
    let seen = pty.seen();
    assert!(!seen.contains("got-TERM-3"), "to the group\n{seen}");

    pty.resize(30, 100);
    pty.wait_for("winch-1-", SHORT);
    barrier(&mut pty);
    let seen = pty.seen();
    assert!(!seen.contains("winch-2-"), "a resize\n{seen}");

    common::signal(tool, libc::SIGALRM);
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(8), "{}", pty.seen());
    Ok(())
}

/// A signal to heph reaches the tool's whole group, so a `sleep` in the
/// foreground of a script is cut short: bash only runs its trap once that child
/// is gone, and it is sleeping well past the deadline.
#[test]
fn relay_reaches_the_foreground_group() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::spawn(&dist, ws.root(), &["run", "//app:fgwork"], &[]);
    pty.wait_for("fgwork-ready", DEADLINE);
    common::signal(pty.pid(), libc::SIGINT);
    pty.wait_for("sleep-was-cut", SHORT);
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(9), "{}", pty.seen());
    Ok(())
}

/// A child that outlives the tool never holds heph past the limit, and is still
/// there at exit.
///
/// Only on Linux does it keep the PTY open, so that heph waits out the limit and
/// says what it cut. On macOS the tool's exit, as the session leader, revokes the
/// PTY: the master reads EOF within milliseconds with the child alive and its
/// writes failing with EIO (measured), so nothing is drained and nothing warns.
#[test]
fn default_run_drain_is_bounded() -> anyhow::Result<()> {
    const LIMIT: Duration = Duration::from_millis(1500);
    let dist = Dist::locate();
    let ws = workspace()?;
    let dir = tempfile::tempdir()?;
    let (pidfile, go) = (dir.path().join("pid"), dir.path().join("go"));
    let limit = LIMIT.as_millis().to_string();
    let mut pty = Pty::spawn(
        &dist,
        ws.root(),
        &["run", "//app:daemon"],
        &[
            ("HEPH_E2E_PIDFILE", &pidfile.to_string_lossy()),
            ("HEPH_E2E_GO", &go.to_string_lossy()),
            ("HEPH_PTY_DRAIN_MS", &limit),
        ],
    );
    pty.wait_for("daemon-ready", DEADLINE);
    let tool = pty.number_after("tool-pid-");
    common::wait_until("the tool to be gone", SHORT, || !common::alive(tool));
    let gone = std::time::Instant::now();
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(5), "{}", pty.seen());
    let elapsed = gone.elapsed();

    let alive_at_exit = running(daemon_pid(&pidfile)?);
    std::fs::write(&go, b"")?;
    assert!(alive_at_exit, "the child died first\n{}", pty.seen());
    assert!(
        elapsed < LIMIT + DRAIN_MARGIN,
        "heph left after {elapsed:?}\n{}",
        pty.seen()
    );
    if cfg!(target_os = "linux") {
        // The limit starts as the run returns, a moment before the tool's pid
        // could be seen gone.
        assert!(
            elapsed + Duration::from_millis(200) >= LIMIT,
            "heph left after {elapsed:?}\n{}",
            pty.seen()
        );
        let seen = pty.seen();
        assert!(seen.contains("bytes_drained"), "no warning\n{seen}");
    }
    Ok(())
}

/// Several hundred KiB and then an immediate exit lose nothing: the screen could
/// not show that, so this one counts bytes.
fn every_line_arrived(pty: &Pty) {
    let output = pty.output();
    let text = String::from_utf8_lossy(&output);
    // What heph drew before the tool shares the first line's row.
    let start = text
        .find("line-000001")
        .expect("the first line went missing");
    let tail = text.get(start..).unwrap_or_default();
    // Once a PTY slave's queue fills, XNU emits `\r\r\n` every 4095 bytes. That
    // was measured on a plain PTY with no heph involved, so on macOS `\r+\n` is
    // one line ending. These tests prove nothing is lost, which still holds.
    // Linux keeps the exact `\r\n`, so a real double translation fails there.
    let lines: Vec<&str> = if cfg!(target_os = "macos") {
        tail.split('\n')
            .map(|line| line.trim_end_matches('\r'))
            .collect()
    } else {
        tail.split("\r\n").collect()
    };
    let mut next = 1;
    for line in lines {
        let Some(number) = line.strip_prefix("line-") else {
            continue;
        };
        assert_eq!(number, format!("{next:06}"), "a line went missing");
        next += 1;
    }
    assert_eq!(next, 40_001, "lines arrived");
    assert!(text.contains("sentinel-end"), "the last line went missing");
}

#[test]
fn default_run_output_complete() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::spawn(&dist, ws.root(), &["run", "//app:many"], &[]);
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(0), "{}", pty.screen());
    every_line_arrived(&pty);
    Ok(())
}

/// A tool that fails has every byte of its output delivered too, the non-zero
/// exit being no reason to cut the drain short.
#[test]
fn failing_tool_output_is_complete() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::spawn(&dist, ws.root(), &["run", "//app:many", "--", "1"], &[]);
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(1), "{}", pty.screen());
    every_line_arrived(&pty);
    Ok(())
}

/// Once the tool has gone nothing may read stdin: a line typed then is the
/// shell's, whose next prompt runs it.
#[test]
fn bytes_typed_after_exit_reach_the_shell() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let mut pty = Pty::shell(&dist, ws.root(), &[]);
    pty.type_line(r#""$HEPH" run //app:quick"#);
    pty.wait_for("quick-end", DEADLINE);
    let tool = pty.number_after("tool-pid-");
    common::wait_until("the tool to be gone", SHORT, || !common::alive(tool));
    pty.type_line("echo typed''-after");
    pty.wait_for("typed-after", SHORT);
    Ok(())
}

/// The tool closes all of its stdio and carries on: that ends its output, not
/// its run. heph waits for it and returns its status.
#[test]
fn tool_closes_slave_without_exiting() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let dir = tempfile::tempdir()?;
    let (closed, go) = (dir.path().join("closed"), dir.path().join("go"));
    let mut pty = Pty::spawn(
        &dist,
        ws.root(),
        &["run", "//app:closes"],
        &[
            ("HEPH_E2E_CLOSED", &closed.to_string_lossy()),
            ("HEPH_E2E_GO", &go.to_string_lossy()),
        ],
    );
    pty.wait_for("closing-stdio", DEADLINE);
    common::wait_until("the tool to close its stdio", SHORT, || closed.exists());
    assert!(common::alive(pty.pid()), "heph left\n{}", pty.seen());
    std::fs::write(&go, b"")?;
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(6), "{}", pty.seen());
    Ok(())
}

/// Nobody can resume heph, so it must not go on stopping itself. Each way that
/// happens ends the run, with the sandbox gone.
#[test]
fn stopped_heph_terminal_gone_does_not_hang() -> anyhow::Result<()> {
    let dist = Dist::locate();
    let ws = workspace()?;
    let gone = |heph: i32, tool: i32, what: &str| {
        common::wait_until(what, DEADLINE, || {
            !common::alive(heph) && !common::alive(tool)
        });
    };

    // The terminal closes while heph is stopped.
    let mut pty = Pty::shell(&dist, ws.root(), &[]);
    let (heph, tool) = stopped_under_a_shell(&mut pty, r#""$HEPH" run //app:stopper"#);
    pty.close_terminal();
    gone(heph, tool, "heph to end once the terminal closed");
    drop(pty);

    // The shell that could have resumed it exits.
    let mut pty = Pty::shell(&dist, ws.root(), &[]);
    let (heph, tool) = stopped_under_a_shell(&mut pty, r#""$HEPH" run //app:stopper"#);
    common::signal(pty.pid(), libc::SIGKILL);
    gone(heph, tool, "heph to end once the shell was gone");
    drop(pty);

    // TERM, then CONT: awake and not the foreground, which would stop it again.
    let mut pty = Pty::script(
        &dist,
        ws.root(),
        r#"
set -m
"$HEPH" run //app:stopper
echo "job-""stopped"; read -r _
wait %1
echo "waited=$?="; read -r _
"#,
        &[],
    );
    pty.wait_for("job-stopped", DEADLINE);
    let heph = pty.number_after("heph-pid-");
    let tool = pty.number_after("tool-pid-");
    common::signal(heph, libc::SIGTERM);
    common::signal(heph, libc::SIGCONT);
    common::wait_until("heph to end after TERM and CONT", DEADLINE, || {
        // Gone, not a zombie: the shell has noted how the job ended.
        !common::alive(heph) && !common::alive(tool)
    });
    pty.send(b"\n");
    pty.wait_for("waited=", SHORT);
    assert_eq!(
        pty.number_after("waited="),
        128 + libc::SIGTERM,
        "{}",
        pty.seen()
    );
    drop(pty);

    // The terminal closes, a tool that traps the hangup survives and then stops:
    // heph never takes that first stop, and the tool's status is heph's.
    let dir = tempfile::tempdir()?;
    let go = dir.path().join("go");
    let mut pty = Pty::spawn(
        &dist,
        ws.root(),
        &["run", "//app:hupstop"],
        &[("HEPH_E2E_GO", &go.to_string_lossy())],
    );
    pty.wait_for("hupstop-ready", DEADLINE);
    let tool = pty.number_after("tool-pid-");
    pty.close_terminal();
    std::fs::write(&go, b"")?;
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(9));
    assert!(!common::alive(tool), "the tool outlived heph");

    for name in ["stopper", "hupstop"] {
        let left = sandboxes(&ws, name);
        assert!(left.is_empty(), "the sandbox of {name} stayed: {left:?}");
    }
    Ok(())
}

/// `--raw` on a build target never passes stdin: two runs that differ only in
/// what they were piped share a cache key, so the second cannot be allowed to get
/// what the first made of it.
#[test]
fn raw_build_target_gets_no_stdin() -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::process::Stdio;

    let dist = Dist::locate();
    let ws = workspace()?;
    let piped = |bytes: &[u8]| -> anyhow::Result<std::process::Output> {
        let mut child = ws
            .cmd(&dist, &["run", "//app:stdin_action", "--raw"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        // The action never reads it, so a closed pipe is no failure.
        drop(child.stdin.take().map(|mut stdin| stdin.write_all(bytes)));
        common::wait_output(child, common::RUN_DEADLINE)
    };

    let a = piped(b"A")?;
    assert!(a.status.success(), "{}", common::describe(&a));
    assert_eq!(String::from_utf8_lossy(&a.stdout), "saw=[]\n");

    let b = piped(b"B")?;
    assert!(b.status.success(), "{}", common::describe(&b));
    assert_eq!(
        String::from_utf8_lossy(&b.stdout),
        "",
        "a cache hit prints nothing"
    );
    Ok(())
}
