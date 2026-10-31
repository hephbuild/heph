//! A `heph` child on a real PTY, read through a terminal emulator.
//!
//! Every assertion goes through [`vt100`] cells — what a person would have
//! seen — never through the raw byte stream. Each read is fed to the parser
//! and the screen is snapshotted into [`Pty::seen`], so text that scrolled
//! away or was painted over still counts as having been on screen.

use super::Dist;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const ROWS: u16 = 40;
pub const COLS: u16 = 140;
const POLL: Duration = Duration::from_millis(20);

/// How the child ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    Code(u32),
    /// The signal's name as the OS spells it (`portable_pty`'s rendering).
    Signal(String),
}

struct Shared {
    parser: vt100::Parser,
    /// Every screen, as rendered after each read, concatenated.
    seen: String,
}

pub struct Pty {
    writer: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
    shared: Arc<Mutex<Shared>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    reader: Option<std::thread::JoinHandle<()>>,
    /// Offset into `seen` up to which [`Pty::wait_for`] has matched.
    cursor: usize,
    _home: tempfile::TempDir,
}

impl Pty {
    /// Spawn `heph <args>` in `cwd` on a fresh PTY, with the harness's
    /// neutral environment plus `env`. Cursor-position queries are answered as
    /// a terminal would (see `common::take_dsr_queries`).
    pub fn spawn(dist: &Dist, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Self {
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
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).expect("spawn under pty");
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().expect("clone pty reader");
        let writer: Arc<Mutex<Box<dyn std::io::Write + Send>>> = Arc::new(Mutex::new(
            pair.master.take_writer().expect("take pty writer"),
        ));
        let shared = Arc::new(Mutex::new(Shared {
            parser: vt100::Parser::new(ROWS, COLS, 0),
            seen: String::new(),
        }));
        let sink = Arc::clone(&shared);
        let dsr = Arc::clone(&writer);
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut tail = Vec::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let Some(chunk) = buf.get(..n) else { break };
                        tail.extend_from_slice(chunk);
                        for _ in 0..super::take_dsr_queries(&mut tail) {
                            let mut w = dsr.lock().expect("writer lock");
                            if w.write_all(super::DSR_REPLY).is_err() || w.flush().is_err() {
                                break;
                            }
                        }
                        let mut s = sink.lock().expect("screen lock");
                        s.parser.process(chunk);
                        let screen = s.parser.screen().contents();
                        s.seen.push_str(&screen);
                        s.seen.push('\n');
                    }
                }
            }
        });
        Self {
            writer,
            shared,
            child,
            master: Some(pair.master),
            reader: Some(reader),
            cursor: 0,
            _home: home,
        }
    }

    /// heph's pid.
    pub fn pid(&self) -> i32 {
        let pid = self.child.process_id().expect("pty child pid");
        i32::try_from(pid).expect("pid fits")
    }

    pub fn send(&self, bytes: &[u8]) {
        let mut w = self.writer.lock().expect("writer lock");
        w.write_all(bytes).expect("write to pty");
        w.flush().expect("flush pty");
    }

    /// Everything that was on screen at some point.
    pub fn seen(&self) -> String {
        self.shared.lock().expect("screen lock").seen.clone()
    }

    /// The screen as it is now.
    pub fn screen(&self) -> String {
        self.shared
            .lock()
            .expect("screen lock")
            .parser
            .screen()
            .contents()
    }

    /// Whether the terminal's cursor is visible now.
    pub fn cursor_visible(&self) -> bool {
        !self
            .shared
            .lock()
            .expect("screen lock")
            .parser
            .screen()
            .hide_cursor()
    }

    /// Block until `needle` is painted on screen after everything earlier
    /// `wait_for`s matched, or panic with what was seen.
    ///
    /// A PTY echoes what is typed, so a needle must not appear in a command
    /// line sent before it (build markers the way `shell_pty.rs`'s
    /// `echo_marker` does).
    pub fn wait_for(&mut self, needle: &str, timeout: Duration) {
        let started = Instant::now();
        loop {
            {
                let s = self.shared.lock().expect("screen lock");
                if let Some(pos) = s.seen.get(self.cursor..).and_then(|t| t.find(needle)) {
                    self.cursor += pos + needle.len();
                    return;
                }
            }
            assert!(
                started.elapsed() < timeout,
                "never saw {needle:?} within {timeout:?}\n--- screen ---\n{}",
                self.screen()
            );
            std::thread::sleep(POLL);
        }
    }

    /// Wait for heph to exit, then for the reader to drain, and say how it
    /// ended; panics if it outlives `timeout` (and kills it).
    pub fn wait_exit(&mut self, timeout: Duration) -> Ended {
        let started = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("wait on pty child") {
                break status;
            }
            let late = started.elapsed() > timeout;
            if late {
                drop(self.child.kill());
            }
            assert!(
                !late,
                "heph did not exit within {timeout:?}\n--- screen ---\n{}",
                self.screen()
            );
            std::thread::sleep(POLL);
        };
        // The slave side closed with the child; give the reader its last bytes.
        // (A tool left running would hold it open: the caller checks that.)
        let drained = Instant::now();
        while self.reader.as_ref().is_some_and(|r| !r.is_finished())
            && drained.elapsed() < Duration::from_secs(5)
        {
            std::thread::sleep(POLL);
        }
        if self
            .reader
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
            && let Some(r) = self.reader.take()
        {
            drop(r.join());
        }
        drop(self.master.take());
        match status.signal() {
            Some(sig) => Ended::Signal(sig.to_string()),
            None => Ended::Code(status.exit_code()),
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            drop(self.child.kill());
        }
    }
}
