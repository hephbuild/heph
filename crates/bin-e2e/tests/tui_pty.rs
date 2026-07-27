//! The interactive TUI, driven through a real PTY.
//!
//! The interactive renderer engages only when stderr is a terminal, so a linked
//! test — which has no controlling terminal — always takes the CI line backend
//! and never executes a single line of this code. What is tested here is what
//! only a terminal can show: that the TUI takes the alternate screen, renders
//! the run to actual cells, and hands the terminal back on exit.
//!
//! Terminal restore is the one that matters most in practice. A TUI that dies
//! without leaving the alternate screen and raw mode leaves the user's shell
//! unusable, and every non-PTY test in the repo passes while it does.

mod common;

use common::Dist;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read as _, Write as _};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// CSI ?1049h / ?1049l — enter and leave the alternate screen.
const ALT_SCREEN_ENTER: &[u8] = b"\x1b[?1049h";
const ALT_SCREEN_LEAVE: &[u8] = b"\x1b[?1049l";

/// CSI 6n — Device Status Report, "where is the cursor?". The child blocks on
/// the reply, so the harness must answer it the way a real terminal would.
const DSR_CURSOR: &[u8] = b"\x1b[6n";

/// Generous: this spawns a release binary that builds a target, on a shared CI
/// runner. Long enough never to be the flake, short enough to fail the job
/// rather than hang it.
const DEADLINE: Duration = Duration::from_secs(180);

#[test]
fn tui_renders_the_run_and_restores_the_terminal() {
    let dist = Dist::locate();
    let ws = common::Workspace::new().expect("workspace");
    // The sleep guarantees the run is observable: without it the target can
    // finish inside a single frame interval and the in-flight rendering this
    // test is about never happens.
    ws.write(
        "pkg/BUILD",
        "target(name = \"ok\", driver = \"bash\", run = \"sleep 1; echo e2e-ok\", cache = False)\n",
    )
    .expect("write BUILD");

    let session = run_in_pty(&dist, ws.root(), &["run", "//pkg:ok"]);

    assert!(
        session.status_success,
        "run failed under a tty\n{}",
        session.report()
    );
    assert!(
        contains(&session.raw, ALT_SCREEN_ENTER),
        "interactive TUI never engaged with a tty attached\n{}",
        session.report()
    );
    assert!(
        contains(&session.raw, ALT_SCREEN_LEAVE),
        "left the terminal in the alternate screen\n{}",
        session.report()
    );
    assert!(
        session.rendered.contains("//pkg:ok"),
        "the target never appeared on the rendered screen\n{}",
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
        !contains(&session.raw, ALT_SCREEN_ENTER),
        "--no-tui still entered the alternate screen\n{}",
        session.report()
    );
}

struct Session {
    status_success: bool,
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

    let home = tempfile::tempdir().expect("home tempdir");
    let mut cmd = CommandBuilder::new(dist.heph());
    for arg in args {
        cmd.arg(arg);
    }
    cmd.cwd(cwd);
    cmd.env("HOME", home.path());
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
    let pump = std::thread::spawn(move || {
        let mut parser = vt100::Parser::new(ROWS, COLS, 0);
        let mut buf = [0u8; 8192];
        // A pty is a pipe, not a terminal: nothing on this end answers queries.
        // The TUI asks where the cursor is (DSR) while setting up its inline
        // viewport and blocks until the terminal replies, so a harness that only
        // reads deadlocks the child. `tail` carries the last few bytes across
        // reads in case a request straddles a chunk boundary.
        let mut tail = Vec::<u8>::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let Some(chunk) = buf.get(..n) else { break };
                    parser.process(chunk);

                    tail.extend_from_slice(chunk);
                    let replies = tail
                        .windows(DSR_CURSOR.len())
                        .filter(|w| *w == DSR_CURSOR)
                        .count();
                    for _ in 0..replies {
                        // Cursor at row 1, column 1 — the state a fresh terminal
                        // is in. The value only positions the inline viewport.
                        if writer.write_all(b"\x1b[1;1R").is_err() || writer.flush().is_err() {
                            break;
                        }
                    }
                    // Keep only enough tail to catch a split request, and drop
                    // anything already answered.
                    if replies > 0 {
                        tail.clear();
                    } else if tail.len() > DSR_CURSOR.len() {
                        let keep = tail.len() - (DSR_CURSOR.len() - 1);
                        tail.drain(..keep);
                    }

                    let screen = parser.screen().contents();
                    let mut guard = sink.lock().expect("capture lock");
                    guard.0.extend_from_slice(chunk);
                    guard.1.push_str(&screen);
                    guard.1.push('\n');
                }
            }
        }
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait().expect("wait on pty child") {
            Some(status) => break Some(status),
            None => {
                if started.elapsed() > DEADLINE {
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
        raw: guard.0.clone(),
        rendered: guard.1.clone(),
    };
    assert!(
        exited,
        "child did not exit within {DEADLINE:?}\n{}",
        session.report()
    );
    session
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
