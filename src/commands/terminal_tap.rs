//! Whether a target's output reached a terminal, recorded from the bytes the
//! CLI's wrappers wrote (D6.1 of the error-reporting spec).
//!
//! The engine keeps a failing target's log and tail whatever happened; what
//! it needs from the CLI is the fact a renderer reads to leave the tail out of
//! the box: at least one byte of the target's output was written to a fd that
//! is a terminal. A target that printed nothing, or whose output went to a
//! file or a pipe (`heph run //t > out`), did not stream. Output that scrolled
//! off the screen cannot be seen from here and counts as shown (E1).
//!
//! The same tap ends a line the target left open on the terminal, so heph's
//! own summary does not glue onto it (D6.3).

use std::io::{self, IsTerminal as _};
use std::os::fd::AsFd;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::task::{Context, Poll};

use tokio::io::{AsyncWrite, AsyncWriteExt as _};

use crate::engine::TerminalUse;

/// What one run's streams wrote to a terminal, shared by its stdout and
/// stderr: the two are usually the same terminal, so the byte that ended up
/// last on it is whichever of them wrote last.
#[derive(Debug, Default)]
pub struct TerminalTap {
    /// 0 while no byte has reached a terminal; else `0x100 | byte`, the last
    /// one written.
    last: AtomicU16,
}

const WROTE: u16 = 0x100;

impl TerminalTap {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// `Relaxed`: both streams are written and read back from the one task
    /// that runs the wrapper, and the value publishes no other data.
    fn record(&self, byte: u8) {
        self.last.store(WROTE | u16::from(byte), Ordering::Relaxed);
    }

    fn streamed(&self) -> bool {
        self.last.load(Ordering::Relaxed) != 0
    }

    /// The last byte on the terminal was not a line break.
    fn ends_open(&self) -> bool {
        let last = self.last.load(Ordering::Relaxed);
        last != 0 && last != WROTE | u16::from(b'\n')
    }
}

/// One of a run's output streams, recording into its [`TerminalTap`] when the
/// fd under it is a terminal.
#[derive(Debug)]
pub struct Tapped<W> {
    inner: W,
    /// `Some` only when `inner` is a terminal: a file or a pipe records nothing.
    tap: Option<Arc<TerminalTap>>,
}

impl<W: AsFd> Tapped<W> {
    pub fn new(inner: W, tap: &Arc<TerminalTap>) -> Self {
        let tap = inner.as_fd().is_terminal().then(|| Arc::clone(tap));
        Self { inner, tap }
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for Tapped<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        let res = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let (Poll::Ready(Ok(n)), Some(tap)) = (&res, &this.tap)
            && let Some(&last) = buf.get(..*n).and_then(<[u8]>::last)
        {
            tap.record(last);
        }
        res
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Once the run is over: flush both streams, end a line the target left open
/// on the terminal (D6.3), and say whether its output streamed there.
///
/// The line break goes to stderr when it is a terminal, where heph's own
/// lines go next; else to stdout. A failed write is no failure of the run (a
/// reader that went away, `| head -1`).
pub async fn finish<O, E>(
    tap: &TerminalTap,
    stdout: &mut Tapped<O>,
    stderr: &mut Tapped<E>,
) -> TerminalUse
where
    O: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
{
    if let Err(error) = stdout.inner.flush().await {
        tracing::debug!(%error, "flushing stdout");
    }
    if let Err(error) = stderr.inner.flush().await {
        tracing::debug!(%error, "flushing stderr");
    }
    if tap.ends_open() {
        let ended = if stderr.tap.is_some() {
            end_line(&mut stderr.inner).await
        } else {
            end_line(&mut stdout.inner).await
        };
        if let Err(error) = ended {
            tracing::debug!(%error, "ending the target's last line");
        }
    }
    TerminalUse {
        streamed: tap.streamed(),
    }
}

async fn end_line(w: &mut (impl AsyncWrite + Unpin)) -> io::Result<()> {
    w.write_all(b"\n").await?;
    w.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_in(dir: &std::path::Path, name: &str) -> tokio::fs::File {
        tokio::fs::File::from_std(std::fs::File::create(dir.join(name)).expect("create"))
    }

    /// Output that went to a file did not stream, however much there was, and
    /// gets no line break of heph's: the file holds exactly what the target
    /// wrote.
    #[tokio::test]
    async fn a_file_is_not_a_terminal() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let tap = TerminalTap::new();
        let mut out = Tapped::new(file_in(dir.path(), "out"), &tap);
        let mut err = Tapped::new(file_in(dir.path(), "err"), &tap);
        out.write_all(b"no newline at the end").await?;
        err.write_all(b"boom").await?;

        let used = finish(&tap, &mut out, &mut err).await;
        assert!(!used.streamed, "nothing reached a terminal");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out"))?,
            "no newline at the end"
        );
        assert_eq!(std::fs::read_to_string(dir.path().join("err"))?, "boom");
        Ok(())
    }

    /// The tap's rule on its own: no byte means not streamed; a last byte other
    /// than `\n` leaves the line open, whichever stream wrote it.
    #[test]
    fn the_last_byte_decides_an_open_line() {
        let tap = TerminalTap::default();
        assert!(!tap.streamed() && !tap.ends_open(), "nothing written");
        tap.record(b'x');
        assert!(tap.streamed() && tap.ends_open());
        tap.record(b'\n');
        assert!(tap.streamed() && !tap.ends_open());
        tap.record(0);
        assert!(tap.ends_open(), "a NUL is a byte too");
    }
}
