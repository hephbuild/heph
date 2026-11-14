//! The recorder: one OS thread per session that writes what the process does.
//!
//! Everything a build task does to feed it is one `std::sync::mpsc` send (I1):
//! the channel is unbounded, the thread does all the IO, and a recording failure
//! only ever stops the recording. The thread folds its own [`BuildState`], so
//! `status.json` does not depend on which view the command drew.
//!
//! Files (all `0600`, every fd `O_CLOEXEC`):
//!
//! - `events.ndjson`: through a `BufWriter`, flushed 250ms after the first
//!   unflushed byte and at exit; capped at [`EVENTS_CAP`] with a `Truncated`
//!   record, after which only `End`/`InflightWritten` are appended.
//! - `status.json`: at most once a second and only when something changed,
//!   written to a temp file and renamed over.
//!
//! Shutdown (design amendment A8): `End` is appended and flushed, then the final
//! `status.json` (carrying the exit) is written, and only then is the lock
//! released — so "lock free and no `End`" means exactly "crashed". The caller
//! waits for that with a timeout, never a bare `join`.
//!
//! Failure (A9): the "stopped" latch is set *before* the single `warn!`, because
//! that `warn!` reaches the log tap, which would otherwise feed the recorder
//! that is failing. Then the thread drops its receiver, so every later send fails
//! at once and the backlog is freed. The thread must not panic: the process's
//! panic hook leaves raw mode under a live TUI.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use hcore::events::{BuildEvent, EventReceiver, now_unix_ms};
use tokio::sync::mpsc::error::TryRecvError;

use super::cleanup::{self, Retention};
use super::format::{SessionEnd, SessionRecord, SessionRecordKind, SessionSnapshot, files};
use crate::tui::progress::BuildState;

/// `events.ndjson` stops growing past this (A10).
pub const EVENTS_CAP: u64 = 64 * 1024 * 1024;

/// Messages queued and not yet taken by the recorder past which it stops: a
/// disk slower than the event rate must not grow the process without bound.
pub const MAX_PENDING: usize = 500_000;

/// Delay from the first unflushed byte to its flush.
const FLUSH_EVERY: Duration = Duration::from_millis(250);
/// Minimum gap between two `status.json` writes.
const STATUS_EVERY: Duration = Duration::from_secs(1);
/// How long the exiting process waits for `End` to be written.
pub const FINISH_TIMEOUT: Duration = Duration::from_secs(3);

/// What the recorder thread is sent.
#[derive(Debug)]
enum Msg {
    Event(BuildEvent),
    Log {
        at_unix_ms: u64,
        level: &'static str,
        msg: String,
    },
    InflightWritten {
        at_unix_ms: u64,
    },
    End {
        at_unix_ms: u64,
        exit_code: Option<i32>,
        panicked: bool,
        ack: mpsc::SyncSender<()>,
    },
}

/// State shared between the recorder thread and every sender.
#[derive(Debug)]
struct Shared {
    /// The failure latch. Once set, nothing is sent and the thread exits.
    stopped: AtomicBool,
    /// Messages sent and not yet received.
    pending: AtomicUsize,
    /// The owner's `flock`, held until `End` is on disk. Here rather than on the
    /// thread so a recorder that stopped early still holds the session alive
    /// until the process finishes it.
    lock: Mutex<Option<File>>,
}

impl Shared {
    /// Latch the failure, then report it — in that order (A9).
    fn stop(&self, why: &str, error: Option<&io::Error>) {
        if !self.stopped.swap(true, Ordering::AcqRel) {
            match error {
                Some(error) => tracing::warn!(%error, "{why}; this run's session stops recording"),
                None => tracing::warn!("{why}; this run's session stops recording"),
            }
        }
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    fn release_lock(&self) {
        drop(
            self.lock
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
    }
}

/// A handle that feeds the recorder. Cheap to clone; every send is one channel
/// push and two atomics, and never blocks.
#[derive(Debug, Clone)]
pub struct SessionTap {
    tx: mpsc::Sender<Msg>,
    shared: Arc<Shared>,
}

impl SessionTap {
    fn send(&self, msg: Msg) {
        if self.shared.is_stopped() {
            return;
        }
        if self.shared.pending.fetch_add(1, Ordering::Relaxed) >= MAX_PENDING {
            self.shared.pending.fetch_sub(1, Ordering::Relaxed);
            self.shared.stop(
                "the session recorder fell more than 500000 records behind",
                None,
            );
            return;
        }
        if self.tx.send(msg).is_err() {
            // The thread is gone (stopped, or finished): nothing to undo but the
            // count.
            self.shared.pending.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Record a build event, already folded into the view.
    pub fn event(&self, ev: BuildEvent) {
        self.send(Msg::Event(ev));
    }

    /// Record a log line the terminal showed.
    pub fn log(&self, level: &'static str, msg: String) {
        self.send(Msg::Log {
            at_unix_ms: now_unix_ms(),
            level,
            msg,
        });
    }

    /// Record that `inflight.txt` was just rewritten.
    pub fn inflight_written(&self) {
        self.send(Msg::InflightWritten {
            at_unix_ms: now_unix_ms(),
        });
    }

    /// Whether the recording stopped on a failure.
    pub fn is_stopped(&self) -> bool {
        self.shared.is_stopped()
    }
}

/// The build-event receiver a backend consumes, with the session tap on its
/// receive side (A1): each event is handed to the recorder right after the view
/// folded it, by move, so what is recorded is exactly what was folded.
#[derive(Debug)]
pub struct TappedReceiver {
    rx: EventReceiver,
    tap: Option<SessionTap>,
}

impl TappedReceiver {
    pub fn new(rx: EventReceiver, tap: Option<SessionTap>) -> Self {
        Self { rx, tap }
    }

    /// Await the next event, fold it with `apply`, record it. `None` once every
    /// sender is gone. Cancel-safe: the event is taken, folded and recorded in
    /// one poll.
    pub async fn recv(&mut self, apply: impl FnOnce(&BuildEvent)) -> Option<()> {
        let ev = self.rx.recv().await?;
        self.fold(ev, apply);
        Some(())
    }

    /// [`Self::recv`] without waiting.
    pub fn try_recv(&mut self, apply: impl FnOnce(&BuildEvent)) -> Result<(), TryRecvError> {
        let ev = self.rx.try_recv()?;
        self.fold(ev, apply);
        Ok(())
    }

    /// Await the next event and only record it — for a backend that has stopped
    /// folding (the line backend, after its summary) but whose process still
    /// emits.
    pub async fn recv_record_only(&mut self) -> Option<()> {
        let ev = self.rx.recv().await?;
        if let Some(tap) = &self.tap {
            tap.event(ev);
        }
        Some(())
    }

    fn fold(&self, ev: BuildEvent, apply: impl FnOnce(&BuildEvent)) {
        apply(&ev);
        if let Some(tap) = &self.tap {
            tap.event(ev);
        }
    }
}

/// A bug in the recorder, for the test that proves one does not fail the app.
/// Compiled out of every non-test build.
#[cfg(test)]
fn injected_panic() {
    panic!("injected recorder panic");
}

#[cfg(not(test))]
fn injected_panic() {}

/// Test-only fault injection. Always default outside tests.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Faults {
    /// Panic on the first event, as a bug in the recorder would.
    pub(crate) panic_on_event: bool,
}

/// What the recorder thread starts with.
pub(crate) struct RecorderStart {
    pub(crate) dir: PathBuf,
    /// The sessions root, cleaned before recording starts.
    pub(crate) root: PathBuf,
    pub(crate) lock: File,
    pub(crate) events: Box<dyn Write + Send>,
    pub(crate) retention: Retention,
    pub(crate) faults: Faults,
}

/// The owner's handle on a running recorder.
#[derive(Debug)]
pub struct Recorder {
    tap: SessionTap,
    finished: AtomicBool,
}

impl Recorder {
    /// Spawn the recorder thread. The caller has created the directory, written
    /// `meta.json` and taken the lock.
    pub(crate) fn spawn(start: RecorderStart) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            stopped: AtomicBool::new(false),
            pending: AtomicUsize::new(0),
            lock: Mutex::new(Some(start.lock)),
        });
        // The thread logs through whatever subscriber the spawner had — the
        // global one in the binary, a test's own in a test.
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let thread_shared = Arc::clone(&shared);
        let RecorderStart {
            dir,
            root,
            events,
            retention,
            faults,
            ..
        } = start;
        std::thread::Builder::new()
            .name("heph-session-recorder".to_owned())
            .spawn(move || {
                let _dispatch = tracing::dispatcher::set_default(&dispatch);
                match cleanup::cleanup(&root, SystemTime::now(), retention) {
                    Ok(report) if !report.removed.is_empty() => {
                        tracing::debug!(removed = report.removed.len(), "cleaned ended sessions");
                    }
                    Ok(_) => {}
                    Err(e) => tracing::debug!(error = %e, "cannot clean ended sessions"),
                }
                let mut thread = RecorderThread {
                    dir,
                    events: BufWriter::new(events),
                    events_bytes: 0,
                    truncated: false,
                    unflushed_since: None,
                    state: BuildState::new(),
                    end: None,
                    dirty: true,
                    last_status: None,
                    line: Vec::new(),
                    shared: thread_shared,
                    faults,
                };
                thread.run(&rx);
            })?;
        Ok(Self {
            tap: SessionTap { tx, shared },
            finished: AtomicBool::new(false),
        })
    }

    /// A tap feeding this recorder.
    pub fn tap(&self) -> SessionTap {
        self.tap.clone()
    }

    /// Write `End` and release the lock, waiting at most `timeout` for the
    /// recorder to say it did. Idempotent: only the first call does anything.
    ///
    /// If the recorder stopped, or does not answer in time, the lock is released
    /// here with no `End` on disk — a viewer reads that as a lost session, which
    /// is the truth about its records.
    pub fn finish(&self, exit_code: Option<i32>, panicked: bool, timeout: Duration) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        let shared = &self.tap.shared;
        if !shared.is_stopped() {
            let (ack, acked) = mpsc::sync_channel(1);
            let sent = self
                .tap
                .tx
                .send(Msg::End {
                    at_unix_ms: now_unix_ms(),
                    exit_code,
                    panicked,
                    ack,
                })
                .is_ok();
            if sent {
                match acked.recv_timeout(timeout) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
                    Err(RecvTimeoutError::Timeout) => tracing::debug!(
                        timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                        "session recorder did not finish in time"
                    ),
                }
            }
        }
        shared.release_lock();
    }
}

struct RecorderThread {
    dir: PathBuf,
    events: BufWriter<Box<dyn Write + Send>>,
    events_bytes: u64,
    truncated: bool,
    /// When the oldest unflushed byte was written.
    unflushed_since: Option<Instant>,
    state: BuildState,
    end: Option<SessionEnd>,
    /// The state changed since `status.json` was last written.
    dirty: bool,
    last_status: Option<Instant>,
    /// Reused serialization buffer.
    line: Vec<u8>,
    shared: Arc<Shared>,
    faults: Faults,
}

/// Whether the loop goes on.
enum Flow {
    Continue,
    Stop,
}

impl RecorderThread {
    fn run(&mut self, rx: &mpsc::Receiver<Msg>) {
        loop {
            if self.shared.is_stopped() {
                return;
            }
            if let Flow::Stop = self.tick() {
                return;
            }
            let msg = match self.next_deadline() {
                Some(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
                None => rx
                    .recv()
                    .map_err(|_disconnected| RecvTimeoutError::Disconnected),
            };
            match msg {
                Ok(msg) => {
                    self.shared.pending.fetch_sub(1, Ordering::Relaxed);
                    if let Flow::Stop = self.handle(msg, rx) {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                // Every tap dropped without an `End`: flush what there is and
                // stop. The lock stays with whoever finishes the session.
                Err(RecvTimeoutError::Disconnected) => {
                    if self.flush().is_ok() {
                        drop(self.write_status());
                    }
                    return;
                }
            }
        }
    }

    /// The next instant something is due: a flush or a status write.
    fn next_deadline(&self) -> Option<Instant> {
        let flush = self.unflushed_since.map(|t| t + FLUSH_EVERY);
        let status = self.dirty.then(|| {
            self.last_status
                .map_or_else(Instant::now, |t| t + STATUS_EVERY)
        });
        match (flush, status) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Do whatever is due.
    fn tick(&mut self) -> Flow {
        let now = Instant::now();
        if self.unflushed_since.is_some_and(|t| now >= t + FLUSH_EVERY)
            && let Err(e) = self.flush()
        {
            return self.fail("cannot write the session's events", &e);
        }
        if self.dirty
            && self.last_status.is_none_or(|t| now >= t + STATUS_EVERY)
            && let Err(e) = self.write_status()
        {
            return self.fail("cannot write the session's status", &e);
        }
        Flow::Continue
    }

    fn handle(&mut self, msg: Msg, rx: &mpsc::Receiver<Msg>) -> Flow {
        let written = match msg {
            Msg::Event(ev) => {
                if self.faults.panic_on_event {
                    injected_panic();
                }
                self.state.apply(&ev);
                self.dirty = true;
                self.append_capped(&ev)
            }
            Msg::Log {
                at_unix_ms,
                level,
                msg,
            } => self.append_capped(&SessionRecord::session(
                at_unix_ms,
                SessionRecordKind::Log {
                    level: level.to_owned(),
                    msg,
                },
            )),
            Msg::InflightWritten { at_unix_ms } => self
                .append(&SessionRecord::session(
                    at_unix_ms,
                    SessionRecordKind::InflightWritten,
                ))
                .and_then(|()| self.flush()),
            Msg::End {
                at_unix_ms,
                exit_code,
                panicked,
                ack,
            } => {
                self.end_session(rx, at_unix_ms, exit_code, panicked);
                // The finisher may have timed out and gone; nothing to tell it.
                ack.send(()).unwrap_or_default();
                return Flow::Stop;
            }
        };
        match written {
            Ok(()) => Flow::Continue,
            Err(e) => self.fail("cannot write the session's events", &e),
        }
    }

    /// Everything queued before `End`, then `End`, the flush, the final status
    /// and the lock — in that order.
    fn end_session(
        &mut self,
        rx: &mpsc::Receiver<Msg>,
        at_unix_ms: u64,
        exit_code: Option<i32>,
        panicked: bool,
    ) {
        while let Ok(msg) = rx.try_recv() {
            self.shared.pending.fetch_sub(1, Ordering::Relaxed);
            // A second `End` cannot be queued: `finish` sends once.
            if let Flow::Stop = self.handle(msg, rx) {
                return;
            }
        }
        let end = SessionRecord::session(
            at_unix_ms,
            SessionRecordKind::End {
                exit_code,
                panicked,
            },
        );
        if let Err(e) = self.append(&end).and_then(|()| self.flush()) {
            self.fail("cannot write the session's end", &e);
            return;
        }
        self.end = Some(SessionEnd {
            at_unix_ms,
            exit_code,
            panicked,
        });
        if let Err(e) = self.write_status() {
            self.fail("cannot write the session's status", &e);
            return;
        }
        self.shared.release_lock();
    }

    fn fail(&self, why: &str, error: &io::Error) -> Flow {
        self.shared.stop(why, Some(error));
        Flow::Stop
    }

    /// Append a build event or log line, unless the cap is reached; the first
    /// record over it is replaced by `Truncated`.
    fn append_capped(&mut self, record: &impl serde::Serialize) -> io::Result<()> {
        if self.truncated {
            return Ok(());
        }
        if !self.encode(record) {
            return Ok(());
        }
        let len = u64::try_from(self.line.len()).unwrap_or(u64::MAX);
        if self.events_bytes.saturating_add(len) > EVENTS_CAP {
            self.truncated = true;
            self.dirty = true;
            return self.append(&SessionRecord::session(
                now_unix_ms(),
                SessionRecordKind::Truncated,
            ));
        }
        self.write_line()
    }

    /// Append a record regardless of the cap (`End`, `Truncated`,
    /// `InflightWritten`: small, and a viewer needs them).
    fn append(&mut self, record: &impl serde::Serialize) -> io::Result<()> {
        if !self.encode(record) {
            return Ok(());
        }
        self.write_line()
    }

    /// Serialize `record` into the line buffer. `false` (and nothing written)
    /// for a record that does not serialize, which no record type here does.
    fn encode(&mut self, record: &impl serde::Serialize) -> bool {
        self.line.clear();
        if let Err(e) = serde_json::to_writer(&mut self.line, record) {
            tracing::debug!(error = %e, "skipping an unserializable session record");
            return false;
        }
        self.line.push(b'\n');
        true
    }

    fn write_line(&mut self) -> io::Result<()> {
        self.events.write_all(&self.line)?;
        self.events_bytes = self
            .events_bytes
            .saturating_add(u64::try_from(self.line.len()).unwrap_or(u64::MAX));
        if self.unflushed_since.is_none() {
            self.unflushed_since = Some(Instant::now());
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.unflushed_since = None;
        self.events.flush()
    }

    fn write_status(&mut self) -> io::Result<()> {
        let mut snapshot: SessionSnapshot = self.state.snapshot();
        snapshot.truncated = self.truncated;
        snapshot.end = self.end.clone();
        write_atomically(&self.dir, files::STATUS, &snapshot)?;
        self.dirty = false;
        self.last_status = Some(Instant::now());
        Ok(())
    }
}

/// Write `value` as JSON to `dir/name` through a `0600` temp file renamed over
/// it, so a reader sees the old file or the new one, never half of one.
pub(crate) fn write_atomically(
    dir: &Path,
    name: &str,
    value: &impl serde::Serialize,
) -> io::Result<()> {
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(tmp.as_file_mut(), value).map_err(io::Error::other)?;
    tmp.persist(dir.join(name)).map_err(|e| e.error)?;
    Ok(())
}
