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
/// `//pkg:bad` fails, once the gate is open.
fn gated_build(gate: &std::path::Path) -> String {
    format!(
        concat!(
            "target(name = \"gate\", driver = \"bash\", cache = False,",
            " run = [\"trap '' INT TERM\", \"while [ ! -e {gate} ]; do sleep 0.1; done\"])\n",
            "target(name = \"ok\", driver = \"bash\", cache = False,",
            " run = [\"echo {out}\", \"sleep 1\"], deps = {{\"gate\": [\"//pkg:gate\"]}})\n",
            "target(name = \"bad\", driver = \"bash\", cache = False,",
            " run = [\"exit 3\"], deps = {{\"gate\": [\"//pkg:gate\"]}})\n",
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
        Self::start_run(prelude, env, "//pkg:ok")
    }

    /// [`Self::start`], running `heph run <run_args>` (shell words) instead.
    fn start_run(prelude: &str, env: &[(&str, &str)], run_args: &str) -> Self {
        Self::start_build(gated_build, prelude, env, run_args)
    }

    /// [`Self::start_run`] with `build(gate)` as `pkg/BUILD`.
    fn start_build(
        build: fn(&std::path::Path) -> String,
        prelude: &str,
        env: &[(&str, &str)],
        run_args: &str,
    ) -> Self {
        Self::start_cmd(build, prelude, env, &format!("run {run_args}"))
    }

    /// [`Self::start_build`], running `heph <args>` (shell words): a command
    /// other than `run`.
    fn start_cmd(
        build: fn(&std::path::Path) -> String,
        prelude: &str,
        env: &[(&str, &str)],
        args: &str,
    ) -> Self {
        let dist = Dist::locate();
        let ws = common::Workspace::new().expect("workspace");
        let gate = ws.root().join("gate-open");
        ws.write("pkg/BUILD", &build(&gate)).expect("write BUILD");
        let script = format!("{prelude}exec \"$HEPH\" {args}");
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

/// The mouse is captured the way the full screens capture it: press and
/// release reports (`?1000h`), SGR-encoded (`?1006h`).
fn mouse_captured(s: &vt100::Screen) -> bool {
    s.mouse_protocol_mode() == vt100::MouseProtocolMode::PressRelease
        && s.mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Sgr
}

fn no_mouse(s: &vt100::Screen) -> bool {
    s.mouse_protocol_mode() == vt100::MouseProtocolMode::None
}

/// The dashboard is up and has the mouse.
fn dashboard_captured(s: &vt100::Screen) -> bool {
    on_dashboard(s) && mouse_captured(s)
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

/// The hint row while one ctrl-c has armed the stop on a full screen
/// (`tui::stop_arm::ARMED_PROMPT`; this crate links no workspace crate).
const ARMED_PROMPT: &str = "ctrl-c again to stop the build";
const DOWN: &[u8] = b"\x1b[B";

/// A full screen whose hint row asks for the second press.
fn armed(s: &vt100::Screen) -> bool {
    s.alternate_screen() && count_rows(s, ARMED_PROMPT) == 1
}

/// The finished line, held by `--dashboard` until q.
fn on_held_line(s: &vt100::Screen) -> bool {
    on_line_once(s) && count_rows(s, "q to quit") == 1
}

/// Each case on its own terminal, all at once, so a failing case is named
/// (its panic message is printed above) rather than only the first. The
/// result of each, by name.
fn each_case<C: Sync, R: Send>(cases: &[C], name: fn(&C) -> &str, run: fn(&C) -> R) -> Vec<R> {
    let results: Vec<Result<R, &str>> = std::thread::scope(|s| {
        let handles: Vec<_> = cases
            .iter()
            .map(|case| (name(case), s.spawn(move || run(case))))
            .collect();
        handles
            .into_iter()
            // The panic's message was printed as it happened.
            .map(|(name, h)| h.join().map_err(|_panic| name))
            .collect()
    });
    let failed: Vec<&str> = results
        .iter()
        .filter_map(|r| r.as_ref().err().copied())
        .collect();
    assert!(failed.is_empty(), "failed: {failed:?}");
    results.into_iter().filter_map(Result::ok).collect()
}

/// Ctrl-c on a full screen takes two presses: the first only arms the stop
/// (the hint row asks for the second, and the build keeps running), the
/// second within 3 s stops it with the exit code one press on the line
/// gives. Another key, or the window lapsing, disarms; a finished build
/// leaves on one press; `kill -INT` stops at once on either screen. The
/// terminal is restored every time.
///
/// Attached (`session attach`), one press detaches and the owner keeps
/// building: `sessions_pty::attach_detaches_without_stopping`.
#[test]
fn ctrl_c_twice_on_full_screens() {
    struct Case {
        name: &'static str,
        /// Played first; heph must still be running after them.
        steps: &'static [Step<'static>],
        /// Then a SIGINT to heph's pid.
        interrupt: bool,
        /// Then these, to the end.
        rest: &'static [Step<'static>],
        /// Whether the build was stopped (else it finished: code 0).
        stops: bool,
    }
    const OPEN: Step<'static> = Step::Keys(TAB);
    const STOPPED: &[Step<'static>] = &[Step::Settle, Step::Settle, Step::OpenGate];
    const FINISH: &[Step<'static>] = &[
        Step::OpenGate,
        Step::Until("the finished dashboard", on_finished_dashboard),
        Step::Keys(b"q"),
    ];
    let cases = [
        Case {
            name: "dashboard: the first press arms, the second stops",
            steps: &[
                Step::Until("the line", on_line),
                OPEN,
                Step::Until("the dashboard", on_dashboard),
                Step::Keys(CTRL_C),
                Step::Until("the prompt", armed),
                Step::Settle,
                Step::Until("the prompt, still", armed),
            ],
            interrupt: false,
            rest: &[
                Step::Keys(CTRL_C),
                Step::Settle,
                Step::Settle,
                Step::OpenGate,
            ],
            stops: true,
        },
        Case {
            name: "line: one press stops",
            steps: &[Step::Until("the line", on_line)],
            interrupt: false,
            rest: &[
                Step::Keys(CTRL_C),
                Step::Settle,
                Step::Settle,
                Step::OpenGate,
            ],
            stops: true,
        },
        Case {
            name: "another key disarms",
            steps: &[
                Step::Until("the line", on_line),
                OPEN,
                Step::Until("the dashboard", on_dashboard),
                Step::Keys(CTRL_C),
                Step::Until("the prompt", armed),
                Step::Keys(DOWN),
                Step::Until("the prompt gone", on_dashboard),
                Step::Keys(CTRL_C),
                Step::Until("the prompt again", armed),
            ],
            interrupt: false,
            rest: FINISH,
            stops: false,
        },
        Case {
            name: "the window lapses",
            steps: &[
                Step::Until("the line", on_line),
                OPEN,
                Step::Until("the dashboard", on_dashboard),
                Step::Keys(CTRL_C),
                Step::Until("the prompt", armed),
                Step::Until("the prompt gone after 3 s", on_dashboard),
                Step::Keys(CTRL_C),
                Step::Until("the prompt again", armed),
            ],
            interrupt: false,
            rest: FINISH,
            stops: false,
        },
        Case {
            name: "finished: one press leaves",
            steps: &[
                Step::Until("the line", on_line),
                OPEN,
                Step::Until("the dashboard", on_dashboard),
                Step::OpenGate,
                Step::Until("the finished dashboard", on_finished_dashboard),
            ],
            interrupt: false,
            rest: &[Step::Keys(CTRL_C)],
            stops: false,
        },
        Case {
            name: "kill -INT on the dashboard",
            steps: &[
                Step::Until("the line", on_line),
                OPEN,
                Step::Until("the dashboard", on_dashboard),
            ],
            interrupt: true,
            rest: STOPPED,
            stops: true,
        },
        Case {
            name: "kill -INT on the line",
            steps: &[Step::Until("the line", on_line)],
            interrupt: true,
            rest: STOPPED,
            stops: true,
        },
    ];
    let run = |case: &Case| -> Option<u32> {
        let mut g = Gated::start("", &[]);
        g.play(case.steps);
        assert!(
            common::alive(g.pty.pid()),
            "{}: heph exited before the stop\n{}",
            case.name,
            g.pty.seen()
        );
        if case.interrupt {
            common::signal(g.pty.pid(), libc::SIGINT);
        }
        g.play(case.rest);
        let ended = g.exit();
        assert_restored(&g.pty, case.name);
        let common::pty::Ended::Code(code) = ended else {
            panic!("{}: {ended:?}\n{}", case.name, g.pty.seen());
        };
        if case.stops {
            assert_ne!(
                code,
                0,
                "{}: the build was not stopped\n{}",
                case.name,
                g.pty.seen()
            );
            // The final line says why the build ended short, whichever
            // producer (key or signal) stopped it. A stop that lands before
            // the targets were matched ends with the cancellation report and
            // no final line at all; either way, no summary may read as an
            // ordinary `✗ <clock>` failure.
            let seen = g.pty.seen();
            assert!(
                seen.contains("✗ stopped · ") || seen.contains("× cancelled"),
                "{}: the final line does not say stopped\n{seen}",
                case.name
            );
            assert!(
                !seen.lines().any(|l| l
                    .strip_prefix("✗ ")
                    .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))),
                "{}: a summary without \"stopped\"\n{seen}",
                case.name
            );
            Some(code)
        } else {
            assert_eq!(code, 0, "{}: exit status\n{}", case.name, g.pty.seen());
            // The ctrl-c that quits a held, finished run stops nothing.
            let seen = g.pty.seen();
            assert!(
                !seen.contains("✗ stopped"),
                "{}: a finished run says stopped\n{seen}",
                case.name
            );
            None
        }
    };
    let codes: Vec<(&str, u32)> = each_case(&cases, |c| c.name, run)
        .into_iter()
        .zip(&cases)
        .filter_map(|(code, case)| Some((case.name, code?)))
        .collect();
    assert!(
        codes.windows(2).all(|w| w[0].1 == w[1].1),
        "every stop ends with the same code: {codes:?}"
    );
}

/// `--dashboard`: the alternate screen is up on the first frame, and the
/// finished build holds until q — on the dashboard, and on the line Esc
/// steps back to (mid-run or after the end) — then exits with the build's
/// code.
#[test]
fn dashboard_flag_opens_and_holds() {
    struct Case {
        name: &'static str,
        target: &'static str,
        /// From the first frame to the held screen.
        steps: &'static [Step<'static>],
        /// The held screen.
        held: fn(&vt100::Screen) -> bool,
    }
    let cases = [
        Case {
            name: "held on the dashboard",
            target: "//pkg:ok",
            steps: &[
                Step::OpenGate,
                Step::Until("the main screen, for the target", paused_for_target),
            ],
            held: on_finished_dashboard,
        },
        Case {
            name: "esc mid-run: held on the line",
            target: "//pkg:ok",
            steps: &[
                Step::Keys(ESC),
                Step::Until("the line", on_line_once),
                Step::OpenGate,
            ],
            held: on_held_line,
        },
        Case {
            name: "esc after the end: held on the line",
            target: "//pkg:ok",
            steps: &[
                Step::OpenGate,
                Step::Until("the finished dashboard", on_finished_dashboard),
                Step::Keys(ESC),
            ],
            held: on_held_line,
        },
        Case {
            name: "a failed build, held on the dashboard",
            target: "//pkg:bad",
            steps: &[Step::OpenGate],
            held: on_finished_dashboard,
        },
    ];
    let run = |case: &Case| {
        let mut g = Gated::start_run("", &[], &format!("-D {}", case.target));
        g.play(&[Step::Until("the dashboard", on_dashboard)]);
        g.play(case.steps);
        g.pty.wait_screen("the held screen", DEADLINE, case.held);
        // Held: still up, and heph still running, well after the build ended.
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            common::alive(g.pty.pid()),
            "{}: heph exited without q\n{}",
            case.name,
            g.pty.seen()
        );
        assert!(
            g.pty.with_screen(case.held),
            "{}: the held screen went\n{}",
            case.name,
            g.pty.screen()
        );
        g.pty.send(b"q");
        let ended = g.exit();
        assert_restored(&g.pty, case.name);
        // The first frame was the dashboard's: the alternate screen went up
        // before anything drew a done count.
        let out = g.pty.output();
        let alt = first_index(&out, b"\x1b[?1049h");
        let done = first_index(&out, b" done");
        assert!(
            matches!((alt, done), (Some(a), Some(d)) if a < d),
            "{}: a done count was drawn before the dashboard went up\n{}",
            case.name,
            g.pty.seen()
        );
        (case.target, ended)
    };
    let ended = each_case(&cases, |c| c.name, run);
    // The build's code, however long it was held: what the same build exits
    // with, without a terminal or -D.
    let dist = Dist::locate();
    for (target, code) in ended {
        let ws = common::Workspace::new().expect("workspace");
        let gate = ws.root().join("gate-open");
        ws.write("pkg/BUILD", &gated_build(&gate))
            .expect("write BUILD");
        std::fs::write(&gate, b"").expect("open the gate");
        let out = ws.run(&dist, &["run", target]).expect("plain run");
        let plain = out.status.code().expect("an exit code");
        assert_eq!(
            code,
            common::pty::Ended::Code(u32::try_from(plain).expect("a code")),
            "{target}: held exit differs from the build's\n{}",
            common::describe(&out)
        );
    }
}

/// `-D` on a command other than `run` that builds (`inspect outputs`): the
/// same dashboard from the first frame, held after the build until q, and the
/// command's own exit code.
#[test]
fn dashboard_flag_on_a_non_run_command() {
    let mut g = Gated::start_cmd(gated_build, "", &[], "inspect outputs -D //pkg:gate");
    g.play(&[
        Step::Until("the dashboard", on_dashboard),
        Step::OpenGate,
        Step::Until("the finished dashboard", on_finished_dashboard),
    ]);
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        common::alive(g.pty.pid()),
        "heph exited without q\n{}",
        g.pty.seen()
    );
    assert!(
        g.pty.with_screen(on_finished_dashboard),
        "the held dashboard went\n{}",
        g.pty.screen()
    );
    g.pty.send(b"q");
    let ended = g.exit();
    assert_restored(&g.pty, "inspect outputs -D");
    assert_eq!(ended, common::pty::Ended::Code(0), "{}", g.pty.seen());
    let out = g.pty.output();
    let alt = first_index(&out, b"\x1b[?1049h");
    let done = first_index(&out, b" done");
    assert!(
        matches!((alt, done), (Some(a), Some(d)) if a < d),
        "a done count was drawn before the dashboard went up\n{}",
        g.pty.seen()
    );
}

/// `-D` without a terminal: one warning, then the plain output it would have
/// shown anyway, no wait for q, and the build's own exit code — on `run` and
/// on a command other than `run`.
#[test]
fn dashboard_flag_without_tty() {
    const WARNING: &str = "--dashboard needs an interactive terminal";
    let dist = Dist::locate();
    let ws = common::Workspace::new().expect("workspace");
    ws.write(
        "pkg/BUILD",
        concat!(
            "target(name = \"ok\", driver = \"bash\", run = \"echo plain-ok\", cache = False)\n",
            "target(name = \"bad\", driver = \"bash\", run = \"exit 3\", cache = False)\n",
        ),
    )
    .expect("write BUILD");
    for (cmd, target) in [
        (&["run"][..], "//pkg:ok"),
        (&["run"], "//pkg:bad"),
        (&["inspect", "outputs"], "//pkg:ok"),
        (&["inspect", "outputs"], "//pkg:bad"),
    ] {
        // Both streams piped; stdin is /dev/null, so a wait for q would hang
        // until the run deadline kills it.
        let argv = |flag: &[&'static str]| -> Vec<&str> {
            cmd.iter().chain(flag).copied().chain([target]).collect()
        };
        let target = format!("{cmd:?} {target}");
        let plain = ws.run(&dist, &argv(&[])).expect("plain run");
        let flagged = ws.run(&dist, &argv(&["-D"])).expect("-D run");
        assert_eq!(
            flagged.status.code(),
            plain.status.code(),
            "{target}: exit code\n{}",
            common::describe(&flagged)
        );
        let stderr = String::from_utf8_lossy(&flagged.stderr);
        assert_eq!(
            stderr.matches(WARNING).count(),
            1,
            "{target}: the warning, once\n{stderr}"
        );
        assert!(
            !String::from_utf8_lossy(&plain.stderr).contains(WARNING),
            "{target}: warned without -D"
        );
        assert_eq!(flagged.stdout, plain.stdout, "{target}: stdout");
    }
    // Stderr a terminal, stdout a pipe: still no TUI, so still the warning,
    // and no alternate screen.
    let session = run_in_pty_piped(&dist, ws.root(), &["run", "-D", "//pkg:ok"]);
    assert!(session.status_success, "{}", session.report());
    assert!(
        !contains(&session.raw, b"\x1b[?1049h"),
        "a piped stdout still opened the dashboard\n{}",
        session.report()
    );
    assert_eq!(
        String::from_utf8_lossy(&session.raw)
            .matches(WARNING)
            .count(),
        1,
        "the warning, once\n{}",
        session.report()
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
    // The mouse is captured before any way out is taken, so that "no mouse
    // mode" afterwards says the exit released it.
    const OPEN: &[Step<'static>] = &[
        Step::Until("the line", on_line),
        Step::Keys(TAB),
        Step::Until("the dashboard, mouse captured", dashboard_captured),
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

/// Where `needle` starts on screen, as a cell (row, col): a wide glyph before
/// it counts as the two columns it takes.
fn find_cell(s: &vt100::Screen, needle: &str) -> Option<(u16, u16)> {
    let (rows, cols) = s.size();
    (0..rows).find_map(|row| {
        let mut text = String::new();
        let mut cols_at = Vec::new();
        for col in 0..cols {
            let Some(cell) = s.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            cols_at.push((text.len(), col));
            text.push_str(match cell.contents() {
                "" => " ",
                c => c,
            });
        }
        let at = text.find(needle)?;
        let col = cols_at.iter().find(|(byte, _)| *byte == at)?.1;
        Some((row, col))
    })
}

/// The cell's text, or "" off screen.
fn cell_text(s: &vt100::Screen, row: u16, col: u16) -> String {
    s.cell(row, col)
        .map(|c| c.contents().to_owned())
        .unwrap_or_default()
}

/// The dashboard tab whose label is clicked, and its label's text there.
const CLICKED_TAB: &str = "Done";

/// Click (SGR press, then release) the dashboard tab labelled [`CLICKED_TAB`]
/// and wait for the rule under the tab row to mark it active: the bytes a
/// terminal sends, read by the shipped binary's own input parser.
fn click_tab(pty: &common::pty::Pty, case: &str) {
    let found = pty.with_screen(|s| find_cell(s, CLICKED_TAB));
    assert!(
        found.is_some(),
        "{case}: no {CLICKED_TAB:?} tab on the dashboard\n{}",
        pty.screen()
    );
    let (row, col) = found.expect("asserted above");
    let rule = row + 1;
    let marked = |s: &vt100::Screen| cell_text(s, rule, col + 1) == "━";
    assert!(
        !pty.with_screen(marked),
        "{case}: {CLICKED_TAB:?} already active before the click\n{}",
        pty.screen()
    );
    // SGR is 1-based; click inside the label, not on its first cell.
    let (x, y) = (col + 2, row + 1);
    pty.send(format!("\x1b[<0;{x};{y}M\x1b[<0;{x};{y}m").as_bytes());
    pty.wait_screen(
        &format!("{case}: the rule under {CLICKED_TAB:?} after the click"),
        DEADLINE,
        marked,
    );
}

/// The line, with the mouse released.
fn line_without_mouse(s: &vt100::Screen) -> bool {
    on_line_once(s) && no_mouse(s)
}

/// The line has come up, before any full screen: no mouse yet.
fn line_up_without_mouse(s: &vt100::Screen) -> bool {
    on_line(s) && no_mouse(s)
}

/// The dashboard up without the mouse (`HEPH_NO_MOUSE`).
fn dashboard_without_mouse(s: &vt100::Screen) -> bool {
    on_dashboard(s) && no_mouse(s)
}

/// Paused for the named target, with the mouse released: a target that holds
/// the terminal gets it without heph's mouse reports.
fn paused_without_mouse(s: &vt100::Screen) -> bool {
    paused_for_target(s) && no_mouse(s)
}

fn finished_dashboard_captured(s: &vt100::Screen) -> bool {
    on_finished_dashboard(s) && mouse_captured(s)
}

/// The mouse is captured only while a full screen is up: on after Tab (press
/// and release, SGR), off on the line, after esc, while a target holds the
/// terminal, after ctrl-c and once heph is gone. Motion tracking (`?1003h`,
/// `?1002h`) is never asked for. `HEPH_NO_MOUSE=1` keeps it off throughout.
/// A click on a tab label, as a terminal encodes it, makes that tab active.
#[test]
fn mouse_capture_only_on_full_screens() {
    struct Case {
        name: &'static str,
        env: &'static [(&'static str, &'static str)],
        /// Played after the line is up, with the mouse not yet captured.
        steps: &'static [Step<'static>],
        /// Then a click on a tab label, when set.
        click: bool,
        /// Then these, to the end.
        rest: &'static [Step<'static>],
        /// Whether the mouse may ever be captured.
        mouse: bool,
    }
    const OPEN: &[Step<'static>] = &[
        Step::Keys(TAB),
        Step::Until("the dashboard, mouse captured", dashboard_captured),
    ];
    let cases = [
        Case {
            name: "tab, a click, esc",
            env: &[],
            steps: OPEN,
            click: true,
            rest: &[
                Step::Keys(ESC),
                Step::Until("the line, mouse released", line_without_mouse),
                Step::OpenGate,
            ],
            mouse: true,
        },
        Case {
            name: "a target holds the terminal",
            env: &[],
            steps: OPEN,
            click: false,
            rest: &[
                Step::OpenGate,
                Step::Until("the target, mouse released", paused_without_mouse),
                Step::Until(
                    "the finished dashboard, mouse captured again",
                    finished_dashboard_captured,
                ),
                Step::Keys(b"q"),
            ],
            mouse: true,
        },
        Case {
            name: "ctrl-c on the dashboard",
            env: &[],
            steps: OPEN,
            click: false,
            rest: &[Step::Keys(CTRL_C), Step::Keys(CTRL_C), Step::OpenGate],
            mouse: true,
        },
        Case {
            name: "HEPH_NO_MOUSE=1",
            env: &[("HEPH_NO_MOUSE", "1")],
            steps: &[
                Step::Keys(TAB),
                Step::Until("the dashboard", on_dashboard),
                Step::Settle,
                Step::Until("the dashboard, mouse not captured", dashboard_without_mouse),
                Step::Keys(ESC),
                Step::Until("the line", line_without_mouse),
            ],
            click: false,
            rest: &[Step::OpenGate],
            mouse: false,
        },
    ];
    each_case(
        &cases,
        |c| c.name,
        |case| {
            let mut g = Gated::start("", case.env);
            g.play(&[Step::Until(
                "the line, mouse not captured",
                line_up_without_mouse,
            )]);
            g.play(case.steps);
            if case.click {
                click_tab(&g.pty, case.name);
            }
            g.play(case.rest);
            g.exit();
            assert_restored(&g.pty, case.name);
            let out = g.pty.output();
            for motion in [&b"\x1b[?1003h"[..], b"\x1b[?1002h"] {
                assert!(
                    !contains(&out, motion),
                    "{}: motion tracking {:?} was asked for",
                    case.name,
                    String::from_utf8_lossy(motion)
                );
            }
            assert_eq!(
                contains(&out, b"\x1b[?1000h"),
                case.mouse,
                "{}: whether the mouse was ever captured",
                case.name
            );
        },
    );
}

const ENTER: &[u8] = b"\r";
/// What the line says while Y stands.
const AUTO_APPROVING: &str = "auto-approving (Y)";
/// The line's words while N stands.
const AUTO_REJECTING: &str = "auto-rejecting (N)";

/// Three approval-gated dependencies of `//pkg:sum`, the run's named target.
/// `//pkg:one`'s notice is ready at once and carries control bytes (a colour,
/// a bell, a screen clear); `//pkg:two`'s takes a second, so the two queue
/// in that order; `//pkg:three` asks only once the gate is open.
fn approval_build(gate: &std::path::Path) -> String {
    format!(
        concat!(
            "target(name = \"gate\", driver = \"bash\", cache = False,",
            " run = [\"trap '' INT TERM\", \"while [ ! -e {gate} ]; do sleep 0.1; done\"])\n",
            "target(name = \"plan_ctl\", driver = \"bash\", cache = False, out = \"plan.txt\",",
            " run = \"printf 'deploy \\\\033[31mALERT\\\\033[0m bell\\\\007 \\\\033[2J end\\\\n' > $OUT\")\n",
            "target(name = \"plan\", driver = \"bash\", cache = False, out = \"plan.txt\",",
            " run = [\"sleep 1\", \"echo 'push api:prod' > $OUT\"])\n",
            "target(name = \"one\", driver = \"bash\", cache = False, run = \"true\",",
            " deps = {{\"plan\": \":plan_ctl\"}},",
            " approval = {{\"required\": True, \"notice\": [\"plan\"]}})\n",
            "target(name = \"two\", driver = \"bash\", cache = False, run = \"true\",",
            " deps = {{\"plan\": \":plan\"}},",
            " approval = {{\"required\": True, \"notice\": [\"plan\"]}})\n",
            "target(name = \"three\", driver = \"bash\", cache = False, run = \"true\",",
            " deps = {{\"plan\": \":plan\", \"gate\": \":gate\"}},",
            " approval = {{\"required\": True, \"notice\": [\"plan\"]}})\n",
            "target(name = \"sum\", driver = \"bash\", cache = False, run = [\"echo {out}\"],",
            " deps = {{\"one\": \":one\", \"two\": \":two\", \"three\": \":three\"}})\n",
        ),
        gate = gate.display(),
        out = TARGET_OUT,
    )
}

/// The approval view's notice-tab row (`◂ esc  ✦ //x  │  plan   1 of 2
/// pending`), while the view is up.
fn approval_tabs(s: &vt100::Screen) -> Option<String> {
    if !s.alternate_screen() {
        return None;
    }
    rows_of(s).find(|r| r.contains("◂ esc"))
}

/// The view shows `addr`'s request, with `status` on the right of its tab
/// row: `1 of 2 pending`, or the decision just taken.
fn viewing(s: &vt100::Screen, addr: &str, status: &str) -> bool {
    approval_tabs(s).is_some_and(|r| r.contains(&format!("{addr} ")) && r.contains(status))
}

/// The decision keys' hint is drawn yellow: the lockout is over.
fn decision_armed(s: &vt100::Screen) -> bool {
    approval_tabs(s).is_some()
        && find_cell(s, "y approve")
            .and_then(|(row, col)| s.cell(row, col))
            .is_some_and(|c| c.fgcolor() == vt100::Color::Idx(3))
}

/// The line's prompt row names `addr`, the oldest pending request.
fn prompted_for(s: &vt100::Screen, addr: &str, pending: &str) -> bool {
    !s.alternate_screen()
        && rows_of(s).any(|r| r.contains(&format!("approval required: {addr}{pending}")))
}

/// The line, saying Y stands.
fn line_auto_approving(s: &vt100::Screen) -> bool {
    on_line(s) && count_rows(s, AUTO_APPROVING) == 1
}

/// The dashboard, its list led by both requests' `✦` rows.
fn dashboard_with_requests(s: &vt100::Screen) -> bool {
    on_dashboard(s) && count_rows(s, "awaits approval") == 2
}

/// The run's scrollback once heph has exited: the rows that record a
/// decision, which only the run's own log prints.
fn decision_lines(pty: &common::pty::Pty) -> Vec<String> {
    pty.rows()
        .into_iter()
        .filter(|r| r.contains("approved //pkg:") || r.contains("rejected //pkg:"))
        .collect()
}

/// Type `key` every 20 ms for `for_ms`.
fn hold_key(pty: &common::pty::Pty, key: &[u8], for_ms: u64) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(for_ms) {
        pty.send(key);
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The approval view decides what it shows and returns to where it was
/// opened. y, once the lockout is over, approves the request on screen; the
/// tab row flashes the decision, then the next request shows with its own
/// lockout; Y approves it, and the one that arrives after; the view closes
/// to the screen that opened it, and the line (and the dashboard's title
/// row) say `auto-approving (Y)` while the build runs. Each decision is one
/// scrollback line, read notices named, `notices not read` for those Y
/// decided. A notice's control bytes reach the screen as text, never as a
/// colour.
#[test]
fn approval_view_decides_and_returns() {
    struct Case {
        name: &'static str,
        /// From the line's prompt to the view.
        open: &'static [Step<'static>],
        /// The screen that opened the view, once it has closed: Y stands.
        origin: fn(&vt100::Screen) -> bool,
        /// From there back to the line.
        back: &'static [Step<'static>],
    }
    let cases = [
        Case {
            name: "from the line",
            open: &[Step::Keys(ENTER)],
            origin: line_auto_approving,
            back: &[],
        },
        Case {
            name: "from the dashboard",
            open: &[
                Step::Keys(TAB),
                Step::Until("the dashboard, with the requests", dashboard_with_requests),
                Step::Keys(b"v"),
            ],
            // Its title row says so.
            origin: |s| on_dashboard(s) && count_rows(s, AUTO_APPROVING) == 1,
            back: &[
                Step::Keys(ESC),
                Step::Until("the line, Y standing", line_auto_approving),
            ],
        },
    ];
    let run = |case: &Case| {
        let mut g = Gated::start_build(approval_build, "", &[], "//pkg:sum");
        g.pty
            .wait_screen("two requests on the line", DEADLINE, |s| {
                prompted_for(s, "//pkg:one", " · 2 pending")
            });
        g.play(case.open);
        g.pty.wait_screen("the view, on the oldest", DEADLINE, |s| {
            viewing(s, "//pkg:one", "1 of 2 pending")
        });
        g.pty
            .wait_screen("the decision keys, armed", DEADLINE, decision_armed);
        // The notice is on screen as text: its escape drew no colour, and
        // its bell and screen clear left the row whole.
        g.pty.with_screen(|s| {
            let (row, col) = find_cell(s, "ALERT").expect("the notice's text on screen");
            let row_text = rows_of(s).nth(usize::from(row)).unwrap_or_default();
            assert!(
                row_text.contains("deploy") && row_text.contains("end"),
                "{}: the notice's row was cut: {row_text:?}",
                case.name
            );
            let fg = s.cell(row, col).map(vt100::Cell::fgcolor);
            assert_ne!(
                fg,
                Some(vt100::Color::Idx(1)),
                "{}: the notice's escape coloured it",
                case.name
            );
        });
        g.pty.send(b"y");
        g.pty.wait_screen("the decision, flashed", DEADLINE, |s| {
            viewing(s, "//pkg:one", "✓ approved //pkg:one")
        });
        g.pty.wait_screen("the next request", DEADLINE, |s| {
            viewing(s, "//pkg:two", "1 of 1 pending")
        });
        g.pty
            .wait_screen("its decision keys, armed", DEADLINE, decision_armed);
        g.pty.send(b"Y");
        g.pty.wait_screen("Y, flashed", DEADLINE, |s| {
            viewing(s, "//pkg:two", "✓ approved //pkg:two")
        });
        // Nothing left: back where it was opened, saying Y stands.
        g.pty
            .wait_screen("the screen that opened it", DEADLINE, case.origin);
        g.play(case.back);
        assert!(
            g.pty
                .with_screen(|s| count_rows(s, "approval required") == 0),
            "{}: a prompt row with nothing pending\n{}",
            case.name,
            g.pty.screen()
        );
        // The third asks after Y: approved as it arrives, no prompt.
        g.play(&[Step::OpenGate]);
        let ended = g.exit();
        assert_eq!(
            ended,
            common::pty::Ended::Code(0),
            "{}: the build failed\n{}",
            case.name,
            g.pty.seen()
        );
        assert!(
            !g.pty.seen().contains("approval required: //pkg:three"),
            "{}: //pkg:three prompted under Y\n{}",
            case.name,
            g.pty.seen()
        );
        assert_restored(&g.pty, case.name);
        let lines = decision_lines(&g.pty);
        let expect: [(&str, &[&str]); 3] = [
            (
                "✓ approved //pkg:one · by key (y) after ",
                &[" · read plan"],
            ),
            (
                "✓ approved //pkg:two · by standing approval (Y) after ",
                &[" · notices not read"],
            ),
            (
                "✓ approved //pkg:three · by standing approval (Y) after ",
                &[" · notices not read"],
            ),
        ];
        assert_eq!(
            lines.len(),
            expect.len(),
            "{}: one line per decision: {lines:?}",
            case.name
        );
        for (head, tails) in expect {
            let line = lines
                .iter()
                .find(|l| l.contains(head))
                .unwrap_or_else(|| panic!("{}: no `{head}` line in {lines:?}", case.name));
            for tail in tails {
                assert!(
                    line.ends_with(tail),
                    "{}: `{line}` does not end `{tail}`",
                    case.name
                );
            }
        }
    };
    each_case(&cases, |c| c.name, run);
}

/// Decision keys held down decide nothing: a stream of Y, then of N, as the
/// view opens on two pending requests leaves both pending, and so does a
/// stream of y right after a decision — the presses are dropped, not queued
/// for when the lockout ends. Esc leaves the request pending and the build
/// waiting for it; N then rejects it, and the build fails with a report
/// naming the rejection.
#[test]
fn held_keys_decide_nothing() {
    let mut g = Gated::start_build(approval_build, "", &[], "//pkg:sum");
    g.pty
        .wait_screen("two requests on the line", DEADLINE, |s| {
            prompted_for(s, "//pkg:one", " · 2 pending")
        });
    // From before the first frame of the view to 0.25 s after: all of it
    // inside the 0.5 s lockout, which starts at that frame. The 0.25 s left
    // over is for a loaded runner reading the presses late.
    g.pty.send(ENTER);
    hold_key(&g.pty, b"Y", 125);
    hold_key(&g.pty, b"N", 125);
    g.pty.wait_screen("the view, armed", DEADLINE, |s| {
        viewing(s, "//pkg:one", "1 of 2 pending") && decision_armed(s)
    });
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        g.pty
            .with_screen(|s| viewing(s, "//pkg:one", "1 of 2 pending")),
        "held Y/N decided something\n{}",
        g.pty.screen()
    );
    // A decision, then y held through its flash: the next request keeps
    // its own lockout, and nothing pressed before it ends decides it.
    g.pty.send(b"y");
    g.pty.wait_screen("the decision, flashed", DEADLINE, |s| {
        viewing(s, "//pkg:one", "✓ approved //pkg:one")
    });
    hold_key(&g.pty, b"y", 400);
    g.pty.wait_screen("the next request, armed", DEADLINE, |s| {
        viewing(s, "//pkg:two", "1 of 1 pending") && decision_armed(s)
    });
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        g.pty
            .with_screen(|s| viewing(s, "//pkg:two", "1 of 1 pending")),
        "a held y was queued for the next request\n{}",
        g.pty.screen()
    );
    // Esc: back on the line, the request still pending, the build waiting.
    g.pty.send(ESC);
    g.pty
        .wait_screen("the line, still prompting", DEADLINE, |s| {
            prompted_for(s, "//pkg:two", " · enter to review")
        });
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        common::alive(g.pty.pid())
            && g.pty
                .with_screen(|s| prompted_for(s, "//pkg:two", " · enter to review")),
        "the build stopped waiting after esc\n{}",
        g.pty.screen()
    );
    // Reject it for real with N, which stands: the build fails on it, its
    // report names the rejection, and the exit code is a failure's.
    g.pty.send(ENTER);
    g.pty.wait_screen("the view, armed", DEADLINE, |s| {
        viewing(s, "//pkg:two", "1 of 1 pending") && decision_armed(s)
    });
    g.pty.send(b"N");
    g.pty.wait_screen("the line, N standing", DEADLINE, |s| {
        on_line(s) && count_rows(s, AUTO_REJECTING) == 1
    });
    g.play(&[Step::OpenGate]);
    let ended = g.exit();
    let common::pty::Ended::Code(code) = ended else {
        panic!("held keys: {ended:?}\n{}", g.pty.seen());
    };
    assert!(
        code != 0 && code != 130,
        "a rejection must fail the build, neither pass nor abort it: {code}\n{}",
        g.pty.seen()
    );
    assert!(
        g.pty.seen().contains("approval denied for //pkg:two"),
        "the report does not name the rejection\n{}",
        g.pty.seen()
    );
    assert_restored(&g.pty, "held keys");
    // The keys that were meant, nothing held: y approved //pkg:one (a held N
    // deciding it would have rejected it), N rejected //pkg:two, and
    // //pkg:three too if it asked before the build stopped.
    let lines = decision_lines(&g.pty);
    let approved: Vec<&String> = lines.iter().filter(|l| l.contains("✓ approved")).collect();
    assert_eq!(approved.len(), 1, "a held key approved: {lines:?}");
    assert!(
        approved[0].contains("✓ approved //pkg:one · by key (y)"),
        "y decided other than what it was pressed for: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("✗ rejected //pkg:two · by standing rejection (N)")),
        "no rejection line for //pkg:two: {lines:?}"
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

fn first_index(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
