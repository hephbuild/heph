//! The agent protocol: create a process inside a long-lived environment.
//!
//! Used by `Agent` sessions — a devenv shell held open for the life of the
//! build, with every target's process forked from inside it.
//!
//! ## Why a client process, and not a `spawn` override
//!
//! An earlier sketch had `ExecSession::spawn` overridden for this mode. It
//! cannot be: `spawn` returns a `proc_exec::Handle`, which can only be built for
//! a child of *this* process, and the whole point here is that the child is
//! forked by something else. Making the return type a trait object would undo
//! the reason `prepare` is the seam at all — `proc_exec` would lose its
//! synchronous spawn, its "the spawn is the API" invariant and its OS-divergent
//! reader discipline behind a `dyn`.
//!
//! So `Agent` is *also* a pure spec transformation. The spec is rewritten to run
//! a small **client** which heph spawns as its own child in the ordinary way:
//!
//! ```text
//!   heph ──spawn──> client ──unix socket──> agent (inside `devenv shell`)
//!                     │         SCM_RIGHTS        │
//!                     └── its own 0/1/2 ──────────┘──fork/exec──> the target's process
//! ```
//!
//! The client is a real child, so `Handle`, the drain, the PTY and the
//! supervisor all work unchanged. The child's stdio are the *same* file
//! descriptors — passed, not proxied — so none of `pluginexec`'s bounded-drain
//! and line-discipline handling is re-derived on a new transport, which §4.6 of
//! the design exists to prevent. The client then exits with the child's status,
//! so the exit code a driver sees is the real one.
//!
//! Cancellation falls out: heph kills the client's process group, the agent sees
//! the socket close, and kills the child it forked.

use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// One process-creation request, client → agent.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ExecRequest {
    pub argv: Vec<String>,
    /// The composed environment, applied with `env_clear` on the far side.
    ///
    /// Sent rather than inherited on purpose. The agent lives inside the devenv
    /// shell, so *its* environment is the shell's — letting a child inherit that
    /// would put the developer's ambient `GOFLAGS`, `RUSTFLAGS` and `PATH` tail
    /// into every build, unhashed, under a cache key that reports as
    /// lockfile-pinned. That is the hole the design's M2 analysis missed on the
    /// first pass, and this field is the fix.
    pub env: Vec<(String, String)>,
    pub cwd: String,
    /// `setsid()` in the child, so heph's supervisor can reap the whole tree.
    pub setsid: bool,
}

/// The agent's reply once the child has exited.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum ExecReply {
    /// Exit status. `None` means killed by a signal.
    Exited {
        code: Option<i32>,
    },
    Error {
        message: String,
    },
}

/// Length-prefixed framing, so a reader knows when a message ends without
/// needing the sender to close — the socket stays open for the reply.
fn write_frame(s: &mut UnixStream, bytes: &[u8]) -> std::io::Result<()> {
    s.write_all(&(bytes.len() as u32).to_le_bytes())?;
    s.write_all(bytes)?;
    s.flush()
}

fn read_frame(s: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    // A hostile or corrupt length must not become a multi-gigabyte allocation.
    if len > 8 * 1024 * 1024 {
        return Err(std::io::Error::other(format!(
            "agent frame of {len} bytes is beyond anything this protocol sends"
        )));
    }
    let mut buf = vec![0u8; len];
    s.read_exact(&mut buf)?;
    Ok(buf)
}

/// Send `fds` alongside one byte of payload.
///
/// `SCM_RIGHTS` needs at least one byte of ordinary data to ride with, so the
/// byte is not decoration.
fn send_fds(sock: &UnixStream, fds: &[RawFd]) -> std::io::Result<()> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast::<libc::c_void>(),
        iov_len: 1,
    };
    let payload = std::mem::size_of_val(fds) as u32;
    // SAFETY: a pure size computation over a length — reads no memory.
    let space = unsafe { libc::CMSG_SPACE(payload) };
    let mut cmsg_buf = vec![0u8; space as usize];

    // SAFETY: `msghdr` is a plain C struct whose fields are all set below;
    // zeroing is how the platform headers expect it to be initialized.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr().cast::<libc::c_void>();
    msg.msg_controllen = space as _;

    // SAFETY: `msg.msg_control` points at `cmsg_buf`, which is alive here and
    // `CMSG_SPACE(payload)` bytes long, so a first header fits.
    let cmsg = unsafe { libc::CMSG_FIRSTHDR(&raw const msg) };
    if cmsg.is_null() {
        return Err(std::io::Error::other("no control message header"));
    }

    // SAFETY: `cmsghdr` is a plain C struct with platform-private padding;
    // zeroing is the only way to initialize it before setting the public fields.
    let mut hdr: libc::cmsghdr = unsafe { std::mem::zeroed() };
    hdr.cmsg_level = libc::SOL_SOCKET;
    hdr.cmsg_type = libc::SCM_RIGHTS;
    // SAFETY: another pure size computation.
    hdr.cmsg_len = unsafe { libc::CMSG_LEN(payload) } as _;

    // SAFETY: `cmsg` is non-null (checked) and points into `cmsg_buf`, which has
    // room for a header plus `payload` bytes.
    unsafe { std::ptr::write(cmsg, hdr) };

    // SAFETY: `cmsg` was just initialized as a valid header.
    let data = unsafe { libc::CMSG_DATA(cmsg) }.cast::<RawFd>();
    // SAFETY: `data` points at the header's payload area, sized by `CMSG_SPACE`
    // for exactly `fds.len()` descriptors; source and destination do not overlap.
    unsafe { std::ptr::copy_nonoverlapping(fds.as_ptr(), data, fds.len()) };

    // SAFETY: `msg` describes `iov` and `cmsg_buf`, both alive for this call,
    // with lengths matching what was written into them.
    let sent = unsafe { libc::sendmsg(sock.as_raw_fd(), &raw const msg, 0) };
    if sent < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Receive exactly `want` descriptors sent by [`send_fds`].
fn recv_fds(sock: &UnixStream, want: usize) -> std::io::Result<Vec<RawFd>> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast::<libc::c_void>(),
        iov_len: 1,
    };
    let payload = (std::mem::size_of::<RawFd>() * want) as u32;
    // SAFETY: a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(payload) };
    let mut cmsg_buf = vec![0u8; space as usize];

    // SAFETY: as in `send_fds`.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr().cast::<libc::c_void>();
    msg.msg_controllen = space as _;

    // SAFETY: `msg` describes `iov` and `cmsg_buf`, both alive for this call.
    let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &raw mut msg, 0) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if n == 0 {
        return Err(std::io::Error::other(
            "agent client closed before sending fds",
        ));
    }

    // SAFETY: `recvmsg` succeeded, so `msg_control` holds what the kernel wrote.
    let cmsg = unsafe { libc::CMSG_FIRSTHDR(&raw const msg) };
    if cmsg.is_null() {
        return Err(std::io::Error::other("no descriptors in message"));
    }

    // Trust the kernel's length, not the request: a peer that sent fewer
    // descriptors than asked would otherwise have us read uninitialized bytes
    // out of the buffer and treat them as file descriptors.
    // SAFETY: `cmsg` is non-null and points at a header the kernel wrote.
    let got_len = unsafe { (*cmsg).cmsg_len };
    // SAFETY: a pure size computation.
    let want_len = unsafe { libc::CMSG_LEN(payload) };
    if (got_len as usize) < want_len as usize {
        return Err(std::io::Error::other(format!(
            "expected {want} descriptors, message carries fewer"
        )));
    }

    // SAFETY: `cmsg` is a valid header, checked above to carry `want`
    // descriptors' worth of payload.
    let data = unsafe { libc::CMSG_DATA(cmsg) }.cast::<RawFd>();
    let mut out = vec![0 as RawFd; want];
    // SAFETY: `data` has at least `want` descriptors (length checked above) and
    // `out` has room for exactly that many; the regions do not overlap.
    unsafe { std::ptr::copy_nonoverlapping(data, out.as_mut_ptr(), want) };
    Ok(out)
}

/// Client side: ask the agent to create the process, and return its exit code.
///
/// `stdio` are the descriptors the child should use — normally this process's
/// own 0/1/2, which heph already wired to the target's pipes or PTY.
pub fn request(socket: &Path, req: &ExecRequest, stdio: [RawFd; 3]) -> anyhow::Result<ExecReply> {
    let mut sock = UnixStream::connect(socket).map_err(|e| {
        anyhow::anyhow!(
            "connecting to exec agent at {}: {e} — the environment's session is gone",
            socket.display()
        )
    })?;
    let body = serde_json::to_vec(req)?;
    write_frame(&mut sock, &body)?;
    send_fds(&sock, &stdio)?;
    let reply = read_frame(&mut sock)?;
    Ok(serde_json::from_slice(&reply)?)
}

/// Where an agent for `key` listens.
///
/// Under the runner's own directory rather than a shared `/tmp` name: two heph
/// processes on one machine each open their own session (§10), and a fixed path
/// would have them fight over one socket.
pub fn socket_path(dir: &Path, key: &str) -> PathBuf {
    // The key is a hex hash, but it is not this function's business to assume
    // that — anything that is not a plain filename component is replaced.
    let safe: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(48)
        .collect();
    dir.join(format!("agent-{safe}.sock"))
}

/// Bind the agent's listener, replacing any stale socket left by a crash.
///
/// A leftover socket file is not a live agent: `bind` would fail with
/// `EADDRINUSE` on a path nothing is listening to, so a crashed session would
/// poison its own key until someone deleted the file by hand.
pub fn bind(socket: &Path) -> std::io::Result<UnixListener> {
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match UnixStream::connect(socket) {
        // Someone is already serving this key.
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!("an exec agent is already listening at {}", socket.display()),
        )),
        Err(_) => {
            // Best-effort: the file may not exist, which is the common case.
            // A real removal failure surfaces as the `bind` error below, which
            // is the more useful message anyway.
            drop(std::fs::remove_file(socket));
            UnixListener::bind(socket)
        }
    }
}

/// Serve one connection: read the request, take the descriptors, run the
/// process, reply with its status.
///
/// `run` is the fork/exec, injected so the protocol can be tested without one.
pub fn serve_one(
    mut sock: UnixStream,
    run: &dyn Fn(ExecRequest, [RawFd; 3]) -> anyhow::Result<Option<i32>>,
) -> anyhow::Result<()> {
    let body = read_frame(&mut sock)?;
    let req: ExecRequest = serde_json::from_slice(&body)?;
    let fds = recv_fds(&sock, 3)?;
    // `recv_fds` returns exactly what was asked for or an error, so this cannot
    // fail — but say so with a type rather than an index that "cannot panic".
    let stdio: [RawFd; 3] = fds.clone().try_into().map_err(|got: Vec<RawFd>| {
        std::io::Error::other(format!(
            "agent expected exactly three descriptors, got {}",
            got.len()
        ))
    })?;

    let reply = match run(req, stdio) {
        Ok(code) => ExecReply::Exited { code },
        Err(e) => ExecReply::Error {
            message: format!("{e:#}"),
        },
    };
    // The descriptors were dup'd into this process by the kernel; the child has
    // its own copies by now, so ours must not be leaked into the next request.
    for fd in fds {
        // SAFETY: each descriptor was created by `recvmsg` for this process and
        // is owned by nothing else here — the child got duplicates across its
        // own fork, so closing ours cannot affect it.
        unsafe {
            libc::close(fd);
        }
    }
    write_frame(&mut sock, &serde_json::to_vec(&reply)?)?;
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result,
    clippy::undocumented_unsafe_blocks,
    reason = "restriction lints scoped to production code; tests are exempt"
)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn req() -> ExecRequest {
        ExecRequest {
            argv: vec!["echo".to_string(), "hi".to_string()],
            env: vec![("A".to_string(), "1".to_string())],
            cwd: "/ws".to_string(),
            setsid: true,
        }
    }

    /// The round trip that matters: a request and three live descriptors reach
    /// the far side intact, and the reply comes back on the same socket.
    #[test]
    fn a_request_and_its_descriptors_survive_the_socket() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let sock_path = socket_path(dir.path(), "abc123");
        let listener = bind(&sock_path)?;

        let seen: Arc<Mutex<Option<(ExecRequest, bool)>>> = Arc::new(Mutex::new(None));
        let seen_c = Arc::clone(&seen);
        let server = std::thread::spawn(move || -> anyhow::Result<()> {
            let (conn, _) = listener.accept()?;
            serve_one(conn, &move |r, fds| {
                // Prove the descriptors are usable on this side, not just that
                // three integers arrived: a number that happens to be a valid
                // fd in this process would pass a weaker check.
                let usable = fds
                    .iter()
                    .all(|&fd| unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1);
                *seen_c.lock().expect("lock") = Some((r, usable));
                Ok(Some(7))
            })
        });

        let f = std::fs::File::open("/dev/null")?;
        let fd = f.as_raw_fd();
        let reply = request(&sock_path, &req(), [fd, fd, fd])?;
        server.join().expect("join")?;

        assert_eq!(reply, ExecReply::Exited { code: Some(7) });
        let (got, usable) = seen.lock().expect("lock").clone().expect("served");
        assert_eq!(got, req());
        assert!(usable, "descriptors must arrive open on the far side");
        Ok(())
    }

    #[test]
    fn an_error_on_the_far_side_comes_back_as_an_error() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let sock_path = socket_path(dir.path(), "err");
        let listener = bind(&sock_path)?;
        let server = std::thread::spawn(move || -> anyhow::Result<()> {
            let (conn, _) = listener.accept()?;
            serve_one(conn, &|_, _| anyhow::bail!("no such program"))
        });

        let f = std::fs::File::open("/dev/null")?;
        let fd = f.as_raw_fd();
        let reply = request(&sock_path, &req(), [fd, fd, fd])?;
        server.join().expect("join")?;

        match reply {
            ExecReply::Error { message } => assert!(message.contains("no such program")),
            other => panic!("expected an error reply, got {other:?}"),
        }
        Ok(())
    }

    /// A crash leaves the socket file behind. Without replacing it, `bind`
    /// fails with `EADDRINUSE` on a path nothing is listening to and the
    /// environment's key is poisoned until someone deletes it by hand.
    #[test]
    fn a_stale_socket_file_is_replaced() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let sock_path = socket_path(dir.path(), "stale");
        std::fs::write(&sock_path, b"not a socket")?;
        let _listener = bind(&sock_path)?;
        Ok(())
    }

    /// …but a *live* agent is not replaced, or two sessions would silently
    /// fight over one socket and each would serve half the build.
    #[test]
    fn a_live_agent_is_not_displaced() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let sock_path = socket_path(dir.path(), "live");
        let _first = bind(&sock_path)?;
        assert!(bind(&sock_path).is_err(), "the second bind must refuse");
        Ok(())
    }

    #[test]
    fn socket_paths_stay_inside_the_runners_directory() {
        let p = socket_path(Path::new("/home/x/.heph/agents"), "../../etc/passwd");
        assert_eq!(p.parent(), Some(Path::new("/home/x/.heph/agents")));
        assert!(!p.to_string_lossy().contains(".."));
    }
}
