use std::io::{self, Write};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::mpsc;
use tracing::Level;
use tracing_subscriber::fmt::MakeWriter;

use crate::tui::session::SessionTap;

enum Inner {
    Direct,
    Buffered { tx: mpsc::UnboundedSender<Vec<u8>> },
}

#[derive(Clone)]
pub struct LogSink {
    inner: Arc<Mutex<Inner>>,
    /// The session's log tap (design amendment A11). Outside `Inner` because
    /// pausing and resuming the TUI replaces `Inner`, and a paused run must keep
    /// recording. Set once, by the first `run_app` that opens a session.
    tap: Arc<OnceLock<SessionTap>>,
}

impl LogSink {
    pub fn new_direct() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::Direct)),
            tap: Arc::new(OnceLock::new()),
        }
    }

    /// Tee every `info!`-and-above line written through this sink into the
    /// session. A second call is ignored: a process has one session.
    pub fn set_session_tap(&self, tap: SessionTap) {
        drop(self.tap.set(tap));
    }

    /// Record a formatted log line of `level` in the session, if the line is
    /// one the terminal shows (`info!` and above). Lossy UTF-8, verbatim
    /// otherwise: a viewer escapes control bytes when it prints.
    fn tee(&self, level: Option<Level>, buf: &[u8]) {
        let (Some(level), Some(tap)) = (level, self.tap.get()) else {
            return;
        };
        // Ordered by verbosity: ERROR < WARN < INFO < DEBUG < TRACE.
        if level > Level::INFO || tap.is_stopped() {
            return;
        }
        let text = String::from_utf8_lossy(buf);
        let text = text.strip_suffix('\n').unwrap_or(&text);
        tap.log(level.as_str(), text.to_owned());
    }

    /// Whether ANSI color should be emitted on this sink. The sink owns the
    /// stderr destination, so it is the single source of truth for color
    /// capability — callers configuring a formatter ask the sink rather than
    /// re-deriving the predicate.
    pub fn color_enabled(&self) -> bool {
        crate::tui::color::stderr_color_enabled()
    }

    pub fn switch_to_buffered(&self) -> mpsc::UnboundedReceiver<Vec<u8>> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut guard = self.inner.lock().expect("log sink poisoned");
        *guard = Inner::Buffered { tx };
        rx
    }

    pub fn switch_to_direct(&self) {
        let mut guard = self.inner.lock().expect("log sink poisoned");
        *guard = Inner::Direct;
    }

    /// Write a diagnostic straight to the sink.
    ///
    /// The sink owns the stderr destination and, while the TUI holds the
    /// terminal, buffers into the renderer — so an out-of-band notice from a
    /// plain OS thread lands as a proper line instead of interleaving mid-frame
    /// and corrupting the display. Best-effort: a diagnostic must never take the
    /// run down over a broken pipe.
    pub fn write_diagnostic(&self, text: &str) {
        drop(self.write_bytes(text.as_bytes()));
    }

    fn write_bytes(&self, buf: &[u8]) -> io::Result<()> {
        let guard = self
            .inner
            .lock()
            .map_err(|_poisoned| io::Error::other("log sink mutex poisoned"))?;
        match &*guard {
            Inner::Direct => {
                let mut stderr = io::stderr().lock();
                stderr.write_all(buf)
            }
            Inner::Buffered { tx } => {
                drop(tx.send(buf.to_vec()));
                Ok(())
            }
        }
    }
}

pub struct LogSinkWriter {
    sink: LogSink,
    /// The level of the event being written, when the formatter said
    /// (`make_writer_for`). `None`: not a log event, never teed.
    level: Option<Level>,
}

impl Write for LogSinkWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.sink.tee(self.level, buf);
        self.sink.write_bytes(buf)?;
        Ok(buf.len())
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.sink.tee(self.level, buf);
        self.sink.write_bytes(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct MakeLogSink {
    sink: LogSink,
}

impl MakeLogSink {
    pub fn new(sink: LogSink) -> Self {
        Self { sink }
    }
}

impl<'a> MakeWriter<'a> for MakeLogSink {
    type Writer = LogSinkWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogSinkWriter {
            sink: self.sink.clone(),
            level: None,
        }
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
        LogSinkWriter {
            sink: self.sink.clone(),
            level: Some(*meta.level()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffered_writer_pushes_bytes_through_channel() {
        let sink = LogSink::new_direct();
        let mut rx = sink.switch_to_buffered();

        let make = MakeLogSink::new(sink.clone());
        let mut w = make.make_writer();
        w.write_all(b"hello\n").expect("write");
        w.write_all(b"world\n").expect("write");

        let first = rx.try_recv().expect("first line");
        let second = rx.try_recv().expect("second line");
        assert_eq!(first, b"hello\n");
        assert_eq!(second, b"world\n");
    }

    #[test]
    fn direct_writer_bypasses_channel() {
        let sink = LogSink::new_direct();
        let make = MakeLogSink::new(sink.clone());
        let mut w = make.make_writer();
        w.write_all(b"x").expect("write");
        // Direct: nothing buffered. If we now switch to buffered, the channel
        // starts empty.
        let mut rx = sink.switch_to_buffered();
        assert!(rx.try_recv().is_err());
    }
}
