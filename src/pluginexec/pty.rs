use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, Interest, ReadBuf};

/// Allocate a new pseudo-terminal pair via `openpty(3)`.
///
/// Returns `(master, slave)` owned fds. The master is set non-blocking
/// so it can drive an `AsyncPty`; the slave keeps the default blocking
/// mode that child processes expect.
pub fn open_pty() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    // SAFETY: openpty writes the two fd out-params we own; remaining pointers
    // are NULL, which the man page documents as "use defaults".
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    set_nonblocking(master)?;
    // SAFETY: openpty handed us ownership of master and slave.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    // SAFETY: see above.
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    Ok((master, slave))
}

/// Copy the calling terminal's line-discipline settings (from stdin) onto
/// a PTY slave fd. `openpty(3)` on macOS leaves the slave's termios
/// unspecified, so without this the child inherits whatever bits happen
/// to be set — most notably ONLCR is often off, which breaks `\r\n`
/// translation and leaves the caret below the prompt.
pub fn inherit_termios(slave: &OwnedFd) -> io::Result<()> {
    // SAFETY: termios is a POD struct; we hand the kernel a writable pointer
    // and read the bits it set.
    let mut tio: libc::termios = unsafe { std::mem::zeroed() };
    // Try stdin, then stderr, then stdout — whichever is a tty.
    let src = [libc::STDIN_FILENO, libc::STDERR_FILENO, libc::STDOUT_FILENO]
        .into_iter()
        // SAFETY: tcgetattr on a valid fd only writes into tio.
        .find(|&fd| unsafe { libc::tcgetattr(fd, &mut tio) } == 0)
        .ok_or_else(io::Error::last_os_error)?;
    let _ = src;
    // SAFETY: applying termios to an owned slave fd; tcsetattr only reads tio.
    if unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &tio) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Push a window size onto a PTY master fd. Without this, bash's readline
/// sees a 0x0 terminal and renders prompts at the wrong position.
pub fn set_winsize(fd: &OwnedFd, rows: u16, cols: u16) -> io::Result<()> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: fd is owned and valid; ws lives for the call.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
        Ok(Self {
            inner: AsyncFd::with_interest(fd, interest)?,
        })
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
                let fd = inner.get_ref().as_raw_fd();
                let unfilled = buf.initialize_unfilled();
                // SAFETY: fd is valid (held by AsyncFd); buf points to writable bytes.
                let n = unsafe {
                    libc::read(fd, unfilled.as_mut_ptr().cast(), unfilled.len())
                };
                if n < 0 {
                    let err = io::Error::last_os_error();
                    // PTY masters return EIO (not EOF) when the slave closes;
                    // translate to a clean EOF so readers terminate.
                    if err.raw_os_error() == Some(libc::EIO) {
                        Ok(0usize)
                    } else {
                        Err(err)
                    }
                } else {
                    Ok(n.cast_unsigned())
                }
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
