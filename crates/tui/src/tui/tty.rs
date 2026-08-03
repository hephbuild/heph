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
//! runtime shutdown hangs until the user presses another key. Instead we drive
//! the terminal through `tokio::io::unix::AsyncFd`, which needs the fd to be
//! non-blocking.
//!
//! # Why this reopens the terminal instead of flipping fd 0
//!
//! `O_NONBLOCK` is a property of the *open file description*, not of the file
//! descriptor. A terminal is opened once and dup'd onto fds 0, 1 and 2, so all
//! three normally share one description — and setting `O_NONBLOCK` on fd 0 sets
//! it on **stdout and stderr too**.
//!
//! That is not academic. It is what made `heph run --shell` lose output: with
//! stdout non-blocking, a write that filled the terminal's output queue
//! returned `EAGAIN` instead of blocking, and the target's output was dropped
//! on the floor — silently, and only once a command printed enough to fill the
//! queue, which is why it read as a flaky test rather than as a bug.
//!
//! So when the fd is a terminal we open its device a second time and set
//! `O_NONBLOCK` on *that* description, leaving the one stdout writes through
//! untouched. Reads are equivalent: both descriptions refer to the same
//! terminal, and the input queue is a property of the device.
//!
//! A non-terminal fd 0 (a pipe, a file) cannot be sharing a description with
//! stdout, so there is nothing to protect and the fd is switched in place, with
//! its original flags restored on drop.
//!
//! Still only one at a time. A private description stops this stomping on other
//! *fds*, not on the terminal's single input queue: two live readers both
//! consume from it and a keystroke lands in whichever the kernel picks. The
//! engine is what guarantees one — at most one target per run gets the terminal
//! (see `Engine::result`'s handling of `ResultOptions::interactive`).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, Interest, ReadBuf};

/// Where a [`TtyReader`]'s bytes come from, and what cleanup it owes.
enum Source {
    /// A private second open of the same terminal. Closing it is the whole
    /// cleanup — the caller's fds never saw a flag change.
    Owned(OwnedFd),
    /// The caller's own fd, switched to non-blocking in place because it is not
    /// a terminal. `original_flags` goes back on drop.
    Borrowed {
        fd: RawFd,
        original_flags: libc::c_int,
    },
}

impl AsRawFd for Source {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            Self::Owned(fd) => fd.as_raw_fd(),
            Self::Borrowed { fd, .. } => *fd,
        }
    }
}

pub struct TtyReader {
    inner: AsyncFd<Source>,
}

impl TtyReader {
    pub fn from_stdin() -> io::Result<Self> {
        Self::from_fd(io::stdin().as_raw_fd())
    }

    /// Read the terminal (or pipe) behind `fd`.
    ///
    /// Takes the descriptor rather than reaching for `stdin()` so the
    /// non-blocking policy above is testable against a pty pair, which is the
    /// only shape in which the bug it exists for can be observed.
    fn from_fd(fd: RawFd) -> io::Result<Self> {
        let source = match reopen_terminal(fd) {
            Some(owned) => Source::Owned(owned),
            None => Source::Borrowed {
                fd,
                original_flags: set_nonblocking(fd)?,
            },
        };
        let restore = restore_flags_of(&source);
        AsyncFd::with_interest(source, Interest::READABLE)
            .inspect_err(|_| restore())
            .map(|inner| Self { inner })
    }
}

/// The cleanup a `Source` owes, as a closure so the error path and `Drop` run
/// the same thing.
fn restore_flags_of(source: &Source) -> impl Fn() + use<> {
    let borrowed = match *source {
        Source::Borrowed { fd, original_flags } => Some((fd, original_flags)),
        Source::Owned(_) => None,
    };
    move || {
        if let Some((fd, flags)) = borrowed {
            // SAFETY: the fd is the caller's and still valid — we never owned
            // it, and only put back the flags it arrived with.
            unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };
        }
    }
}

/// Set `O_NONBLOCK` on `fd`, returning the flags it had before.
fn set_nonblocking(fd: RawFd) -> io::Result<libc::c_int> {
    // SAFETY: F_GETFL/F_SETFL on a valid fd only read/write file status flags.
    let original_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if original_flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: see above.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, original_flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(original_flags)
}

/// Open a second, private description of the terminal `fd` refers to, already
/// non-blocking. `None` when `fd` is not a terminal, or when the terminal
/// cannot be reopened — both leave the caller to switch `fd` itself.
///
/// `O_NOCTTY` because acquiring a controlling terminal is emphatically not what
/// this open is for.
fn reopen_terminal(fd: RawFd) -> Option<OwnedFd> {
    // SAFETY: isatty only inspects the fd.
    if unsafe { libc::isatty(fd) } != 1 {
        return None;
    }
    let mut name = [0 as libc::c_char; libc::PATH_MAX as usize];
    // SAFETY: ttyname_r writes a NUL-terminated path into the buffer we own,
    // bounded by the length we pass.
    if unsafe { libc::ttyname_r(fd, name.as_mut_ptr(), name.len()) } != 0 {
        return None;
    }
    // SAFETY: `open` reads the NUL-terminated path ttyname_r just wrote.
    let opened = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY,
        )
    };
    if opened < 0 {
        return None;
    }
    // SAFETY: `open` handed us ownership of this fd.
    let owned = unsafe { OwnedFd::from_raw_fd(opened) };
    // The name is resolved, then opened — two steps, so what came back is only
    // *probably* the same terminal. Compare the device identity and fall back
    // rather than read someone else's keystrokes.
    same_char_device(fd, owned.as_raw_fd()).then_some(owned)
}

/// Whether two fds refer to the same character device.
fn same_char_device(a: RawFd, b: RawFd) -> bool {
    let is_char = |st: &libc::stat| st.st_mode & libc::S_IFMT == libc::S_IFCHR;
    match (fstat(a), fstat(b)) {
        (Some(a), Some(b)) => is_char(&a) && is_char(&b) && a.st_rdev == b.st_rdev,
        _ => false,
    }
}

fn fstat(fd: RawFd) -> Option<libc::stat> {
    // SAFETY: fstat only writes the stat buffer we hand it.
    let mut st = unsafe { std::mem::zeroed::<libc::stat>() };
    // SAFETY: see above; fd is valid for the duration of the call.
    (unsafe { libc::fstat(fd, &mut st) } == 0).then_some(st)
}

impl Drop for TtyReader {
    fn drop(&mut self) {
        restore_flags_of(self.inner.get_ref())();
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
                let fd = inner.get_ref().as_raw_fd();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use tokio::io::AsyncReadExt as _;

    fn flags(fd: RawFd) -> libc::c_int {
        // SAFETY: F_GETFL on a valid fd.
        unsafe { libc::fcntl(fd, libc::F_GETFL) }
    }

    fn is_nonblocking(fd: RawFd) -> bool {
        flags(fd) & libc::O_NONBLOCK != 0
    }

    /// `(master, slave)` of a fresh pty, both owned.
    fn open_pty() -> (OwnedFd, OwnedFd) {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        // SAFETY: openpty writes the two fds we then take ownership of; the
        // remaining pointers are NULL, documented as "use defaults".
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0, "openpty: {}", io::Error::last_os_error());
        // SAFETY: openpty handed us ownership of the master fd.
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        // SAFETY: and of the slave fd.
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        (master, slave)
    }

    /// The regression, stated as the property that was violated: reading a
    /// terminal must not turn the caller's descriptor non-blocking.
    ///
    /// Stdin, stdout and stderr share one open file description whenever the
    /// terminal was opened once and dup'd — the ordinary case — and
    /// `O_NONBLOCK` lives on the description. Setting it here therefore made
    /// *writes to stdout* fail with `EAGAIN` once the terminal's output queue
    /// filled, and `heph run --shell` dropped that output on the floor.
    #[tokio::test]
    async fn reading_a_terminal_leaves_the_callers_fd_blocking() {
        let (_master, slave) = open_pty();
        let fd = slave.as_raw_fd();
        let before = flags(fd);
        assert!(
            !is_nonblocking(fd),
            "precondition: a fresh pty slave is blocking"
        );

        let reader = TtyReader::from_fd(fd).expect("reader over a pty slave");
        assert!(
            !is_nonblocking(fd),
            "the caller's fd (shared with stdout) was switched to non-blocking"
        );
        drop(reader);
        assert_eq!(before, flags(fd), "the caller's fd was left modified");
    }

    /// …and it still reads what is typed. The point of the reopen is that it is
    /// the *same* terminal, so bytes written to the master arrive.
    #[tokio::test]
    async fn a_terminal_reader_still_receives_input() {
        let (master, slave) = open_pty();
        let mut reader = TtyReader::from_fd(slave.as_raw_fd()).expect("reader over a pty slave");

        let mut master_file = std::fs::File::from(master);
        master_file.write_all(b"hi\n").expect("write to pty master");
        master_file.flush().expect("flush pty master");

        let mut buf = [0u8; 3];
        reader.read_exact(&mut buf).await.expect("read from tty");
        assert_eq!(&buf, b"hi\n");
    }

    /// A non-terminal stdin cannot be sharing a description with stdout, so it
    /// takes the in-place path — and gets its flags back on drop.
    #[tokio::test]
    async fn a_pipe_is_switched_in_place_and_restored() {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: pipe writes two fds into the array we own.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: pipe handed us ownership of the read end.
        let read_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        // SAFETY: and of the write end.
        let write_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        let fd = read_end.as_raw_fd();
        let before = flags(fd);

        let mut reader = TtyReader::from_fd(fd).expect("reader over a pipe");
        assert!(
            is_nonblocking(fd),
            "a pipe has to be switched in place — there is no second open of it"
        );

        let mut writer = std::fs::File::from(write_end);
        writer.write_all(b"ok").expect("write to pipe");
        drop(writer);
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).await.expect("read pipe");
        assert_eq!(buf, b"ok");

        drop(reader);
        assert_eq!(before, flags(fd), "the pipe's flags were not restored");
        drop(read_end);
    }
}
