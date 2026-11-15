//! `heph session` on a real terminal: what only a PTY can show.
//!
//! `attach` runs the interactive TUI on another process's recording, so its
//! drawing, its `q` and its terminal restore are a terminal's to see; and a
//! run whose TUI is paused for a target that holds the terminal keeps
//! recording, which only a run on a terminal ever does.
//!
//! Every screen assertion reads `vt100` cells (`common::pty`).

mod common;

use common::pty::{Ended, Pty};
use common::session::{Bg, SHORT, list_entry, recorded, sessions_dir};
use common::{Dist, Workspace};
use std::time::{Duration, Instant};

/// A release binary on a shared CI runner.
const DEADLINE: Duration = Duration::from_secs(120);

/// `slow` keeps a session live long enough to attach and detach; `hold` is a
/// single named target, so it gets the terminal and the TUI pauses for it.
const BUILD: &str = r#"
target(name = "slow", driver = "bash", run = "sleep 15", cache = False)
target(name = "hold", driver = "bash", run = 'echo hold-""started; sleep 6; echo hold-""done', cache = False)
"#;

fn workspace() -> Workspace {
    let ws = Workspace::new().expect("workspace");
    ws.write("pkg/BUILD", BUILD).expect("write BUILD");
    ws
}

/// `attach` draws the session's progress view, `q` detaches with 0 and hands
/// the terminal back, and the session runs on.
#[test]
fn attach_renders_and_detaches() {
    let dist = Dist::locate();
    let ws = workspace();
    let mut run = Bg::spawn(&ws, &dist, &["run", "//pkg:slow"]);
    run.wait_executing("//pkg:slow");

    let home = ws.home().to_string_lossy().into_owned();
    // A shell holds the terminal around attach, so its settings can be read
    // after attach exits.
    let script = format!(
        r#"
echo "before-""attach"; read -r _
"$HEPH" session attach {}
echo "after-attach=$?="; read -r _
"#,
        run.id
    );
    let mut pty = Pty::script(&dist, ws.root(), &script, &[("HOME", &home)]);
    pty.wait_for("before-attach", SHORT);
    let before = pty.termios();
    assert!(before.is_cooked(), "{before:?}");
    pty.send(b"\n");
    pty.wait_for("//pkg:slow", SHORT);
    assert!(
        pty.dsr_queries() > 0,
        "the interactive TUI never engaged\n{}",
        pty.screen()
    );
    pty.send(b"q");
    pty.wait_for("after-attach=", SHORT);
    assert_eq!(pty.number_after("after-attach="), 0, "{}", pty.seen());
    assert_eq!(pty.termios(), before, "attach left the terminal changed");
    assert!(pty.cursor_visible(), "cursor left hidden\n{}", pty.screen());
    pty.send(b"\n");
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(0), "{}", pty.seen());

    let entry = list_entry(&ws, &dist, &run.id);
    assert_eq!(
        entry["state"], "running",
        "detach ended the session: {entry}"
    );
    assert!(run.wait().success(), "{}", run.stderr());
}

/// While a target holds the terminal the TUI is paused, and the run still
/// records: the target's `ExecuteStart` reaches `events.ndjson` while it runs.
///
/// A lone target sends nothing between its pause and its resume, so this sees
/// what was queued as the pause began; a run that builds while paused has no
/// fixture here.
#[test]
fn records_while_paused() {
    let dist = Dist::locate();
    let ws = workspace();
    let home = ws.home().to_string_lossy().into_owned();
    let mut pty = Pty::spawn(&dist, ws.root(), &["run", "//pkg:hold"], &[("HOME", &home)]);
    // Printed by the target on the terminal: the TUI has stepped aside.
    pty.wait_for("hold-started", DEADLINE);

    let dirs: Vec<_> = std::fs::read_dir(sessions_dir(ws.home()))
        .expect("a sessions dir")
        .map(|e| e.expect("entry").path())
        .collect();
    let [dir] = dirs.as_slice() else {
        panic!("one session expected: {dirs:?}")
    };
    let started = Instant::now();
    while !recorded(dir, "ExecuteStart", "//pkg:hold") {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "nothing recorded while paused"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !pty.seen().contains("hold-done"),
        "the target finished before the check: it proves nothing\n{}",
        pty.seen()
    );
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(0), "{}", pty.seen());
}
