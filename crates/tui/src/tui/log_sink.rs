use std::io::{self, Seek as _, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::Level;
use tracing_subscriber::fmt::MakeWriter;

use crate::tui::session::{RecordReader, SessionRecordKind, SessionSlot};

/// One write to the sink — a formatted log line or a diagnostic — and its
/// number, from 1, in the order the sink took them. The session's `Log`
/// record for it carries the same number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkLine {
    pub seq: u64,
    pub bytes: Vec<u8>,
}

enum Out {
    Direct,
    Buffered { tx: mpsc::UnboundedSender<SinkLine> },
}

/// Everything behind the sink's one lock: numbering a line, recording it and
/// handing it to the printer are one critical section, so the numbers reach
/// the recorder and the printer in the same order, with no gap and no reuse.
struct State {
    out: Out,
    /// The last number handed out; 0 before the first line.
    seq: u64,
}

/// Where the process's terminal output goes, and the session recording it.
///
/// `main` builds one and hands it to the command, beside the session itself.
/// The sink keeps its own reference to the session only to tee every line it
/// prints into it; the session reaches [`run_app`](crate::tui::run_app) as its
/// own argument, never through the sink.
#[derive(Clone)]
pub struct LogSink {
    inner: Arc<Mutex<State>>,
    /// The process's session (design amendment A11). Outside the swapped
    /// output because pausing and resuming the TUI replaces it, and a paused
    /// run must keep recording.
    session: Arc<SessionSlot>,
}

/// Where a full screen took the terminal over: the last line printed before
/// it, and a byte offset in `events.ndjson` at or before the record of every
/// later line. See [`LogSink::replay_mark`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayMark {
    pub last_printed: u64,
    offset: u64,
}

/// What a [`LogSink::replay`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Replayed {
    /// The last number the replay covers: the printer drops every queued line
    /// numbered at most this, and prints the later ones.
    pub through: u64,
    /// Lines printed from `events.ndjson`.
    pub printed: u64,
    /// Lines in the range that were not on disk (past the ceiling, dropped,
    /// or no session to read): the one `WARN` says how many.
    pub skipped: u64,
}

/// How long a replay waits for the recorder to flush.
pub const REPLAY_FLUSH_TIMEOUT: Duration = Duration::from_secs(3);

impl LogSink {
    /// A sink recording into `session` once a run opens it. The same slot is
    /// what the caller hands [`run_app`](crate::tui::run_app).
    pub fn new_direct_with(session: Arc<SessionSlot>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(State {
                out: Out::Direct,
                seq: 0,
            })),
            session,
        }
    }

    /// The session this sink tees into. Crate-private: a command gets the
    /// session as its own argument.
    pub(crate) fn session(&self) -> &Arc<SessionSlot> {
        &self.session
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Number `buf`, record it in the session and print it (or queue it for
    /// the printer), all under the sink's lock. Every write to the sink is a
    /// line the terminal shows, at the terminal's filter (the subscriber
    /// calls the writer only for events that passed it), so each is recorded:
    /// `level` is `None` for a diagnostic.
    fn emit(&self, level: Option<Level>, buf: &[u8]) -> io::Result<()> {
        let (written, stopped) = {
            let mut state = self.lock();
            state.seq += 1;
            let seq = state.seq;
            let stopped = self
                .session
                .record_log(seq, level.map_or("", |l| l.as_str()), buf);
            let written = match &state.out {
                Out::Direct => io::stderr().lock().write_all(buf),
                Out::Buffered { tx } => {
                    drop(tx.send(SinkLine {
                        seq,
                        bytes: buf.to_vec(),
                    }));
                    Ok(())
                }
            };
            (written, stopped)
        };
        // Its `warn!` comes back through this sink: only once the lock is free.
        if let Some(stopped) = stopped {
            stopped.report();
        }
        written
    }

    /// Mark the point where a full screen takes the terminal over. Takes
    /// every line still queued in `rx`, which the caller prints before it
    /// switches, and returns the mark [`replay`](Self::replay) starts from
    /// when the screen closes. While the screen is up the caller keeps only
    /// the numbers of the lines it drops, never their text.
    ///
    /// The offset is read **before** the lock is taken: every line numbered
    /// past the mark is numbered after it, so its record lands after every
    /// byte already on disk. Read after, a record flushed in between would be
    /// behind it and never replayed.
    pub fn replay_mark(
        &self,
        rx: &mut mpsc::UnboundedReceiver<SinkLine>,
    ) -> (Vec<SinkLine>, ReplayMark) {
        let offset = self
            .session
            .replay_source()
            .and_then(|(_, events)| std::fs::metadata(events).ok())
            .map_or(0, |m| m.len());
        let state = self.lock();
        let mut queued = Vec::new();
        while let Ok(line) = rx.try_recv() {
            queued.push(line);
        }
        let mark = ReplayMark {
            last_printed: state.seq,
            offset,
        };
        drop(state);
        (queued, mark)
    }

    /// The lines numbered past `mark.last_printed`, read back from the
    /// session's `events.ndjson` and handed to `print` in order, each once,
    /// with its newline: what a closed full screen kept off the terminal.
    ///
    /// It first takes the latest number, then flushes the recorder through it
    /// (every record numbered up to it was sent before the flush asked, the
    /// numbering and the send being one critical section), then reads from
    /// `mark`'s offset, so it reads only what was written meanwhile. The range
    /// is exactly `(mark.last_printed, through]`. A line in the range not on
    /// disk — past the log ceiling, dropped by a stopped recorder, or no
    /// session — is counted, and one `WARN` says how many were skipped; a
    /// stopped recorder fails the flush at once rather than being waited for.
    ///
    /// Blocking (a flush wait of at most `flush_timeout`, and file IO): call
    /// it off the render task. It must not be called while `print` holds
    /// anything the sink's writers need.
    pub fn replay(
        &self,
        mark: ReplayMark,
        flush_timeout: Duration,
        mut print: impl FnMut(&[u8]),
    ) -> Replayed {
        let through = self.lock().seq;
        let wanted = through.saturating_sub(mark.last_printed);
        if wanted == 0 {
            return Replayed {
                through,
                printed: 0,
                skipped: 0,
            };
        }
        let mut printed = 0;
        let mut why: Option<String> = None;
        match self.session.replay_source() {
            None => why = Some("this run records no session".to_owned()),
            Some((tap, events)) => {
                if let Err(e) = tap.flush(flush_timeout) {
                    why = Some(e.to_string());
                }
                // What is on disk is printed even when the flush failed.
                if let Err(e) = read_lines(&events, mark, through, |bytes| {
                    printed += 1;
                    print(bytes);
                }) {
                    why = Some(format!("reading {}: {e}", events.display()));
                }
            }
        }
        let skipped = wanted.saturating_sub(printed);
        if skipped > 0 {
            match why {
                Some(why) => tracing::warn!(
                    skipped,
                    reason = %why,
                    "{skipped} log lines written while the screen was up are not shown"
                ),
                None => tracing::warn!(
                    skipped,
                    "{skipped} log lines written while the screen was up are not shown: \
                     the session dropped them"
                ),
            }
        }
        Replayed {
            through,
            printed,
            skipped,
        }
    }

    /// Whether ANSI color should be emitted on this sink. The sink owns the
    /// stderr destination, so it is the single source of truth for color
    /// capability — callers configuring a formatter ask the sink rather than
    /// re-deriving the predicate.
    pub fn color_enabled(&self) -> bool {
        crate::tui::color::stderr_color_enabled()
    }

    pub fn switch_to_buffered(&self) -> mpsc::UnboundedReceiver<SinkLine> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.lock().out = Out::Buffered { tx };
        rx
    }

    pub fn switch_to_direct(&self) {
        self.lock().out = Out::Direct;
    }

    /// Write a diagnostic straight to the sink.
    ///
    /// The sink owns the stderr destination and, while the TUI holds the
    /// terminal, buffers into the renderer — so an out-of-band notice from a
    /// plain OS thread lands as a proper line instead of interleaving mid-frame
    /// and corrupting the display. Best-effort: a diagnostic must never take the
    /// run down over a broken pipe.
    pub fn write_diagnostic(&self, text: &str) {
        drop(self.emit(None, text.as_bytes()));
    }
}

/// What every encoded `Log` record contains: the recorder writes compact
/// JSON, and `SessionRecordKind` is tagged by `type`.
const LOG_RECORD: &[u8] = br#""type":"Log""#;

/// Hand `print` each `Log` record of `events` numbered in
/// `(mark.last_printed, through]`, from `mark`'s offset, in order and once
/// each, with its newline back on. Only lines holding [`LOG_RECORD`] are
/// decoded: the build events between them, the bulk of a busy file, are
/// passed over.
fn read_lines(
    events: &Path,
    mark: ReplayMark,
    through: u64,
    mut print: impl FnMut(&[u8]),
) -> io::Result<()> {
    let mut file = std::fs::File::open(events)?;
    file.seek(SeekFrom::Start(mark.offset))?;
    let mut reader = RecordReader::new(io::BufReader::new(file));
    let needle = memchr::memmem::Finder::new(LOG_RECORD);
    let mut last = mark.last_printed;
    let mut line = Vec::new();
    while let Some(record) = reader.next_containing(&needle)? {
        let Some(SessionRecordKind::Log { msg, seq, .. }) = record.session else {
            continue;
        };
        // Numbers only grow in the file; a smaller one was printed already.
        if seq <= last {
            continue;
        }
        if seq > through {
            break;
        }
        line.clear();
        line.extend_from_slice(msg.as_bytes());
        line.push(b'\n');
        print(&line);
        last = seq;
        if seq == through {
            break;
        }
    }
    Ok(())
}

pub struct LogSinkWriter {
    sink: LogSink,
    /// The level of the event being written, when the formatter said
    /// (`make_writer_for`). `None`: not a log event, recorded with no level.
    level: Option<Level>,
}

impl Write for LogSinkWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.sink.emit(self.level, buf)?;
        Ok(buf.len())
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.sink.emit(self.level, buf)
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
    use crate::tui::session::SessionRecord;

    fn log_record(seq: u64, msg: &str) -> Vec<u8> {
        let mut line = serde_json::to_vec(&SessionRecord::session(
            seq,
            SessionRecordKind::Log {
                level: "INFO".into(),
                msg: msg.into(),
                seq,
            },
        ))
        .expect("encode");
        line.push(b'\n');
        line
    }

    /// The replay reads only the `Log` records in the range, however many
    /// build events lie between the mark and the replay, and its prefilter
    /// matches what the recorder writes.
    #[test]
    fn replay_reads_logs_through_a_large_event_volume() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.ndjson");
        let mut bytes = log_record(1, "before the mark");
        assert!(
            memchr::memmem::find(&bytes, LOG_RECORD).is_some(),
            "a Log record holds the prefilter's needle"
        );
        let offset = bytes.len() as u64;
        // A build event naming the needle in a string is escaped, so it is
        // passed over too.
        let event = concat!(
            r#"{"at_unix_ms":5,"kind":{"type":"ResultEnd","addr":"//a:b","#,
            r#""error":"said \"type\":\"Log\""}}"#,
            "\n"
        );
        for seq in 2..=5 {
            for _ in 0..50_000 {
                bytes.extend_from_slice(event.as_bytes());
            }
            bytes.extend_from_slice(&log_record(seq, &format!("line {seq}")));
        }
        std::fs::write(&path, &bytes).expect("write");

        let mark = ReplayMark {
            last_printed: 1,
            offset,
        };
        let mut printed = Vec::new();
        read_lines(&path, mark, 4, |line| printed.push(line.to_vec())).expect("replay");
        assert_eq!(
            printed,
            vec![
                b"line 2\n".to_vec(),
                b"line 3\n".to_vec(),
                b"line 4\n".to_vec()
            ],
            "exactly the range, in order"
        );
    }

    #[test]
    fn buffered_writer_pushes_bytes_through_channel() {
        let sink = LogSink::new_direct_with(Default::default());
        let mut rx = sink.switch_to_buffered();

        let make = MakeLogSink::new(sink.clone());
        let mut w = make.make_writer();
        w.write_all(b"hello\n").expect("write");
        w.write_all(b"world\n").expect("write");

        let first = rx.try_recv().expect("first line");
        let second = rx.try_recv().expect("second line");
        assert_eq!((first.seq, &*first.bytes), (1, &b"hello\n"[..]));
        assert_eq!((second.seq, &*second.bytes), (2, &b"world\n"[..]));
    }

    #[test]
    fn direct_writer_bypasses_channel() {
        let sink = LogSink::new_direct_with(Default::default());
        let make = MakeLogSink::new(sink.clone());
        let mut w = make.make_writer();
        w.write_all(b"x").expect("write");
        // Direct: nothing buffered. If we now switch to buffered, the channel
        // starts empty.
        let mut rx = sink.switch_to_buffered();
        assert!(rx.try_recv().is_err());
    }
}
