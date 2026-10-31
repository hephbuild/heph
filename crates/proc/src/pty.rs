//! Pseudo-terminals: the pair, its size and line discipline, an async master,
//! and the one owner of "the caller's terminal is raw".

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, Interest, ReadBuf};

/// A window size: `(rows, cols)`.
pub type WinSize = (u16, u16);

/// Allocate a new pseudo-terminal pair.
///
/// Returns `(master, slave)` owned fds, both `CLOEXEC` so neither leaks into
/// the sidecar or an unrelated child: a stray copy of the slave would keep the
/// master from ever seeing EOF. The slave is `O_NOCTTY`, so opening it can
/// never make it heph's controlling terminal. The master is non-blocking so it
/// can drive an [`AsyncPty`]; the slave keeps the default blocking mode that
/// child processes expect.
pub fn open_pty() -> io::Result<(OwnedFd, OwnedFd)> {
    // SAFETY: plain open of the multiplexer; the fd is checked below.
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if master < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: posix_openpt handed us ownership of this fd.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    // SAFETY: a live pty master.
    if unsafe { libc::grantpt(master.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a live pty master.
    if unsafe { libc::unlockpt(master.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let name = slave_name(master.as_raw_fd())?;
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let slave = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if slave < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `open` handed us ownership of this fd.
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    set_nonblocking(master.as_raw_fd())?;
    Ok((master, slave))
}

/// The path of `master`'s slave, in a buffer of our own: `ptsname(3)` returns a
/// static one, and two threads opening a PTY at once would read each other's.
#[cfg(target_os = "linux")]
fn slave_name(master: RawFd) -> io::Result<[libc::c_char; 128]> {
    let mut name = [0 as libc::c_char; 128];
    // SAFETY: the buffer and its length are ours; glibc NUL-terminates it.
    let rc = unsafe { libc::ptsname_r(master, name.as_mut_ptr(), name.len()) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(name)
}

/// As the Linux one. macOS's libc crate has no `ptsname_r`, and this ioctl is
/// what it would make.
#[cfg(target_os = "macos")]
fn slave_name(master: RawFd) -> io::Result<[libc::c_char; 128]> {
    // `_IOC(IOC_OUT, 't', 83, 128)`, from <sys/ttycom.h>.
    const TIOCPTYGNAME: libc::c_ulong = 0x4080_7453;
    let mut name = [0 as libc::c_char; 128];
    // SAFETY: the ioctl writes at most the 128 bytes its number encodes.
    if unsafe { libc::ioctl(master, TIOCPTYGNAME, name.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(name)
}

/// Copy the calling terminal's line-discipline settings onto a PTY slave fd.
/// A fresh PTY's termios is unspecified on macOS, so without this the child
/// inherits whatever bits happen to be set — most notably ONLCR is often off,
/// which breaks `\r\n` translation and leaves the caret below the prompt.
pub fn inherit_termios(slave: &OwnedFd) -> io::Result<()> {
    // SAFETY: termios is a POD struct; we hand the kernel a writable pointer
    // and read the bits it set.
    let mut tio: libc::termios = unsafe { std::mem::zeroed() };
    // Try stdin, then stderr, then stdout — whichever is a tty.
    [libc::STDIN_FILENO, libc::STDERR_FILENO, libc::STDOUT_FILENO]
        .into_iter()
        // SAFETY: tcgetattr on a valid fd only writes into tio.
        .find(|&fd| unsafe { libc::tcgetattr(fd, &mut tio) } == 0)
        .ok_or_else(io::Error::last_os_error)?;
    // SAFETY: applying termios to an owned slave fd; tcsetattr only reads tio.
    if unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &tio) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Push a window size onto a PTY master fd. Without this, bash's readline
/// sees a 0x0 terminal and renders prompts at the wrong position. Setting a
/// size that differs makes the kernel signal the PTY's foreground group, which
/// is how a resize reaches the tool.
pub fn set_winsize(fd: &impl AsRawFd, (rows, cols): WinSize) -> io::Result<()> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: fd is valid for the call; ws lives for the call.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The size of the terminal on the caller's fds: stdin, then stderr, then
/// stdout, whichever is a tty that reports one. `None` with no terminal.
pub fn caller_winsize() -> Option<WinSize> {
    [libc::STDIN_FILENO, libc::STDERR_FILENO, libc::STDOUT_FILENO]
        .into_iter()
        .find_map(|fd| {
            // SAFETY: `winsize` is POD; a zeroed one is a valid out-parameter.
            let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
            // SAFETY: TIOCGWINSZ writes into `ws`, which lives for the call.
            let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
            (rc == 0 && ws.ws_row > 0 && ws.ws_col > 0).then_some((ws.ws_row, ws.ws_col))
        })
}

/// Whether this process's group is the foreground job of the terminal on `fd`.
/// `false` when `fd` is no terminal, or the terminal has gone.
pub fn is_foreground(fd: RawFd) -> bool {
    // SAFETY: no preconditions; a bad fd is an error return.
    let fg = unsafe { libc::tcgetpgrp(fd) };
    // SAFETY: no preconditions.
    fg >= 0 && fg == unsafe { libc::getpgrp() }
}

/// Whether the terminal on `fd` still answers `tcgetpgrp`: it has not closed.
pub fn terminal_alive(fd: RawFd) -> bool {
    // SAFETY: no preconditions; a bad fd is an error return.
    unsafe { libc::tcgetpgrp(fd) >= 0 }
}

/// The caller's terminal, and what it looked like before heph touched it. The
/// one owner of its raw mode: restored on drop, so every exit that unwinds puts
/// it back, and always from the termios captured here, so a second stop can
/// never restore the raw one.
#[derive(Debug)]
pub struct RawTerminal {
    fd: RawFd,
    original: libc::termios,
}

// SAFETY-free: `termios` is plain data and `fd` a number; the struct holds no
// pointer. Spelled out because libc's `termios` is not guaranteed `Send` on
// every target's definition.
const _: () = {
    const fn assert_send<T: Send + Sync>() {}
    assert_send::<RawTerminal>();
};

impl RawTerminal {
    /// Record the termios of the terminal on `fd`. Changes nothing.
    pub fn capture(fd: RawFd) -> io::Result<Self> {
        // SAFETY: termios is POD; tcgetattr only writes into it.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: tcgetattr on a bad fd is an error return.
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, original })
    }

    /// Make the terminal raw, computed from the original termios.
    pub fn enter(&self) -> io::Result<()> {
        let mut raw = self.original;
        // SAFETY: `raw` is a valid termios.
        unsafe { libc::cfmakeraw(&mut raw) };
        // SAFETY: tcsetattr only reads `raw`; a bad fd is an error return.
        if unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Put the original termios back. Idempotent; a terminal that has gone is
    /// an error nobody can act on, so it is ignored.
    pub fn restore(&self) {
        // SAFETY: tcsetattr only reads `original`; a bad fd is an error return.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        self.restore();
    }
}

fn set_nonblocking(fd: libc::c_int) -> io::Result<()> {
    // SAFETY: fcntl with F_GETFL/F_SETFL on a valid fd.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: see above.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Async wrapper over a PTY master fd, exposing `AsyncRead` + `AsyncWrite`.
pub struct AsyncPty {
    inner: AsyncFd<OwnedFd>,
}

impl AsyncPty {
    pub fn new(fd: OwnedFd) -> io::Result<Self> {
        Self::with_interest(fd, Interest::READABLE | Interest::WRITABLE)
    }

    /// Use this when the fd was opened with `O_RDONLY` or `O_WRONLY`; mio
    /// rejects registration with EINVAL if the requested interest exceeds the
    /// fd's access mode.
    pub fn with_interest(fd: OwnedFd, interest: Interest) -> io::Result<Self> {
        set_nonblocking(fd.as_raw_fd())?;
        // SAFETY: `fd` is an `OwnedFd`, so it is open, it stays open for as
        // long as the `AsyncFd` owns it (the `AsyncFd` is what closes it), and
        // `as_raw_fd` returns the same descriptor every time.
        let inner = unsafe { AsyncFd::register_with_interest(fd, interest) }?;
        Ok(Self { inner })
    }

    /// One read that never waits, for a caller that must know what is there
    /// *now*: `WouldBlock` means nothing is, and `Ok(0)` that the slave closed.
    /// A poll of the `AsyncRead` side cannot answer that, because the reactor's
    /// readiness is cached and can report `Pending` with data available.
    pub fn read_now(&self, buf: &mut [u8]) -> io::Result<usize> {
        read_master(self.as_raw_fd(), buf)
    }
}

/// A read of a PTY master. It returns EIO (not EOF) on Linux when the slave
/// closes, which is translated to a clean EOF so readers terminate.
fn read_master(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: the caller holds `fd` open; `buf` is writable for its length.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n >= 0 {
        return Ok(n.cast_unsigned());
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EIO) {
        Ok(0)
    } else {
        Err(err)
    }
}

impl AsRawFd for AsyncPty {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.get_ref().as_raw_fd()
    }
}

impl AsyncRead for AsyncPty {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = match self.inner.poll_read_ready(cx) {
                Poll::Ready(Ok(g)) => g,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };
            let res = guard.try_io(|inner| {
                read_master(inner.get_ref().as_raw_fd(), buf.initialize_unfilled())
            });
            match res {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_would_block) => continue,
            }
        }
    }
}

impl AsyncWrite for AsyncPty {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = match self.inner.poll_write_ready(cx) {
                Poll::Ready(Ok(g)) => g,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };
            let res = guard.try_io(|inner| {
                let fd = inner.get_ref().as_raw_fd();
                // SAFETY: fd is valid (held by AsyncFd); buf points to readable bytes.
                let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n.cast_unsigned())
                }
            });
            match res {
                Ok(r) => return Poll::Ready(r),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract the slave's ownership rests on: neither end can leak into
    /// a child, and opening the slave never makes it a controlling terminal.
    #[test]
    fn open_pty_is_cloexec_and_the_master_nonblocking() {
        let (master, slave) = open_pty().expect("open_pty");
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            // SAFETY: a live fd.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert_ne!(flags & libc::FD_CLOEXEC, 0, "fd {fd} is not CLOEXEC");
        }
        // SAFETY: a live fd.
        let fl = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(fl & libc::O_NONBLOCK, 0);
        // SAFETY: a live fd.
        assert_eq!(unsafe { libc::isatty(slave.as_raw_fd()) }, 1);
    }

    /// A second enter and restore are computed from the captured termios, so
    /// a restore after a re-entered raw mode is still the caller's.
    #[test]
    fn raw_terminal_restores_what_it_captured() {
        let (_master, slave) = open_pty().expect("open_pty");
        let read = |fd: RawFd| {
            // SAFETY: termios is POD.
            let mut t: libc::termios = unsafe { std::mem::zeroed() };
            // SAFETY: a live fd and a live out-parameter.
            assert_eq!(unsafe { libc::tcgetattr(fd, &mut t) }, 0);
            t
        };
        let fd = slave.as_raw_fd();
        let before = read(fd);
        let raw = RawTerminal::capture(fd).expect("capture");
        raw.enter().expect("enter");
        assert_eq!(read(fd).c_lflag & libc::ICANON, 0, "raw");
        raw.restore();
        raw.enter().expect("enter again");
        drop(raw);
        let after = read(fd);
        // PENDIN is the line discipline's state, not a setting: BSD sets it
        // itself when ICANON comes back on, to re-scan what is queued.
        assert_eq!(
            after.c_lflag & !libc::PENDIN,
            before.c_lflag & !libc::PENDIN
        );
        assert_eq!(after.c_oflag, before.c_oflag);
    }

    /// What a stop's "read until EAGAIN" rests on: nothing there is `WouldBlock`
    /// rather than a wait, and a closed slave is 0 on both OSes (EIO on Linux).
    #[tokio::test]
    async fn read_now_tells_empty_from_closed() {
        let (master, slave) = open_pty().expect("open_pty");
        let master = AsyncPty::new(master).expect("register");
        let mut buf = [0u8; 8];
        let empty = master.read_now(&mut buf).expect_err("nothing written yet");
        assert_eq!(empty.kind(), io::ErrorKind::WouldBlock);

        let mut writer = std::fs::File::from(slave);
        io::Write::write_all(&mut writer, b"x").expect("write");
        let n = master.read_now(&mut buf).expect("one byte is waiting");
        assert_eq!((n, buf[0]), (1, b'x'));

        drop(writer);
        assert_eq!(master.read_now(&mut buf).expect("closed"), 0);
    }

    #[test]
    fn capture_on_a_non_terminal_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = std::fs::File::create(dir.path().join("f")).expect("file");
        let raw = RawTerminal::capture(f.as_raw_fd());
        raw.expect_err("a file has no termios");
    }
}
