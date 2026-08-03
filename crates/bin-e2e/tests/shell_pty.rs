//! `heph run --shell`, driven through a real PTY, with live keystrokes.
//!
//! `tui_pty.rs` proves the progress TUI renders and restores the terminal; it
//! never types anything back in. Shell mode is a different code path
//! entirely — it puts the child on its own PTY and relays bytes both
//! directions concurrently for the run's whole lifetime — so it needs a
//! harness that can hold a session open, send keystrokes mid-run, and read
//! the reply before deciding what to send next.
//!
//! This is the regression surface for PR #245 ("merge the stdout/stderr
//! drains into one bounded reader"): a child that is simultaneously
//! producing output and consuming stdin, with a human-paced consumer on the
//! far end, is exactly the shape the bounded drain channel and the spawned
//! `pump_stdin` writer were built for. Shell mode's stdin/stdout both run
//! over `pty::AsyncPty` (a genuine tokio `AsyncRead`/`AsyncWrite`, not the
//! `block_in_place` pipe backend `proc_exec` uses for a non-PTY child), so
//! none of these tests exercise the bounded channel or `pump_stdin` directly
//! — see `crates/plugin-exec/src/pluginexec/mod.rs`'s
//! `test_run_slow_sink_does_not_deadlock_with_concurrent_stdin` for that.
//! What this file covers is the thing only a real terminal can show: that
//! `--shell` itself stays interactive and exits promptly end to end.

mod common;

use common::Dist;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DSR_CURSOR: &[u8] = b"\x1b[6n";

/// Generous: spawns a release binary plus an interactive bash child, on a
/// shared CI runner.
const DEADLINE: Duration = Duration::from_secs(60);

/// How often `wait_for` re-checks the accumulated output.
const POLL: Duration = Duration::from_millis(20);

/// An `echo` command plus the marker it prints, built so the marker does
/// **not** appear in the command line itself. Returns `(command, marker)`.
///
/// A PTY echoes back what we type. `wait_for` only searches bytes after its
/// cursor, which protects it from output that arrived *earlier* — but the
/// echo of the line we are about to send arrives *later*, after the cursor,
/// so it matches like real output. `send_str("echo done\n")` followed by
/// `wait_for("done")` therefore returns the instant our own keystrokes come
/// back, before bash has run anything at all.
///
/// That is worse than a no-op assertion, and it is what made this file flaky.
/// A wait that returns early does not just prove nothing itself — it hands
/// the command's entire runtime to whatever waits *next*. Here that was the
/// `wait_for("shellpty#")` prompt sync, whose own 10s budget then had to
/// cover 200 stderr writes (or 900 KiB of output) that the sentinel wait was
/// supposed to have absorbed. On a loaded `linux/arm64` runner it did not,
/// and the test failed inside `wait_for` while the command was still
/// streaming.
///
/// bash concatenates the empty quotes away, so the executed command prints
/// `head + tail` whole while the echoed line carries `''` in the middle and
/// cannot satisfy `wait_for(marker)`.
fn echo_marker(head: &str, tail: &str) -> (String, String) {
    (format!("echo {head}''{tail}"), format!("{head}{tail}"))
}

struct ShellSession {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    captured: Arc<Mutex<Vec<u8>>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    /// Byte offset into `captured` that the last successful `wait_for` has
    /// already accounted for. See `wait_for` for why this exists.
    cursor: usize,
}

impl ShellSession {
    /// Spawn `heph run --shell <target>` (or any other args) under a fresh
    /// PTY, with a background thread relaying the child's output into a
    /// shared buffer and auto-answering cursor-position queries the way a
    /// real terminal would (the inline TUI viewport blocks on one during its
    /// own setup on the outer terminal, before shell mode takes over).
    fn spawn(dist: &Dist, cwd: &std::path::Path, args: &[&str]) -> Self {
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
        cmd.env("TERM", "xterm-256color");

        let child = pair.slave.spawn_command(cmd).expect("spawn under pty");
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().expect("clone pty reader");
        let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(
            pair.master.take_writer().expect("take pty writer"),
        ));
        let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = Arc::clone(&captured);
        let dsr_writer = Arc::clone(&writer);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut tail = Vec::<u8>::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let Some(chunk) = buf.get(..n) else { break };
                        tail.extend_from_slice(chunk);
                        let replies = tail
                            .windows(DSR_CURSOR.len())
                            .filter(|w| *w == DSR_CURSOR)
                            .count();
                        for _ in 0..replies {
                            let mut w = dsr_writer.lock().expect("writer lock");
                            if w.write_all(b"\x1b[1;1R").is_err() || w.flush().is_err() {
                                break;
                            }
                        }
                        if replies > 0 {
                            tail.clear();
                        } else if tail.len() > DSR_CURSOR.len() {
                            let keep = tail.len() - (DSR_CURSOR.len() - 1);
                            tail.drain(..keep);
                        }
                        sink.lock().expect("capture lock").extend_from_slice(chunk);
                    }
                }
            }
        });

        Self {
            writer,
            captured,
            child,
            _master: pair.master,
            cursor: 0,
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        let mut w = self.writer.lock().expect("writer lock");
        w.write_all(bytes).expect("write to pty");
        w.flush().expect("flush pty");
    }

    fn send_str(&mut self, s: &str) {
        self.send(s.as_bytes());
    }

    fn output_snapshot(&self) -> Vec<u8> {
        self.captured.lock().expect("capture lock").clone()
    }

    fn output_lossy(&self) -> String {
        String::from_utf8_lossy(&self.output_snapshot()).into_owned()
    }

    /// Block until `needle` shows up in output captured *since the last
    /// successful `wait_for`*, or panic with everything captured so far.
    ///
    /// This deliberately does not scan the whole history: `captured` only
    /// ever grows, so once a needle has appeared once it stays "found"
    /// forever — a fixed marker like the `shellpty#` prompt (baked into the
    /// `PS1='shellpty# '` line whose own PTY echo already contains the
    /// literal substring) matches on the very first check and then every
    /// later `wait_for("shellpty#", ..)` in the file would return instantly
    /// without ever having observed a *new* prompt. That silently drops the
    /// synchronization callers rely on ("are we really back at a prompt
    /// now?") and lets the test race ahead on nothing but luck. Tracking a
    /// cursor and only searching bytes after it makes every call require a
    /// fresh occurrence of `needle`.
    ///
    /// The cursor does **not** protect against the terminal echoing the line
    /// we are about to send: that echo lands after the cursor and matches
    /// like real output, so a needle visible in the typed command satisfies
    /// this the moment our own keystrokes come back. Build such markers with
    /// [`echo_marker`], which keeps the literal out of the command line.
    fn wait_for(&mut self, needle: &str, timeout: Duration) {
        let started = Instant::now();
        let needle = needle.as_bytes();
        if needle.is_empty() {
            return;
        }
        loop {
            // Search *under* the lock rather than cloning the buffer first.
            // `output_snapshot` copies everything captured so far, and this
            // runs every `POLL`; against a command that emits 900 KiB that is a
            // fresh ~900 KiB allocation and memcpy 50 times a second, with the
            // capture lock held for each one — so the relay thread cannot drain
            // the pty while it happens, the child backs up, and the whole thing
            // degrades as the output grows. It only stayed invisible while
            // these waits were matching their own echo and returning on the
            // first poll; making them wait for real output surfaced it as a
            // 60s timeout on `large-output-done`.
            {
                let buf = self.captured.lock().expect("capture lock");
                let unseen = buf.get(self.cursor..).unwrap_or(&[]);
                if let Some(pos) = unseen.windows(needle.len()).position(|w| w == needle) {
                    self.cursor += pos + needle.len();
                    return;
                }
            }
            assert!(
                started.elapsed() < timeout,
                "never saw {:?} within {timeout:?}\n--- captured ---\n{}",
                String::from_utf8_lossy(needle),
                self.output_lossy(),
            );
            std::thread::sleep(POLL);
        }
    }

    /// Wait for the child to exit, returning (exited, elapsed, success).
    fn wait_exit(&mut self, timeout: Duration) -> (bool, Duration, bool) {
        let started = Instant::now();
        loop {
            match self.child.try_wait().expect("wait on pty child") {
                Some(status) => return (true, started.elapsed(), status.success()),
                None => {
                    if started.elapsed() > timeout {
                        return (false, started.elapsed(), false);
                    }
                    std::thread::sleep(POLL);
                }
            }
        }
    }

    /// Wait for `init.sh`'s banner (proof the interactive bash session came
    /// up and is reading commands), then override its fixed `PS1='$ '` with
    /// a unique marker so later `wait_for` calls can't collide with shell
    /// furniture or command output that happens to contain "$ ".
    fn ready(&mut self) {
        self.wait_for("Shell mode, to exit", DEADLINE);
        self.send_str("PS1='shellpty# '\n");
        self.wait_for("shellpty#", Duration::from_secs(10));
    }
}

/// Round-trip: typed input reaches the child, its response comes back, and
/// this holds over several exchanges — not just the first keystroke after
/// launch. Then `exit` must end the session promptly.
#[test]
fn shell_echoes_typed_input_and_exits_promptly() {
    let dist = Dist::locate();
    let ws = common::Workspace::new().expect("workspace");
    ws.write(
        "pkg/BUILD",
        "target(name = \"sh\", driver = \"bash\", run = \"true\", cache = False)\n",
    )
    .expect("write BUILD");

    let mut session = ShellSession::spawn(&dist, ws.root(), &["run", "--shell", "//pkg:sh"]);
    session.ready();

    for i in 0..3 {
        // Split so the marker cannot be satisfied by the PTY's echo of this
        // very line — otherwise the round-trip this test exists to prove is
        // never actually observed. See `echo_marker`.
        let (cmd, marker) = echo_marker("marco-", &format!("{i}-polo"));
        session.send_str(&format!("{cmd}\n"));
        session.wait_for(&marker, Duration::from_secs(10));
        session.wait_for("shellpty#", Duration::from_secs(10));
    }

    let before_exit = Instant::now();
    session.send_str("exit\n");
    let (exited, elapsed, success) = session.wait_exit(DEADLINE);
    assert!(
        exited,
        "heph did not exit after `exit`\n{}",
        session.output_lossy()
    );
    assert!(
        success,
        "shell session exited non-zero\n{}",
        session.output_lossy()
    );
    assert!(
        // Generous: process teardown (pty relay thread, child reaping,
        // sandbox cleanup handoff) is real work whose wall-clock cost varies
        // with how loaded the CI runner is. This is a "did we hang"
        // threshold, not a precision timing check — a genuine hang is still
        // caught independently by `wait_exit`'s 60s `DEADLINE` above
        // returning `exited: false`.
        elapsed < Duration::from_secs(15),
        "exit took {elapsed:?} after `exit` was typed at {before_exit:?} ago — should be prompt",
    );
}

/// Ctrl-C during an interactive shell must interrupt the foreground child
/// (not the `heph` process, and not the whole session) and return control to
/// the bash prompt — not hang, and not tear down the session. A second
/// command typed afterwards must still work, and the eventual `exit` must
/// still be prompt.
#[test]
fn ctrl_c_interrupts_foreground_child_not_the_session() {
    let dist = Dist::locate();
    let ws = common::Workspace::new().expect("workspace");
    ws.write(
        "pkg/BUILD",
        "target(name = \"sh\", driver = \"bash\", run = \"true\", cache = False)\n",
    )
    .expect("write BUILD");

    let mut session = ShellSession::spawn(&dist, ws.root(), &["run", "--shell", "//pkg:sh"]);
    session.ready();

    // A long foreground sleep. If Ctrl-C failed to reach the child (or
    // reached the wrong process), getting back to a prompt would take the
    // full 30s instead of a moment.
    //
    // Deliberately NOT chained as `sleep 30; echo done` to detect
    // completion: whether a shell continues on to the next `;`-separated
    // command after the previous one was killed by SIGINT is itself
    // shell-version-dependent. Heph's sandboxed exec PATH for a plain
    // `bash` target is `/usr/local/bin:/usr/bin:/bin` (see
    // `crates/plugin-exec/src/pluginexec/mod.rs`'s `sandbox_path_display`),
    // which on macOS resolves to Apple's ancient `/bin/bash` 3.2.57 —
    // confirmed independently of heph (a bare PTY running that exact
    // binary) that it *does* run the next `;`-separated command even after
    // the previous one died from Ctrl-C, unlike a modern bash 5.x which
    // aborts the rest of the list. A test that asserts the trailing
    // command's output never appears is therefore not testing Ctrl-C at
    // all on this shell — it is testing an assumption that happens to be
    // false here, and would previously "pass" only because a synchronization
    // bug in `wait_for` (see its doc comment above) let the check race ahead
    // and observe the output before old bash had gotten around to printing
    // it. Measuring how quickly the prompt returns is the version-independent
    // way to prove the child was actually interrupted.
    session.send_str("sleep 30\n");
    // Wait for the shell to have actually echoed our typed command before
    // interrupting it — sending Ctrl-C the instant bytes are written would
    // race bash's own read of the line.
    session.wait_for("sleep 30", Duration::from_secs(10));
    // Give bash a short, generous moment to actually fork+exec `sleep` as
    // the foreground child before interrupting it — there is no
    // terminal-observable event for "the child is now running" to wait on
    // instead, since `sleep` itself produces no output.
    std::thread::sleep(Duration::from_millis(500));

    let interrupted_at = Instant::now();
    session.send(b"\x03"); // Ctrl-C

    // Back at the prompt well under the sleep's duration.
    session.wait_for("shellpty#", Duration::from_secs(10));
    let recovered = interrupted_at.elapsed();
    assert!(
        recovered < Duration::from_secs(10),
        "took {recovered:?} to get back to a prompt after Ctrl-C — the child was not interrupted\n{}",
        session.output_lossy(),
    );

    // The session itself must still be alive and usable after the interrupt.
    // Split marker: matching our own echo would prove the PTY relayed a
    // keystroke, not that bash survived the interrupt and ran a command.
    let (alive_cmd, alive_marker) = echo_marker("still-", "alive");
    session.send_str(&format!("{alive_cmd}\n"));
    session.wait_for(&alive_marker, Duration::from_secs(10));

    let before_exit = Instant::now();
    session.send_str("exit\n");
    let (exited, elapsed, success) = session.wait_exit(DEADLINE);
    assert!(
        exited,
        "heph did not exit after `exit`\n{}",
        session.output_lossy()
    );
    assert!(
        success,
        "shell session exited non-zero\n{}",
        session.output_lossy()
    );
    assert!(
        // Generous: process teardown (pty relay thread, child reaping,
        // sandbox cleanup handoff) is real work whose wall-clock cost varies
        // with how loaded the CI runner is. This is a "did we hang"
        // threshold, not a precision timing check — a genuine hang is still
        // caught independently by `wait_exit`'s 60s `DEADLINE` above
        // returning `exited: false`.
        elapsed < Duration::from_secs(15),
        "exit took {elapsed:?} after `exit` was typed {before_exit:?} ago — should be prompt",
    );
}

/// Exit promptness under three shapes that stress the output side while the
/// session stays otherwise idle on stdin: a child chatty on one stream and
/// silent on the other, and a child producing output far beyond typical
/// terminal-scroll volume. All three must let `exit` end the session quickly
/// once the foreground command finishes — no lingering drain, no zombie
/// wait.
#[test]
fn exit_is_prompt_after_chatty_and_large_output() {
    let dist = Dist::locate();
    let ws = common::Workspace::new().expect("workspace");
    ws.write(
        "pkg/BUILD",
        "target(name = \"sh\", driver = \"bash\", run = \"true\", cache = False)\n",
    )
    .expect("write BUILD");

    let mut session = ShellSession::spawn(&dist, ws.root(), &["run", "--shell", "//pkg:sh"]);
    session.ready();

    // Chatty on stderr, silent on stdout — the exact shape #245's commit
    // message names as the case the old per-stream `tokio::join!` starved.
    //
    // Both waits here are *synchronization*, not assertions: this test's
    // claim is about how fast `exit` returns, which is measured by
    // `wait_exit` below. So they get `DEADLINE` rather than a tight bound —
    // a genuine hang still fails the test, just at 60s instead of 15s, and
    // a budget sized to a loaded runner's worst case is the only kind that
    // does not eventually go flaky.
    let (batch_cmd, batch_marker) = echo_marker("stderr-batch", "-done");
    session.send_str(&format!(
        "for i in $(seq 1 200); do echo err-$i >&2; done; {batch_cmd}\n"
    ));
    session.wait_for(&batch_marker, DEADLINE);
    session.wait_for("shellpty#", DEADLINE);

    // Comfortably past the 512 KiB drain bound plus the 64 KiB pipe.
    let (large_cmd, large_marker) = echo_marker("large-output", "-done");
    // `fold` so the 900 KiB arrives as lines rather than one 900 KiB line. The
    // volume is the point (comfortably past the 512 KiB drain bound plus the
    // 64 KiB pipe); a single unbroken line is not, and a pty's line discipline
    // handles one that long badly enough that the transfer does not finish
    // inside a 60s budget. That never showed while these waits were matching
    // their own echo instead of the output.
    session.send_str(&format!(
        "head -c 900000 /dev/zero | tr '\\0' 'x' | fold -w 256; {large_cmd}\n"
    ));
    session.wait_for(&large_marker, DEADLINE);
    session.wait_for("shellpty#", DEADLINE);

    let before_exit = Instant::now();
    session.send_str("exit\n");
    let (exited, elapsed, success) = session.wait_exit(DEADLINE);
    assert!(
        exited,
        "heph did not exit after `exit`\n{}",
        session.output_lossy()
    );
    assert!(
        success,
        "shell session exited non-zero\n{}",
        session.output_lossy()
    );
    assert!(
        // Generous: process teardown (pty relay thread, child reaping,
        // sandbox cleanup handoff) is real work whose wall-clock cost varies
        // with how loaded the CI runner is. This is a "did we hang"
        // threshold, not a precision timing check — a genuine hang is still
        // caught independently by `wait_exit`'s 60s `DEADLINE` above
        // returning `exited: false`.
        elapsed < Duration::from_secs(15),
        "exit took {elapsed:?} after `exit` was typed {before_exit:?} ago — should be prompt",
    );
}

/// [`echo_marker`]'s contract, in both directions.
///
/// Cheap enough to belong here despite the `bin-e2e` bar (see
/// `.claude/testing.md`): it spawns no heph binary and needs no staged dist —
/// it only pins the helper every other test in this file now depends on. The
/// `bash` half is not ceremony: the whole trick rests on the shell folding
/// `''` away, and the shell that actually runs these sessions on macOS CI is
/// bash 3.2.57.
#[test]
fn echo_marker_prints_the_marker_without_typing_it() {
    let (cmd, marker) = echo_marker("stderr-batch", "-done");
    assert_eq!(marker, "stderr-batch-done");
    assert!(
        !cmd.contains(&marker),
        "command {cmd:?} contains the marker it prints, so the terminal's echo \
         of it would satisfy wait_for({marker:?}) before the command had run"
    );

    let out = std::process::Command::new("bash")
        .arg("-c")
        .arg(&cmd)
        .output()
        .expect("run bash");
    assert!(out.status.success(), "bash rejected {cmd:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        marker,
        "bash must still print the marker whole"
    );
}
