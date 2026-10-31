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

/// Generous: spawns a release binary plus an interactive bash child, on a
/// shared CI runner.
const DEADLINE: Duration = Duration::from_secs(60);

/// How often `wait_for` re-checks the accumulated output.
const POLL: Duration = Duration::from_millis(20);

/// The prompt [`ShellSession::ready`] installs, as `wait_for` looks for it.
const PROMPT: &str = "shellpty#";

/// The line that installs [`PROMPT`]. Typed so that it does not contain the
/// prompt itself — bash folds the `''` away, the echo keeps it. See `ready` for
/// why that is load-bearing.
const SET_PROMPT: &str = "PS1='shell''pty# '";

/// One poll's outcome in [`scan_once`].
#[derive(Debug, PartialEq, Eq)]
enum Scan {
    /// `needle` ends at this absolute offset; the cursor moves here.
    Found { end: usize },
    /// Not present yet. The next poll may start here — everything before it has
    /// been searched.
    Resume { from: usize },
}

/// Search `buf[scan_from..]` for `needle`, and say where the next poll should
/// pick up.
///
/// Split out of `wait_for` so the thing that actually went wrong is testable
/// without a pty, a child process, or a 900 KiB transfer.
///
/// The rule that matters: on a miss the next scan resumes at the end of what was
/// just searched, minus `needle.len() - 1` so an occurrence straddling a poll
/// boundary is still found. Re-searching from `scan_from` every poll instead
/// makes the work O(bytes x polls) with the capture lock held, which starves the
/// pty relay thread and stalls the very output being waited on.
fn scan_once(buf: &[u8], needle: &[u8], scan_from: usize) -> Scan {
    let hay = buf.get(scan_from..).unwrap_or(&[]);
    if let Some(pos) = hay.windows(needle.len()).position(|w| w == needle) {
        return Scan::Found {
            end: scan_from + pos + needle.len(),
        };
    }
    Scan::Resume {
        from: buf
            .len()
            .saturating_sub(needle.len().saturating_sub(1))
            .max(scan_from),
    }
}

#[cfg(test)]
mod scan_tests {
    use super::{Scan, scan_once};

    /// The straddling case the overlap exists for: a needle split across two
    /// polls is found by the second one.
    ///
    /// Without the `needle.len() - 1` rewind, `Resume { from }` would land past
    /// the needle's first byte and the marker would never be seen — the test
    /// would hang to its deadline with the bytes sitting in the buffer.
    #[test]
    fn a_needle_split_across_two_polls_is_found() {
        let needle = b"done";
        let first = b"...do";
        let Scan::Resume { from } = scan_once(first, needle, 0) else {
            panic!("must not match on a partial needle");
        };
        let mut whole = first.to_vec();
        whole.extend_from_slice(b"ne...");
        assert_eq!(
            scan_once(&whole, needle, from),
            Scan::Found { end: 7 },
            "the second poll must find a needle that straddles the boundary"
        );
    }

    /// A miss advances the resume point, so the next poll re-reads only the
    /// overlap rather than the whole tail. This is the O(bytes x polls) fix, and
    /// it is asserted as an offset because that is the only observable form it
    /// has.
    #[test]
    fn a_miss_does_not_rescan_from_the_start() {
        let needle = b"done";
        let buf = vec![b'x'; 100_000];
        let Scan::Resume { from } = scan_once(&buf, needle, 0) else {
            panic!("no match expected");
        };
        assert_eq!(
            from,
            buf.len() - (needle.len() - 1),
            "a miss must leave only the straddle overlap to re-read"
        );
    }

    /// The resume point never rewinds behind where this `wait_for` call started,
    /// so a needle already consumed by an earlier call cannot satisfy a later
    /// one — the property the `cursor` exists for.
    #[test]
    fn the_resume_point_never_rewinds_behind_the_cursor() {
        let needle = b"shellpty#";
        let buf = b"shellpty# ".to_vec();
        let Scan::Resume { from } = scan_once(&buf, needle, 10) else {
            panic!("must not re-match output before the cursor");
        };
        assert!(from >= 10, "resumed at {from}, behind the cursor at 10");
    }
}

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
    master: Box<dyn portable_pty::MasterPty + Send>,
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
                        for _ in 0..common::take_dsr_queries(&mut tail) {
                            let mut w = dsr_writer.lock().expect("writer lock");
                            if w.write_all(common::DSR_REPLY).is_err() || w.flush().is_err() {
                                break;
                            }
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
            master: pair.master,
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

    /// Resize the caller's window, as dragging a terminal's edge does.
    fn resize(&self, rows: u16, cols: u16) {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize the pty");
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
    /// forever — a fixed marker like the `shellpty#` prompt matches on the
    /// first prompt and then every
    /// later `wait_for(PROMPT, ..)` in the file would return instantly
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
        assert!(
            self.try_wait_for(needle, timeout),
            "never saw {needle:?} within {timeout:?}\n--- captured ---\n{}",
            self.output_lossy(),
        );
    }

    /// [`Self::wait_for`], reporting a timeout instead of panicking.
    fn try_wait_for(&mut self, needle: &str, timeout: Duration) -> bool {
        let started = Instant::now();
        let needle = needle.as_bytes();
        if needle.is_empty() {
            return true;
        }
        // Where the next poll starts searching. Distinct from `cursor`, which
        // only moves on a match: this advances every poll to the end of what has
        // already been searched and found not to contain the needle.
        //
        // Without it each poll re-searched the whole unseen tail, so the work was
        // O(bytes x polls) rather than O(bytes) — and it ran with the capture
        // lock held. Against the 900 KiB case that is ~45 MB/s of memcmp at 50
        // polls a second, during which the relay thread cannot append, so the pty
        // backs up and the child stalls. The test then misses its 60s budget
        // *because it was watching*, which is why it failed on a different CI
        // target almost every run rather than reproducibly.
        //
        // Searching under the lock (rather than cloning first) is still right,
        // and for the same reason — the clone was the earlier version of this
        // bug. Both halves are needed: don't copy, and don't re-read.
        let mut scan_from = self.cursor;
        loop {
            {
                let buf = self.captured.lock().expect("capture lock");
                match scan_once(&buf, needle, scan_from) {
                    Scan::Found { end } => {
                        self.cursor = end;
                        return true;
                    }
                    Scan::Resume { from } => scan_from = from,
                }
            }
            if started.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(POLL);
        }
    }

    /// Ctrl-C [`FOLLOWING_TOOL`] until its shell shows a fresh prompt.
    ///
    /// One Ctrl-C is not enough to rely on: a bash in a `sleep` loop with an `INT`
    /// trap sometimes runs on, the loop alive and `^C` the last thing on the
    /// terminal. That is bash, not heph: it
    /// reproduces on a bare PTY with no heph involved (bash 3.2 and 5.3, 2-3% of
    /// attempts with the Ctrl-C's timing spread over one nap), and a second
    /// Ctrl-C ends the loop. A heph that drops every Ctrl-C still fails this.
    fn interrupt_to_prompt(&mut self) {
        let started = Instant::now();
        loop {
            self.send(b"\x03");
            if self.try_wait_for(PROMPT, Duration::from_secs(2)) {
                return;
            }
            assert!(
                started.elapsed() < DEADLINE,
                "no {PROMPT:?} after repeated Ctrl-C within {DEADLINE:?}\n--- captured ---\n{}",
                self.output_lossy(),
            );
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

    /// Wait for `init.sh`'s banner, then override its fixed `PS1='$ '` with
    /// a unique marker so later `wait_for` calls can't collide with shell
    /// furniture or command output that happens to contain "$ ", and return
    /// once bash has **shown** that prompt.
    ///
    /// The banner is printed while `init.sh` is still running, before bash's
    /// readline has prepared the terminal, so the `PS1` line usually lands
    /// while the session's pty is still canonical — the kernel echoes it. That
    /// echo must not end this wait, which is why [`SET_PROMPT`] does not
    /// contain [`PROMPT`]: when it did, `ready` returned on the echo, and a
    /// caller's next keystroke could reach bash before readline was reading.
    /// For a Ctrl-D that is fatal on Linux only. A canonical-mode EOF is
    /// stored by the line discipline as a NUL, so once readline switches the
    /// terminal to non-canonical it reads `\0` (`set-mark`, silent) instead of
    /// `\x04`, bash sits at its new prompt, and heph never exits. macOS keeps
    /// the `\x04`. That was `ctrl_d_ends_the_session_like_exit` failing ~1% of
    /// the time on `linux/*` with nothing after the prompt.
    ///
    /// The prompt itself is safe to sync on: readline prepares the terminal
    /// (non-canonical, and bracketed paste — the `\x1b[?2004h` before it)
    /// *before* it draws the prompt.
    fn ready(&mut self) {
        self.wait_for("Shell mode, to exit", DEADLINE);
        self.send_str(&format!("{SET_PROMPT}\n"));
        self.wait_for(PROMPT, Duration::from_secs(10));
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
        session.wait_for(PROMPT, Duration::from_secs(10));
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

/// Ctrl-D at the prompt must end the session as cleanly as typing `exit`.
///
/// It is not a signal and never becomes one: shell mode puts the outer terminal
/// in raw mode for the pty relay, so the `0x04` is a byte handed to the inner
/// pty, whose line discipline turns it into EOF for bash — which then exits, and
/// heph exits with it. That whole chain is what this asserts, and none of it
/// exists off a terminal: without a pty there is no line discipline to make EOF
/// out of a byte, and `heph` never enters shell mode at all.
///
/// Distinct from `shell_echoes_typed_input_and_exits_promptly`, which types the
/// word `exit` — an ordinary line of input that proves nothing about the EOF
/// path. A regression that broke only Ctrl-D (a relay that swallows `0x04`, or
/// a raw-mode window that lets the outer tty eat it) leaves that test green.
#[test]
fn ctrl_d_ends_the_session_like_exit() {
    let dist = Dist::locate();
    let ws = common::Workspace::new().expect("workspace");
    ws.write(
        "pkg/BUILD",
        "target(name = \"sh\", driver = \"bash\", run = \"true\", cache = False)\n",
    )
    .expect("write BUILD");

    let mut session = ShellSession::spawn(&dist, ws.root(), &["run", "--shell", "//pkg:sh"]);
    session.ready();

    // EOT. At a prompt with no partial line pending, bash reads it as
    // end-of-input and exits — the same terminal convention as `exit`.
    session.send(&[0x04]);

    let (exited, elapsed, success) = session.wait_exit(DEADLINE);
    assert!(
        exited,
        "heph did not exit after ctrl-d\n{}",
        session.output_lossy()
    );
    assert!(
        success,
        "a session ended by ctrl-d must exit zero, like `exit` does\n{}",
        session.output_lossy()
    );
    // Same "did we hang" threshold, and the same reasoning, as the `exit` test
    // above — not a precision timing check.
    assert!(
        elapsed < Duration::from_secs(15),
        "exit took {elapsed:?} after ctrl-d — should be prompt\n{}",
        session.output_lossy()
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
    session.wait_for(PROMPT, Duration::from_secs(10));
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
    session.wait_for(PROMPT, DEADLINE);

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
    session.wait_for(PROMPT, DEADLINE);

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

/// The invocation sandboxes of `//pkg:tool` under the workspace's home: the
/// one a running `--shell` session's `init.sh` lives in.
fn entrypoint_shell_dirs(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(root.join(".heph/sandbox/pkg/__invoke/tool"))
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// `heph run --shell` on an entrypoint (D13): an interactive bash in
/// the program's world, whose `run` is the program's argv plus the caller's
/// args — `a b` and the empty string intact — or a helper's own args when it
/// is given any. Ctrl-C at the prompt is the shell's, not heph's; the shell's
/// exit status is heph's; the terminal is left as it was found and the init
/// file is gone.
#[test]
fn entrypoint_shell_pty() {
    let dist = Dist::locate();
    let ws = common::Workspace::new().expect("workspace");
    ws.write(
        "pkg/BUILD",
        "target(name = \"tool\", driver = \"exec\", entrypoint = True, args = True,\n\
         run = [\"/bin/sh\", \"-c\", \"printf '<%s>' \\\"$@\\\"; echo\", \"tool\"])\n",
    )
    .expect("write BUILD");

    let mut session = ShellSession::spawn(
        &dist,
        ws.root(),
        &["run", "--shell", "//pkg:tool", "--", "a b", ""],
    );
    session.ready();
    assert_eq!(
        entrypoint_shell_dirs(ws.root()).len(),
        1,
        "the init file lives in the invocation's sandbox while the shell runs"
    );

    // `run` with no args: the program's argv plus the caller's.
    session.send_str("run\n");
    session.wait_for("<a b><>", Duration::from_secs(10));
    session.wait_for(PROMPT, Duration::from_secs(10));

    // `run x`: its own args, not the originals. The line ends right after it.
    session.send_str("run override\n");
    session.wait_for("<override>\r\n", Duration::from_secs(10));
    session.wait_for(PROMPT, Duration::from_secs(10));

    // `show`: what `run` would execute, args shell-quoted — "what will it
    // run" (R20). The typed `'q r'` is not the printed `q\ r`.
    session.send_str("show 'q r'\n");
    session.wait_for(r"q\ r", Duration::from_secs(10));
    session.wait_for(PROMPT, Duration::from_secs(10));

    // Ctrl-C at the prompt reaches the shell, not heph.
    session.send(b"\x03");
    session.wait_for(PROMPT, Duration::from_secs(10));
    let (alive_cmd, alive_marker) = echo_marker("still-", "here");
    session.send_str(&format!("{alive_cmd}\n"));
    session.wait_for(&alive_marker, Duration::from_secs(10));
    session.wait_for(PROMPT, Duration::from_secs(10));

    session.send_str("exit 3\n");
    let started = Instant::now();
    let code = loop {
        match session.child.try_wait().expect("wait on pty child") {
            Some(status) => break status.exit_code(),
            None => {
                assert!(
                    started.elapsed() < DEADLINE,
                    "heph did not exit after `exit 3`\n{}",
                    session.output_lossy()
                );
                std::thread::sleep(POLL);
            }
        }
    };
    assert_eq!(
        code,
        3,
        "the shell's status is heph's\n{}",
        session.output_lossy()
    );

    let mut screen = vt100::Parser::new(40, 140, 0);
    screen.process(&session.output_snapshot());
    assert!(
        !screen.screen().alternate_screen(),
        "left the alternate screen on"
    );
    assert!(!screen.screen().hide_cursor(), "left the cursor hidden");
    assert!(
        entrypoint_shell_dirs(ws.root()).is_empty(),
        "the invocation's sandbox (and its init file) outlived the shell"
    );
}

/// What the `run` helper executes in the two tests below: it reports the wrap
/// runner's env, echoes a typed line, then reports each window size from its
/// WINCH trap until a Ctrl-C ends it.
///
/// This loop can lose a Ctrl-C, which is why [`ShellSession::interrupt_to_prompt`]
/// exists. Don't "fix" it with `sleep & wait`: that hangs too, with the macOS
/// `/bin/bash` 3.2 spinning on the CPU.
const FOLLOWING_TOOL: &str = r#"driver = "bash", cache = False, run = """
echo "runner-mark-[${XR_MARK:-none}]"
echo "read-""ready"
read -r line
echo "typed-[$line]"
trap 'echo "resized-to-$(/bin/stty size)-end"' WINCH
trap 'exit 0' INT
echo "size-""ready"
while true; do sleep 0.05 || true; done
""""#;

/// A `--shell` session at its prompt runs [`FOLLOWING_TOOL`]: typed stdin and a
/// live resize both reach it, the trap's marker being the barrier and the size
/// it reads the new one. The shell then exits cleanly.
fn tool_follows_the_terminal(session: &mut ShellSession, mark: &str) {
    session.ready();
    session.send_str("run\n");
    session.wait_for(&format!("runner-mark-[{mark}]"), DEADLINE);
    session.wait_for("read-ready", DEADLINE);
    session.send_str("hello-tool\n");
    session.wait_for("typed-[hello-tool]", DEADLINE);
    session.wait_for("size-ready", DEADLINE);
    session.resize(30, 100);
    session.wait_for("resized-to-30 100-end", DEADLINE);
    session.interrupt_to_prompt();
    session.send_str("exit\n");
    let (exited, _, success) = session.wait_exit(DEADLINE);
    assert!(
        exited && success,
        "exited={exited} success={success}\n{}",
        session.output_lossy()
    );
}

/// `--shell` on a build target under a wrap runner, today's local-process kind:
/// `/usr/bin/env` is all it needs, so a missing prerequisite fails rather than
/// skips. The runner's env reaching the tool shows the wrap really happened; it
/// names `PATH` because a runner supplies the environment its targets run in,
/// and the shell comes from there rather than from heph's own `PATH`.
#[test]
fn local_runner_shell_keeps_working() {
    let dist = Dist::locate();
    let ws = common::Workspace::new().expect("workspace");
    ws.write(
        "pkg/BUILD",
        &format!(
            r#"
target(
    name = "runner",
    driver = "bash",
    run = """cat > $OUT <<'JSON'
{{"version": 1, "fingerprint": "fp-1", "runner": "wrap",
 "config": {{"prefix": ["/usr/bin/env"],
             "env": {{"XR_MARK": "wrapped", "PATH": "/usr/local/bin:/usr/bin:/bin"}}}}}}
JSON""",
    out = "runner.json",
)
target(name = "tool", runner = "//pkg:runner", {FOLLOWING_TOOL})
"#
        ),
    )
    .expect("write BUILD");

    let mut session = ShellSession::spawn(&dist, ws.root(), &["run", "--shell", "//pkg:tool"]);
    tool_follows_the_terminal(&mut session, "wrapped");
}

/// The same with no runner at all: a resize while the shell runs reaches the
/// tool through the driver's PTY.
#[test]
fn build_target_shell_forwards_resize() {
    let dist = Dist::locate();
    let ws = common::Workspace::new().expect("workspace");
    ws.write(
        "pkg/BUILD",
        &format!("target(name = \"tool\", {FOLLOWING_TOOL})\n"),
    )
    .expect("write BUILD");

    let mut session = ShellSession::spawn(&dist, ws.root(), &["run", "--shell", "//pkg:tool"]);
    tool_follows_the_terminal(&mut session, "none");
}

/// [`SET_PROMPT`]'s contract, in both directions — the same two halves as
/// `echo_marker_prints_the_marker_without_typing_it`, for the line `ready`
/// syncs on.
///
/// The first half is the regression guard. When the typed line contained the
/// prompt, `ready` returned on the terminal's echo of it, and a Ctrl-D sent
/// next could reach bash before readline had taken its terminal out of
/// canonical mode; on Linux that EOF is then read back as a NUL byte and bash
/// never exits (see `ready`). Nothing about that is deterministic in a pty
/// run — it failed ~1% of the time, Linux only — but the precondition is, and
/// this pins it.
#[test]
fn set_prompt_installs_the_prompt_without_typing_it() {
    assert!(
        !SET_PROMPT.contains(PROMPT),
        "{SET_PROMPT:?} contains the prompt it installs, so the terminal's echo \
         of it would satisfy wait_for({PROMPT:?}) before bash has shown a prompt"
    );

    let out = std::process::Command::new("bash")
        .arg("-c")
        .arg(format!("{SET_PROMPT}; printf %s \"$PS1\""))
        .output()
        .expect("run bash");
    assert!(out.status.success(), "bash rejected {SET_PROMPT:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with(PROMPT),
        "bash must still install the prompt whole, got {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}
