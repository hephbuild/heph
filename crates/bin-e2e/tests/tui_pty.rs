//! The interactive TUI, driven through a real PTY.
//!
//! The interactive renderer engages only when stderr is a terminal, so a linked
//! test — which has no controlling terminal — always takes the CI line backend
//! and never executes a single line of this code. What is tested here is what
//! only a terminal can show: that the TUI builds its viewport, renders the run
//! to actual cells, and hands the terminal back on exit.
//!
//! The line is a ratatui *inline* viewport — it draws in place below the
//! prompt and clears itself on exit — so its signals are the inline handshake
//! (a cursor-position query the terminal must answer) and the cursor hide/show
//! pair. The dashboard (`tab`) is a full screen on the alternate screen, and
//! the tests from [`Step`] on drive it as a person would: wait for a screen,
//! type, resize, and read the vt100 cells left behind.
//!
//! Terminal restore is the one that matters most in practice. A TUI that exits
//! leaving the cursor hidden and the viewport painted leaves the user's shell a
//! mess, and every non-PTY test in the repo passes while it does.

mod common;

// `DSR_CURSOR` is the cursor-position query. Only the interactive backend ever
// issues one, which makes it the marker for "the TUI engaged"; answering every
// occurrence is the harness's job — see `common::take_dsr_queries`.
use common::{DSR_CURSOR, Dist};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read as _, Write as _};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// CSI ?25l / ?25h — hide and show the cursor, around the TUI's lifetime.
const CURSOR_HIDE: &[u8] = b"\x1b[?25l";
const CURSOR_SHOW: &[u8] = b"\x1b[?25h";

/// Generous: this spawns a release binary that builds a target, on a shared CI
/// runner. Long enough never to be the flake, short enough to fail the job
/// rather than hang it.
const DEADLINE: Duration = Duration::from_secs(180);

/// The sleep is in a *dependency*, not in the target the command names, and
/// that is the whole trick.
///
/// A run that names one target hands that target the terminal: the engine wraps
/// its execution in the interactive wrapper, which **pauses the TUI for the
/// target's entire runtime** so the target owns the terminal. So the box is only
/// on screen before execution starts — and with the sleep in the named target
/// itself, that window was "however long discovery, BUILD evaluation and hashing
/// take", against an 80 ms frame tick. On a warm, fast runner that window closed
/// before the first tick and the run drew *zero* frames: the terminal received a
/// viewport, an immediate collapse, the target's own `e2e-ok`, and the final
/// summary, with nothing rendered in between. That is the flake this shape
/// removes, and it read as "the target never appeared" because the box carrying
/// its name was never painted at all.
///
/// Dependencies never inherit the terminal (`ResultOptions::default()`), so
/// while `//pkg:slow` runs the TUI stays up and ticking — a guaranteed second of
/// frames, all of them carrying the run's label in the box footer.
const BUILD: &str = concat!(
    "target(name = \"slow\", driver = \"bash\", run = \"sleep 1\", cache = False)\n",
    "target(name = \"ok\", driver = \"bash\", run = \"echo e2e-ok\", cache = False,",
    " deps = {\"slow\": [\"//pkg:slow\"]})\n",
);

#[test]
fn tui_renders_the_run_and_restores_the_terminal() {
    let dist = Dist::locate();
    let ws = common::Workspace::new().expect("workspace");
    ws.write("pkg/BUILD", BUILD).expect("write BUILD");

    let session = run_in_pty(&dist, ws.root(), &["run", "//pkg:ok"]);

    assert!(
        session.status_success,
        "run failed under a tty\n{}",
        session.report()
    );
    assert!(
        contains(&session.raw, DSR_CURSOR),
        "interactive TUI never engaged with a tty attached\n{}",
        session.report()
    );
    // The vt100-rendered screens, not the raw bytes: this is the assertion the
    // file exists for — that the viewport put the run into actual cells — and
    // with the box up for the dependency's whole runtime there are frames enough
    // to catch it at a read boundary. (It was weakened to a raw-byte scan when
    // the box was only up for a race-length window; the fix for that was the
    // window, not the assertion.)
    // The line: its done count, the one segment that is up for the whole run
    // (the label shows only while the matcher streams).
    assert!(
        session.rendered.contains("/ 1 done"),
        "the line never appeared in the rendered output\n{}",
        session.report()
    );

    // Handed the terminal back: the last thing the TUI does with the cursor is
    // show it again. A crash or a missing teardown leaves the final hide
    // unmatched and the user typing blind into an invisible prompt.
    let hid = last_index(&session.raw, CURSOR_HIDE);
    let shown = last_index(&session.raw, CURSOR_SHOW);
    assert!(
        hid.is_some(),
        "TUI never hid the cursor\n{}",
        session.report()
    );
    assert!(
        shown > hid,
        "exited with the cursor still hidden\n{}",
        session.report()
    );
}

/// `heph auth status` probes credentials, and a probe under a `runner` builds
/// that runner — so it runs under the same TUI as `run`. Under a terminal that
/// TUI must engage, must not swallow the table (it is printed in a pause, onto
/// the terminal, after the probes), and must hand the cursor back.
#[test]
fn auth_status_prints_its_table_and_restores_the_terminal() {
    let dist = Dist::locate();
    let ws = common::Workspace::new().expect("workspace");
    ws.write(
        "auth/BUILD",
        r#"target(name = "ok", driver = "auth.credential",
       sources = [heph.auth.exec(["sh", "-c", "printf 'v'"])],
       present = {"env": {"OK": "${value}"}})"#,
    )
    .expect("write BUILD");

    let session = run_in_pty(&dist, ws.root(), &["auth", "status", "//auth/..."]);

    assert!(session.status_success, "{}", session.report());
    assert!(
        contains(&session.raw, DSR_CURSOR),
        "auth status never engaged the TUI with a tty attached\n{}",
        session.report()
    );
    assert!(
        session.rendered.contains("//auth:ok") && session.rendered.contains("exec(sh)"),
        "the status table never reached the terminal\n{}",
        session.report()
    );
    assert_cursor_restored(&session);
}

/// `heph auth login` hands the terminal to each vendor sign-in command and takes
/// it back afterwards. Two stale credentials, so the TUI goes down, comes back
/// up between them, and goes down again: a resume that wedged (the crossterm
/// `EventStream` rebuild has, before) shows here as a child that never exits.
#[test]
fn auth_login_lends_the_terminal_to_each_sign_in_and_restores_it() {
    let dist = Dist::locate();
    let ws = common::Workspace::new().expect("workspace");
    ws.write(
        "auth/BUILD",
        r#"
target(name = "a", driver = "auth.credential",
       sources = [heph.auth.exec(["heph-e2e-no-such-tool"],
                                 login = [["sh", "-c", "echo e2e-login-a"]])],
       present = {"env": {"A": "${value}"}})
target(name = "b", driver = "auth.credential",
       sources = [heph.auth.exec(["heph-e2e-no-such-tool"],
                                 login = [["sh", "-c", "echo e2e-login-b"]])],
       present = {"env": {"B": "${value}"}})
"#,
    )
    .expect("write BUILD");

    let session = run_in_pty(&dist, ws.root(), &["auth", "login"]);

    assert!(session.status_success, "{}", session.report());
    assert!(
        contains(&session.raw, DSR_CURSOR),
        "auth login never engaged the TUI with a tty attached\n{}",
        session.report()
    );
    for marker in ["e2e-login-a", "e2e-login-b"] {
        assert!(
            session.rendered.contains(marker),
            "the sign-in command's output ({marker}) never reached the terminal\n{}",
            session.report()
        );
    }
    assert_cursor_restored(&session);
}

/// The last thing the TUI does with the cursor is show it again. A crash or a
/// missing teardown leaves the final hide unmatched and the user typing blind.
fn assert_cursor_restored(session: &Session) {
    let hid = last_index(&session.raw, CURSOR_HIDE);
    let shown = last_index(&session.raw, CURSOR_SHOW);
    assert!(
        hid.is_some(),
        "TUI never hid the cursor\n{}",
        session.report()
    );
    assert!(
        shown > hid,
        "exited with the cursor still hidden\n{}",
        session.report()
    );
}

/// A run prints its results in a last pause, and nothing is drawn after it, so
/// the resume that follows must not rebuild the viewport. Rebuilding costs a
/// cursor query, and the run is over before a slow terminal (a remote box)
/// answers it. That reply then reached the user's shell as `^[[35;1R`, and on
/// any terminal the exit waited one round-trip for a box nobody saw.
#[test]
fn no_cursor_query_after_the_results_are_printed() {
    let dist = Dist::locate();
    let ws = common::Workspace::new().expect("workspace");
    ws.write(
        "pkg/BUILD",
        "target(name = \"out\", driver = \"bash\", run = \"echo e2e-final-output > out.txt\", \
         out = \"out.txt\", cache = False)\n",
    )
    .expect("write BUILD");

    let session = run_in_pty(&dist, ws.root(), &["run", "--cat-out", "//pkg:out"]);

    assert!(
        session.status_success,
        "run failed under a tty\n{}",
        session.report()
    );
    let printed = last_index(&session.raw, b"e2e-final-output")
        .unwrap_or_else(|| panic!("--cat-out never printed the output\n{}", session.report()));
    assert!(
        contains(&session.raw, DSR_CURSOR),
        "interactive TUI never engaged with a tty attached\n{}",
        session.report()
    );
    let after = session.raw.get(printed..).unwrap_or_default();
    assert!(
        !contains(after, DSR_CURSOR),
        "queried the cursor after printing the results\n{}",
        session.report()
    );
}

/// With `--no-tui` the interactive renderer must stay off even though stderr
/// *is* a terminal — the escape hatch for users piping through a tool that
/// allocates a tty, and the only configuration in which the flag can be
/// observed to do anything at all.
#[test]
fn no_tui_flag_wins_over_an_attached_terminal() {
    let dist = Dist::locate();
    let ws = common::Workspace::new().expect("workspace");
    ws.write(
        "pkg/BUILD",
        "target(name = \"ok\", driver = \"bash\", run = \"echo e2e-ok\", cache = False)\n",
    )
    .expect("write BUILD");

    let session = run_in_pty(&dist, ws.root(), &["run", "--no-tui", "//pkg:ok"]);

    assert!(session.status_success, "{}", session.report());
    assert!(
        !contains(&session.raw, DSR_CURSOR),
        "--no-tui still built the inline viewport\n{}",
        session.report()
    );
    assert!(
        !contains(&session.raw, CURSOR_HIDE),
        "--no-tui still took the cursor\n{}",
        session.report()
    );
}

/// With stdout piped into a reader, the interactive renderer must stay off even
/// though stderr is a terminal. The reader prints to that same terminal while
/// the TUI holds it raw, so `heph q //p | grep x` printed its matches as a
/// staircase with the TUI's redraws interleaved into them. The matches must
/// still come through, and stderr falls back to the line backend.
#[test]
fn stdout_into_a_pipe_keeps_the_tui_off() {
    let dist = Dist::locate();
    let ws = common::Workspace::new().expect("workspace");
    ws.write("pkg/BUILD", BUILD).expect("write BUILD");

    let session = run_in_pty_piped(&dist, ws.root(), &["q", "all", "//pkg"]);

    assert!(session.status_success, "{}", session.report());
    assert!(
        !contains(&session.raw, DSR_CURSOR),
        "a piped stdout still built the inline viewport\n{}",
        session.report()
    );
    assert!(
        !contains(&session.raw, CURSOR_HIDE),
        "a piped stdout still took the cursor\n{}",
        session.report()
    );
    // Cooked mode: the reader's newlines come back as CRLF, so each address
    // starts its own line rather than where the last one ended.
    assert!(
        contains(&session.raw, b"//pkg:ok\r\n") && contains(&session.raw, b"//pkg:slow\r\n"),
        "the matches did not reach the terminal as whole lines\n{}",
        session.report()
    );
}

/// The named target announces itself, then sleeps far longer than the test can
/// wait — so "the run ended" can only mean the Ctrl-C ended it. `cache = False`
/// keeps a second run from short-circuiting to a cache hit.
const SLEEPS_FOREVER: &str = concat!(
    "target(name = \"slow\", driver = \"bash\", cache = False,",
    " run = [\"echo e2e-running\", \"sleep 600\"])\n",
);

/// Ctrl-C must cancel a run whose *named* target holds the terminal.
///
/// Naming a single target hands it the terminal, which pauses the TUI for that
/// target's entire runtime (see the `BUILD` const at the top of this file, which
/// leans on the same behaviour for the opposite reason). That pause used to suppress the
/// shutdown trigger wholesale, and the two producers fail in opposite
/// directions: the TUI's key handler cannot see the press (the event stream is
/// dropped while paused) and the kernel SIGINT — which the cooked-mode terminal
/// does deliver — was thrown away by the suppression. So Ctrl-C was inert for
/// the whole run, and so was the second-press abort behind it. Suppression is
/// now scoped to a pause that hands the *keyboard* over (`--shell`,
/// `PauseFor::Input`), and `shell_pty.rs` owns that side.
///
/// PTY-only by construction, twice over: the interactive backend engages only
/// on a tty, and a Ctrl-C is only a SIGINT when it comes from a controlling
/// terminal with a foreground process group. A linked test has neither, so it
/// would pass on the broken code without executing any of it.
#[test]
fn ctrl_c_cancels_a_run_whose_target_holds_the_terminal() {
    let dist = Dist::locate();
    let ws = common::Workspace::new().expect("workspace");
    ws.write("pkg/BUILD", SLEEPS_FOREVER).expect("write BUILD");

    // 90s: long enough for a release binary to reach the target on a cold, busy
    // runner, short enough that an ignored Ctrl-C fails the job rather than
    // sitting on it for the full 600s sleep.
    let session = run_in_pty_typing(
        &dist,
        ws.root(),
        &["run", "//pkg:slow"],
        Some(Interact {
            marker: "e2e-running".to_string(),
            send: vec![0x03],
        }),
        Duration::from_secs(90),
    );

    // Without this the test could pass on a run that died before it ever
    // executed the target — no Ctrl-C sent, nothing about cancellation proved.
    assert!(
        session.sent_input,
        "the target never reached its sleep, so no ctrl-c was ever sent\n{}",
        session.report()
    );
    assert!(
        !session.status_success,
        "a cancelled run reported success\n{}",
        session.report()
    );

    // Cancelling is not licence to wreck the terminal: the TUI resumes out of
    // the pause on its way out, and must still hand the cursor back.
    let hid = last_index(&session.raw, CURSOR_HIDE);
    let shown = last_index(&session.raw, CURSOR_SHOW);
    assert!(
        hid.is_some(),
        "TUI never hid the cursor\n{}",
        session.report()
    );
    assert!(
        shown > hid,
        "cancelled run exited with the cursor still hidden\n{}",
        session.report()
    );
}

/// What a test does at the terminal, one step at a time.
enum Step<'a> {
    /// Block until the screen as it is now passes the check, named for the
    /// failure message.
    Until(&'a str, fn(&vt100::Screen) -> bool),
    /// Type these bytes.
    Keys(&'a [u8]),
    /// Resize the window, as a terminal emulator does.
    Resize(u16, u16),
    /// Let a few frame ticks pass: after a resize, so that a check that the
    /// stale screen already passes is made against a redrawn one.
    Settle,
    /// Open the gate the build's dependency waits on: the build may finish.
    OpenGate,
    /// Keep the rows on screen now, under this name.
    Snapshot(&'a str),
    /// Anything else the test does between two steps.
    Do(&'a (dyn Fn() + Sync)),
}

const TAB: &[u8] = b"\t";
const ESC: &[u8] = b"\x1b";
const CTRL_C: &[u8] = b"\x03";
/// The dashboard's hint row starts with this, live or finished, at any width
/// these tests use; nothing on the line says it.
const DASHBOARD_HINT: &str = "↑/↓ select";
/// The hint row once the run has finished under the dashboard.
const FINISHED_HINT: &str = "q or esc leave";
/// What the named target prints, on the main screen, while the TUI is paused
/// for it.
const TARGET_OUT: &str = "tty-target-out";

/// A build that waits for the test: `//pkg:gate` loops until its file exists
/// (ignoring the INT and TERM a stop sends, so the hard abort has something to
/// abort), and `//pkg:ok`, the named target, holds the terminal for a second —
/// so every run here also pauses the TUI for a target and resumes it.
fn gated_build(gate: &std::path::Path) -> String {
    format!(
        concat!(
            "target(name = \"gate\", driver = \"bash\", cache = False,",
            " run = [\"trap '' INT TERM\", \"while [ ! -e {gate} ]; do sleep 0.1; done\"])\n",
            "target(name = \"ok\", driver = \"bash\", cache = False,",
            " run = [\"echo {out}\", \"sleep 1\"], deps = {{\"gate\": [\"//pkg:gate\"]}})\n",
        ),
        gate = gate.display(),
        out = TARGET_OUT,
    )
}

/// A terminal with a gated build in it.
struct Gated {
    pty: common::pty::Pty,
    gate: std::path::PathBuf,
    _ws: common::Workspace,
}

impl Gated {
    /// `prelude` runs in the shell first (output above the line), then
    /// `heph run //pkg:ok` replaces it.
    fn start(prelude: &str, env: &[(&str, &str)]) -> Self {
        let dist = Dist::locate();
        let ws = common::Workspace::new().expect("workspace");
        let gate = ws.root().join("gate-open");
        ws.write("pkg/BUILD", &gated_build(&gate))
            .expect("write BUILD");
        let script = format!("{prelude}exec \"$HEPH\" run //pkg:ok");
        let pty = common::pty::Pty::script_at_cursor(&dist, ws.root(), &script, env);
        Self { pty, gate, _ws: ws }
    }

    /// Play `steps`; the snapshots they took, by name.
    fn play<'a>(&self, steps: &[Step<'a>]) -> Vec<(&'a str, Vec<String>)> {
        let mut snapshots = Vec::new();
        for step in steps {
            match step {
                Step::Until(what, ok) => self.pty.wait_screen(what, DEADLINE, ok),
                Step::Keys(bytes) => self.pty.send(bytes),
                Step::Resize(rows, cols) => self.pty.resize(*rows, *cols),
                Step::Settle => std::thread::sleep(Duration::from_millis(400)),
                Step::OpenGate => std::fs::write(&self.gate, b"").expect("open the gate"),
                Step::Snapshot(name) => snapshots.push((*name, self.pty.rows())),
                Step::Do(f) => f(),
            }
        }
        snapshots
    }

    /// Wait for heph to exit, then make sure the gate's loop ends too: a run
    /// that died before opening it leaves the dependency running.
    fn exit(&mut self) -> common::pty::Ended {
        let ended = self.pty.wait_exit(DEADLINE);
        drop(std::fs::write(&self.gate, b""));
        ended
    }
}

fn rows_of(s: &vt100::Screen) -> impl Iterator<Item = String> + '_ {
    s.rows(0, s.size().1)
}

fn count_rows(s: &vt100::Screen, needle: &str) -> usize {
    rows_of(s).filter(|r| r.contains(needle)).count()
}

/// The line is up: the main screen, with its done count.
fn on_line(s: &vt100::Screen) -> bool {
    !s.alternate_screen() && count_rows(s, " done") > 0
}

/// The line is up exactly once, and no dashboard row was left behind.
fn on_line_once(s: &vt100::Screen) -> bool {
    !s.alternate_screen() && count_rows(s, " done") == 1 && count_rows(s, DASHBOARD_HINT) == 0
}

fn on_dashboard(s: &vt100::Screen) -> bool {
    s.alternate_screen() && count_rows(s, DASHBOARD_HINT) == 1
}

/// The dashboard fills the window it has now: its hint row is the last row,
/// and there is one of it.
fn dashboard_fits(s: &vt100::Screen) -> bool {
    let last = rows_of(s).last().unwrap_or_default();
    on_dashboard(s) && last.contains(DASHBOARD_HINT)
}

fn on_finished_dashboard(s: &vt100::Screen) -> bool {
    s.alternate_screen() && count_rows(s, FINISHED_HINT) == 1
}

/// Paused for the named target: the main screen, with what it printed.
fn paused_for_target(s: &vt100::Screen) -> bool {
    !s.alternate_screen() && count_rows(s, TARGET_OUT) > 0
}

/// Once heph is gone: the main screen, a visible cursor, no mouse mode, and
/// none of the dashboard drawn into the scrollback.
fn assert_restored(pty: &common::pty::Pty, case: &str) {
    let screen = pty.screen();
    pty.with_screen(|s| {
        assert!(
            !s.alternate_screen(),
            "{case}: left on the alternate screen\n{screen}"
        );
        assert!(!s.hide_cursor(), "{case}: cursor left hidden\n{screen}");
        assert_eq!(
            s.mouse_protocol_mode(),
            vt100::MouseProtocolMode::None,
            "{case}: mouse capture left on\n{screen}"
        );
        assert_eq!(
            count_rows(s, DASHBOARD_HINT),
            0,
            "{case}: the dashboard was drawn into the scrollback\n{screen}"
        );
    });
}

/// `tab` puts the dashboard on the alternate screen and `esc` takes it off,
/// and the main screen comes back cell for cell: what was printed above the
/// line, and the line on the same row.
///
/// It is also the Q11 proof, as far as an emulator goes: the sequence pair is
/// `?1049h`/`?1049l` (save the cursor and switch; switch back and restore it),
/// sent while an inline viewport owns the bottom of the main screen. The line
/// coming back on its own row is what says the inline viewport's idea of where
/// it is survived the switch. vt100 implements xterm's 1049; tmux and the
/// macOS Terminal are not emulated here.
#[test]
fn tab_opens_and_esc_restores_scrollback() {
    let mut g = Gated::start("printf 'scrollback-%s\\n' 1 2 3 4 5; ", &[]);
    let shots = g.play(&[
        Step::Until("the line", on_line),
        Step::Snapshot("before"),
        Step::Keys(TAB),
        Step::Until("the dashboard", on_dashboard),
        Step::Keys(ESC),
        Step::Until("the line, once", on_line_once),
        Step::Snapshot("after"),
        Step::OpenGate,
    ]);
    let ended = g.exit();
    assert_eq!(ended, common::pty::Ended::Code(0), "{}", g.pty.seen());
    assert_restored(&g.pty, "esc then build end");

    let [(_, before), (_, after)] = shots.as_slice() else {
        panic!("two snapshots");
    };
    let report = format!(
        "--- before ---\n{}\n--- after ---\n{}",
        before.join("\n"),
        after.join("\n")
    );
    // Above the line: the prelude, where the shell printed it. Its last row
    // being found at all says the line anchored below it, not over it.
    let above = before
        .iter()
        .position(|r| r.contains("scrollback-5"))
        .unwrap_or_else(|| panic!("the prelude is not on screen\n{report}"));
    assert_eq!(
        before.get(..=above),
        after.get(..=above),
        "the main screen above the line changed across the dashboard\n{report}"
    );
    let line_row = |rows: &[String]| rows.iter().position(|r| r.contains(" done"));
    assert_eq!(
        line_row(before),
        line_row(after),
        "the line came back on another row\n{report}"
    );

    // Q11: the sequences the switch used. Bytes, not cells, because the
    // question is which sequence it was.
    let out = g.pty.output();
    assert!(
        contains(&out, b"\x1b[?1049h"),
        "never entered with ?1049h\n{report}"
    );
    assert!(
        contains(&out, b"\x1b[?1049l"),
        "never left with ?1049l\n{report}"
    );
}

/// The numbers of heph's debug-knob lines on screen, top to bottom.
fn debug_log_numbers(rows: &[String]) -> Vec<u64> {
    rows.iter()
        .filter_map(|r| {
            let rest = r.split("heph-debug-log ").nth(1)?;
            rest.split_whitespace().next()?.parse().ok()
        })
        .collect()
}

/// I3 on the screen: heph's lines written while the dashboard is up — an
/// `INFO`, a `DEBUG` let through by `HEPH_LOG` (the `-v` case) and a
/// diagnostic, in turn, from a thread writing one every 40 ms from before
/// `tab` until after `esc` — are printed on return, once each and in order,
/// with none lost around either key. The backend drops them while the screen
/// is up, so the only way back is the replay from `events.ndjson`. The same
/// with `HEPH_NO_SESSION=1`, whose session directory is private.
///
/// Not covered here: `events.ndjson` past its cap (the binary has no knob
/// for a small cap; `LogSink::replay` past the cap is a unit test).
#[test]
fn logs_while_dashboard_open_flush_in_order() {
    let run = |name: &str, no_session: bool| {
        let logs = tempfile::tempdir().expect("tempdir");
        let on = logs.path().join("emit");
        let on_str = on.to_str().expect("utf-8 path").to_owned();
        let mut env = vec![
            ("HEPH_DEBUG_TUI_LOG", on_str.as_str()),
            (
                "HEPH_LOG",
                "info,fuser=error,object_store=warn,heph_debug_tui_log=debug",
            ),
        ];
        if no_session {
            env.push(("HEPH_NO_SESSION", "1"));
        }
        let mut g = Gated::start("", &env);
        let start = || std::fs::write(&on, b"").expect("start the lines");
        let stop = || std::fs::remove_file(&on).expect("stop the lines");
        let shots = g.play(&[
            Step::Until("the line", on_line),
            // Room for every line the run prints, so the screen holds them all.
            Step::Resize(100, 140),
            Step::Settle,
            Step::Until("one line at 100x140", on_line_once),
            Step::Do(&start),
            Step::Settle,
            Step::Snapshot("before tab"),
            Step::Keys(TAB),
            Step::Until("the dashboard", on_dashboard),
            Step::Settle,
            Step::Settle,
            Step::Keys(ESC),
            Step::Until("the line, once", on_line_once),
            Step::Settle,
            Step::Do(&stop),
            Step::Settle,
            Step::Snapshot("stopped"),
            Step::OpenGate,
        ]);
        let ended = g.exit();
        assert_eq!(
            ended,
            common::pty::Ended::Code(0),
            "{name}: exit\n{}",
            g.pty.seen()
        );
        let [(_, before), (_, stopped)] = shots.as_slice() else {
            panic!("two snapshots");
        };
        let screen = g.pty.screen();
        let before_tab = debug_log_numbers(before).last().copied().unwrap_or(0);
        let printed = debug_log_numbers(stopped);
        let last = printed.last().copied().unwrap_or(0);
        assert_eq!(
            printed,
            (1..=last).collect::<Vec<_>>(),
            "{name}: heph's lines are not each printed once, in order\n{}",
            stopped.join("\n")
        );
        // The dashboard was up for two settles: lines were written under it.
        assert!(
            last >= before_tab + 10,
            "{name}: too few lines written while the dashboard was up \
             ({before_tab} before tab, {last} in all)\n{screen}"
        );
        for kind in ["info", "debug", "diagnostic"] {
            assert!(
                stopped
                    .iter()
                    .any(|r| r.contains("heph-debug-log") && r.trim_end().ends_with(kind)),
                "{name}: no {kind} line\n{screen}"
            );
        }
        // Once heph is gone, nothing more and nothing again.
        let rows: Vec<String> = screen.lines().map(str::to_owned).collect();
        assert_eq!(
            debug_log_numbers(&rows),
            printed,
            "{name}: lines changed after the run\n{screen}"
        );
        assert_restored(&g.pty, name);
    };
    let failed: Vec<&str> = std::thread::scope(|s| {
        let handles = [("session", false), ("HEPH_NO_SESSION=1", true)]
            .map(|(name, no_session)| (name, s.spawn(move || run(name, no_session))));
        handles
            .into_iter()
            .filter_map(|(name, h)| h.join().is_err().then_some(name))
            .collect()
    });
    assert!(failed.is_empty(), "lines lost or repeated with: {failed:?}");
}

/// The build ends with the dashboard up: it stays, saying how to leave, until
/// `q` — which exits 0 and prints the summary. On the way, the named target
/// took the terminal: the dashboard left for it and came back.
#[test]
fn finish_while_dashboard_open_holds() {
    let mut g = Gated::start("", &[]);
    g.play(&[
        Step::Until("the line", on_line),
        Step::Keys(TAB),
        Step::Until("the dashboard", on_dashboard),
        Step::OpenGate,
        Step::Until("the main screen, for the target", paused_for_target),
        Step::Until("the finished dashboard", on_finished_dashboard),
    ]);
    // Held: still up, and heph still running, well after the build ended.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        common::alive(g.pty.pid()),
        "heph exited under the dashboard"
    );
    assert!(
        g.pty.alternate_screen(),
        "the dashboard closed on its own\n{}",
        g.pty.screen()
    );

    g.pty.send(b"q");
    let ended = g.exit();
    assert_eq!(ended, common::pty::Ended::Code(0), "{}", g.pty.seen());
    assert_restored(&g.pty, "q after finish");
    let screen = g.pty.screen();
    assert!(screen.contains("1 / 1 done"), "no summary\n{screen}");
    assert!(
        screen.contains(TARGET_OUT),
        "the target's output did not stay on the main screen\n{screen}"
    );
}

/// Shrink and grow on the line and on the dashboard: each redraw fills the
/// window it has, once, with nothing left over from the old size.
#[test]
fn resize_in_each_mode() {
    let mut g = Gated::start("", &[]);
    g.play(&[
        Step::Until("the line", on_line),
        Step::Resize(20, 80),
        Step::Settle,
        Step::Until("one line at 20x80", on_line_once),
        Step::Resize(40, 140),
        Step::Settle,
        Step::Until("one line at 40x140", on_line_once),
        Step::Keys(TAB),
        Step::Until("the dashboard", on_dashboard),
        Step::Resize(20, 80),
        Step::Until("the dashboard filling 20x80", dashboard_fits),
        Step::Resize(40, 140),
        Step::Until("the dashboard filling 40x140", dashboard_fits),
        Step::Resize(24, 100),
        Step::Keys(ESC),
        Step::Settle,
        Step::Until("one line at 24x100, after the dashboard", on_line_once),
        Step::OpenGate,
    ]);
    let ended = g.exit();
    assert_eq!(ended, common::pty::Ended::Code(0), "{}", g.pty.seen());
    assert_restored(&g.pty, "after resizes");
}

/// Every way out of a run with the dashboard up hands the terminal back: the
/// main screen, a visible cursor, no mouse mode, and no dashboard in the
/// scrollback.
#[test]
fn terminal_restored_on_every_exit() {
    struct Case {
        name: &'static str,
        env: &'static [(&'static str, &'static str)],
        steps: &'static [Step<'static>],
        /// The exit status, when the case fixes one.
        code: Option<u32>,
        /// What must be on the main screen once heph is gone.
        shows: &'static [&'static str],
    }
    const OPEN: &[Step<'static>] = &[
        Step::Until("the line", on_line),
        Step::Keys(TAB),
        Step::Until("the dashboard", on_dashboard),
    ];
    let cases = [
        Case {
            name: "q",
            env: &[],
            steps: &[
                Step::OpenGate,
                Step::Until("the finished dashboard", on_finished_dashboard),
                Step::Keys(b"q"),
            ],
            code: Some(0),
            shows: &["1 / 1 done"],
        },
        Case {
            name: "esc after the build ends",
            env: &[],
            steps: &[
                Step::OpenGate,
                Step::Until("the finished dashboard", on_finished_dashboard),
                Step::Keys(ESC),
            ],
            code: Some(0),
            shows: &["1 / 1 done"],
        },
        Case {
            name: "the build's end, back on the line",
            env: &[],
            steps: &[
                Step::Keys(ESC),
                Step::Until("the line", on_line_once),
                Step::OpenGate,
            ],
            code: Some(0),
            shows: &["1 / 1 done"],
        },
        Case {
            name: "pause for the target and resume",
            env: &[],
            steps: &[
                Step::OpenGate,
                Step::Until("the main screen, for the target", paused_for_target),
                Step::Until("the dashboard back", on_finished_dashboard),
                Step::Keys(b"q"),
            ],
            code: Some(0),
            shows: &[TARGET_OUT],
        },
        Case {
            name: "ctrl-c",
            env: &[],
            // Two presses on a full screen: the first only arms. The gate
            // ignores the stop; opening it lets the stop finish.
            steps: &[Step::Keys(CTRL_C), Step::Keys(CTRL_C), Step::OpenGate],
            code: None,
            shows: &[],
        },
        Case {
            name: "hard abort (ctrl-c past the stop)",
            env: &[],
            // Arm, stop, then one press past the stop: it goes direct.
            steps: &[
                Step::Keys(CTRL_C),
                Step::Keys(CTRL_C),
                Step::Settle,
                Step::Keys(CTRL_C),
            ],
            code: Some(130),
            shows: &["aborted (second Ctrl-C)"],
        },
        Case {
            name: "a panic caught elsewhere",
            env: &[("HEPH_DEBUG_TUI_PANIC", "caught")],
            // The panic lands as the dashboard goes up and is caught; the
            // screen stays, and Esc is read as one key (raw mode), not held
            // for a newline.
            steps: &[
                Step::Settle,
                Step::Until("the dashboard, still up", on_dashboard),
                Step::Keys(ESC),
                Step::Until("the line, by a key read in raw mode", on_line_once),
                Step::OpenGate,
            ],
            code: Some(0),
            shows: &["1 / 1 done"],
        },
        Case {
            name: "a panic in the app task",
            env: &[("HEPH_DEBUG_TUI_PANIC", "app")],
            // The panic lands a second after the build: under the dashboard
            // again, not in the pause for the target (where the hook's own
            // print is already on the main screen and proves nothing).
            steps: &[
                Step::OpenGate,
                Step::Until("the main screen, for the target", paused_for_target),
                Step::Until("the dashboard back", on_dashboard),
            ],
            code: Some(101),
            shows: &["HEPH_DEBUG_TUI_PANIC=App: a deliberate panic"],
        },
        Case {
            name: "a panic in the backend",
            env: &[("HEPH_DEBUG_TUI_PANIC", "backend")],
            steps: &[
                Step::OpenGate,
                Step::Until("the main screen, for the target", paused_for_target),
                Step::Until("the finished dashboard", on_finished_dashboard),
            ],
            code: Some(101),
            shows: &["HEPH_DEBUG_TUI_PANIC=Backend: a deliberate panic"],
        },
    ];
    let run = |case: &Case| {
        let mut g = Gated::start("", case.env);
        g.play(OPEN);
        g.play(case.steps);
        let ended = g.exit();
        if let Some(code) = case.code {
            assert_eq!(
                ended,
                common::pty::Ended::Code(code),
                "{}: exit status\n{}",
                case.name,
                g.pty.seen()
            );
        }
        assert_restored(&g.pty, case.name);
        let screen = g.pty.screen();
        for text in case.shows {
            assert_eq!(
                screen.matches(text).count(),
                1,
                "{}: {text:?} not on the main screen once\n{screen}",
                case.name
            );
        }
    };
    // Each case on its own terminal, all at once; every failing case is
    // named (its panic message is printed above), not only the first.
    let failed: Vec<&str> = std::thread::scope(|s| {
        let handles: Vec<_> = cases
            .iter()
            .map(|case| (case.name, s.spawn(move || run(case))))
            .collect();
        handles
            .into_iter()
            .filter_map(|(name, h)| h.join().is_err().then_some(name))
            .collect()
    });
    assert!(
        failed.is_empty(),
        "the terminal was not restored after: {failed:?}"
    );
}

struct Session {
    status_success: bool,
    /// Whether the [`Interact`] marker was ever seen, and so the keystroke sent.
    /// Distinguishes "the run ignored Ctrl-C" from "the run never got far
    /// enough to be sent one".
    sent_input: bool,
    /// Every byte the child wrote to the pty.
    raw: Vec<u8>,
    /// Concatenation of the screen as rendered after each read — so an
    /// assertion sees text that was actually painted into cells at some point,
    /// not text that merely passed through as bytes.
    rendered: String,
}

impl Session {
    fn report(&self) -> String {
        format!(
            "--- rendered screens ---\n{}\n--- raw (lossy) ---\n{}",
            self.rendered,
            String::from_utf8_lossy(&self.raw)
        )
    }
}

fn run_in_pty(dist: &Dist, cwd: &std::path::Path, args: &[&str]) -> Session {
    run_in_pty_typing(dist, cwd, args, None, DEADLINE)
}

/// Type something at the running child: once `marker` shows up in its output,
/// `send` goes down the pty, once. The marker is what makes it a synchronisation
/// point rather than a sleep — the keystroke lands when the run has actually
/// reached the state under test.
struct Interact {
    marker: String,
    send: Vec<u8>,
}

/// `run_in_pty`, plus the ability to type at the child and its own deadline.
///
/// The keystroke is written from the reader thread, which already owns the pty
/// writer (it answers the TUI's cursor queries there) — a second handle would
/// have to be split off `take_writer`, which hands out exactly one.
fn run_in_pty_typing(
    dist: &Dist,
    cwd: &std::path::Path,
    args: &[&str],
    interact: Option<Interact>,
    deadline: Duration,
) -> Session {
    let home = tempfile::tempdir().expect("home tempdir");
    let mut cmd = CommandBuilder::new(dist.heph());
    for arg in args {
        cmd.arg(arg);
    }
    drive_pty(cmd, cwd, home.path(), interact, deadline)
}

/// `heph <args> | cat`, run in a pty: heph's stderr is the terminal and its
/// stdout a pipe into a reader that prints to that same terminal — the shape of
/// `heph q //p | grep x`.
fn run_in_pty_piped(dist: &Dist, cwd: &std::path::Path, args: &[&str]) -> Session {
    let home = tempfile::tempdir().expect("home tempdir");
    let mut cmd = CommandBuilder::new("sh");
    cmd.arg("-c");
    cmd.arg("\"$0\" \"$@\" | cat");
    cmd.arg(dist.heph());
    for arg in args {
        cmd.arg(arg);
    }
    drive_pty(cmd, cwd, home.path(), None, DEADLINE)
}

fn drive_pty(
    mut cmd: CommandBuilder,
    cwd: &std::path::Path,
    home: &std::path::Path,
    interact: Option<Interact>,
    deadline: Duration,
) -> Session {
    const ROWS: u16 = 40;
    const COLS: u16 = 140;

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    cmd.cwd(cwd);
    cmd.env("HOME", home);
    cmd.env("HEPH_CWD", cwd);
    cmd.env("HEPH_NO_SELF_UPDATE", "1");
    cmd.env("HEPH_DISABLE_TELEMETRY", "1");
    cmd.env("RUST_BACKTRACE", "1");
    // crossterm needs a terminfo-resolvable TERM; the CI runner's inherited
    // value may be `dumb` or unset.
    cmd.env("TERM", "xterm-256color");

    let mut child = pair.slave.spawn_command(cmd).expect("spawn under pty");
    // Drop the slave handle so the reader sees EOF once the child exits;
    // otherwise this process keeps the write end open forever.
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("clone pty reader");
    let mut writer = pair.master.take_writer().expect("take pty writer");
    let captured = Arc::new(Mutex::new((Vec::<u8>::new(), String::new())));
    let sink = Arc::clone(&captured);
    let sent_input = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sent_flag = Arc::clone(&sent_input);
    let pump = std::thread::spawn(move || {
        let mut parser = vt100::Parser::new(ROWS, COLS, 0);
        let mut buf = [0u8; 8192];
        // A pty is a pipe, not a terminal: nothing on this end answers queries.
        // The TUI asks where the cursor is (DSR) while setting up its inline
        // viewport and blocks until the terminal replies, so a harness that only
        // reads deadlocks the child. `tail` carries bytes across reads in case a
        // request straddles a chunk boundary — see `take_dsr_queries`, which owns
        // that rule and the boundary case it used to get wrong.
        let mut tail = Vec::<u8>::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let Some(chunk) = buf.get(..n) else { break };
                    parser.process(chunk);

                    tail.extend_from_slice(chunk);
                    for _ in 0..common::take_dsr_queries(&mut tail) {
                        if writer.write_all(common::DSR_REPLY).is_err() || writer.flush().is_err() {
                            break;
                        }
                    }

                    let screen = parser.screen().contents();
                    let mut guard = sink.lock().expect("capture lock");
                    guard.0.extend_from_slice(chunk);
                    guard.1.push_str(&screen);
                    guard.1.push('\n');

                    // Marker matched against everything captured so far, not just
                    // this chunk: a read boundary can land in the middle of it.
                    if let Some(it) = interact.as_ref()
                        && !sent_flag.load(std::sync::atomic::Ordering::Relaxed)
                        && contains(&guard.0, it.marker.as_bytes())
                    {
                        sent_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                        drop(writer.write_all(&it.send).and_then(|()| writer.flush()));
                    }
                }
            }
        }
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait().expect("wait on pty child") {
            Some(status) => break Some(status),
            None => {
                if started.elapsed() > deadline {
                    child.kill().expect("kill timed-out pty child");
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };

    // The child is gone; dropping the master closes the read end so the pump
    // thread finishes and every byte it captured is visible here.
    drop(pair.master);
    assert!(pump.join().is_ok(), "pty reader thread panicked");

    let exited = status.is_some();
    let guard = captured.lock().expect("capture lock");
    let session = Session {
        status_success: status.is_some_and(|s| s.success()),
        sent_input: sent_input.load(std::sync::atomic::Ordering::Relaxed),
        raw: guard.0.clone(),
        rendered: guard.1.clone(),
    };
    assert!(
        exited,
        "child did not exit within {deadline:?}{}\n{}",
        if session.sent_input {
            " — the input this test typed was delivered and ignored"
        } else {
            ""
        },
        session.report()
    );
    session
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn last_index(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}
