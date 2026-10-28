//! Running a program in the caller's terminal, the way `system(3)` does.
//!
//! The one kind of spawn in heph that is *not* captured: the program inherits
//! heph's stdin, stdout and stderr (no pipe, no tee, no new session), runs in
//! the cwd and with the env it is handed, and its exit status is reported as a
//! structured [`Exit`]. A driver reaches it through [`spawn_foreground`] when a
//! run asks for `RunStdio::Inherit`; exec's `Inherit` arm is the first caller,
//! and the plugin SDK re-exports this module so a third-party driver gets the
//! same terminal and signal behaviour.
//!
//! # A temporary process model (interim; the tool-shims spec's D2 replaces it)
//!
//! The child stays in heph's process group, exactly as under `system(3)`, so
//! terminal-generated signals (Ctrl-C, Ctrl-\, Ctrl-Z, the SIGHUP when the
//! terminal closes) reach it straight from the kernel. A signal addressed to
//! heph's *pid* alone does not, so the caller hands [`ForegroundChild::wait`] a
//! channel of signals to relay, and the wait delivers each with
//! `kill(child.id(), signo)`, skipping the ones the terminal already delivered
//! to the shared group ([`should_relay`]).
//!
//! Every place that depends on the shared group says so with a `D2:` comment
//! naming what changes when the tool moves to a group of its own.
//!
//! Every subprocess goes through this crate; this is where that holds for the
//! ones heph runs in the foreground.

use crate::{PathPolicy, RunnerRef, SpawnedAs};
use hcore::hasync::Cancellable;
use hproc::proc_exec;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub use hcore::process::{Disposition, Exit, Signal};

/// The receiving end of a run's signal relay: the same type as the driver
/// contract's `SignalReceiver`. A closed channel means "no more signals",
/// never a cancel.
pub type SignalReceiver = tokio::sync::mpsc::UnboundedReceiver<Signal>;

/// A program to run in the caller's terminal.
///
/// `#[non_exhaustive]`: built with [`ForegroundCommand::new`] and the `with_*`
/// setters, so a later field is not a source break for a third-party driver.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ForegroundCommand {
    /// `argv[0]`. Without a `/` it is looked up on the `PATH` in `env` (after
    /// [`PathPolicy`] composed it), never on heph's.
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// The whole environment: nothing of heph's leaks in.
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
    /// Dispositions set in the child before it execs, after the defaults this
    /// spawn applies itself (see [`spawn_foreground`]). The caller's, as the
    /// host recorded them at startup.
    pub dispositions: Vec<(Signal, Disposition)>,
}

impl ForegroundCommand {
    pub fn new(
        program: impl Into<PathBuf>,
        args: Vec<OsString>,
        env: Vec<(OsString, OsString)>,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        Self {
            program: program.into(),
            args,
            env,
            cwd: cwd.into(),
            dispositions: Vec::new(),
        }
    }

    pub fn with_dispositions(mut self, dispositions: Vec<(Signal, Disposition)>) -> Self {
        self.dispositions = dispositions;
        self
    }
}

/// Signals reset to `SIG_DFL` in every foreground child before
/// [`ForegroundCommand::dispositions`] apply. heph ignores SIGTTOU/SIGTTIN for
/// its whole life, and an ignored disposition survives `exec`; SIGINT/SIGQUIT
/// are listed so a handler-less ignore in heph can never leak either. A
/// recorded `Ignore` for any of them (a `nohup`'d SIGINT) is re-applied after.
// D2: with the tool in its own group and the terminal handed over with
// `tcsetpgrp`, recheck SIGTTOU/SIGTTIN against the job-control setup.
const RESET_TO_DEFAULT: [libc::c_int; 4] =
    [libc::SIGINT, libc::SIGQUIT, libc::SIGTTOU, libc::SIGTTIN];

/// The soft `RLIMIT_NOFILE` heph inherited, before it raised its own.
static INHERITED_NOFILE: std::sync::OnceLock<libc::rlim_t> = std::sync::OnceLock::new();

/// Record the open-file soft limit heph was started with, so a foreground child
/// gets it back rather than heph's raised one (R8). Call once at the top of
/// `main`, before anything raises it; later calls are no-ops. A cdylib never
/// calls it, and its foreground children keep the process's current limit.
pub fn record_inherited_nofile() {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: a valid resource and a pointer to a live `rlimit`.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } == 0 {
        // A second call keeps the first record: it is the one taken before
        // anything changed the limit.
        let _first_wins = INHERITED_NOFILE.set(lim.rlim_cur);
    }
}

/// The `rlimit` to set in the child, computed in the parent so `pre_exec` only
/// makes the syscall. `None` when nothing was recorded or nothing would change.
fn nofile_to_restore() -> Option<libc::rlimit> {
    let recorded = *INHERITED_NOFILE.get()?;
    let mut now = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: a valid resource and a pointer to a live `rlimit`.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut now) } != 0 {
        return None;
    }
    let cur = recorded.min(now.rlim_max);
    (cur != now.rlim_cur).then_some(libc::rlimit {
        rlim_cur: cur,
        rlim_max: now.rlim_max,
    })
}

/// Spawn `cmd` in the caller's terminal under `runner`, composing `PATH` by
/// `path` exactly as a captured spawn would (`prefix ++ env's PATH ++ suffix`).
///
/// Two error channels, as [`crate::spawn_io_with_path`]: resolving the runner
/// is an `anyhow` error, and the `execve` (or `chdir`) failing stays a typed
/// [`std::io::Error`] so the caller can branch on `NotFound` or `E2BIG` and
/// name what it configured.
///
/// The child inherits fds 0–2 as heph has them (a closed one stays closed),
/// does not `setsid`, and stays in heph's process group. In the child, before
/// exec, it resets [`RESET_TO_DEFAULT`] to `SIG_DFL`, applies `cmd`'s
/// dispositions, and restores the recorded `RLIMIT_NOFILE` soft limit
/// ([`record_inherited_nofile`]). The pid is tracked with the process
/// supervisor (by pid, not pgid: the child leads no group), so a SIGKILLed heph
/// does not orphan it.
pub async fn spawn_foreground(
    runner: RunnerRef<'_>,
    cmd: ForegroundCommand,
    path: &PathPolicy,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<(std::io::Result<ForegroundChild>, SpawnedAs)> {
    // The runner seam rewrites a `proc_exec::Spec`; the stdio and group flags
    // here are never spawned through `proc_exec`, they only satisfy the type.
    let mut spec = proc_exec::Spec {
        program: cmd.program,
        args: cmd.args,
        env: cmd.env,
        cwd: cmd.cwd,
        stdin: proc_exec::StdioSpec::Inherit,
        stdout: proc_exec::StdioSpec::Inherit,
        stderr: proc_exec::StdioSpec::Inherit,
        setsid: false,
        ctty: false,
    };
    crate::prepare(runner, &mut spec, path, ctoken).await?;
    let spawned_as = SpawnedAs {
        program: spec.program.clone(),
        cwd: spec.cwd.clone(),
    };
    let child = spawn_inherited(
        &spec.program,
        &spec.args,
        &spec.env,
        &spec.cwd,
        &cmd.dispositions,
    );
    Ok((child, spawned_as))
}

fn spawn_inherited(
    program: &Path,
    args: &[OsString],
    env: &[(OsString, OsString)],
    cwd: &Path,
    dispositions: &[(Signal, Disposition)],
) -> std::io::Result<ForegroundChild> {
    use std::os::unix::process::CommandExt as _;

    // The child must never exist without the supervisor that reaps it.
    hproc::process_supervisor::ensure_started().map_err(|e| {
        std::io::Error::other(format!(
            "not spawning {program:?}: {e}\n\
             hint: heph cannot track child processes without its supervisor; \
             check `ulimit -n` and the process limit (`ulimit -u`)"
        ))
    })?;

    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    // Everything the child needs is computed here, in the parent: the closure
    // below runs between fork and exec and must not allocate (R8).
    let handlers: Vec<(libc::c_int, libc::sighandler_t)> = RESET_TO_DEFAULT
        .iter()
        .map(|&sig| (sig, libc::SIG_DFL))
        .chain(dispositions.iter().map(|(sig, d)| {
            (
                sig.as_raw(),
                match d {
                    Disposition::Default => libc::SIG_DFL,
                    Disposition::Ignore => libc::SIG_IGN,
                },
            )
        }))
        .collect();
    let nofile = nofile_to_restore();
    let pre_exec = move || {
        for &(sig, handler) in &handlers {
            // SAFETY: `signal(2)` is async-signal-safe, as the code between
            // fork and exec must be. A failure (an invalid number) leaves the
            // disposition as inherited, which is all a caller could do anyway.
            unsafe { libc::signal(sig, handler) };
        }
        if let Some(lim) = &nofile {
            // SAFETY: a plain syscall on a live `rlimit`; lowering the soft
            // limit (or keeping it under the hard one) is always permitted.
            unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, lim) };
        }
        Ok(())
    };
    // SAFETY: the closure runs between fork and exec, calls only
    // async-signal-safe functions on data computed before the fork, and
    // allocates nothing.
    unsafe { cmd.pre_exec(pre_exec) };

    // D2: with its own group the child would `setpgid(0, 0)` here and heph
    // would hand it the terminal with `tcsetpgrp`, taking it back on exit.
    // The `pre_exec` closure makes this a `fork`; on macOS a child forked
    // while another thread forks for the first time can die in libSystem's
    // atfork handler before it reaches exec (`hproc::fork_safety`).
    hproc::fork_safety::prime();
    let child = cmd.spawn()?;
    // D2: report and track the pgid instead (`track(pgid)`), as every
    // captured spawn does.
    let track = i32::try_from(child.id())
        .ok()
        .and_then(hproc::process_supervisor::register_child);
    Ok(ForegroundChild { child, track })
}

/// A program running in the caller's terminal. Wait for it with
/// [`ForegroundChild::wait`]; dropping it unwaited kills it.
#[derive(Debug)]
pub struct ForegroundChild {
    child: std::process::Child,
    track: Option<hproc::process_supervisor::TrackGuard>,
}

impl ForegroundChild {
    /// The child's pid, while it is not yet reaped. Only for reporting: the
    /// relay signals through the child handle itself.
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Wait for the program to end, relaying `signals` to it meanwhile.
    ///
    /// One task selects over the signals and the child's exit, so it never
    /// signals a reaped pid (R10): the exit is *observed* without reaping
    /// (`waitid(WNOWAIT)`, the same on Linux and macOS), then relaying stops,
    /// the pid is untracked, and only then is the child reaped. A signal sent
    /// before the spawn waits in the channel and is delivered on the first
    /// turn; one still queued when the exit is observed is dropped.
    ///
    /// Cancellation kills the child with `kill(pid, SIGKILL)` (not `killpg`:
    /// it leads no group) and still waits for it, so the result is the status
    /// it died with. A run in the caller's terminal is not cancelled once it
    /// has started (heph is transparent while it runs); this is the hard
    /// stop for a host going down.
    pub async fn wait(
        mut self,
        mut signals: SignalReceiver,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> std::io::Result<Exit> {
        let pid = self.pid()?;
        let (exited_tx, mut exited) = tokio::sync::oneshot::channel();
        // A plain thread rather than `spawn_blocking`: this also runs inside a
        // plugin cdylib, whose runtime has no blocking pool to rely on. One
        // thread per foreground run, which is one per `heph run`.
        std::thread::Builder::new()
            .name("heph-fg-wait".to_string())
            .spawn(move || drop(exited_tx.send(observe_exit(pid))))?;

        let mut signals_open = true;
        let mut killed = false;
        let mut cancelled = ctoken.cancelled();
        let observed = loop {
            tokio::select! {
                // Exit first: once it is observed, nothing more is sent.
                biased;
                observed = &mut exited => break observed,
                sig = signals.recv(), if signals_open => match sig {
                    Some(sig) => self.relay(sig),
                    None => signals_open = false,
                },
                () = &mut cancelled, if !killed => {
                    killed = true;
                    // SAFETY: the child is not reaped (only this function
                    // reaps it, below), so its pid is still its own.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        };
        // Stop relaying, then untrack, then reap.
        drop(signals);
        drop(self.track.take());
        match observed {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(std::io::Error::other("exit observer thread vanished")),
        }
        // A zombie by now, so this returns at once.
        let status = self.child.wait()?;
        // Neither exited nor signalled is a stop, which `wait` does not report.
        Ok(Exit::from_std(status).unwrap_or(Exit::Code(1)))
    }

    fn pid(&self) -> std::io::Result<libc::pid_t> {
        self.child
            .id()
            .try_into()
            .map_err(|e| std::io::Error::other(format!("child pid out of range: {e}")))
    }

    /// Deliver `sig` to the child unless [`should_relay`] says the terminal
    /// already did.
    fn relay(&self, sig: Signal) {
        let Ok(pid) = self.pid() else { return };
        if !should_relay(sig, &RelayContext::probe(pid)) {
            tracing::debug!(
                signo = sig.as_raw(),
                pid,
                "relay: skipped, the terminal delivered it"
            );
            return;
        }
        // SAFETY: the child is not reaped yet (see `wait`), so `pid` is still
        // this child's; signalling it is the point.
        // D2: `killpg(pgid, sig)` once the tool leads its own group.
        if unsafe { libc::kill(pid, sig.as_raw()) } != 0 {
            tracing::debug!(
                signo = sig.as_raw(),
                pid,
                error = %std::io::Error::last_os_error(),
                "relay: kill failed"
            );
        }
    }
}

impl Drop for ForegroundChild {
    fn drop(&mut self) {
        // Reached unwaited only when `wait`'s future was dropped, or the child
        // was never waited. `try_wait` reaps a finished child; a live one is
        // still unreaped, so its pid is its own and killing it is safe.
        if matches!(self.child.try_wait(), Ok(None)) {
            drop(self.child.kill());
            drop(self.child.wait());
        }
    }
}

/// Block until `pid` has exited, without reaping it.
fn observe_exit(pid: libc::pid_t) -> std::io::Result<()> {
    loop {
        // SAFETY: zeroed `siginfo_t` is a valid out-parameter.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        #[expect(clippy::cast_sign_loss, reason = "pid is a live child's, so positive")]
        let id = pid as libc::id_t;
        // SAFETY: a valid idtype/id pair and a live, writable `siginfo_t`.
        let rc = unsafe { libc::waitid(libc::P_PID, id, &mut info, libc::WEXITED | libc::WNOWAIT) };
        if rc == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// The signals a relay forwards to a foreground child (E17).
pub const RELAYED_SIGNALS: [libc::c_int; 7] = [
    libc::SIGTERM,
    libc::SIGUSR1,
    libc::SIGUSR2,
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGHUP,
    libc::SIGWINCH,
];

/// Signals a terminal generates for its foreground group: relaying one of
/// these while the tool shares that group would deliver it twice.
const TERMINAL_SIGNALS: [libc::c_int; 4] =
    [libc::SIGINT, libc::SIGQUIT, libc::SIGHUP, libc::SIGWINCH];

/// What [`should_relay`] decides on: the process groups at the moment a
/// signal arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayContext {
    /// The controlling terminal's foreground group, or `None` with no
    /// controlling terminal (ENXIO opening `/dev/tty`, ENOTTY, EBADF).
    pub terminal_pgrp: Option<libc::pid_t>,
    /// heph's own process group.
    pub own_pgrp: libc::pid_t,
    /// The tool's process group, or `None` when it cannot be read.
    pub tool_pgrp: Option<libc::pid_t>,
}

impl RelayContext {
    /// Read the groups now, for the tool at `tool_pid`.
    pub fn probe(tool_pid: libc::pid_t) -> Self {
        Self {
            terminal_pgrp: controlling_terminal_pgrp(),
            // SAFETY: no preconditions.
            own_pgrp: unsafe { libc::getpgrp() },
            tool_pgrp: {
                // SAFETY: no preconditions; a bad pid is an ESRCH return.
                let g = unsafe { libc::getpgid(tool_pid) };
                (g >= 0).then_some(g)
            },
        }
    }
}

fn controlling_terminal_pgrp() -> Option<libc::pid_t> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let tty = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOCTTY)
        .open("/dev/tty")
        .ok()?;
    // SAFETY: a live fd.
    let g = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
    (g >= 0).then_some(g)
}

/// Whether a signal addressed to heph should be forwarded to its foreground
/// tool (R2). TERM, USR1 and USR2 always are. INT, QUIT, HUP and WINCH are
/// skipped only when a terminal exists, heph's group is its foreground group,
/// and the tool is in heph's group: then the kernel already delivered it to the
/// tool, and forwarding would make two. Anything outside [`RELAYED_SIGNALS`] is
/// never forwarded.
///
/// Accepted limit (X5): a `kill -INT/-QUIT/-HUP <heph pid>` from elsewhere
/// while heph is the interactive foreground is dropped, because it cannot be
/// told apart from the terminal's own.
// D2: with the tool in a group of its own the terminal never signals it
// directly, so this skip goes and every relayed signal is forwarded.
pub fn should_relay(sig: Signal, ctx: &RelayContext) -> bool {
    let raw = sig.as_raw();
    if !RELAYED_SIGNALS.contains(&raw) {
        return false;
    }
    if !TERMINAL_SIGNALS.contains(&raw) {
        return true;
    }
    let terminal_delivered =
        ctx.terminal_pgrp == Some(ctx.own_pgrp) && ctx.tool_pgrp == Some(ctx.own_pgrp);
    !terminal_delivered
}

/// Die of `sig`, as the program did: restore its default disposition, make
/// sure no core is dumped whatever the caller's limit (the program already
/// dumped its own, if any), unblock it and raise it. Falls back to exiting
/// `128 + sig` if the signal does not kill (it always should).
pub fn reraise(sig: i32) -> ! {
    let no_core = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: a valid resource and a pointer to a live `rlimit`.
    unsafe { libc::setrlimit(libc::RLIMIT_CORE, &no_core) };
    // SAFETY: restoring a default disposition.
    unsafe { libc::signal(sig, libc::SIG_DFL) };
    // SAFETY: a zeroed `sigset_t` is initialized by `sigemptyset` just below.
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: `set` is a live, writable `sigset_t`.
    unsafe { libc::sigemptyset(&mut set) };
    // SAFETY: as above.
    unsafe { libc::sigaddset(&mut set, sig) };
    // SAFETY: `set` is initialized; a null old-mask pointer is allowed.
    unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut()) };
    // SAFETY: raising a signal at this process.
    unsafe { libc::raise(sig) };
    std::process::exit(128 + sig)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;

    fn host_path() -> Vec<(OsString, OsString)> {
        std::env::var_os("PATH")
            .map(|p| (OsString::from("PATH"), p))
            .into_iter()
            .collect()
    }

    fn sh(script: &str) -> ForegroundCommand {
        ForegroundCommand::new(
            "sh",
            vec![OsString::from("-c"), OsString::from(script)],
            host_path(),
            std::env::temp_dir(),
        )
    }

    async fn spawn(cmd: ForegroundCommand) -> ForegroundChild {
        let ctoken = StdCancellationToken::new();
        let (child, _) = spawn_foreground(RunnerRef::local(), cmd, &PathPolicy::default(), &ctoken)
            .await
            .expect("runner");
        child.expect("spawn")
    }

    const SIGNALS: [(libc::c_int, &str); 8] = [
        (libc::SIGTERM, "TERM"),
        (libc::SIGUSR1, "USR1"),
        (libc::SIGUSR2, "USR2"),
        (libc::SIGINT, "INT"),
        (libc::SIGQUIT, "QUIT"),
        (libc::SIGHUP, "HUP"),
        (libc::SIGWINCH, "WINCH"),
        (libc::SIGCHLD, "CHLD"),
    ];

    /// R2's rows. `fg` is "a tty exists and heph's group is its foreground
    /// group, with the tool in heph's group"; only then are the four
    /// terminal-generated signals skipped. `kill 0` and `kill -<pgid>` from a
    /// shell reach heph and the tool alike, and are seen here exactly as the
    /// terminal's own (the fg row): the X5 limit.
    #[test]
    fn relay_skip_table() {
        const HEPH: libc::pid_t = 100;
        const OTHER: libc::pid_t = 200;
        let ctx = |terminal_pgrp, tool_pgrp| RelayContext {
            terminal_pgrp,
            own_pgrp: HEPH,
            tool_pgrp,
        };
        // (row, context, signals forwarded)
        let rows: &[(&str, RelayContext, &[&str])] = &[
            (
                "tty, heph in the foreground, tool in heph's group",
                ctx(Some(HEPH), Some(HEPH)),
                &["TERM", "USR1", "USR2"],
            ),
            (
                "tty, heph in the background",
                ctx(Some(OTHER), Some(HEPH)),
                &["TERM", "USR1", "USR2", "INT", "QUIT", "HUP", "WINCH"],
            ),
            (
                "no tty (ENXIO/ENOTTY/EBADF)",
                ctx(None, Some(HEPH)),
                &["TERM", "USR1", "USR2", "INT", "QUIT", "HUP", "WINCH"],
            ),
            (
                "tool moved to another group or session",
                ctx(Some(HEPH), Some(OTHER)),
                &["TERM", "USR1", "USR2", "INT", "QUIT", "HUP", "WINCH"],
            ),
            (
                "tool's group unreadable",
                ctx(Some(HEPH), None),
                &["TERM", "USR1", "USR2", "INT", "QUIT", "HUP", "WINCH"],
            ),
            (
                "tty foreground is the tool's own group",
                ctx(Some(OTHER), Some(OTHER)),
                &["TERM", "USR1", "USR2", "INT", "QUIT", "HUP", "WINCH"],
            ),
        ];
        for (row, ctx, want) in rows {
            let got: Vec<&str> = SIGNALS
                .iter()
                .filter(|(sig, _)| should_relay(Signal::from_raw(*sig), ctx))
                .map(|(_, name)| *name)
                .collect();
            assert_eq!(&got, want, "row: {row}");
        }
    }

    #[test]
    fn relay_context_probe_reads_this_process() {
        // SAFETY: no preconditions.
        let me = unsafe { libc::getpid() };
        let ctx = RelayContext::probe(me);
        // SAFETY: no preconditions.
        assert_eq!(ctx.own_pgrp, unsafe { libc::getpgrp() });
        assert_eq!(ctx.tool_pgrp, Some(ctx.own_pgrp));
    }

    #[tokio::test]
    async fn foreground_spawn_signal_before_spawn() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        // Queued before the child exists: it must wait in the channel and be
        // delivered once there is a child to deliver it to.
        tx.send(Signal::from_raw(libc::SIGTERM)).expect("send");
        let child = spawn(sh("exec sleep 30")).await;
        let started = std::time::Instant::now();
        let exit = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            child.wait(rx, &StdCancellationToken::new()),
        )
        .await
        .expect("the queued TERM must end the child")
        .expect("wait");
        assert_eq!(exit, Exit::Signal(libc::SIGTERM));
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
    }

    #[tokio::test]
    async fn foreground_spawn_signal_after_exit() {
        // USR1 is ignored in the child (a recorded disposition), so a delivery
        // while it lives is harmless; the wait must report the child's own
        // status, and once it returns nothing can be relayed any more: the
        // receiver is gone with the child.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let cmd = sh("exit 3")
            .with_dispositions(vec![(Signal::from_raw(libc::SIGUSR1), Disposition::Ignore)]);
        let child = spawn(cmd).await;
        let spam = {
            let tx = tx.clone();
            std::thread::spawn(move || {
                while tx.send(Signal::from_raw(libc::SIGUSR1)).is_ok() {
                    std::thread::yield_now();
                }
            })
        };
        let exit = child
            .wait(rx, &StdCancellationToken::new())
            .await
            .expect("wait");
        assert_eq!(exit, Exit::Code(3));
        spam.join().expect("spammer");
        assert!(
            tx.send(Signal::from_raw(libc::SIGTERM)).is_err(),
            "a signal after the exit must have nowhere to go"
        );
    }

    #[tokio::test]
    async fn foreground_spawn_applies_dispositions() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        // Ignored: the self-signal is a no-op and the script runs on.
        let cmd = sh("kill -USR1 $$; exit 4")
            .with_dispositions(vec![(Signal::from_raw(libc::SIGUSR1), Disposition::Ignore)]);
        let exit = spawn(cmd)
            .await
            .wait(rx, &StdCancellationToken::new())
            .await
            .expect("wait");
        assert_eq!(exit, Exit::Code(4));
        // Default: it dies of it.
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let exit = spawn(sh("kill -USR1 $$; exit 4"))
            .await
            .wait(rx, &StdCancellationToken::new())
            .await
            .expect("wait");
        assert_eq!(exit, Exit::Signal(libc::SIGUSR1));
    }

    #[tokio::test]
    async fn foreground_spawn_closed_channel_is_not_a_cancel() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(tx);
        let exit = spawn(sh("sleep 0.2; exit 5"))
            .await
            .wait(rx, &StdCancellationToken::new())
            .await
            .expect("wait");
        assert_eq!(exit, Exit::Code(5));
    }

    #[tokio::test]
    async fn foreground_spawn_cancel_kills_the_pid() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let ctoken = StdCancellationToken::new();
        let child = spawn(sh("exec sleep 30")).await;
        ctoken.cancel();
        let exit =
            tokio::time::timeout(std::time::Duration::from_secs(20), child.wait(rx, &ctoken))
                .await
                .expect("cancel must end the child")
                .expect("wait");
        assert_eq!(exit, Exit::Signal(libc::SIGKILL));
    }

    #[tokio::test]
    async fn foreground_spawn_missing_program_is_not_found() {
        let ctoken = StdCancellationToken::new();
        let (child, _) = spawn_foreground(
            RunnerRef::local(),
            ForegroundCommand::new("heph-no-such-program", vec![], host_path(), "/"),
            &PathPolicy::default(),
            &ctoken,
        )
        .await
        .expect("runner");
        let err = child.expect_err("must not spawn");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    /// Set in the re-executed copies of this test binary that race two first
    /// foreground spawns (see below).
    const FIRST_FORKS_CHILD: &str = "HEPH_EXECRUNNER_FIRST_FORKS_CHILD";

    /// Two foreground spawns racing in a fresh process — one of a program that
    /// does not exist, one of a short `sh` — must both reach `exec`. On macOS
    /// a child forked while the process's libnotify globals were still being
    /// initialized died in libSystem's `atfork` handler with SIGKILL, and std
    /// reported the missing program as spawned (`hproc::fork_safety`). The race
    /// only exists before the first initialization, so each attempt is a
    /// fresh process; this was `foreground_spawn_missing_program_is_not_found`
    /// failing in ~1 of 3 runs of the whole test binary.
    #[test]
    fn first_concurrent_foreground_spawns_reach_exec() {
        if std::env::var_os(FIRST_FORKS_CHILD).is_some() {
            return;
        }
        let exe = std::env::current_exe().expect("current_exe");
        for attempt in 0..30 {
            let out = std::process::Command::new(&exe)
                .args([
                    "--exact",
                    "foreground::tests::first_forks_child",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(FIRST_FORKS_CHILD, "1")
                .output()
                .expect("re-exec the test binary");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success(),
                "attempt {attempt}: {}\nstdout: {stdout}\nstderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(stdout.contains("1 passed"), "child ran nothing: {stdout}");
        }
    }

    /// Only does anything in a re-executed binary (see above).
    #[test]
    fn first_forks_child() {
        if std::env::var_os(FIRST_FORKS_CHILD).is_none() {
            return;
        }
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let runtime = || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime")
        };
        let missing = {
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                runtime().block_on(async {
                    spawn_foreground(
                        RunnerRef::local(),
                        ForegroundCommand::new("heph-no-such-program", vec![], host_path(), "/"),
                        &PathPolicy::default(),
                        &StdCancellationToken::new(),
                    )
                    .await
                    .expect("runner")
                    .0
                    .map(drop)
                })
            })
        };
        let sh_exit = std::thread::spawn(move || {
            barrier.wait();
            runtime().block_on(async {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                drop(tx);
                spawn(sh("exit 5"))
                    .await
                    .wait(rx, &StdCancellationToken::new())
                    .await
                    .expect("wait")
            })
        });
        let err = missing
            .join()
            .expect("missing-program thread")
            .expect_err("a missing program must not spawn");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(sh_exit.join().expect("sh thread"), Exit::Code(5));
    }

    /// Set in the re-executed copy of this test binary, which closes fds 0–2
    /// itself: closing them in the shared test process would hand the next
    /// `open` of any parallel test fd 0.
    const CLOSED_FDS_CHILD: &str = "HEPH_EXECRUNNER_CLOSED_FDS_CHILD";

    #[test]
    fn foreground_spawn_keeps_closed_fds_closed() {
        if std::env::var_os(CLOSED_FDS_CHILD).is_some() {
            return;
        }
        let exe = std::env::current_exe().expect("current_exe");
        let out = std::process::Command::new(exe)
            .args([
                "--exact",
                "foreground::tests::closed_fds_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CLOSED_FDS_CHILD, "1")
            .output()
            .expect("re-exec the test binary");
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "child failed: {}\nstdout: {stdout}\nstderr: {stderr}",
            out.status
        );
        assert!(stdout.contains("1 passed"), "child ran nothing: {stdout}");
    }

    /// Only does anything in the re-executed binary (see above).
    #[test]
    fn closed_fds_child() {
        if std::env::var_os(CLOSED_FDS_CHILD).is_none() {
            return;
        }
        // SAFETY: closing this process's own stdin; nothing else runs here.
        unsafe { libc::close(0) };
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let exit = rt.block_on(async {
            let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
            // Duplicating fd 0 fails with EBADF when it stayed closed;
            // anything heph opened in its place (a pipe, /dev/null) would
            // succeed. (`<&0` alone is a no-op in sh, so it dups to fd 3.)
            spawn(sh(
                "if { true 3<&0; } 2>/dev/null; then exit 1; else exit 0; fi",
            ))
            .await
            .wait(rx, &StdCancellationToken::new())
            .await
            .expect("wait")
        });
        assert_eq!(exit, Exit::Code(0), "fd 0 must reach the child closed");
    }
}
