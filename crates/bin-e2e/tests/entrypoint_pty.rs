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
"#;

fn workspace() -> anyhow::Result<Workspace> {
    let ws = Workspace::new()?;
    ws.write("app/BUILD", BUILD)?;
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

/// Signals sent with `kill` while heph is the terminal's foreground (R2):
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
    let mut pty = Pty::spawn(&dist, ws.root(), &["run", "//app:trapper"], &[]);
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
