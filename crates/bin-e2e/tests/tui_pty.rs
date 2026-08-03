//! The interactive TUI, driven through a real PTY.
//!
//! The interactive renderer engages only when stderr is a terminal, so a linked
//! test — which has no controlling terminal — always takes the CI line backend
//! and never executes a single line of this code. What is tested here is what
//! only a terminal can show: that the TUI builds its viewport, renders the run
//! to actual cells, and hands the terminal back on exit.
//!
//! The TUI uses a ratatui *inline* viewport, not the alternate screen — it
//! draws in place below the prompt and clears itself on exit. So the signals
//! are the inline handshake (a cursor-position query the terminal must answer)
//! and the cursor hide/show pair, not `?1049h`.
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
    assert!(
        session.rendered.contains("//pkg:ok"),
        "the target never appeared in the rendered output\n{}",
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

fn last_index(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}
