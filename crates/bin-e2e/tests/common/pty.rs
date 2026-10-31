//! A `heph` child on a real PTY, read through a terminal emulator.
//!
//! Every assertion goes through [`vt100`] cells — what a person would have
//! seen — never through the raw byte stream. Each read is fed to the parser
//! and the screen is snapshotted into [`Pty::seen`], so text that scrolled
//! away or was painted over still counts as having been on screen. The one
//! exception is [`Pty::output`], for a test that counts every byte of a large
//! transfer, which no screen can hold.
//!
//! What runs on the PTY is `heph` itself ([`Pty::spawn`]), or a shell that runs
//! it as a job: [`Pty::shell`] is the interactive one a person has, and
//! [`Pty::script`] is one with job control and nothing else, because an
//! interactive bash puts its own termios back whenever a job stops and so hides
//! what heph left behind.

use super::Dist;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::ffi::OsStr;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const ROWS: u16 = 40;
pub const COLS: u16 = 140;
const POLL: Duration = Duration::from_millis(20);
/// How long the reader blocks before looking whether the terminal was closed.
const READER_TICK_US: i32 = 50_000;

/// How the child ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    Code(u32),
    /// The signal's name as the OS spells it (`portable_pty`'s rendering).
    Signal(String),
}

/// What `tcgetattr` says of the terminal, down to what a shell would notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Termios {
    pub iflag: libc::tcflag_t,
    pub oflag: libc::tcflag_t,
    pub cflag: libc::tcflag_t,
    pub lflag: libc::tcflag_t,
    pub cc: Vec<u8>,
}

impl Termios {
    /// A line-editing terminal, as opposed to the raw one heph runs a tool under.
    pub fn is_cooked(&self) -> bool {
        self.lflag & libc::ICANON != 0 && self.lflag & libc::ECHO != 0
    }
}

struct Shared {
    parser: vt100::Parser,
    /// Every screen, as rendered after each read, concatenated.
    seen: String,
    /// Every byte, in order.
    output: Vec<u8>,
    /// How many cursor-position queries were answered.
    dsr_queries: usize,
}

type Writer = Arc<Mutex<Option<Box<dyn std::io::Write + Send>>>>;

pub struct Pty {
    writer: Writer,
    shared: Arc<Mutex<Shared>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    reader: Option<std::thread::JoinHandle<()>>,
    closing: Arc<AtomicBool>,
    /// Offset into `seen` up to which [`Pty::wait_for`] has matched.
    cursor: usize,
    /// The number the next prompt of a [`Pty::shell`] will carry.
    prompt: u32,
    _home: tempfile::TempDir,
}

impl Pty {
    /// Spawn `heph <args>` in `cwd` on a fresh PTY, with the harness's
    /// neutral environment plus `env`. Cursor-position queries are answered as
    /// a terminal would (see `common::take_dsr_queries`).
    pub fn spawn(dist: &Dist, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Self {
        Self::spawn_program(dist, dist.heph().as_os_str(), cwd, args, env)
    }

    /// An interactive bash on the PTY, with `$HEPH` naming the binary, at its
    /// first prompt. It has no line editor, so what is typed is echoed plainly
    /// and the terminal stays cooked while it reads.
    pub fn shell(dist: &Dist, cwd: &Path, env: &[(&str, &str)]) -> Self {
        let mut env = env.to_vec();
        // `\#` makes each prompt one that no earlier screen can satisfy.
        env.push(("PS1", r"shpty-\#> "));
        let mut pty = Self::spawn_program(
            dist,
            OsStr::new("bash"),
            cwd,
            &["--norc", "--noprofile", "--noediting", "-i"],
            &env,
        );
        pty.prompt = 1;
        pty.wait_prompt();
        pty
    }

    /// `script` under a bash with job control that is not interactive: it reads
    /// nothing from the keyboard unless told to, and leaves the terminal as a
    /// stopped job left it. `$HEPH` names the binary.
    pub fn script(dist: &Dist, cwd: &Path, script: &str, env: &[(&str, &str)]) -> Self {
        Self::spawn_program(
            dist,
            OsStr::new("bash"),
            cwd,
            &["--norc", "--noprofile", "-c", script],
            env,
        )
    }

    fn spawn_program(
        dist: &Dist,
        program: &OsStr,
        cwd: &Path,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pty");
        let home = tempfile::tempdir().expect("home tempdir");
        let mut cmd = CommandBuilder::new(program);
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
        cmd.env("HEPH", dist.heph());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).expect("spawn under pty");
        drop(pair.slave);

        // A descriptor of its own rather than `try_clone_reader`, so that the
        // reader can notice `closing` and let go of its copy of the master: the
        // terminal only closes when the last one does.
        let raw = pair.master.as_raw_fd().expect("the master's fd");
        // SAFETY: a live fd; the duplicate is owned below.
        let dup = unsafe { libc::fcntl(raw, libc::F_DUPFD_CLOEXEC, 3) };
        assert!(dup >= 0, "dup master: {}", std::io::Error::last_os_error());
        // SAFETY: just duplicated, and owned by nothing else.
        let read_fd = unsafe { OwnedFd::from_raw_fd(dup) };
        let writer: Writer = Arc::new(Mutex::new(Some(
            pair.master.take_writer().expect("take pty writer"),
        )));
        let shared = Arc::new(Mutex::new(Shared {
            parser: vt100::Parser::new(ROWS, COLS, 0),
            seen: String::new(),
            output: Vec::new(),
            dsr_queries: 0,
        }));
        let closing = Arc::new(AtomicBool::new(false));
        let reader = std::thread::spawn({
            let sink = Arc::clone(&shared);
            let dsr = Arc::clone(&writer);
            let closing = Arc::clone(&closing);
            move || read_terminal(read_fd, &sink, &dsr, &closing)
        });
        Self {
            writer,
            shared,
            child,
            master: Some(pair.master),
            reader: Some(reader),
            closing,
            cursor: 0,
            prompt: 0,
            _home: home,
        }
    }

    /// The pid of what runs directly on the PTY: heph, or the shell above it.
    pub fn pid(&self) -> i32 {
        let pid = self.child.process_id().expect("pty child pid");
        i32::try_from(pid).expect("pid fits")
    }

    pub fn send(&self, bytes: &[u8]) {
        let mut guard = self.writer.lock().expect("writer lock");
        let w = guard.as_mut().expect("the terminal was closed");
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

    /// Every byte the terminal received. Only for a test that has to count them;
    /// anything a person would see belongs to [`Pty::screen`].
    pub fn output(&self) -> Vec<u8> {
        self.shared.lock().expect("screen lock").output.clone()
    }

    /// How many cursor-position queries the terminal was asked. The TUI's inline
    /// viewport asks; a run that keeps off the terminal must not.
    pub fn dsr_queries(&self) -> usize {
        self.shared.lock().expect("screen lock").dsr_queries
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

    /// Whether the terminal is on its alternate screen now.
    pub fn alternate_screen(&self) -> bool {
        self.shared
            .lock()
            .expect("screen lock")
            .parser
            .screen()
            .alternate_screen()
    }

    /// The device of the PTY's slave side: what `tty` prints for a process that
    /// inherited this terminal.
    pub fn tty_name(&self) -> PathBuf {
        self.master
            .as_ref()
            .expect("the terminal was closed")
            .tty_name()
            .expect("the terminal has a name")
    }

    /// The terminal's settings now.
    pub fn termios(&self) -> Termios {
        let master = self.master.as_ref().expect("the terminal was closed");
        let fd = master.as_raw_fd().expect("the master's fd");
        // SAFETY: termios is POD; tcgetattr only writes into it.
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: a live fd and a live out-parameter.
        let rc = unsafe { libc::tcgetattr(fd, &mut t) };
        assert_eq!(rc, 0, "tcgetattr: {}", std::io::Error::last_os_error());
        Termios {
            iflag: t.c_iflag,
            oflag: t.c_oflag,
            cflag: t.c_cflag,
            // PENDIN is the line discipline's own state: BSD sets it when ICANON
            // comes back on.
            lflag: t.c_lflag & !libc::PENDIN,
            cc: t.c_cc.to_vec(),
        }
    }

    /// Resize the window, as a terminal emulator would.
    pub fn resize(&self, rows: u16, cols: u16) {
        self.master
            .as_ref()
            .expect("the terminal was closed")
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize the pty");
        self.shared
            .lock()
            .expect("screen lock")
            .parser
            .screen_mut()
            .set_size(rows, cols);
    }

    /// Close the terminal, as closing its window does: every descriptor of the
    /// master goes, and whatever the session still has running gets its hangup.
    pub fn close_terminal(&mut self) {
        self.closing.store(true, Ordering::SeqCst);
        if let Some(reader) = self.reader.take() {
            drop(reader.join());
        }
        drop(self.writer.lock().expect("writer lock").take());
        drop(self.master.take());
    }

    /// Block until `needle` is painted on screen after everything earlier
    /// `wait_for`s matched, or panic with what was seen.
    ///
    /// A PTY echoes what is typed, so a needle must not appear in a command
    /// line sent before it (build markers the way `shell_pty.rs`'s
    /// `echo_marker` does).
    ///
    /// Each read snapshots the whole screen, so text still on screen matches
    /// again: a needle has to be unique to the moment it stands for.
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

    /// Type a line into a [`Pty::shell`]: a command it will read at the prompt
    /// it is at now.
    pub fn type_line(&self, line: &str) {
        self.send(line.as_bytes());
        self.send(b"\n");
    }

    /// Block until a [`Pty::shell`] shows its next prompt, which proves that it
    /// has the terminal back. Every command line typed moves it on by one.
    pub fn wait_prompt(&mut self) {
        let prompt = format!("shpty-{}> ", self.prompt);
        // Trailing spaces fall off a rendered row.
        self.wait_for(prompt.trim_end(), Duration::from_secs(60));
        self.prompt += 1;
    }

    /// Ask a [`Pty::shell`] for the status of the last command, and return it.
    pub fn last_status(&mut self) -> i32 {
        // The quotes keep the typed line from satisfying the search below.
        let number = self.prompt;
        self.type_line(&format!("echo st''atus-{number}=$?="));
        self.wait_prompt();
        self.number_after(&format!("status-{number}="))
    }

    /// The number printed right after the first `prefix` on screen so far.
    pub fn number_after(&self, prefix: &str) -> i32 {
        let seen = self.seen();
        let number = seen
            .split(prefix)
            .nth(1)
            .map(|rest| {
                rest.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
            })
            .and_then(|digits| digits.parse().ok());
        assert!(number.is_some(), "no number after {prefix:?}\n{seen}");
        number.unwrap_or_default()
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

/// The reader thread: every byte goes to the emulator, and every query is
/// answered. Ends at the terminal's end, or when told to let go of the master.
fn read_terminal(
    fd: OwnedFd,
    shared: &Mutex<Shared>,
    writer: &Mutex<Option<Box<dyn std::io::Write + Send>>>,
    closing: &AtomicBool,
) {
    let mut reader = std::fs::File::from(fd);
    let mut buf = [0u8; 8192];
    let mut tail = Vec::new();
    while !closing.load(Ordering::SeqCst) {
        if !readable_soon(reader.as_raw_fd()) {
            continue;
        }
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let Some(chunk) = buf.get(..n) else { break };
        tail.extend_from_slice(chunk);
        let queries = super::take_dsr_queries(&mut tail);
        for _ in 0..queries {
            let mut guard = writer.lock().expect("writer lock");
            let Some(w) = guard.as_mut() else { break };
            if w.write_all(super::DSR_REPLY).is_err() || w.flush().is_err() {
                break;
            }
        }
        let mut s = shared.lock().expect("screen lock");
        s.dsr_queries += queries;
        s.output.extend_from_slice(chunk);
        s.parser.process(chunk);
        let screen = s.parser.screen().contents();
        s.seen.push_str(&screen);
        s.seen.push('\n');
    }
}

/// `select`, not `poll`: macOS documents `poll` as not supporting devices.
fn readable_soon(fd: i32) -> bool {
    // SAFETY: fd_set is POD and zero is its empty value.
    let mut set: libc::fd_set = unsafe { std::mem::zeroed() };
    // SAFETY: a live set, and the master is a low-numbered fd in a test process.
    unsafe { libc::FD_SET(fd, &mut set) };
    let mut tick = libc::timeval {
        tv_sec: 0,
        tv_usec: READER_TICK_US,
    };
    // SAFETY: every pointer is live for the call.
    let ready = unsafe {
        libc::select(
            fd + 1,
            &mut set,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut tick,
        )
    };
    // An error is for the read to report.
    ready != 0
}

impl Drop for Pty {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            drop(self.child.kill());
        }
        self.closing.store(true, Ordering::SeqCst);
    }
}
