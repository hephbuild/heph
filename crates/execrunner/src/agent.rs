//! Agent mode: running a target inside an environment somebody else is holding
//! open.
//!
//! A wrap runner pays its environment's startup cost on every exec. For a
//! `devenv shell` that is seconds, per target. Agent mode pays it once: a
//! long-lived process lives *inside* the environment and forks targets on
//! request.
//!
//! # Why there are two hidden subcommands
//!
//! Only something already inside the environment can create a process there, so
//! heph starts exactly one helper in it — `heph __runner-agent --socket S`.
//! That is the agent.
//!
//! But heph now has a target it did not fork: it cannot `waitpid` it, cannot
//! put it in a process group, and cannot hand it a pipe or a PTY. Everything
//! heph's output streaming and cancellation is built on assumes the target is
//! *this* process's child. So heph forks a small client — `heph
//! __runner-exec` — exactly where it would have forked the target, with stdio
//! already wired. The client hands those descriptors to the agent over
//! `SCM_RIGHTS`, and the agent `dup2`s them onto the target's 0/1/2.
//!
//! ```text
//! heph ──fork──▶ __runner-exec ──socket──▶ __runner-agent ──fork──▶ target
//!                      │          SCM_RIGHTS        │
//!                      └────── its own fds 0,1,2 ───┘
//! ```
//!
//! **Passed, not proxied.** The output bytes never travel through the socket,
//! so none of the bounded-drain or PTY line-discipline handling is re-derived
//! on a new transport. From heph's side an agent-mode target looks like every
//! other target: one process it forked, whose output it reads and whose exit
//! code it trusts. The client is that illusion, and it costs one small process
//! per target.
//!
//! # Framing
//!
//! `SOCK_STREAM` on both platforms — **not** `SOCK_SEQPACKET`, which macOS does
//! not support on `AF_UNIX`. Descriptors passed over a byte stream attach to
//! the byte they rode with, so a reader that reads past that byte associates
//! them with the wrong request. The discipline that makes this safe: the
//! descriptors ride on a single-byte `sendmsg`, and the receiver's `recvmsg`
//! asks for exactly one byte. Everything after is ordinary length-prefixed
//! reads.
//!
//! Frames are binary rather than JSON because argv and env are `OsString`s: a
//! non-UTF-8 argument or environment value is legal on every supported target,
//! and a text encoding would either corrupt it or bloat it several-fold.

use hproc::proc_exec;
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};
use std::ffi::OsString;
use std::io::{IoSlice, IoSliceMut, Read, Write as _};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Environment variable carrying the agent socket path to the client.
///
/// The **only** control value that rides the environment. Everything else —
/// cwd, the ctty flag, argv — travels in the request frame, so the agent's
/// strip is one exact key rather than a prefix scan. A prefix scan would
/// silently rob a target of a legitimately-named variable, and the target's
/// environment would then differ from the one its cache key describes.
pub const SOCK_ENV: &str = "HEPH_RUNNER_SOCK";

/// The `heph __runner-agent` subcommand name.
pub const AGENT_SUBCOMMAND: &str = "__runner-agent";
/// The `heph __runner-exec` subcommand name.
pub const CLIENT_SUBCOMMAND: &str = "__runner-exec";

/// Cap on a single frame, so a corrupt length cannot make either side try to
/// allocate the address space.
const MAX_FRAME: u32 = 64 * 1024 * 1024;

// ---------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------

/// What the client asks the agent to run.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecRequest {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
    /// Make the passed stdin the target's controlling terminal — the `--shell`
    /// path. Mirrors `proc_exec::Spec::ctty`.
    pub ctty: bool,
}

/// How the target ended.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecOutcome {
    Exited(i32),
    /// Killed by a signal. The client re-raises it on itself so heph's
    /// `ExitStatus` reports `WIFSIGNALED` exactly as a local spawn would.
    Signaled(i32),
    /// The agent could not run it at all.
    Failed(String),
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

fn take_u32(r: &mut impl Read) -> std::io::Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn take_bytes(r: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let n = take_u32(r)?;
    if n > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("exec-runner frame field of {n} bytes exceeds the {MAX_FRAME} cap"),
        ));
    }
    let mut buf = vec![0u8; n as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

impl ExecRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4096);
        put_bytes(&mut out, self.program.as_os_str().as_bytes());
        out.extend_from_slice(&(self.args.len() as u32).to_le_bytes());
        for a in &self.args {
            put_bytes(&mut out, a.as_bytes());
        }
        out.extend_from_slice(&(self.env.len() as u32).to_le_bytes());
        for (k, v) in &self.env {
            put_bytes(&mut out, k.as_bytes());
            put_bytes(&mut out, v.as_bytes());
        }
        put_bytes(&mut out, self.cwd.as_os_str().as_bytes());
        out.push(u8::from(self.ctty));
        out
    }

    pub fn decode(mut r: impl Read) -> std::io::Result<Self> {
        let program = PathBuf::from(OsString::from_vec(take_bytes(&mut r)?));
        let argc = take_u32(&mut r)?;
        let mut args = Vec::with_capacity(argc.min(1024) as usize);
        for _ in 0..argc {
            args.push(OsString::from_vec(take_bytes(&mut r)?));
        }
        let envc = take_u32(&mut r)?;
        let mut env = Vec::with_capacity(envc.min(4096) as usize);
        for _ in 0..envc {
            let k = OsString::from_vec(take_bytes(&mut r)?);
            let v = OsString::from_vec(take_bytes(&mut r)?);
            env.push((k, v));
        }
        let cwd = PathBuf::from(OsString::from_vec(take_bytes(&mut r)?));
        let mut ctty = [0u8; 1];
        r.read_exact(&mut ctty)?;
        Ok(Self {
            program,
            args,
            env,
            cwd,
            ctty: ctty[0] != 0,
        })
    }
}

impl ExecOutcome {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        match self {
            Self::Exited(c) => {
                out.push(0);
                out.extend_from_slice(&c.to_le_bytes());
            }
            Self::Signaled(s) => {
                out.push(1);
                out.extend_from_slice(&s.to_le_bytes());
            }
            Self::Failed(m) => {
                out.push(2);
                put_bytes(&mut out, m.as_bytes());
            }
        }
        out
    }

    pub fn decode(mut r: impl Read) -> std::io::Result<Self> {
        let mut kind = [0u8; 1];
        r.read_exact(&mut kind)?;
        let mut n = [0u8; 4];
        let k = kind[0];
        match k {
            0 => {
                r.read_exact(&mut n)?;
                Ok(Self::Exited(i32::from_le_bytes(n)))
            }
            1 => {
                r.read_exact(&mut n)?;
                Ok(Self::Signaled(i32::from_le_bytes(n)))
            }
            2 => Ok(Self::Failed(
                String::from_utf8_lossy(&take_bytes(&mut r)?).into_owned(),
            )),
            other => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown exec-runner outcome tag {other}"),
            )),
        }
    }
}

fn write_frame(sock: &mut UnixStream, body: &[u8]) -> std::io::Result<()> {
    sock.write_all(&(body.len() as u32).to_le_bytes())?;
    sock.write_all(body)?;
    sock.flush()
}

fn read_frame(sock: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let n = take_u32(sock)?;
    if n > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("exec-runner frame of {n} bytes exceeds the {MAX_FRAME} cap"),
        ));
    }
    let mut buf = vec![0u8; n as usize];
    sock.read_exact(&mut buf)?;
    Ok(buf)
}

// ---------------------------------------------------------------------
// Descriptor passing
// ---------------------------------------------------------------------

/// Send stdio over `sock`, riding on a single byte.
fn send_fds(sock: &UnixStream, fds: [BorrowedFd<'_>; 3]) -> std::io::Result<()> {
    let mut space = [std::mem::MaybeUninit::<u8>::uninit(); 256];
    let mut buf = SendAncillaryBuffer::new(&mut space);
    if !buf.push(SendAncillaryMessage::ScmRights(&fds)) {
        return Err(std::io::Error::other(
            "exec-runner: control buffer too small for three descriptors",
        ));
    }
    let iov = [IoSlice::new(&[0u8])];
    let sent = retry_eintr(|| sendmsg(sock.as_fd(), &iov, &mut buf, SendFlags::empty()))?;
    if sent != 1 {
        return Err(std::io::Error::other(format!(
            "exec-runner: descriptor handshake wrote {sent} bytes, expected 1"
        )));
    }
    Ok(())
}

/// Receive exactly the three stdio descriptors.
///
/// Reads **one** byte, which is what keeps the descriptors associated with this
/// request rather than whichever one the reader happened to buffer past.
fn recv_fds(sock: &UnixStream) -> std::io::Result<[OwnedFd; 3]> {
    let mut space = [std::mem::MaybeUninit::<u8>::uninit(); 256];
    let mut buf = RecvAncillaryBuffer::new(&mut space);
    let mut byte = [0u8; 1];
    let mut iov = [IoSliceMut::new(&mut byte)];
    let msg = retry_eintr(|| recvmsg(sock.as_fd(), &mut iov, &mut buf, RecvFlags::empty()))?;
    if msg.bytes == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "exec-runner: client closed before the descriptor handshake",
        ));
    }

    let mut fds: Vec<OwnedFd> = Vec::with_capacity(3);
    for m in buf.drain() {
        if let RecvAncillaryMessage::ScmRights(rights) = m {
            fds.extend(rights);
        }
    }

    // A truncated control message means the kernel closed the descriptors it
    // could not fit. Left unchecked, we would `dup2` whatever happened to land
    // in the array — the agent's own stdio, or another connection's pipe.
    if fds.len() != 3 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "exec-runner: expected 3 descriptors from the client, got {}",
                fds.len()
            ),
        ));
    }

    // Mark them close-on-exec before any other connection can fork.
    //
    // Linux could have done this atomically in `recvmsg` via `MSG_CMSG_CLOEXEC`;
    // macOS has no such flag, so it is a separate syscall on both for one
    // implementation. Without it a concurrently-forked target inherits this
    // request's pipes — heph's reader then never sees EOF, because a process it
    // knows nothing about is holding the write end — and inherits the control
    // socket too, which is what cancellation's EOF depends on. The agent
    // serialises receive-and-spawn (see `serve`) so nothing forks inside this
    // window.
    for fd in &fds {
        rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)?;
    }

    let mut it = fds.into_iter();
    match (it.next(), it.next(), it.next()) {
        (Some(a), Some(b), Some(c)) => Ok([a, b, c]),
        _ => Err(std::io::Error::other(
            "exec-runner: descriptor count changed under us",
        )),
    }
}

fn retry_eintr<T>(mut f: impl FnMut() -> rustix::io::Result<T>) -> std::io::Result<T> {
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(std::io::Error::from(e)),
        }
    }
}

// ---------------------------------------------------------------------
// The client — `heph __runner-exec`
// ---------------------------------------------------------------------

/// Everything the client needs, read from its own process.
///
/// Its argv *is* the target's argv and its environ *is* the target's
/// environment, so nothing is re-encoded on the way in.
fn client_request(argv: Vec<OsString>) -> anyhow::Result<ExecRequest> {
    let mut it = argv.into_iter();
    let program = it
        .next()
        .ok_or_else(|| anyhow::anyhow!("{CLIENT_SUBCOMMAND}: no program after `--`"))?;
    let env = std::env::vars_os()
        .filter(|(k, _)| k != SOCK_ENV)
        .collect::<Vec<_>>();
    Ok(ExecRequest {
        program: PathBuf::from(program),
        args: it.collect(),
        env,
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        ctty: false,
    })
}

/// The `heph __runner-exec` entry point. Never returns.
///
/// Dispatched from the first statement of `main`, before clap, logging or any
/// runtime — the same shape `__supervisor` uses. Its argv is the *target's*
/// argv, so unlike `__supervisor` it must be a pure prefix strip rather than a
/// flag parse, and it must read `args_os` (a non-UTF-8 filename would panic
/// `args()`).
pub fn client_main(argv_after_sep: Vec<OsString>) -> ! {
    match client_run(argv_after_sep) {
        Ok(outcome) => finish(outcome),
        Err(e) => {
            eprintln!("heph {CLIENT_SUBCOMMAND}: {e:#}");
            std::process::exit(126);
        }
    }
}

fn client_run(argv_after_sep: Vec<OsString>) -> anyhow::Result<ExecOutcome> {
    let sock_path = std::env::var_os(SOCK_ENV).ok_or_else(|| {
        anyhow::anyhow!(
            "{SOCK_ENV} is not set. `heph {CLIENT_SUBCOMMAND}` is an internal subcommand heph \
             spawns for a target running under an agent exec runner; it is not meant to be run \
             by hand."
        )
    })?;
    let req = client_request(argv_after_sep)?;

    // SAFETY: fd 0 is this process's own stdin, open for the whole call.
    let stdin = unsafe { BorrowedFd::borrow_raw(0) };
    // SAFETY: fd 1 is this process's own stdout, open for the whole call.
    let stdout = unsafe { BorrowedFd::borrow_raw(1) };
    // SAFETY: fd 2 is this process's own stderr, open for the whole call.
    let stderr = unsafe { BorrowedFd::borrow_raw(2) };
    let fds = [stdin, stdout, stderr];
    exec_via_agent(Path::new(&sock_path), &req, fds)
}

/// Run one request against an agent and wait for its outcome.
///
/// The whole client protocol in one function, so the `__runner-exec` binary and
/// the protocol tests drive exactly the same code — a test that reimplemented
/// the handshake would prove the test right, not the client.
pub fn exec_via_agent(
    socket: &Path,
    req: &ExecRequest,
    fds: [BorrowedFd<'_>; 3],
) -> anyhow::Result<ExecOutcome> {
    let mut sock = start_via_agent(socket, req, fds)?;
    let body = read_frame(&mut sock).map_err(|e| {
        anyhow::anyhow!(
            "await the target's exit status from the agent: {e}. If the agent died, the target's \
             fate is unknown."
        )
    })?;
    Ok(ExecOutcome::decode(&body[..])?)
}

/// The request half of [`exec_via_agent`]: connect, hand over the descriptors,
/// send the request, and return the live connection without waiting.
///
/// Split out because *dropping* the returned socket is precisely what a killed
/// client looks like from the agent's side, and that is the case worth testing
/// deliberately rather than approximating.
pub fn start_via_agent(
    socket: &Path,
    req: &ExecRequest,
    fds: [BorrowedFd<'_>; 3],
) -> anyhow::Result<UnixStream> {
    let mut sock = UnixStream::connect(socket).map_err(|e| {
        anyhow::anyhow!(
            "connect to exec-runner agent at {socket:?}: {e}. The session that owns it may have \
             died."
        )
    })?;
    send_fds(&sock, fds).map_err(|e| anyhow::anyhow!("hand stdio to the agent: {e}"))?;
    write_frame(&mut sock, &req.encode())
        .map_err(|e| anyhow::anyhow!("send exec request to the agent: {e}"))?;
    Ok(sock)
}

/// Start an agent on `socket` in this process, for tests.
///
/// The real agent is a subcommand of the heph binary; this is the same
/// [`serve`] loop without the process boundary, so a test can exercise the
/// descriptor handshake, the fork, and the cancellation escalation without a
/// release build.
pub fn serve_for_test(socket: &Path) -> anyhow::Result<()> {
    serve(socket)
}

/// Exit exactly the way the target did.
fn finish(outcome: ExecOutcome) -> ! {
    match outcome {
        ExecOutcome::Exited(code) => std::process::exit(code),
        ExecOutcome::Signaled(sig) => {
            // Re-raise so heph's `ExitStatus` reports `WIFSIGNALED` rather than
            // an exit code that merely encodes one. Cores are suppressed first:
            // re-raising SIGSEGV would otherwise write a multi-MB core per
            // crashing target, and on macOS spawn ReportCrash.
            //
            let lim = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: setrlimit on this process, at the very end of its life,
            // single-threaded.
            unsafe { libc::setrlimit(libc::RLIMIT_CORE, &lim) };
            // SAFETY: as above — restore the default disposition so the raise
            // actually terminates us the way the target was terminated.
            unsafe {
                libc::signal(sig, libc::SIG_DFL);
            }
            // SAFETY: as above.
            unsafe {
                libc::raise(sig);
            }
            // A signal we could not re-raise (blocked, or SIGKILL, which never
            // reaches us) still has to be distinguishable from success.
            std::process::exit(128 + sig)
        }
        ExecOutcome::Failed(msg) => {
            eprintln!("heph {CLIENT_SUBCOMMAND}: agent could not run the target: {msg}");
            std::process::exit(126)
        }
    }
}

// ---------------------------------------------------------------------
// The agent — `heph __runner-agent`
// ---------------------------------------------------------------------

/// The `heph __runner-agent --socket <path>` entry point.
///
/// Runs until the socket is removed or the process is killed. Dispatched before
/// clap like the client, but it *does* take flags, so it parses its own tiny
/// argument list.
pub fn agent_main(socket: PathBuf) -> ! {
    // Only here, never in `serve` — `serve_for_test` shares that loop in-process,
    // and a test harness's stdin is closed, so watching it there would read EOF
    // immediately and kill the test runner's own process group.
    watch_parent(socket.clone());

    let code = match serve(&socket) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("heph {AGENT_SUBCOMMAND}: {e:#}");
            1
        }
    };
    std::process::exit(code)
}

/// Undo what the shell that launched us left in place.
///
/// The agent's parent is whatever `devenv shell` (or any other environment
/// wrapper) runs commands under, and two pieces of signal state survive
/// `execve`:
///
/// - `SIGCHLD` set to `SIG_IGN` makes the kernel auto-reap, so `waitpid`
///   returns `ECHILD` and **every** target's exit status is unobtainable.
/// - A blocked or ignored `SIGINT` breaks the agent's own cancellation
///   escalation.
///
/// "Assume nothing about the environment" applies most sharply here, because
/// this is the one place heph puts a process inside somebody else's shell.
fn reset_inherited_signal_state() {
    // SAFETY: called once, before any thread is spawned or any child forked.
    unsafe { libc::signal(libc::SIGCHLD, libc::SIG_DFL) };
    // SAFETY: as above. Writing to a closed client socket must be an `EPIPE`
    // this loop can report, not a signal that kills the whole agent.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };

    // SAFETY: `sigset_t` is a plain bitmask; zeroing then `sigemptyset` is the
    // documented way to build an empty set.
    let mut empty: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: `empty` is a live, correctly-sized `sigset_t`.
    unsafe { libc::sigemptyset(&mut empty) };
    // SAFETY: as above; a null oldset is allowed.
    unsafe { libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) };
}

fn serve(socket: &Path) -> anyhow::Result<()> {
    reset_inherited_signal_state();

    // Dispatched at the top of `main`, the agent never reaches heph's own
    // `raise_open_file_limit`. It holds ~4 descriptors per in-flight target,
    // against a macOS soft limit of 256.
    raise_fd_limit();

    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("create agent socket dir {parent:?}: {e}"))?;
    }
    // A hard-killed session leaves its socket file behind; binding over it is
    // safe only because the path carries the session's fingerprint and pid.
    // Absent is the normal case, so the error is genuinely nothing to report.
    _ = std::fs::remove_file(socket);

    let listener = UnixListener::bind(socket)
        .map_err(|e| anyhow::anyhow!("bind agent socket {socket:?}: {e}"))?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("agent runtime: {e}"))?;

    for conn in listener.incoming() {
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "exec-runner agent: accept failed");
                continue;
            }
        };
        // Receive-and-mark runs here, on the accept thread, *before* any spawn
        // is started — so no fork can observe a descriptor between `recvmsg`
        // and its `FD_CLOEXEC`. macOS has neither `MSG_CMSG_CLOEXEC` nor
        // `accept4(SOCK_CLOEXEC)`, so serialising is what makes the two
        // platforms behave the same rather than the Darwin one racing.
        let fds = match recv_fds(&conn) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "exec-runner agent: descriptor handshake failed");
                continue;
            }
        };
        rt.spawn(handle_conn(conn, fds));
    }
    Ok(())
}

/// Exit when heph goes away.
///
/// stdin is a pipe whose write end heph holds for the session's lifetime, so
/// EOF here means the parent is gone. This is the only teardown that cannot be
/// skipped: the OS closes descriptors at process exit whether or not the parent
/// ran any destructor, so it covers a panic, a `process::exit`, and a
/// `SIGKILL` — none of which a teardown hook would ever see.
///
/// Without it a session agent outlives every heph that started one. Each
/// survivor holds a socket, its own process group, and whatever descriptors it
/// was handed, and they accumulate one per build.
///
/// `killpg` on our own group on the way out, so targets mid-flight go too
/// rather than being reparented and left running.
fn watch_parent(socket: PathBuf) {
    std::thread::spawn(move || {
        let mut sink = [0u8; 64];
        loop {
            match std::io::stdin().read(&mut sink) {
                // EOF: the parent's write end is closed.
                Ok(0) => break,
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        _ = std::fs::remove_file(&socket);
        // SAFETY: reads this process's own process-group id.
        let pgrp = unsafe { libc::getpgrp() };
        // SAFETY: our own process group, which `setsid` in the parent made us
        // the leader of.
        unsafe { libc::killpg(pgrp, libc::SIGTERM) };
        std::process::exit(0);
    });
}

fn raise_fd_limit() {
    // SAFETY: `rlimit` is a plain struct of integers.
    let mut lim: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: `lim` is a live, correctly-sized `rlimit`.
    let got = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) };
    if got == 0 {
        lim.rlim_cur = lim.rlim_max;
        // SAFETY: as above; raising the soft limit to the hard one always
        // validates.
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) };
    }
}

async fn handle_conn(conn: UnixStream, fds: [OwnedFd; 3]) {
    let mut conn = conn;
    let outcome = match run_one(&mut conn, fds).await {
        Ok(o) => o,
        Err(e) => ExecOutcome::Failed(format!("{e:#}")),
    };
    if let Err(e) = write_frame(&mut conn, &outcome.encode()) {
        tracing::warn!(error = %e, "exec-runner agent: could not report the outcome");
    }
}

async fn run_one(conn: &mut UnixStream, fds: [OwnedFd; 3]) -> anyhow::Result<ExecOutcome> {
    let body = read_frame(conn).map_err(|e| anyhow::anyhow!("read exec request: {e}"))?;
    let req = ExecRequest::decode(&body[..]).map_err(|e| anyhow::anyhow!("decode request: {e}"))?;

    let [stdin, stdout, stderr] = fds;
    // Built rather than hand-rolled: `Spec`'s `pre_exec` already does the
    // `setsid` + `TIOCSCTTY` this needs, on both backends, async-signal-safely,
    // and `Handle` already owns the SIGCHLD bookkeeping. The agent links
    // `hproc`, so reimplementing fork/dup2 here would be a second copy of
    // something that is already correct.
    let spec = proc_exec::Spec {
        program: req.program.clone(),
        args: req.args,
        // `env_clear` semantics: the target gets exactly what the client
        // forwarded. Inheriting the agent's environment instead would put the
        // developer's ambient state into every build, unhashed, under a
        // fingerprint-pinned cache key.
        env: req.env,
        cwd: req.cwd,
        stdin: proc_exec::StdioSpec::Fd(stdin),
        stdout: proc_exec::StdioSpec::Fd(stdout),
        stderr: proc_exec::StdioSpec::Fd(stderr),
        // Its own session, so cancelling one target cannot reach another's
        // process group — and so this agent's `killpg` reaches the whole tree.
        setsid: true,
        ctty: req.ctty,
    };

    let handle =
        proc_exec::spawn(spec).map_err(|e| anyhow::anyhow!("spawn {:?}: {e}", req.program))?;
    let pgid = handle.pid();

    let cancel = Arc::new(hcore::hasync::StdCancellationToken::new());
    let waiter = handle.spawn_wait(cancel.clone());

    // heph killing the client does *not* kill the target: different session,
    // different tree. Socket EOF is the signal that the client is gone —
    // guaranteed by the kernel even when the client is `SIGKILL`ed — and it is
    // this agent's job to escalate from there.
    let eof = tokio::task::spawn_blocking({
        let peer = conn.try_clone().ok();
        move || {
            if let Some(mut peer) = peer {
                let mut sink = [0u8; 1];
                // Only EOF matters; any byte or error means the same thing.
                _ = peer.read(&mut sink);
            }
        }
    });

    // `&mut waiter` in the select so the arm that loses can still await it —
    // the cancel path has to reap the target it just signalled, or the client
    // is told the target is gone while it is still running.
    let mut waiter = waiter;
    tokio::select! {
        status = &mut waiter => {
            match status {
                Ok(Ok(st)) => Ok(outcome_of(st)),
                Ok(Err(e)) => Ok(ExecOutcome::Failed(format!("wait for target: {e}"))),
                Err(e) => Ok(ExecOutcome::Failed(format!("target wait task: {e}"))),
            }
        }
        _ = eof => {
            // Mirror `proc_exec`'s own escalation so a cancelled agent target
            // dies the way a cancelled local one does.
            killpg(pgid, libc::SIGINT);
            match tokio::time::timeout(proc_exec::CANCEL_GRACE, &mut waiter).await {
                Ok(Ok(Ok(st))) => Ok(outcome_of(st)),
                _ => {
                    killpg(pgid, libc::SIGKILL);
                    // Reap it before answering. heph queues the sandbox for
                    // removal as soon as the client is reaped, so replying
                    // while the target is still alive leaves a process writing
                    // into a directory the cleaner is deleting.
                    _ = waiter.await;
                    Ok(ExecOutcome::Signaled(libc::SIGKILL))
                }
            }
        }
    }
}

fn outcome_of(status: std::process::ExitStatus) -> ExecOutcome {
    use std::os::unix::process::ExitStatusExt as _;
    if let Some(sig) = status.signal() {
        ExecOutcome::Signaled(sig)
    } else {
        ExecOutcome::Exited(status.code().unwrap_or(1))
    }
}

fn killpg(pgid: i32, sig: i32) {
    if pgid <= 0 {
        return;
    }
    // SAFETY: killpg on a pgid this agent created via `setsid`.
    unsafe {
        libc::killpg(pgid, sig);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> ExecRequest {
        ExecRequest {
            program: PathBuf::from("/bin/echo"),
            args: vec![OsString::from("a"), OsString::from("b")],
            env: vec![(OsString::from("K"), OsString::from("V"))],
            cwd: PathBuf::from("/sandbox"),
            ctty: true,
        }
    }

    #[test]
    fn request_round_trips() {
        let r = req();
        let decoded = ExecRequest::decode(&r.encode()[..]).expect("decode");
        assert_eq!(decoded, r);
    }

    /// argv and env are `OsString` all the way through the wire format for one
    /// reason: a non-UTF-8 argument or value is legal on every supported
    /// target, and a text encoding would corrupt it invisibly.
    #[test]
    fn request_preserves_non_utf8_bytes() {
        let raw = OsString::from_vec(vec![0xff, 0xfe, b'x']);
        let mut r = req();
        r.args = vec![raw.clone()];
        r.env = vec![(OsString::from("K"), raw.clone())];
        r.program = PathBuf::from(OsString::from_vec(vec![b'/', 0xff]));

        let decoded = ExecRequest::decode(&r.encode()[..]).expect("decode");
        assert_eq!(decoded.args, vec![raw.clone()]);
        assert_eq!(decoded.env, vec![(OsString::from("K"), raw)]);
        assert_eq!(decoded.program, r.program);
    }

    #[test]
    fn empty_argv_and_env_round_trip() {
        let r = ExecRequest {
            program: PathBuf::from("/bin/true"),
            args: vec![],
            env: vec![],
            cwd: PathBuf::from("/"),
            ctty: false,
        };
        assert_eq!(ExecRequest::decode(&r.encode()[..]).expect("decode"), r);
    }

    #[test]
    fn outcomes_round_trip() {
        for o in [
            ExecOutcome::Exited(0),
            ExecOutcome::Exited(42),
            ExecOutcome::Signaled(11),
            ExecOutcome::Failed("nope".to_string()),
        ] {
            assert_eq!(ExecOutcome::decode(&o.encode()[..]).expect("decode"), o);
        }
    }

    #[test]
    fn a_corrupt_length_is_rejected_rather_than_allocated() {
        let mut bytes = (MAX_FRAME + 1).to_le_bytes().to_vec();
        bytes.extend_from_slice(b"junk");
        let err = ExecRequest::decode(&bytes[..]).expect_err("must reject");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_truncated_frame_is_an_error_not_a_partial_request() {
        let full = req().encode();
        let err = ExecRequest::decode(&full[..full.len() / 2]).expect_err("must reject");
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    /// The socket path is the only control value that rides the environment,
    /// and the client must strip it so the target's environment is exactly what
    /// its cache key describes.
    #[test]
    fn the_client_strips_the_socket_variable() {
        // SAFETY: single-threaded test.
        unsafe { std::env::set_var(SOCK_ENV, "/tmp/whatever.sock") };
        let r = client_request(vec![OsString::from("/bin/true")]).expect("request");
        assert!(!r.env.iter().any(|(k, _)| k == SOCK_ENV));
        // SAFETY: as above.
        unsafe { std::env::remove_var(SOCK_ENV) };
    }
}
