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
