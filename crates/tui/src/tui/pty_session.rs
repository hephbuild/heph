//! The host's side of a run on a PTY: heph is the terminal the tool talks to.
//!
//! [`PtySession::start`] opens the pair and makes the caller's terminal raw;
//! one task then pumps stdin to the master and the master to fd 1, copies window
//! size changes across, and answers the tool's stops. [`PtySession::finish`] is
//! called when the run returned, whatever it returned: a non-zero exit is an
//! `Err` as well, so `Err` is no reason to cut output short. It drains what the
//! tool wrote, bounded, and only then puts the terminal back.
//!
//! The master reaching EOF means "output done", not "tool done": a tool can
//! close its stdio and carry on. Only `finish` ends the session. A run that
//! failed before spawning closed the slave, so its drain ends at once.
//!
//! Memory stays fixed: one reused buffer per direction, and a direction does not
//! read until it has written what it holds, so a slow fd 1 backs up into the PTY
//! and from there into the tool. Output flushes when the master has nothing more
//! to read, since a flush is a trip to the blocking pool and the next write
//! carries the bytes along anyway; and before a stop and throughout the drain,
//! where nothing may be left behind.
//!
//! Each direction advances by one `step`, whose every await is a single
//! cancel-safe primitive, because `select!` drops the losing branches.

use std::io::{self, IsTerminal};
use std::os::fd::{OwnedFd, RawFd};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures::FutureExt as _;
use hproc::proc_exec::DRAIN_DEADLINE;
use hproc::pty::{
    AsyncPty, RawTerminal, caller_winsize, inherit_termios, is_foreground, open_pty, set_winsize,
    terminal_alive,
};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, Interest};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::tty::TtyReader;

/// Stdin is a keyboard, or at worst a paste.
const BUF_SIZE: usize = 8 * 1024;
/// Output can be a flood, and each write of it is a trip to the blocking pool.
const OUTPUT_BUF_SIZE: usize = 64 * 1024;
/// How much a stop reads from the master before stopping regardless, so a tool
/// that keeps writing (another process in its group) cannot delay the stop.
const STOP_READ_BOUND: usize = 64 * 1024;
/// How long a stop waits for a TERM or HUP that has been delivered but not yet
/// seen by its listener.
const SIGNAL_SETTLE: Duration = Duration::from_millis(10);

/// **Test-only.** Overrides the drain limit in milliseconds, because a test that
/// proves the drain is bounded needs a limit it can tell from a clean exit, and
/// bin-e2e reaches the binary through nothing else. Not a user setting.
const DRAIN_LIMIT_ENV: &str = "HEPH_PTY_DRAIN_MS";
/// **Test-only.** Fails the opening of the PTY, because nothing else can: a test
/// has no way of running a host out of PTYs, and that failure must stay loud.
const OPEN_FAILS_ENV: &str = "HEPH_PTY_OPEN_FAILS";

/// What the run takes from a [`PtySession`]. The slave moves to the tool, and
/// keeping no copy of it here is what lets the master reach EOF.
///
/// The channels are plain tokio ones so this crate does not depend on `plugin`:
/// they are exactly `PtyHandover`'s `StopSender` and `ContinueReceiver`.
#[derive(Debug)]
pub struct PtyEnds {
    pub slave: OwnedFd,
    /// The wait reports a stop of the tool here.
    pub stops: mpsc::UnboundedSender<()>,
    /// The session says here that the tool's group may continue.
    pub continues: mpsc::UnboundedReceiver<()>,
}

/// A running pump. Dropping it counts as [`finish`](Self::finish), though nobody
/// can then wait for the drain.
#[derive(Debug)]
pub struct PtySession {
    finish: oneshot::Sender<Instant>,
    task: JoinHandle<()>,
}

impl PtySession {
    /// Open the PTY, copy the caller's termios and window size to it, go raw and
    /// start pumping. Must run on the runtime. `Err` (no PTY, no raw mode) leaves
    /// the terminal as it was and is the handover's failure; there is no
    /// fallback.
    ///
    /// The terminal is heph's own only when fd 0 is a tty it is in the foreground
    /// of. With none, which only an entrypoint's `--shell` can reach, it never
    /// goes raw and never stops itself, and a stop just continues the tool.
    /// Stdin is pumped unless fd 0 is a tty heph is in the background of, where a
    /// read would EIO.
    ///
    /// Registers process-lifetime SIGTSTP, SIGWINCH, SIGTERM and SIGHUP handlers.
    pub fn start() -> anyhow::Result<(Self, PtyEnds)> {
        anyhow::ensure!(
            std::env::var_os(OPEN_FAILS_ENV).is_none(),
            "opening a PTY for the tool: failed by {OPEN_FAILS_ENV}"
        );
        let stdin_is_terminal = io::stdin().is_terminal();
        let foreground = stdin_is_terminal && is_foreground(libc::STDIN_FILENO);
        let stdin = if stdin_is_terminal && !foreground {
            None
        } else {
            // A regular file on epoll or /dev/null on kqueue cannot be
            // registered: the tool then simply gets no input.
            TtyReader::from_stdin()
                .inspect_err(|error| tracing::debug!(%error, "not pumping stdin to the PTY"))
                .ok()
                .map(|reader| Box::new(reader) as Box<dyn AsyncRead + Send + Unpin>)
        };
        Self::start_with(Options {
            sink: Box::new(tokio::io::stdout()),
            stdin,
            terminal: foreground.then_some(libc::STDIN_FILENO),
            limit: drain_limit(),
        })
    }

    fn start_with(options: Options) -> anyhow::Result<(Self, PtyEnds)> {
        let (master, slave) = open_pty().context("opening a PTY for the tool")?;
        // There is nothing to copy when heph has no terminal.
        if let Err(error) = inherit_termios(&slave) {
            tracing::debug!(%error, "the PTY keeps its default termios");
        }
        if let Some(size) = caller_winsize() {
            set_winsize(&master, size).context("copying the window size to the PTY")?;
        }
        let writer = master.try_clone().context("cloning the PTY master")?;
        let writer = AsyncPty::with_interest(writer, Interest::WRITABLE)
            .context("registering the PTY master for writing")?;
        let reader = AsyncPty::with_interest(master, Interest::READABLE)
            .context("registering the PTY master for reading")?;

        let listen = |kind: SignalKind, name: &str| {
            signal(kind).with_context(|| format!("listening for {name}"))
        };
        let tstp = listen(SignalKind::from_raw(libc::SIGTSTP), "SIGTSTP")?;
        let winch = listen(SignalKind::window_change(), "SIGWINCH")?;
        let dying = Dying {
            term: listen(SignalKind::terminate(), "SIGTERM")?,
            hup: listen(SignalKind::hangup(), "SIGHUP")?,
            seen: false,
        };

        // Last, so no failure above has to undo it.
        let terminal = options
            .terminal
            .map(|fd| -> anyhow::Result<Terminal> {
                let raw = RawTerminal::capture(fd).context("reading the terminal's settings")?;
                raw.enter().context("making the terminal raw")?;
                let restore_scope = hcore::shutdown::scoped_terminal_restore(raw.restorer());
                Ok(Terminal {
                    raw,
                    fd,
                    _restore_scope: restore_scope,
                })
            })
            .transpose()?;

        let (stops, stops_rx) = mpsc::unbounded_channel();
        let (continues_tx, continues) = mpsc::unbounded_channel();
        let (finish, finish_rx) = oneshot::channel();
        let pump = Pump {
            input: Input {
                stdin: options.stdin,
                terminal: options.terminal,
                writer,
                buf: Box::new([0; BUF_SIZE]),
                pos: 0,
                len: 0,
            },
            output: Output {
                reader,
                sink: options.sink,
                buf: Box::new([0; OUTPUT_BUF_SIZE]),
                pos: 0,
                len: 0,
                flush_owed: false,
                eof: false,
                sink_dead: false,
                sink_error: None,
                dropped: 0,
                read: 0,
            },
            stops: stops_rx,
            continues: continues_tx,
            tstp,
            winch,
            dying,
            terminal,
            // SAFETY: no preconditions.
            parent: unsafe { libc::getppid() },
            limit: options.limit,
        };
        let task = tokio::spawn(pump.run(finish_rx));
        Ok((
            Self { finish, task },
            PtyEnds {
                slave,
                stops,
                continues,
            },
        ))
    }

    /// The run returned, whatever it returned. Stops stdin at once, drains the
    /// master up to the limit, which starts now, and restores the terminal last.
    /// A cut is a `tracing::warn!` with the byte count, once the terminal is
    /// cooked again so that the line renders.
    pub async fn finish(self) {
        if self.finish.send(Instant::now()).is_err() {
            tracing::debug!("the PTY pump had already gone");
        }
        if let Err(error) = self.task.await {
            tracing::warn!(%error, "the PTY pump did not end cleanly");
        }
    }
}

struct Options {
    sink: Box<dyn AsyncWrite + Send + Unpin>,
    stdin: Option<Box<dyn AsyncRead + Send + Unpin>>,
    /// The fd of the terminal to take over, if heph has one of its own.
    terminal: Option<RawFd>,
    limit: Duration,
}

fn drain_limit() -> Duration {
    std::env::var(DRAIN_LIMIT_ENV)
        .ok()
        .and_then(|ms| ms.parse().ok())
        .map_or(DRAIN_DEADLINE, Duration::from_millis)
}

struct Terminal {
    raw: RawTerminal,
    fd: RawFd,
    /// For what skips `raw`'s `Drop`: a hard abort, and a panic that aborts.
    _restore_scope: hcore::shutdown::TerminalRestoreScope,
}

/// Whether heph was told to die, which a stop must not sleep through.
struct Dying {
    term: Signal,
    hup: Signal,
    seen: bool,
}

impl Dying {
    /// `seen`, after giving a signal already delivered the time to be noticed.
    async fn settled(&mut self) -> bool {
        tokio::time::sleep(SIGNAL_SETTLE).await;
        self.seen |= self.term.recv().now_or_never().flatten().is_some();
        self.seen |= self.hup.recv().now_or_never().flatten().is_some();
        self.seen
    }
}

struct Pump {
    input: Input,
    output: Output,
    stops: mpsc::UnboundedReceiver<()>,
    continues: mpsc::UnboundedSender<()>,
    tstp: Signal,
    winch: Signal,
    dying: Dying,
    terminal: Option<Terminal>,
    parent: libc::pid_t,
    limit: Duration,
}

impl Pump {
    async fn run(mut self, mut finish: oneshot::Receiver<Instant>) {
        let started = loop {
            // Stdin goes before output, or a busy tool starves the keyboard. A
            // master that is full pends, so stdin cannot starve output in turn.
            tokio::select! {
                biased;
                // A dropped session counts, so its terminal still gets restored.
                finished = &mut finish => break finished.unwrap_or_else(|_| Instant::now()),
                Some(()) = self.stops.recv() => self.answer_stop().await,
                // Raw mode generates no TSTP, so this is a `kill -TSTP <heph>`.
                Some(()) = self.tstp.recv() => self.answer_stop().await,
                Some(()) = self.dying.term.recv() => self.dying.seen = true,
                Some(()) = self.dying.hup.recv() => self.dying.seen = true,
                Some(()) = self.winch.recv(), if self.terminal.is_some() => self.sync_size(),
                in_background = self.input.step() => {
                    if in_background {
                        self.answer_stop().await;
                    }
                }
                () = self.output.step() => {
                    // Nobody has the slave open, so nobody can read this. It also
                    // narrows the window for eating a keystroke meant for the shell.
                    if self.output.eof {
                        self.input.stdin = None;
                    }
                }
            }
        };

        // A stdin read must not outlive the tool, or it eats the next keystroke.
        self.input.stdin = None;
        let drained_from = self.output.read;
        let limit = self.limit.saturating_sub(started.elapsed());
        let done = self.output.drain();
        let cut = tokio::time::timeout(limit, done).await.is_err();
        drop(self.terminal.take());
        // After the restore, so the lines render cooked.
        if cut {
            tracing::warn!(
                bytes_drained = self.output.read - drained_from,
                limit_ms = self.limit.as_millis(),
                "the tool's output was cut: its PTY was still open at the drain limit",
            );
        }
        self.output.report_loss();
    }

    /// Answer a stop, in the order the terminal needs. Step 1, no polling of
    /// stdin until the end of this, holds because `run` is not in its `select!`:
    /// `bg` wakes the process, and a read then returns EIO for good.
    async fn answer_stop(&mut self) {
        if self.terminal.is_some() {
            self.output.settle(self.limit).await;
            self.stop_and_resume().await;
        }
        if self.continues.send(()).is_err() {
            tracing::debug!("the run has gone, and with it the tool to continue");
        }
    }

    /// Stop until heph is the foreground again, or skip it when nobody could ever
    /// resume heph: the terminal closed, the parent shell exited, no shell above
    /// heph shares its session, a wrapper shares its group, or it was told to die.
    /// The checks also guard the first stop, as a HUP-trapping tool can survive a
    /// closed terminal and then stop.
    async fn stop_and_resume(&mut self) {
        let Some(terminal) = &self.terminal else {
            return;
        };
        loop {
            // SAFETY: no preconditions.
            let parent_alive = unsafe { libc::getppid() } == self.parent;
            let resumable = terminal_alive(terminal.fd)
                && parent_alive
                // SAFETY: no preconditions.
                && parent_in_session(unsafe { libc::getppid() })
                && leads_own_group();
            if !resumable || self.dying.settled().await {
                // Not going raw, and neither reading a terminal that a stopped
                // heph may have lost.
                if !is_foreground(terminal.fd) {
                    self.input.stdin = None;
                }
                return;
            }
            terminal.raw.restore();
            // `raise`, not `kill(getpid(), …)`: this call returning is the resume,
            // so there is no SIGCONT listener. On Linux `kill` targets the group
            // leader, and on a tokio worker it can return before the stop reaches
            // this thread, which would then go raw on a stopping process. `raise`
            // targets the calling thread, which stops before it returns.
            // SAFETY: no preconditions; SIGSTOP has no handler to run.
            unsafe { libc::raise(libc::SIGSTOP) };
            if is_foreground(terminal.fd) {
                break;
            }
        }
        if let Err(error) = terminal.raw.enter() {
            tracing::warn!(%error, "could not make the terminal raw again");
        }
        self.sync_size();
    }

    /// Copy the caller's size to the PTY; the kernel signals the tool if it
    /// differs. So a resize reaches the tool once, and after the size has landed.
    fn sync_size(&self) {
        let Some(size) = caller_winsize() else {
            return;
        };
        if let Err(error) = set_winsize(&self.input.writer, size) {
            tracing::debug!(%error, "could not copy the window size to the PTY");
        }
    }
}

/// Whether heph's parent is in heph's session. A job-control shell always is, so
/// without it (`ssh -t host heph run //x`, `tmux new heph …`, heph leading the
/// session or otherwise orphaned) a stopped heph has nobody to resume it. An
/// error counts as no, and so does parent 0: `getppid` returns it when the parent
/// is in another PID namespace (`docker exec -it`, `kubectl exec -it`), and
/// `getsid(0)` would then answer for heph itself, which always matches.
fn parent_in_session(parent: libc::pid_t) -> bool {
    if parent <= 0 {
        return false;
    }
    // SAFETY: no preconditions; a pid `getsid` may not ask about is an error.
    let parent_session = unsafe { libc::getsid(parent) };
    // SAFETY: as above.
    parent_session >= 0 && parent_session == unsafe { libc::getsid(0) }
}

/// Whether heph is its group's leader, as a shell's job is. Otherwise a wrapper
/// shares the group (`/bin/sh -c 'heph run //x; echo after'`, `make`) and does
/// not stop with heph, so the shell never sees a stop and its prompt never comes.
fn leads_own_group() -> bool {
    // SAFETY: no preconditions.
    let group = unsafe { libc::getpgrp() };
    // SAFETY: no preconditions.
    group == unsafe { libc::getpid() }
}

/// Stdin to the master.
struct Input {
    /// `None` once stdin has ended, failed or must no longer be read.
    stdin: Option<Box<dyn AsyncRead + Send + Unpin>>,
    /// The terminal heph owns, whose read fails while heph is in its background.
    terminal: Option<RawFd>,
    writer: AsyncPty,
    buf: Box<[u8; BUF_SIZE]>,
    pos: usize,
    len: usize,
}

impl Input {
    /// `true` when a read of the terminal failed because heph is in its
    /// background (`kill -STOP <heph>`, `bg`). That is no end of stdin: the
    /// keyboard comes back with `fg`, so the caller goes through the stop path.
    async fn step(&mut self) -> bool {
        let Some(stdin) = self.stdin.as_mut() else {
            return std::future::pending().await;
        };
        let moved =
            if let Some(pending) = self.buf.get(self.pos..self.len).filter(|p| !p.is_empty()) {
                let written = self.writer.write(pending).await;
                written.map(|n| self.pos += n).map(|()| true)
            } else {
                let read = stdin.read(self.buf.as_mut_slice()).await;
                if read.is_err() && self.terminal.is_some_and(|fd| !is_foreground(fd)) {
                    return true;
                }
                read.map(|n| {
                    self.pos = 0;
                    self.len = n;
                    n > 0
                })
            };
        // EOF or a read or write error ends stdin and nothing else.
        match moved {
            Ok(true) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Ok(false) | Err(_) => self.stdin = None,
        }
        false
    }
}

/// The master to fd 1.
struct Output {
    reader: AsyncPty,
    sink: Box<dyn AsyncWrite + Send + Unpin>,
    buf: Box<[u8; OUTPUT_BUF_SIZE]>,
    pos: usize,
    len: usize,
    flush_owed: bool,
    /// The slave closed: output is done, the tool may not be.
    eof: bool,
    /// fd 1 failed. Reading goes on and discards until the tool exits, or a
    /// HUP-trapping tool would block on a full buffer forever.
    sink_dead: bool,
    /// What killed fd 1, unless it was the terminal or pipe simply going away.
    sink_error: Option<io::Error>,
    dropped: u64,
    read: u64,
}

impl Output {
    fn holds_data(&self) -> bool {
        self.pos < self.len || self.flush_owed
    }

    /// One step while the tool runs: a flush waits for as long as the master has
    /// more, and is immediate once it has none, so a prompt is not held back.
    async fn step(&mut self) {
        if self.flush_owed && self.pos >= self.len && !self.eof && self.read_ahead() {
            return;
        }
        self.step_flushing().await;
    }

    /// Whether a non-blocking read of the master found anything to account for.
    fn read_ahead(&mut self) -> bool {
        let now = self.reader.read_now(self.buf.as_mut_slice());
        if matches!(&now, Err(e) if e.kind() == io::ErrorKind::WouldBlock) {
            return false;
        }
        self.took(now);
        true
    }

    /// One step that flushes as soon as all it holds is written. Bounded paths
    /// take this: a tool that keeps writing would otherwise put the flush off past
    /// the end of the bound.
    async fn step_flushing(&mut self) {
        if let Some(pending) = self.buf.get(self.pos..self.len).filter(|p| !p.is_empty()) {
            if self.sink_dead {
                self.dropped += pending.len() as u64;
                self.pos = self.len;
                return;
            }
            match self.sink.write(pending).await {
                Ok(0) => self.lose_sink(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.pos += n;
                    self.flush_owed = true;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => self.lose_sink(error),
            }
        } else if self.flush_owed {
            if self.sink_dead {
                self.flush_owed = false;
                return;
            }
            match self.sink.flush().await {
                Ok(()) => self.flush_owed = false,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => self.lose_sink(error),
            }
        } else if self.eof {
            std::future::pending::<()>().await;
        } else {
            let read = self.reader.read(self.buf.as_mut_slice()).await;
            self.took(read);
        }
    }

    /// Account for a read of the master. Only EAGAIN from [`Self::settle`]'s
    /// non-blocking one stays out of here.
    fn took(&mut self, read: io::Result<usize>) {
        match read {
            Ok(0) => self.eof = true,
            Ok(n) => {
                self.pos = 0;
                self.len = n;
                self.read += n as u64;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                tracing::debug!(%error, "reading the PTY master failed; treating it as closed");
                self.eof = true;
            }
        }
    }

    fn lose_sink(&mut self, error: io::Error) {
        self.sink_dead = true;
        self.flush_owed = false;
        // EIO is a closed terminal and EPIPE a closed pipe. EAGAIN from a
        // non-blocking fd 1 does lose output nobody chose to lose: that one is
        // reported.
        if !matches!(error.raw_os_error(), Some(libc::EIO | libc::EPIPE)) {
            self.sink_error = Some(error);
        }
    }

    /// Run to the end of output: the master closed and all of it reached fd 1.
    async fn drain(&mut self) {
        while !self.eof || self.holds_data() {
            self.step_flushing().await;
        }
    }

    /// Before a stop: what the tool has written reaches the terminal ahead of
    /// the shell's prompt. Reads until EAGAIN or the byte bound, and neither a
    /// tool that keeps writing nor a stalled fd 1 can delay the stop past `limit`.
    async fn settle(&mut self, limit: Duration) {
        let settled = async {
            let mut read = 0;
            loop {
                while self.holds_data() {
                    self.step_flushing().await;
                }
                if self.eof || read >= STOP_READ_BOUND {
                    return;
                }
                let now = self.reader.read_now(self.buf.as_mut_slice());
                if matches!(&now, Err(e) if e.kind() == io::ErrorKind::WouldBlock) {
                    return;
                }
                read += now.as_ref().map_or(0, |n| *n);
                self.took(now);
            }
        };
        if tokio::time::timeout(limit, settled).await.is_err() {
            tracing::debug!("stopping with the tool's output not fully written");
        }
    }

    fn report_loss(&self) {
        if let Some(error) = &self.sink_error {
            tracing::warn!(%error, bytes_dropped = self.dropped, "the tool's output was lost");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::fd::AsRawFd as _;
    use tokio::io::{DuplexStream, duplex};

    /// Parent 0 is what a parent in another PID namespace reads as: without the
    /// guard `getsid(0)` is heph's own session, and heph would stop itself with
    /// nobody to resume it.
    #[test]
    fn parent_in_another_pid_namespace_is_not_in_session() {
        assert!(!parent_in_session(0));
        assert!(!parent_in_session(-1));
        // SAFETY: no preconditions.
        let me = unsafe { libc::getpid() };
        assert!(parent_in_session(me), "heph's own session matches itself");
    }
    use tokio::time::{sleep, timeout};

    /// Far beyond anything a test should wait for, so that a bounded elapsed time
    /// proves the drain ended by itself and not at the limit.
    const LONG_LIMIT: Duration = Duration::from_secs(60);
    /// The most that a stalled sink may let in: the output buffer, and the PTY's
    /// own queues, which are tens of KiB on both OSes.
    const STALL_BOUND: usize = OUTPUT_BUF_SIZE + 64 * 1024;

    fn options(sink: DuplexStream, limit: Duration) -> Options {
        Options {
            sink: Box::new(sink),
            stdin: None,
            terminal: None,
            limit,
        }
    }

    /// Make a slave non-blocking, and hand it back as a writable file.
    fn nonblocking(slave: OwnedFd) -> std::fs::File {
        // SAFETY: F_GETFL on a live fd.
        let flags = unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_GETFL) };
        // SAFETY: F_SETFL on a live fd.
        unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
        std::fs::File::from(slave)
    }

    /// Letters only, so the line discipline passes them untouched.
    fn pattern(index: usize) -> u8 {
        b"abcdefghijklmnopqrstuvwxyz"[index % 26]
    }

    /// Write until the PTY has stayed full for a while, or past the bound.
    async fn write_until_full(slave: &mut std::fs::File) -> usize {
        let mut total = 0;
        let mut stalls = 0;
        while stalls < 20 && total <= STALL_BOUND {
            let chunk: Vec<u8> = (total..total + 1024).map(pattern).collect();
            match slave.write(&chunk) {
                Ok(n) => {
                    total += n;
                    stalls = 0;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    stalls += 1;
                    sleep(Duration::from_millis(10)).await;
                }
                Err(e) => panic!("writing to the slave: {e}"),
            }
        }
        total
    }

    #[tokio::test]
    async fn pump_slow_stdout_bounded_memory() {
        let (sink, mut released) = duplex(1024);
        let (session, ends) =
            PtySession::start_with(options(sink, LONG_LIMIT)).expect("start the session");
        let mut slave = nonblocking(ends.slave);

        // Nothing reads the sink, so the pump can write 1 KiB of it. The rest
        // must back up into the tool rather than into the pump's memory.
        let total = write_until_full(&mut slave).await;
        assert!(
            total <= STALL_BOUND,
            "a stalled sink took {total} bytes: the pump is buffering"
        );

        // Released: not one of them was lost or reordered. Read before the slave
        // closes, since macOS discards what is unread when it does.
        let mut out = vec![0; total];
        timeout(Duration::from_secs(30), released.read_exact(&mut out))
            .await
            .expect("every byte arrives once the sink is released")
            .expect("read the sink");
        let expected: Vec<u8> = (0..total).map(pattern).collect();
        assert!(out == expected, "the bytes arrived altered");

        drop(slave);
        session.finish().await;
    }

    #[tokio::test]
    async fn tool_closes_slave_without_exiting() {
        let (sink, mut out) = duplex(1024);
        let (session, mut ends) =
            PtySession::start_with(options(sink, LONG_LIMIT)).expect("start the session");
        let mut slave = std::fs::File::from(ends.slave);
        slave.write_all(b"last words").expect("write");
        let mut got = [0; 10];
        out.read_exact(&mut got).await.expect("read the sink");
        assert_eq!(&got, b"last words");

        // The tool closes its stdio and carries on. That is output done, and only
        // that: the session must still answer the tool.
        drop(slave);
        sleep(Duration::from_millis(100)).await;
        assert!(!session.task.is_finished(), "EOF ended the session");
        ends.stops.send(()).expect("the pump is still listening");
        timeout(Duration::from_secs(30), ends.continues.recv())
            .await
            .expect("the stop is answered after EOF")
            .expect("the pump is still there");

        // The tool exits: nothing is left to drain, so no waiting for the limit.
        let started = Instant::now();
        session.finish().await;
        assert!(started.elapsed() < LONG_LIMIT / 2);
    }

    /// A non-zero exit and a failure to spawn both reach `finish` the same way,
    /// and neither may wait out the limit: the slave closed, so EOF is immediate.
    /// That the terminal comes back is `default_run_restores_the_terminal`'s: a
    /// restore needs the foreground of a controlling terminal, and this has none.
    #[tokio::test]
    async fn pty_run_error_before_spawn_ends_pump() {
        let (terminal, terminal_slave) = open_pty().expect("a terminal to take over");
        let lflag = |fd: RawFd| {
            // SAFETY: termios is POD; tcgetattr only writes into it.
            let mut t: libc::termios = unsafe { std::mem::zeroed() };
            // SAFETY: a live fd and a live out-parameter.
            assert_eq!(unsafe { libc::tcgetattr(fd, &mut t) }, 0);
            t.c_lflag
        };
        let fd = terminal_slave.as_raw_fd();

        let (sink, _out) = duplex(1024);
        let (session, ends) = PtySession::start_with(Options {
            terminal: Some(fd),
            ..options(sink, LONG_LIMIT)
        })
        .expect("start the session");
        assert_eq!(lflag(fd) & libc::ICANON, 0, "the terminal went raw");

        drop(ends);
        let started = Instant::now();
        session.finish().await;
        assert!(started.elapsed() < LONG_LIMIT / 2, "waited for the limit");
        drop(terminal);
    }

    /// A line-buffered fd 1 in miniature: what it was written shows only once
    /// flushed, which is how `tokio::io::stdout` treats a line with no newline yet.
    struct FlushedSink {
        unflushed: Vec<u8>,
        shown: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl AsyncWrite for FlushedSink {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            self.unflushed.extend_from_slice(buf);
            std::task::Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            let unflushed = std::mem::take(&mut self.unflushed);
            self.shown.lock().expect("lock").extend(unflushed);
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    fn flushed_sink() -> (
        Box<dyn AsyncWrite + Send + Unpin>,
        std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    ) {
        let shown = std::sync::Arc::default();
        let sink = FlushedSink {
            unflushed: Vec::new(),
            shown: std::sync::Arc::clone(&shown),
        };
        (Box::new(sink), shown)
    }

    /// A prompt has no newline and nothing follows it. With the master idle, the
    /// flush has to come by itself, with the slave still open and the run going.
    #[tokio::test]
    async fn an_idle_master_flushes_at_once() {
        let (sink, shown) = flushed_sink();
        let (session, ends) = PtySession::start_with(Options {
            sink,
            stdin: None,
            terminal: None,
            limit: LONG_LIMIT,
        })
        .expect("start the session");
        let mut slave = std::fs::File::from(ends.slave);
        slave.write_all(b"prompt> ").expect("write");
        timeout(Duration::from_secs(30), async {
            while shown.lock().expect("lock").as_slice() != b"prompt> " {
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the prompt reaches fd 1 with the tool still running");
        drop(slave);
        session.finish().await;
    }

    /// A drain cut at the limit, the slave still open: what it read has reached
    /// fd 1 by the time the session ends, ahead of the terminal coming back.
    #[tokio::test]
    async fn a_cut_drain_leaves_nothing_unflushed() {
        let (sink, shown) = flushed_sink();
        let (session, ends) = PtySession::start_with(Options {
            sink,
            stdin: None,
            terminal: None,
            limit: Duration::from_millis(200),
        })
        .expect("start the session");
        let mut slave = std::fs::File::from(ends.slave);
        slave.write_all(b"no newline").expect("write");
        session.finish().await;
        assert_eq!(shown.lock().expect("lock").as_slice(), b"no newline");
        drop(slave);
    }

    /// A tool that never pauses must neither put off a stop nor the flush ahead of
    /// it: the settle reads a bounded amount, then flushes, whatever is still
    /// arriving.
    #[tokio::test]
    async fn a_stop_flushes_under_a_tool_that_keeps_writing() {
        let (_terminal, terminal_slave) = open_pty().expect("a terminal to take over");
        let (sink, shown) = flushed_sink();
        let (session, mut ends) = PtySession::start_with(Options {
            sink,
            stdin: None,
            terminal: Some(terminal_slave.as_raw_fd()),
            limit: LONG_LIMIT,
        })
        .expect("start the session");
        let mut slave = std::fs::File::from(ends.slave);
        let writing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (first_write, wrote) = oneshot::channel();
        let writer = std::thread::spawn({
            let writing = std::sync::Arc::clone(&writing);
            move || {
                let mut first_write = Some(first_write);
                while writing.load(std::sync::atomic::Ordering::Relaxed) {
                    if slave.write_all(&[b'x'; 4096]).is_err() {
                        break;
                    }
                    if let Some(first_write) = first_write.take() {
                        first_write.send(()).expect("the test is waiting");
                    }
                }
            }
        });
        // Or the stop can be answered with nothing yet written to flush.
        wrote.await.expect("the writer wrote");
        ends.stops.send(()).expect("send the stop");
        timeout(Duration::from_secs(30), ends.continues.recv())
            .await
            .expect("the tool is continued, however much it writes")
            .expect("the pump is still there");
        assert!(
            !shown.lock().expect("lock").is_empty(),
            "the stop did not flush"
        );
        writing.store(false, std::sync::atomic::Ordering::Relaxed);
        // Blocking would keep the pump from the read the writer is waiting on.
        tokio::task::spawn_blocking(move || writer.join().expect("the writer ends"))
            .await
            .expect("join the writer");
        session.finish().await;
    }

    /// A tool's background job keeps the slave open: the drain stops at the limit.
    #[tokio::test]
    async fn drain_stops_at_the_limit() {
        let limit = Duration::from_millis(100);
        let (sink, _out) = duplex(1024);
        let (session, ends) = PtySession::start_with(options(sink, limit)).expect("start");
        let started = Instant::now();
        session.finish().await;
        assert!(started.elapsed() >= limit, "ended before the limit");
        drop(ends);
    }

    /// With no resuming it, a stop must not happen: this test binary would stop
    /// and the run would hang. A pty slave that is nobody's controlling terminal
    /// answers `tcgetpgrp` with an error, exactly as a closed terminal does.
    #[tokio::test]
    async fn a_stop_nobody_could_resume_only_continues_the_tool() {
        let (_terminal, terminal_slave) = open_pty().expect("a terminal to take over");
        let (sink, _out) = duplex(1024);
        let (session, mut ends) = PtySession::start_with(Options {
            terminal: Some(terminal_slave.as_raw_fd()),
            ..options(sink, LONG_LIMIT)
        })
        .expect("start the session");
        ends.stops.send(()).expect("send the stop");
        timeout(Duration::from_secs(30), ends.continues.recv())
            .await
            .expect("the tool is continued")
            .expect("the pump is still there");
        drop(ends);
        session.finish().await;
    }

    /// EIO on fd 1 is a terminal that closed. A tool that traps the HUP keeps
    /// writing, and must not block on a full PTY.
    #[tokio::test]
    async fn a_dead_stdout_keeps_the_tool_writing() {
        struct DeadSink;
        impl AsyncWrite for DeadSink {
            fn poll_write(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                _: &[u8],
            ) -> std::task::Poll<io::Result<usize>> {
                std::task::Poll::Ready(Err(io::Error::from_raw_os_error(libc::EIO)))
            }
            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }
        let (session, ends) = PtySession::start_with(Options {
            sink: Box::new(DeadSink),
            stdin: None,
            terminal: None,
            limit: LONG_LIMIT,
        })
        .expect("start the session");
        let mut slave = nonblocking(ends.slave);
        let mut written = 0;
        timeout(Duration::from_secs(30), async {
            while written <= STALL_BOUND {
                match slave.write(&[b'x'; 1024]) {
                    Ok(n) => written += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        sleep(Duration::from_millis(1)).await;
                    }
                    Err(e) => panic!("writing to the slave: {e}"),
                }
            }
        })
        .await
        .expect("the pump kept reading after fd 1 died");
        drop(slave);
        session.finish().await;
    }

    #[tokio::test]
    async fn stdin_reaches_the_tool() {
        let (sink, _out) = duplex(64 * 1024);
        let (mut keyboard, stdin) = duplex(1024);
        let (session, ends) = PtySession::start_with(Options {
            stdin: Some(Box::new(stdin)),
            ..options(sink, LONG_LIMIT)
        })
        .expect("start the session");
        let mut tool = AsyncPty::with_interest(ends.slave, Interest::READABLE).expect("register");

        keyboard.write_all(b"hi\n").await.expect("type");
        let mut got = [0; 3];
        timeout(Duration::from_secs(30), tool.read_exact(&mut got))
            .await
            .expect("the input arrives")
            .expect("read the slave");
        assert_eq!(&got, b"hi\n");

        drop(tool);
        session.finish().await;
    }
}
