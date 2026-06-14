use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tracing_subscriber::fmt::MakeWriter;

enum Inner {
    Direct,
    Buffered { tx: mpsc::UnboundedSender<Vec<u8>> },
}

#[derive(Clone)]
pub struct LogSink {
    inner: Arc<Mutex<Inner>>,
}

impl LogSink {
    pub fn new_direct() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::Direct)),
        }
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
}

impl Write for LogSinkWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.sink.write_bytes(buf)?;
        Ok(buf.len())
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
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
