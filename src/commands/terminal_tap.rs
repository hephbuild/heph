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
//!
//! The tap sees the order the bytes were handed to tokio's stdout and stderr,
//! not the order they reached the terminal: each hands its bytes to its own
//! blocking thread, so a stdout chunk and the stderr chunk after it can land
//! either way round (as in `log.txt`, U18). The line break is then decided on
//! the wrong last byte: a missing break or a blank line, never lost output.
//! Likewise `--raw` over several targets at once gives each run its own tap
//! on one shared terminal. Both are cosmetic, and accepted.

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

/// [`finish`], then, while the wrapper still holds the terminal, the one line
/// a run that streamed gets when its process failed (D6.1c), on heph's
/// stderr: `✗ //t · exit status 1 · heph log //t`. Every renderer leaves
/// that failure's box out (`FailureRecord::told_streamed`), so this line is
/// all the run says of it. What the wrapper hands back to the engine.
pub async fn finish_run<O, E>(
    tap: &TerminalTap,
    stdout: &mut Tapped<O>,
    stderr: &mut Tapped<E>,
    result: anyhow::Result<crate::engine::RunEnd>,
) -> anyhow::Result<TerminalUse>
where
    O: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
{
    let used = finish(tap, stdout, stderr).await;
    let end = result?;
    if let Some(rec) = end.exec_failure.as_deref().filter(|_| used.streamed) {
        let color = stderr.tap.is_some() && crate::tui::color::stderr_color_enabled();
        let line = format!(
            "{}\n",
            crate::tui::failure_report::streamed_exec_line(rec, color)
        );
        let told = async {
            stderr.inner.write_all(line.as_bytes()).await?;
            stderr.inner.flush().await
        };
        if let Err(error) = told.await {
            tracing::debug!(%error, "telling the streamed run's failure");
        }
    }
    Ok(used)
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

    /// A stream as `Tapped::new` makes it over a terminal (`tap`) or a file
    /// (`None`), writing into memory.
    fn in_memory(tap: Option<&Arc<TerminalTap>>) -> Tapped<Vec<u8>> {
        Tapped {
            inner: Vec::new(),
            tap: tap.map(Arc::clone),
        }
    }

    /// A line left open on the terminal is ended on stderr, where heph's own
    /// lines go next, when stderr is a terminal; on stdout when only stdout
    /// is. A closed line gets nothing added; nothing written, nothing added.
    #[tokio::test]
    async fn finish_ends_an_open_line_on_the_terminal_stream() -> anyhow::Result<()> {
        // Both terminals: stdout left the line open, stderr gets the break.
        let tap = TerminalTap::new();
        let (mut out, mut err) = (in_memory(Some(&tap)), in_memory(Some(&tap)));
        err.write_all(b"warn\n").await?;
        out.write_all(b"50%").await?;
        let used = finish(&tap, &mut out, &mut err).await;
        assert!(used.streamed);
        assert_eq!(
            (out.inner.as_slice(), err.inner.as_slice()),
            (&b"50%"[..], &b"warn\n\n"[..])
        );

        // Only stdout a terminal: the break goes there, not into the file.
        let tap = TerminalTap::new();
        let (mut out, mut err) = (in_memory(Some(&tap)), in_memory(None));
        out.write_all(b"open").await?;
        err.write_all(b"to a file").await?;
        let used = finish(&tap, &mut out, &mut err).await;
        assert!(used.streamed);
        assert_eq!(
            (out.inner.as_slice(), err.inner.as_slice()),
            (&b"open\n"[..], &b"to a file"[..])
        );

        // A closed line, and nothing written: nothing added.
        let tap = TerminalTap::new();
        let (mut out, mut err) = (in_memory(Some(&tap)), in_memory(Some(&tap)));
        out.write_all(b"done\n").await?;
        let used = finish(&tap, &mut out, &mut err).await;
        assert!(used.streamed);
        assert_eq!(
            (out.inner.as_slice(), err.inner.as_slice()),
            (&b"done\n"[..], &b""[..])
        );
        let tap = TerminalTap::new();
        let (mut out, mut err) = (in_memory(Some(&tap)), in_memory(Some(&tap)));
        let used = finish(&tap, &mut out, &mut err).await;
        assert!(!used.streamed, "a terminal that got no byte did not stream");
        assert!(out.inner.is_empty() && err.inner.is_empty());
        Ok(())
    }

    /// A writer that takes at most `max` bytes per write.
    struct Short {
        got: Vec<u8>,
        max: usize,
    }

    impl AsyncWrite for Short {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let n = buf.len().min(self.max);
            self.got.extend_from_slice(buf.get(..n).unwrap_or_default());
            Poll::Ready(Ok(n))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// A short write records the last byte the writer took, not the last
    /// byte it was offered.
    #[tokio::test]
    async fn a_short_write_records_the_last_byte_taken() -> anyhow::Result<()> {
        let tap = TerminalTap::new();
        let mut out = Tapped {
            inner: Short {
                got: Vec::new(),
                max: 2,
            },
            tap: Some(Arc::clone(&tap)),
        };
        let n = out.write(b"a\nb").await?;
        assert_eq!(n, 2);
        assert!(
            !tap.ends_open(),
            "the writer took `a\\n`: the line is closed"
        );
        assert_eq!(out.inner.got, b"a\n");
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
