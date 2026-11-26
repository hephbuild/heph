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
use common::session::{Bg, SHORT, list_entry, recorded, session_dirs, sessions_dir};
use common::{Dist, Workspace};
use std::time::{Duration, Instant};

/// A release binary on a shared CI runner.
const DEADLINE: Duration = Duration::from_secs(120);

/// `slow` keeps a session live long enough to attach and detach; `hold` is a
/// single named target, so it gets the terminal and the TUI pauses for it;
/// `fail` fails at once, for a finished session to export.
const BUILD: &str = r#"
target(name = "slow", driver = "bash", run = "sleep 15", cache = False)
target(name = "hold", driver = "bash", run = 'echo hold-""started; sleep 6; echo hold-""done', cache = False)
target(name = "fail", driver = "bash", run = "exit 1", cache = False)
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

/// Spec row `attach_a_downloaded_file`: a finished run's session, exported
/// to a file (as a CI artifact is downloaded), is attached by path on a
/// terminal: it replays to its end, exits with the run's code, and hands the
/// terminal back as it found it. `export -o -` onto that terminal is
/// refused with 125: the file is binary.
#[test]
fn attach_a_downloaded_file() {
    let dist = Dist::locate();
    let ws = workspace();
    let run = ws.run(&dist, &["run", "//pkg:fail"]).expect("run");
    let code = run.status.code().expect("the run exited with a code");
    assert_ne!(code, 0, "{}", common::describe(&run));
    let id = common::session::id_in(&String::from_utf8_lossy(&run.stderr))
        .unwrap_or_else(|| panic!("no session announced\n{}", common::describe(&run)));
    let out = tempfile::tempdir().expect("tempdir");
    let path = out.path().join("downloaded.hephsession");
    let path_arg = path.to_str().expect("a UTF-8 temp path");
    let exported = ws
        .run(&dist, &["session", "export", &id, "-o", path_arg])
        .expect("export");
    assert!(exported.status.success(), "{}", common::describe(&exported));
    // The file stands alone: the session it came from is gone.
    std::fs::remove_dir_all(sessions_dir(ws.home())).expect("remove the sessions");

    let home = ws.home().to_string_lossy().into_owned();
    let script = format!(
        r#"
echo "before-""attach"; read -r _
"$HEPH" session attach '{path_arg}'
echo "attach=$?="; read -r _
"$HEPH" session export '{path_arg}' -o -
echo "piped=$?="; read -r _
"#
    );
    let mut pty = Pty::script(&dist, ws.root(), &script, &[("HOME", &home)]);
    pty.wait_for("before-attach", SHORT);
    let before = pty.termios();
    assert!(before.is_cooked(), "{before:?}");
    pty.send(b"\n");
    pty.wait_for("attach=", SHORT);
    assert_eq!(pty.number_after("attach="), code, "{}", pty.seen());
    let seen = pty.seen();
    assert!(
        seen.contains(&format!("heph: replaying {path_arg}")),
        "{seen}"
    );
    assert!(seen.contains("//pkg:fail"), "{seen}");
    assert_eq!(pty.termios(), before, "attach left the terminal changed");
    assert!(pty.cursor_visible(), "cursor left hidden\n{}", pty.screen());
    assert!(!pty.alternate_screen(), "{}", pty.screen());

    // The file exported again, onto the terminal: it opens, then the
    // terminal is refused, naming why.
    pty.send(b"\n");
    pty.wait_for("piped=", SHORT);
    assert_eq!(pty.number_after("piped="), 125, "{}", pty.seen());
    assert!(
        pty.seen().contains("not writing it to a terminal"),
        "{}",
        pty.seen()
    );
    pty.send(b"\n");
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(0), "{}", pty.seen());
}

/// The dashboard's hint row starts with this; nothing on the line says it.
const DASHBOARD_HINT: &str = "↑/↓ select";

fn on_dashboard(s: &vt100::Screen) -> bool {
    s.alternate_screen() && common::pty::screen_rows(s).any(|r| r.contains(DASHBOARD_HINT))
}

/// `//pkg:gated` runs until `open` exists; `//pkg:fails` until `fail` does,
/// then fails.
fn gated_workspace() -> (Workspace, std::path::PathBuf, std::path::PathBuf) {
    let ws = Workspace::new().expect("workspace");
    let open = ws.root().join("gate-open");
    let fail = ws.root().join("gate-fail");
    let wait =
        |gate: &std::path::Path| format!("while [ ! -e {} ]; do sleep 0.1; done", gate.display());
    ws.write(
        "pkg/BUILD",
        &format!(
            concat!(
                "target(name = \"gated\", driver = \"bash\", cache = False, run = [\"{}\"])\n",
                "target(name = \"fails\", driver = \"bash\", cache = False, run = [\"{}\", \"exit 3\"])\n",
            ),
            wait(&open),
            wait(&fail),
        ),
    )
    .expect("write BUILD");
    (ws, open, fail)
}

/// Wait until the next attach in `pty` has engaged the interactive TUI: it
/// has sent more cursor queries than the `before` it was started after.
fn wait_engaged(pty: &Pty, before: usize) {
    common::wait_until("the attached TUI to engage", SHORT, || {
        pty.dsr_queries() > before
    });
}

/// Attached, the full screens never stop the build: `q` on the dashboard
/// detaches with 0, and so does one ctrl-c (the attached clause of
/// `tui_pty::ctrl_c_twice_on_full_screens`: there is no second press to
/// arm, the viewer owns no build); the run goes on and succeeds. Attached
/// at a failing build's end, with the dashboard open, the viewer exits with
/// the build's code instead of holding. `-D` is refused at once, naming
/// the flag: a viewer has no build to open the dashboard on.
#[test]
fn attach_detaches_without_stopping() {
    let dist = Dist::locate();
    let (ws, open, fail) = gated_workspace();
    let home = ws.home().to_string_lossy().into_owned();
    let mut run = Bg::spawn(&ws, &dist, &["run", "//pkg:gated"]);
    run.wait_executing("//pkg:gated");

    let refused = ws
        .cmd(&dist, &["session", "attach", "-D", &run.id])
        .output()
        .expect("run attach -D");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success(), "attach -D succeeded\n{stderr}");
    assert!(stderr.contains("--dashboard"), "{stderr}");

    let script = format!(
        r#"
echo "before-""attach"; read -r _
"$HEPH" session attach {id}
echo "after-q=$?="; read -r _
"$HEPH" session attach {id}
echo "after-ctrl-c=$?="; read -r _
"#,
        id = run.id
    );
    let mut pty = Pty::script(&dist, ws.root(), &script, &[("HOME", &home)]);
    pty.wait_for("before-attach", SHORT);
    pty.send(b"\n");
    wait_engaged(&pty, 0);
    pty.wait_for("//pkg:gated", SHORT);
    pty.send(b"\t");
    pty.wait_screen("the attached dashboard", SHORT, on_dashboard);
    pty.send(b"q");
    pty.wait_for("after-q=", SHORT);
    assert_eq!(pty.number_after("after-q="), 0, "{}", pty.seen());
    assert!(
        !pty.alternate_screen(),
        "q left the dashboard up\n{}",
        pty.screen()
    );

    let queries = pty.dsr_queries();
    pty.send(b"\n");
    wait_engaged(&pty, queries);
    pty.send(b"\t");
    pty.wait_screen("the attached dashboard", SHORT, on_dashboard);
    pty.send(b"\x03");
    pty.wait_for("after-ctrl-c=", SHORT);
    assert_eq!(pty.number_after("after-ctrl-c="), 0, "{}", pty.seen());
    pty.send(b"\n");
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(0), "{}", pty.seen());

    let entry = list_entry(&ws, &dist, &run.id);
    assert_eq!(
        entry["state"], "running",
        "a detach stopped the build: {entry}"
    );
    std::fs::write(&open, b"").expect("open the gate");
    assert!(run.wait().success(), "{}", run.stderr());

    // Attached when a failing build ends, on the dashboard.
    let mut run = Bg::spawn(&ws, &dist, &["run", "//pkg:fails"]);
    run.wait_executing("//pkg:fails");
    let script = format!(
        r#"
echo "before-""attach"; read -r _
"$HEPH" session attach {}
echo "after-end=$?="; read -r _
"#,
        run.id
    );
    let mut pty = Pty::script(&dist, ws.root(), &script, &[("HOME", &home)]);
    pty.wait_for("before-attach", SHORT);
    pty.send(b"\n");
    wait_engaged(&pty, 0);
    pty.wait_for("//pkg:fails", SHORT);
    pty.send(b"\t");
    pty.wait_screen("the attached dashboard", SHORT, on_dashboard);
    std::fs::write(&fail, b"").expect("open the failing gate");
    let status = run.wait();
    let code = status.code().expect("the run exited with a code");
    assert_ne!(code, 0, "the failing build succeeded\n{}", run.stderr());
    pty.wait_for("after-end=", SHORT);
    assert_eq!(pty.number_after("after-end="), code, "{}", pty.seen());
    assert!(
        !pty.alternate_screen(),
        "the dashboard was left up\n{}",
        pty.screen()
    );
    pty.send(b"\n");
    assert_eq!(pty.wait_exit(DEADLINE), Ended::Code(0), "{}", pty.seen());
}

/// While a target holds the terminal the TUI is paused, and the run still
/// records: the target's `ExecuteStart` reaches `session.db` while it runs.
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

    // The root's catalog and cleanup stamp are not sessions.
    let dirs = session_dirs(&sessions_dir(ws.home()));
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
