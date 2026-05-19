//! Client-side terminal input reader.
//!
//! Lives in the CLI/TUI side rather than in a driver: drivers receive a
//! generic `AsyncRead` over the wrapper, and the client is responsible for
//! sourcing those bytes from wherever the user is typing. This keeps the
//! transport-stdio<->driver-stdio boundary intact for a future where the
//! driver runs in a separate process or on a different host.
//!
//! We cannot use `tokio::io::stdin()`: per tokio's docs, it spawns a global
//! blocking thread parked on `read(0, …)` that cannot be cancelled, so
//! runtime shutdown hangs until the user presses another key. Instead we
//! put fd 0 into non-blocking mode and drive it via `tokio::io::unix::AsyncFd`.
//! On drop we restore the original fd flags so the parent shell still sees
//! a blocking stdin afterwards.
//!
//! Important: this stomps on stdin shared with everything else in the
//! process. Only construct one `TtyReader` at a time.

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, Interest, ReadBuf};

pub struct TtyReader {
    inner: AsyncFd<RawFd>,
    original_flags: libc::c_int,
}

impl TtyReader {
    pub fn from_stdin() -> io::Result<Self> {
        let fd = io::stdin().as_raw_fd();
        // SAFETY: F_GETFL/F_SETFL on stdin — they only read/write file status flags.
        let original_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if original_flags < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: see above.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, original_flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let inner = AsyncFd::with_interest(fd, Interest::READABLE).map_err(|e| {
            // Restore the flags before bubbling the error.
            // SAFETY: same fd, restoring its previous flags.
            unsafe { libc::fcntl(fd, libc::F_SETFL, original_flags) };
            e
        })?;
        Ok(Self {
            inner,
            original_flags,
        })
    }
}

impl Drop for TtyReader {
    fn drop(&mut self) {
        let fd = *self.inner.get_ref();
        // SAFETY: fd still valid (we never owned it); restoring its prior flags.
        unsafe { libc::fcntl(fd, libc::F_SETFL, self.original_flags) };
    }
}

impl AsyncRead for TtyReader {
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
                let fd = *inner.get_ref();
                let unfilled = buf.initialize_unfilled();
                // SAFETY: fd valid, buf points to writable bytes.
                let n = unsafe { libc::read(fd, unfilled.as_mut_ptr().cast(), unfilled.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
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
