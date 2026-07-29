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

struct ShellSession {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    captured: Arc<Mutex<Vec<u8>>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
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

    /// Block until `needle` shows up in the accumulated output, or panic with
    /// everything captured so far.
    fn wait_for(&self, needle: &str, timeout: Duration) {
        let started = Instant::now();
        loop {
            if self.output_lossy().contains(needle) {
                return;
            }
            assert!(
                started.elapsed() < timeout,
                "never saw {needle:?} within {timeout:?}\n--- captured ---\n{}",
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
        let marker = format!("marco-{i}-polo");
        session.send_str(&format!("echo {marker}\n"));
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
        elapsed < Duration::from_secs(5),
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
    // reached the wrong process), this would still be running 30s later and
    // the marker sent after it would only appear after that whole sleep.
    session.send_str("sleep 30; echo slept-done\n");
    // Give the child a moment to actually get into the sleep before
    // interrupting it — otherwise this races bash's own startup. Snapshot
    // right before interrupting: the terminal's local echo of the line just
    // typed lands almost immediately and *does* contain the literal text
    // "slept-done" as part of the command itself, so the completion check
    // below must only look at bytes that arrive *after* this point.
    std::thread::sleep(Duration::from_millis(500));
    let before_len = session.output_snapshot().len();

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
    // Never seeing "slept-done" *after* the interrupt was sent — that would
    // mean `echo slept-done` actually ran, i.e. the sleep completed and
    // Ctrl-C never interrupted it.
    let after = session.output_snapshot();
    let since_interrupt = String::from_utf8_lossy(after.get(before_len..).unwrap_or(&[]));
    assert!(
        !since_interrupt.contains("slept-done"),
        "sleep ran to completion — Ctrl-C never interrupted it\n{}",
        session.output_lossy(),
    );

    // The session itself must still be alive and usable after the interrupt.
    session.send_str("echo still-alive\n");
    session.wait_for("still-alive", Duration::from_secs(10));

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
        elapsed < Duration::from_secs(5),
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
    session.send_str("for i in $(seq 1 200); do echo err-$i >&2; done; echo stderr-batch-done\n");
    session.wait_for("stderr-batch-done", Duration::from_secs(15));
    session.wait_for("shellpty#", Duration::from_secs(10));

    // Comfortably past the 512 KiB drain bound plus the 64 KiB pipe.
    session.send_str("head -c 900000 /dev/zero | tr '\\0' 'x'; echo large-output-done\n");
    session.wait_for("large-output-done", Duration::from_secs(20));
    session.wait_for("shellpty#", Duration::from_secs(10));

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
        elapsed < Duration::from_secs(5),
        "exit took {elapsed:?} after `exit` was typed {before_exit:?} ago — should be prompt",
    );
}
